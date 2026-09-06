//! RELAY DAW plugin: Share a track to listeners, or Join someone else's.
//!
//! Threads: the host audio thread runs [`RelayPlugin::process`], which only
//! touches preallocated buffers and atomics. Every string, socket, and
//! allocation lives on the fan-out worker ([`fanout`]) or the editor.

mod clipboard;
mod dsp;
mod editor;
mod fanout;
mod local_listen;
mod meter;
mod p2p;
mod signal;
mod spectrum;
mod slug;
mod status;
mod ws;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};

use relay_session::{
    DEFAULT_CONNECT_PORT, MonitorMode, SessionConfig, SessionControl, SessionRole, SessionRuntime,
    WireCodec,
};
use truce::prelude::*;
use truce_core::custom_state::{PersistField, StateCursor};
use truce_egui::EguiEditor;

use fanout::Fanout;

pub(crate) use RelayParamsParamId as P;

static NEXT_SSRC: AtomicU32 = AtomicU32::new(0x5245_0001);

#[derive(ParamEnum, Debug)]
pub enum Mode {
    #[name = "Share"]
    Share,
    #[name = "Join"]
    Join,
}

impl Mode {
    pub const fn role(self) -> SessionRole {
        match self {
            Self::Share => SessionRole::ConnectListen,
            Self::Join => SessionRole::ConnectJoin,
        }
    }

    pub const fn is_share(self) -> bool {
        matches!(self, Self::Share)
    }
}

#[derive(ParamEnum, Debug)]
pub enum Monitor {
    #[name = "Dry"]
    Dry,
    #[name = "Mix"]
    Mix,
    #[name = "Remote"]
    Remote,
}

#[derive(ParamEnum, Debug)]
pub enum Codec {
    #[name = "Opus"]
    Opus,
    #[name = "FLAC"]
    Flac,
    #[name = "PCM"]
    Pcm,
}

impl Codec {
    pub const fn wire(self) -> WireCodec {
        match self {
            Self::Opus => WireCodec::Opus,
            Self::Flac => WireCodec::Flac,
            Self::Pcm => WireCodec::Pcm,
        }
    }
}

/// Free-text session settings saved with the host project.
#[derive(State, Clone, Debug, PartialEq, Eq)]
pub struct SessionPersist {
    /// `host:port` to dial in Join mode.
    pub peer: String,
    /// Public room name; the listen link is `/<name>`.
    pub name: String,
    /// Optional room password. Empty means anyone with the link can listen.
    pub password: String,
}

impl Default for SessionPersist {
    fn default() -> Self {
        Self {
            peer: default_peer(),
            name: slug::new_slug(),
            password: String::new(),
        }
    }
}

pub fn default_peer() -> String {
    format!("127.0.0.1:{DEFAULT_CONNECT_PORT}")
}

/// Shared handle to [`SessionPersist`]. The editor writes it, the fan-out
/// worker reads it, the audio thread never touches it.
#[derive(Clone, Debug, Default)]
pub struct SessionStore(Arc<RwLock<SessionPersist>>);

impl SessionStore {
    pub fn read(&self) -> SessionPersist {
        self.0.read().map(|guard| guard.clone()).unwrap_or_default()
    }

    pub fn update(&self, edit: impl FnOnce(&mut SessionPersist)) {
        if let Ok(mut guard) = self.0.write() {
            edit(&mut guard);
        }
    }
}

impl PersistField for SessionStore {
    fn persist_write(&self, buf: &mut Vec<u8>) {
        self.0.persist_write(buf);
    }

    fn persist_read(&self, cursor: &mut StateCursor) {
        self.0.persist_read(cursor);
    }
}

#[derive(Params)]
pub struct RelayParams {
    #[param(name = "Mode")]
    pub mode: EnumParam<Mode>,

    #[param(name = "Live", default = 1)]
    pub live: BoolParam,

    #[param(name = "Codec")]
    pub codec: EnumParam<Codec>,

    #[param(
        name = "Bitrate",
        range = "discrete(64, 256)",
        default = 192,
        unit = "custom:kbps"
    )]
    pub bitrate: IntParam,

    #[param(name = "FLAC", range = "discrete(0, 8)", default = 5)]
    pub flac_level: IntParam,

    #[param(name = "Port", range = "discrete(1, 65535)", default = 17_492)]
    pub port: IntParam,

    #[param(name = "Monitor", default = 1)]
    pub monitor: EnumParam<Monitor>,

    #[param(
        name = "Send",
        range = "linear(-24, 12)",
        unit = "dB",
        default = 0.0,
        smooth = "exp(5)"
    )]
    pub send: FloatParam,

    #[param(
        name = "Hear",
        range = "linear(-24, 12)",
        unit = "dB",
        default = 0.0,
        smooth = "exp(5)"
    )]
    pub hear: FloatParam,

    #[meter]
    pub meter_left: MeterSlot,

    #[meter]
    pub meter_right: MeterSlot,

    #[persist = "session"]
    pub session: SessionStore,

    #[skip]
    pub control: Arc<SessionControl>,

    #[skip]
    pub spectrum: Arc<spectrum::SpectrumTap>,
}

impl RelayParams {
    /// Push every atomic setting into the session control. Wait-free, so
    /// it runs at the top of every block.
    pub fn publish_atomics(&self) {
        let control = &self.control;
        control.set_linked(self.live.value());
        control.set_web_wanted(true);
        control.set_role(self.mode.value().role());
        control.set_codec(self.codec.value().wire());
        control.set_bitrate_kbps(u32::try_from(self.bitrate.value().clamp(64, 256)).unwrap_or(192));
        control.set_flac_level(u8::try_from(self.flac_level.value().clamp(0, 8)).unwrap_or(5));
        control.set_bind_port(u16::try_from(self.port.value().clamp(1, 65_535)).unwrap_or(1));
    }
}

/// Stateless descriptor; everything mutable lives in [`RelayDsp`].
pub struct RelayPlugin;

#[derive(Default)]
pub struct RelayDsp {
    runtime: Option<SessionRuntime>,
    fanout: Option<Fanout>,
    /// Interleaved send signal (post Send gain).
    wet: Vec<f32>,
    /// Interleaved untouched input.
    dry: Vec<f32>,
    /// Interleaved render target.
    out: Vec<f32>,
    hear_latency: u32,
    device_rate: u32,
}

impl RelayDsp {
    fn ensure_fanout(&mut self, params: &RelayParams) {
        if self.fanout.as_ref().is_some_and(Fanout::is_alive) {
            return;
        }
        self.fanout = Some(Fanout::spawn(
            Arc::clone(&params.control),
            params.session.clone(),
        ));
    }

    fn rebuild_runtime(&mut self, params: &RelayParams, rate: u32, max_block: usize) {
        self.runtime = None;
        self.device_rate = rate;
        let mode = params.mode.value();
        let config = SessionConfig {
            mode: mode.role().session_mode(),
            device_rate_hz: rate as usize,
            frame_duration: dsp::frame_from_host(rate as usize, max_block),
            lan: params.codec.value().wire().is_pcm(),
            ssrc: NEXT_SSRC.fetch_add(1, Ordering::Relaxed),
            monitor: MonitorMode::Remote,
        };
        let Ok(runtime) = SessionRuntime::start_with(config, Arc::clone(&params.control)) else {
            self.hear_latency = 0;
            params
                .control
                .set_last_error("session engine failed to start");
            return;
        };
        params.control.clear_last_error();
        self.hear_latency = if mode.is_share() {
            0
        } else {
            runtime.playback_target_frames()
        };
        self.runtime = Some(runtime);
    }
}

impl PluginLogic for RelayPlugin {
    type Params = RelayParams;
    type DspState = RelayDsp;

    const PRESERVE_DSP_STATE: bool = true;

    fn init(params: &RelayParams, _cx: &InitContext) -> RelayDsp {
        let mut state = RelayDsp::default();
        state.ensure_fanout(params);
        state
    }

    fn reset(state: &mut RelayDsp, params: &RelayParams, config: &AudioConfig) {
        params.publish_atomics();
        state.ensure_fanout(params);

        let samples = config.max_block_size.saturating_mul(2).max(2);
        if state.wet.len() < samples {
            state.wet.resize(samples, 0.0);
            state.dry.resize(samples, 0.0);
            state.out.resize(samples, 0.0);
        }

        let rate = config.sample_rate.round().clamp(8_000.0, 192_000.0) as u32;
        params.control.set_device_rate_hz(rate);
        params.spectrum.rate.store(rate, Ordering::Relaxed);
        params.spectrum.audio.clear();
        params
            .control
            .set_block_frames(u32::try_from(config.max_block_size).unwrap_or(u32::MAX));
        if state.runtime.is_none() || state.device_rate != rate {
            state.rebuild_runtime(params, rate, config.max_block_size);
        }
    }

    fn latency(state: &RelayDsp) -> u32 {
        state.hear_latency
    }

    fn process(
        state: &mut RelayDsp,
        params: &RelayParams,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        params.publish_atomics();

        let frames = buffer.num_samples();
        let needed = frames.saturating_mul(2);
        if needed == 0 {
            return ProcessStatus::Normal;
        }
        params
            .control
            .set_block_frames(u32::try_from(frames).unwrap_or(u32::MAX));
        if needed > state.wet.len() {
            dsp::host_passthrough(buffer, frames);
            return ProcessStatus::Normal;
        }

        let wet = &mut state.wet[..needed];
        let dry = &mut state.dry[..needed];
        let out = &mut state.out[..needed];

        dsp::copy_inputs(buffer, frames, dry);
        wet.copy_from_slice(dry);
        dsp::apply_gain(wet, db_to_linear(params.send.read_after(frames)));

        let rendered = if let Some(runtime) = state.runtime.as_mut() {
            let _ = runtime.process_capture(wet);
            runtime.render(out, dry).rendered_samples
        } else {
            out.fill(0.0);
            0
        };

        if params.mode.value().is_share() {
            dsp::write_outputs(buffer, frames, dry);
        } else {
            dsp::apply_gain(out, db_to_linear(params.hear.read_after(frames)));
            match params.monitor.value() {
                Monitor::Dry => dsp::write_outputs(buffer, frames, dry),
                Monitor::Remote => {
                    dsp::splice_dry(out, dry, rendered);
                    dsp::write_outputs(buffer, frames, out);
                }
                Monitor::Mix => {
                    dsp::mix_into(out, dry);
                    dsp::write_outputs(buffer, frames, out);
                }
            }
        }

        if params.spectrum.active.load(Ordering::Relaxed) {
            params.spectrum.audio.push_frames(wet);
        }
        let (left, right) = dsp::stereo_peaks(wet);
        context.set_meter(P::MeterLeft, left);
        context.set_meter(P::MeterRight, right);
        ProcessStatus::Normal
    }

    fn editor(params: Arc<RelayParams>) -> Box<dyn Editor> {
        Box::new(
            EguiEditor::with_ui(params, editor::WINDOW, editor::RelayUi::default())
                .resizable(false)
                .with_context_setup(editor::setup_context)
                .with_visuals(editor::visuals()),
        )
    }
}

truce::plugin! {
    logic: RelayPlugin,
    params: RelayParams,
}

truce::enable_rt_paranoid!();

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use truce_test::{InputSource, assertions, driver};

    #[test]
    fn info_is_valid() {
        truce_test::assert_valid_info::<Plugin>();
    }

    #[test]
    fn bus_config_effect() {
        truce_test::assert_bus_config_effect::<Plugin>();
    }

    #[test]
    fn has_editor() {
        truce_test::assert_has_editor::<Plugin>();
    }

    #[test]
    fn share_passes_input_through() {
        let result = driver!(Plugin)
            .duration(Duration::from_millis(80))
            .input(InputSource::Constant(0.25))
            .run();
        assertions::assert_no_nans(&result);
        assertions::assert_nonzero(&result);
        assertions::assert_peak_below(&result, 0.26);
    }

    #[test]
    fn share_output_ignores_send_and_hear() {
        let result = driver!(Plugin)
            .set_param(P::Send, 1.0)
            .set_param(P::Hear, 1.0)
            .duration(Duration::from_millis(80))
            .input(InputSource::Constant(0.25))
            .run();
        assertions::assert_no_nans(&result);
        assertions::assert_nonzero(&result);
        assertions::assert_peak_below(&result, 0.26);
    }

    #[test]
    fn join_mix_keeps_dry_when_remote_is_silent() {
        let result = driver!(Plugin)
            .set_param(P::Mode, 1.0)
            .duration(Duration::from_millis(80))
            .input(InputSource::Constant(0.25))
            .run();
        assertions::assert_no_nans(&result);
        assertions::assert_nonzero(&result);
        assertions::assert_peak_below(&result, 0.26);
    }

    #[test]
    fn join_remote_underrun_falls_back_to_dry() {
        let result = driver!(Plugin)
            .set_param(P::Mode, 1.0)
            .set_param(P::Monitor, 1.0)
            .duration(Duration::from_millis(80))
            .input(InputSource::Constant(0.25))
            .run();
        assertions::assert_no_nans(&result);
        assertions::assert_nonzero(&result);
        assertions::assert_peak_below(&result, 0.26);
    }

    #[test]
    fn process_is_allocation_free() {
        truce_test::assert_no_audio_alloc(|| {
            driver!(Plugin)
                .duration(Duration::from_millis(40))
                .input(InputSource::Constant(0.25))
                .run()
        });
    }

    #[cfg(feature = "lv2")]
    #[test]
    fn lv2_wrapper_glue_is_allocation_free() {
        assert_eq!(truce_lv2::rt_paranoid_smoke::<Plugin>(), 0);
    }

    #[test]
    fn state_round_trips() {
        truce_test::assert_state_round_trip::<Plugin>();
    }

    #[test]
    fn session_persist_round_trips() {
        let params = RelayParams::new();
        params.session.update(|session| {
            session.peer = "10.0.0.7:17492".into();
            session.name = "late-night-mix".into();
            session.password = "mix-secret".into();
        });
        let blob = params.serialize_persist();

        let fresh = RelayParams::new();
        fresh.load_persist(&blob);
        assert_eq!(
            fresh.session.read(),
            SessionPersist {
                peer: "10.0.0.7:17492".into(),
                name: "late-night-mix".into(),
                password: "mix-secret".into(),
            }
        );
    }

    #[test]
    fn defaults_to_live_share() {
        let params = RelayParams::new();
        assert!(params.mode.value().is_share());
        assert!(params.live.value());
        assert_eq!(params.codec.value(), Codec::Opus);
        assert_eq!(params.bitrate.value(), 192);
        assert_eq!(params.monitor.value(), Monitor::Mix);
        let session = params.session.read();
        assert!(!session.name.is_empty());
        assert_eq!(session.peer, default_peer());
    }

    #[test]
    fn publish_atomics_mirrors_params() {
        let params = RelayParams::new();
        params.bitrate.set_value(96);
        params.live.set_value(false);
        params.publish_atomics();
        assert_eq!(params.control.bitrate_kbps(), 96);
        assert!(!params.control.linked(), "Live is never forced back on");
        assert!(params.control.web_wanted());
    }

    #[test]
    fn same_rate_reset_keeps_runtime() {
        let params = RelayParams::new();
        let mut state = RelayDsp::default();
        let config = AudioConfig::new(48_000.0, 128);
        RelayPlugin::reset(&mut state, &params, &config);
        let first = state
            .runtime
            .as_ref()
            .map(|r| core::ptr::from_ref(r) as usize);
        assert!(first.is_some());
        RelayPlugin::reset(&mut state, &params, &config);
        let second = state
            .runtime
            .as_ref()
            .map(|r| core::ptr::from_ref(r) as usize);
        assert_eq!(first, second);
        assert_eq!(state.device_rate, 48_000);
        assert!(state.fanout.as_ref().is_some_and(Fanout::is_alive));
    }

    #[test]
    fn rate_change_rebuilds_runtime() {
        let params = RelayParams::new();
        let mut state = RelayDsp::default();
        RelayPlugin::reset(&mut state, &params, &AudioConfig::new(48_000.0, 128));
        let before = NEXT_SSRC.load(Ordering::Relaxed);
        RelayPlugin::reset(&mut state, &params, &AudioConfig::new(44_100.0, 128));
        assert!(NEXT_SSRC.load(Ordering::Relaxed) > before);
        assert!(state.runtime.is_some());
        assert_eq!(state.device_rate, 44_100);
    }

    #[test]
    fn fanout_syncs_session_strings_into_control() {
        let params = RelayParams::new();
        params.session.update(|session| {
            session.name = "Sync-Test-Room".into();
            session.peer = "192.168.1.9:17492".into();
            session.password = "pw".into();
        });
        let fanout = Fanout::spawn(Arc::clone(&params.control), params.session.clone());
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if params.control.session_name().unwrap_or_default() == "sync-test-room" {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            params.control.session_name().unwrap_or_default(),
            "sync-test-room"
        );
        assert_eq!(
            params.control.peer().unwrap_or_default(),
            "192.168.1.9:17492"
        );
        assert!(!params.control.password_hex().is_empty());
        drop(fanout);
    }
}
