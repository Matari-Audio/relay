//! RELAY: an insert that shares the track it sits on, or plays someone
//! else's. The audio thread only moves samples through two `rtrb` rings;
//! the link thread in `relay-core` does the networking.
#![forbid(unsafe_code)]

mod ui;

use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex, RwLock, Weak};

use moose::prelude::*;
use moose_core::custom_state::{PersistField, StateCursor};
use relay_core::{Link, Peak, Playout, Role, Shared};

pub(crate) use RelayParamsParamId as P;

#[derive(ParamEnum, Debug)]
pub enum Mode {
    #[name = "Off"]
    Off,
    #[name = "Share"]
    Share,
    #[name = "Join"]
    Join,
}

/// The ceiling for internet audio. Opus adapts below it to the slowest
/// listener's connection.
#[derive(ParamEnum, Debug)]
pub enum Quality {
    #[name = "510 kbps"]
    Max,
    #[name = "192 kbps"]
    High,
    #[name = "128 kbps"]
    Medium,
    #[name = "64 kbps"]
    Low,
}

impl Quality {
    pub fn bps(&self) -> u32 {
        match self {
            Self::Max => relay_core::MAX_BPS,
            Self::High => 192_000,
            Self::Medium => 128_000,
            Self::Low => 64_000,
        }
    }
}

#[derive(Params)]
pub struct RelayParams {
    #[param(name = "Mode", default = 1)]
    pub mode: EnumParam<Mode>,
    /// Level of this plugin's output: what you hear.
    #[param(name = "Output", range = "linear(-24, 12)", unit = "dB", default = 0.0)]
    pub output: FloatParam,
    /// Level of the room's audio: what is shared (Share) or played (Join).
    #[param(name = "Input", range = "linear(-24, 12)", unit = "dB", default = 0.0)]
    pub input: FloatParam,
    #[param(name = "Internet quality", default = 1)]
    pub quality: EnumParam<Quality>,
    #[param(name = "Mute input", default = false)]
    pub mute_input: BoolParam,
    #[param(name = "Mute output", default = false)]
    pub mute_output: BoolParam,
    #[param(name = "Local passthrough", default = true)]
    pub passthrough: BoolParam,
    /// False: Matari's pixel style. True: the smooth Barlow style.
    #[persist = "standard_ui"]
    pub standard_ui: RwLock<bool>,
    #[persist = "session"]
    pub link: Session,
}

/// The editor's and link's handle on [`Shared`]; saves room, password,
/// direct host and the room's host key with the project.
///
/// Also owns the instance's link thread, which outlives activations: the
/// thread's stop flag is never cleared, so it is spawned once per instance.
pub struct Session(pub Arc<Shared>, Arc<Mutex<Wire>>);

impl Default for Session {
    fn default() -> Self {
        let shared = Shared::new();
        shared.set_text(&shared.room, &relay_core::room_name());
        shared.set_text(&shared.host_key, &relay_core::random_key());
        Self(shared, Arc::default())
    }
}

/// The link thread and, between activations, the processor parked by
/// `deactivate` with the audio ends of the link's rings.
#[derive(Default)]
struct Wire {
    /// Stops and joins the link thread on drop.
    link: Option<Link>,
    parked: Option<RelayProcessor>,
}

impl Session {
    fn wire(&self) -> std::sync::MutexGuard<'_, Wire> {
        self.1.lock().expect("relay wire lock poisoned")
    }

    /// Starts the link thread; later calls do nothing. Main thread only.
    fn start(&self) {
        let mut wire = self.wire();
        if wire.link.is_none() {
            let ((tx, tx_out), (rx_in, rx)) = relay_core::rings();
            wire.link = Some(Link::spawn(Arc::clone(&self.0), tx_out, rx_in));
            wire.parked = Some(RelayProcessor::new(
                Arc::clone(&self.0),
                tx,
                rx,
                Arc::downgrade(&self.1),
            ));
        }
    }

    /// The processor last parked by `deactivate`, so its ring ends and
    /// gains carry across activations. Main thread only.
    fn checkout(&self) -> RelayProcessor {
        self.start();
        self.wire().parked.take().unwrap_or_else(|| {
            // Only after a processor was dropped without `deactivate`
            // (hot reload): play unlinked rather than fail activation.
            let ((tx, _), (_, rx)) = relay_core::rings();
            RelayProcessor::new(Arc::clone(&self.0), tx, rx, Weak::new())
        })
    }
}

#[derive(State, Clone, Debug, Default, PartialEq, Eq)]
struct Saved {
    room: String,
    password: String,
    peer: String,
    host_key: String,
}

impl PersistField for Session {
    fn persist_write(&self, buf: &mut Vec<u8>) {
        let s = &self.0;
        RwLock::new(Saved {
            room: Shared::text(&s.room),
            password: Shared::text(&s.password),
            peer: Shared::text(&s.peer),
            host_key: Shared::text(&s.host_key),
        })
        .persist_write(buf);
    }

    fn persist_read(&self, cursor: &mut StateCursor) {
        let saved = RwLock::new(Saved::default());
        saved.persist_read(cursor);
        let saved = saved.into_inner().unwrap_or_default();
        let s = &self.0;
        if !saved.room.is_empty() {
            s.set_text(&s.room, &saved.room);
        }
        s.set_text(&s.password, &saved.password);
        s.set_text(&s.peer, &saved.peer);
        if !saved.host_key.is_empty() {
            s.set_text(&s.host_key, &saved.host_key);
        }
    }
}

pub struct Relay;

pub struct RelayProcessor {
    shared: Arc<Shared>,
    tx: relay_core::rtrb::Producer<f32>,
    playout: Playout,
    left: Vec<f32>,
    right: Vec<f32>,
    /// The input and output gains the last block ended on, linear.
    gain: [f32; 2],
    dry: f32,
    /// The activation rate, clamped to what the link encodes.
    rate: u32,
    /// Where `deactivate` parks this processor for the next activation.
    home: Weak<Mutex<Wire>>,
}

impl RelayProcessor {
    fn new(
        shared: Arc<Shared>,
        tx: relay_core::rtrb::Producer<f32>,
        rx: relay_core::rtrb::Consumer<f32>,
        home: Weak<Mutex<Wire>>,
    ) -> Self {
        Self {
            shared,
            tx,
            playout: Playout::new(rx),
            left: Vec::new(),
            right: Vec::new(),
            gain: [1.0; 2],
            dry: 1.0,
            rate: 48_000,
            home,
        }
    }
}

impl Plugin for Relay {
    type Shared = RelayParams;
    type MainThread = ();
    type Processor = RelayProcessor;

    /// Starts the link at instantiation (CLAP, VST3); other formats start
    /// it on the first `activate`.
    fn main_thread(shared: &Arc<RelayParams>) {
        shared.link.start();
    }

    fn activate(
        shared: &RelayParams,
        config: &AudioConfig,
        _cx: &InitContext,
    ) -> Result<RelayProcessor, PreparationError> {
        let mut state = shared.link.checkout();
        state.left.resize(config.max_block_size, 0.0);
        state.right.resize(config.max_block_size, 0.0);
        state.rate = config.sample_rate.round().clamp(8_000.0, 384_000.0) as u32;
        <Self as Plugin>::reset(&mut state, shared);
        Ok(state)
    }

    fn deactivate(processor: RelayProcessor) {
        if let Some(home) = processor.home.upgrade() {
            home.lock().expect("relay wire lock poisoned").parked = Some(processor);
        }
    }

    /// May run on the audio thread: no allocation.
    fn reset(state: &mut RelayProcessor, _params: &RelayParams) {
        state.shared.rate.store(state.rate, Relaxed);
        state.left.fill(0.0);
        state.right.fill(0.0);
    }

    fn process(
        state: &mut RelayProcessor,
        params: &RelayParams,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        _context: &mut ProcessContext,
    ) -> ProcessStatus {
        let role = match params.mode.value() {
            Mode::Off => Role::Off,
            Mode::Share | Mode::Join => Role::Room,
        };
        state.shared.set_role(role);
        state
            .shared
            .bitrate_cap
            .store(params.quality.value().bps(), Relaxed);
        let n = buffer.num_samples().min(state.left.len());
        state.shared.block.store(n as u32, Relaxed);
        let outs = buffer.num_output_channels().min(2);
        for ch in 0..outs {
            let (input, output) = buffer.io(ch);
            if !std::ptr::eq(input.as_ptr(), output.as_ptr()) {
                output[..n].copy_from_slice(&input[..n]);
            }
        }
        let (l, r) = (&mut state.left[..n], &mut state.right[..n]);
        l.copy_from_slice(&buffer.input(0)[..n]);
        r.copy_from_slice(&buffer.input(buffer.num_input_channels().min(2) - 1)[..n]);
        let gain = [
            if params.mute_input.value() {
                0.0
            } else {
                db_to_linear(params.input.value())
            },
            if params.mute_output.value() {
                0.0
            } else {
                db_to_linear(params.output.value())
            },
        ];
        if role != Role::Off {
            ramp(l, state.gain[0], gain[0]);
            ramp(r, state.gain[0], gain[0]);
            if let Ok(chunk) = state.tx.write_chunk_uninit(n * 2) {
                chunk.fill_from_iter(l.iter().zip(r.iter()).flat_map(|(a, b)| [*a, *b]));
            }
        }
        let shared = &state.shared;
        shared.note_peak(Peak::InL, peak(l));
        shared.note_peak(Peak::InR, peak(r));
        let dry = f32::from(role == Role::Off || params.passthrough.value());
        for ch in 0..outs {
            ramp(&mut buffer.output(ch)[..n], state.dry, dry);
        }
        if role != Role::Off {
            l.fill(0.0);
            r.fill(0.0);
            state.playout.render(l, r);
            shared.latency.store(state.playout.target() as u32, Relaxed);
            add(buffer, l, r, outs);
        }
        let mut out = [0.0; 2];
        for (ch, rail) in out.iter_mut().enumerate().take(outs) {
            let o = &mut buffer.output(ch)[..n];
            ramp(o, state.gain[1], gain[1]);
            *rail = peak(o);
        }
        // A mono output shows on both rails.
        shared.note_peak(Peak::OutL, out[0]);
        shared.note_peak(Peak::OutR, out[outs.saturating_sub(1)]);
        state.gain = gain;
        state.dry = dry;
        ProcessStatus::Normal
    }

    fn editor(params: Arc<RelayParams>) -> Box<dyn Editor> {
        ui::editor(params)
    }
}

/// Mix a stereo pair into the first `outs` output channels.
fn add(buffer: &mut AudioBuffer, l: &[f32], r: &[f32], outs: usize) {
    for (ch, src) in [l, r].into_iter().enumerate().take(outs) {
        for (o, s) in buffer.output(ch).iter_mut().zip(src) {
            *o += s;
        }
    }
}

fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0, |m, s| m.max(s.abs()))
}

/// Scale by a gain gliding from `from` to `to` over the block, so moving
/// a fader never clicks.
fn ramp(x: &mut [f32], from: f32, to: f32) {
    let step = (to - from) / x.len().max(1) as f32;
    for (i, s) in x.iter_mut().enumerate() {
        *s *= from + step * (i + 1) as f32;
    }
}

moose::plugin! {
    plugin: Relay,
    params: RelayParams,
}

#[cfg(test)]
mod state {
    use super::*;
    use moose_core::export::PluginExport;
    use moose_core::state::{restore_plugin, snapshot_plugin};
    use moose_params::Params;

    /// Saved by RELAY 0.2.0 on truce 6.3: Share, Output 0.25, Input 0.75,
    /// 64 kbps, and the session below.
    const TRUCE_STATE: &[u8] = include_bytes!("../tests/fixtures/relay-0.2.0-truce.state");

    fn texts(p: &PluginShell) -> [String; 4] {
        let s = &p.params().link.0;
        [&s.room, &s.password, &s.peer, &s.host_key].map(Shared::text)
    }

    fn values(p: &PluginShell) -> [f64; 4] {
        [P::Mode, P::Output, P::Input, P::Quality]
            .map(|id| p.params().get_normalized(id.into()).unwrap())
    }

    #[test]
    fn default_room_connects_automatically_at_192_kbps() {
        let p = PluginShell::create();
        assert_eq!(p.params().quality.value().bps(), 192_000);
        assert!(matches!(p.params().mode.value(), Mode::Share));
    }

    #[test]
    fn truce_session_loads() {
        let mut p = PluginShell::create();
        restore_plugin(&mut p, TRUCE_STATE).unwrap();
        assert_eq!(values(&p), [0.5, 0.25, 0.75, 1.0]);
        assert_eq!(
            texts(&p),
            [
                "quiet-dusty-papaya",
                "hunter2",
                "192.168.1.20",
                "fixture-host-key"
            ]
            .map(String::from)
        );
    }

    #[test]
    fn round_trips() {
        let mut a = PluginShell::create();
        restore_plugin(&mut a, TRUCE_STATE).unwrap();
        let mut b = PluginShell::create();
        restore_plugin(&mut b, &snapshot_plugin(&a)).unwrap();
        assert_eq!(values(&b), values(&a));
        assert_eq!(texts(&b), texts(&a));
        assert!(!*b.params().standard_ui.read().unwrap());
        *b.params().standard_ui.write().unwrap() = true;
        let mut c = PluginShell::create();
        restore_plugin(&mut c, &snapshot_plugin(&b)).unwrap();
        assert!(*c.params().standard_ui.read().unwrap());
        assert_eq!(values(&c), values(&a));
        assert_eq!(texts(&c), texts(&a));
    }

    #[test]
    fn empty_state_is_rejected() {
        let mut p = PluginShell::create();
        let before = texts(&p);
        assert!(restore_plugin(&mut p, &[]).is_err());
        assert_eq!(texts(&p), before);
    }
}

moose::enable_rt_paranoid!();

#[cfg(all(test, feature = "rt-paranoid"))]
mod reset_realtime_tests {
    #[test]
    fn reset_is_realtime_after_prepare() {
        moose_test::assert_reset_realtime::<moose::Lifecycle<super::Relay>, f32>(
            &super::RelayParams::default(),
            &moose::prelude::AudioConfig::new(48_000.0, 512),
        );
    }
}
