//! Off-callback runtime: a worker thread owns sockets and codec work.

use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use relay_audio::RenderReport;
use relay_domain::{ConnectionState, MediaRoute, SessionMode};
use relay_rt::WriteOutcome;

use crate::codec::{
    CodecSettings, FLAC_LEVEL_DEFAULT, FlacSettings, OPUS_BITRATE_DEFAULT_KBPS, OpusSettings,
    WireCodec,
};
use crate::engine::{
    CallbackFace, EngineCommand, MonitorMode, SessionConfig, SessionEngine, SessionSnapshot,
    SessionWorker,
};
use crate::plane::PlaneError;

/// Product role applied by [`SessionRuntime`] when the session is linked.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum SessionRole {
    /// Bind a Connect listener.
    ConnectListen = 0,
    /// Join a Connect peer.
    ConnectJoin = 1,
    /// Bind a local unpaid Stream hub.
    StreamHub = 2,
    /// Publish to a Stream hub.
    StreamPublish = 3,
    /// Listen to a Stream hub.
    StreamListen = 4,
    /// Encode, send to localhost, decode, and play back in one instance.
    Loopback = 5,
}

impl SessionRole {
    /// Parses a stored role byte.
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::ConnectListen),
            1 => Some(Self::ConnectJoin),
            2 => Some(Self::StreamHub),
            3 => Some(Self::StreamPublish),
            4 => Some(Self::StreamListen),
            5 => Some(Self::Loopback),
            _ => None,
        }
    }

    /// Product surface implied by this role.
    #[must_use]
    pub const fn session_mode(self) -> SessionMode {
        match self {
            Self::ConnectListen | Self::ConnectJoin | Self::Loopback => SessionMode::Connect,
            Self::StreamHub | Self::StreamPublish | Self::StreamListen => SessionMode::Stream,
        }
    }
}

/// Lock-free control/status cell shared by the editor, callback, and worker.
#[derive(Debug)]
pub struct SessionControl {
    stop: AtomicBool,
    linked: AtomicBool,
    role: AtomicU8,
    codec: AtomicU8,
    bitrate_kbps: AtomicU32,
    flac_level: AtomicU8,
    bind_port: AtomicU32,
    peer: Mutex<String>,
    session_name: Mutex<String>,
    password: Mutex<String>,
    pcm: Mutex<Vec<f32>>,
    pcm_generation: AtomicU64,
    rx_pcm: Mutex<Vec<f32>>,
    cloud_rx: AtomicBool,
    web_ok: AtomicBool,
    web_silent: AtomicBool,
    web_wake: AtomicBool,
    web_wanted: AtomicBool,
    web_listeners: AtomicU32,
    lan_http_port: AtomicU32,
    lan_listeners: AtomicU32,
    device_rate_hz: AtomicU32,
    block_frames: AtomicU32,
    snapshot_state: AtomicU8,
    snapshot_peers: AtomicU32,
    snapshot_port: AtomicU32,
    snapshot_mode: AtomicU8,
    snapshot_bound: AtomicBool,
    snapshot_dropouts: AtomicU32,
    last_error: Mutex<String>,
    who: Mutex<String>,
}

impl Default for SessionControl {
    fn default() -> Self {
        Self {
            stop: AtomicBool::new(false),
            linked: AtomicBool::new(false),
            role: AtomicU8::new(SessionRole::ConnectListen as u8),
            codec: AtomicU8::new(WireCodec::Opus as u8),
            bitrate_kbps: AtomicU32::new(OPUS_BITRATE_DEFAULT_KBPS),
            flac_level: AtomicU8::new(FLAC_LEVEL_DEFAULT),
            bind_port: AtomicU32::new(u32::from(crate::DEFAULT_CONNECT_PORT)),
            peer: Mutex::new(String::new()),
            session_name: Mutex::new(String::new()),
            password: Mutex::new(String::new()),
            pcm: Mutex::new(Vec::new()),
            rx_pcm: Mutex::new(Vec::new()),
            cloud_rx: AtomicBool::new(false),
            pcm_generation: AtomicU64::new(0),
            web_ok: AtomicBool::new(false),
            web_silent: AtomicBool::new(false),
            web_wake: AtomicBool::new(false),
            web_wanted: AtomicBool::new(false),
            web_listeners: AtomicU32::new(0),
            lan_http_port: AtomicU32::new(0),
            lan_listeners: AtomicU32::new(0),
            device_rate_hz: AtomicU32::new(48_000),
            block_frames: AtomicU32::new(0),
            snapshot_state: AtomicU8::new(encode_state(ConnectionState::Idle)),
            snapshot_peers: AtomicU32::new(0),
            snapshot_port: AtomicU32::new(0),
            snapshot_mode: AtomicU8::new(0),
            snapshot_bound: AtomicBool::new(false),
            snapshot_dropouts: AtomicU32::new(0),
            last_error: Mutex::new(String::new()),
            who: Mutex::new(String::new()),
        }
    }
}

impl SessionControl {
    /// Requests the worker to connect or host using the current role/peer.
    pub fn set_linked(&self, linked: bool) {
        self.linked.store(linked, Ordering::Release);
    }

    /// Returns whether the worker should stay linked.
    #[must_use]
    pub fn linked(&self) -> bool {
        self.linked.load(Ordering::Acquire)
    }

    /// Sets the product role used on the next link.
    pub fn set_role(&self, role: SessionRole) {
        self.role.store(role as u8, Ordering::Release);
    }

    /// Returns the stored product role.
    #[must_use]
    pub fn role(&self) -> SessionRole {
        SessionRole::from_u8(self.role.load(Ordering::Acquire))
            .unwrap_or(SessionRole::ConnectListen)
    }

    /// Sets the wire codec used by the worker send path.
    pub fn set_codec(&self, codec: WireCodec) {
        self.codec.store(codec as u8, Ordering::Release);
    }

    /// Returns the stored wire codec.
    #[must_use]
    pub fn codec(&self) -> WireCodec {
        WireCodec::from_u8(self.codec.load(Ordering::Acquire)).unwrap_or(WireCodec::Opus)
    }

    /// Sets the Opus bitrate in kilobits per second.
    pub fn set_bitrate_kbps(&self, kbps: u32) {
        self.bitrate_kbps
            .store(OpusSettings::new(kbps).bitrate_kbps(), Ordering::Release);
    }

    /// Opus bitrate in kilobits per second.
    #[must_use]
    pub fn bitrate_kbps(&self) -> u32 {
        OpusSettings::new(self.bitrate_kbps.load(Ordering::Acquire)).bitrate_kbps()
    }

    /// Sets FLAC compression effort, 0 (fast) through 8 (smallest).
    pub fn set_flac_level(&self, level: u8) {
        self.flac_level
            .store(FlacSettings::new(level).compression(), Ordering::Release);
    }

    /// FLAC compression effort, 0 through 8.
    #[must_use]
    pub fn flac_level(&self) -> u8 {
        FlacSettings::new(self.flac_level.load(Ordering::Acquire)).compression()
    }

    /// Selected codec with the settings that belong to it.
    #[must_use]
    pub fn codec_settings(&self) -> CodecSettings {
        CodecSettings::from_parts(self.codec(), self.bitrate_kbps(), self.flac_level())
    }

    /// Sets the optional session password. Empty means the room is open.
    ///
    /// # Errors
    ///
    /// Returns [`ControlLockError`] if the password mutex is poisoned.
    pub fn set_password(&self, password: impl Into<String>) -> Result<(), ControlLockError> {
        let mut guard = self.password.lock().map_err(|_| ControlLockError)?;
        *guard = password.into();
        Ok(())
    }

    /// Copies the current session password.
    ///
    /// # Errors
    ///
    /// Returns [`ControlLockError`] if the password mutex is poisoned.
    pub fn password(&self) -> Result<String, ControlLockError> {
        self.password
            .lock()
            .map(|guard| guard.clone())
            .map_err(|_| ControlLockError)
    }

    /// SHA-256 token for the current password, or zeros when open.
    #[must_use]
    pub fn password_token(&self) -> [u8; 32] {
        self.password()
            .map(|value| crate::password_token(&value))
            .unwrap_or([0; 32])
    }

    /// Hex SHA-256 of the current password. Empty when the room is open.
    #[must_use]
    pub fn password_hex(&self) -> String {
        self.password()
            .map(|value| crate::password_hex(&value))
            .unwrap_or_default()
    }

    /// Sets the UDP bind port for listen/hub roles.
    pub fn set_bind_port(&self, port: u16) {
        self.bind_port.store(u32::from(port), Ordering::Release);
    }

    /// Returns the UDP bind port.
    #[must_use]
    pub fn bind_port(&self) -> u16 {
        u16::try_from(self.bind_port.load(Ordering::Acquire)).unwrap_or(0)
    }

    /// Replaces the peer/hub address string (`host:port`).
    ///
    /// # Errors
    ///
    /// Returns [`ControlLockError`] if the peer mutex is poisoned.
    pub fn set_peer(&self, peer: impl Into<String>) -> Result<(), ControlLockError> {
        let mut guard = self.peer.lock().map_err(|_| ControlLockError)?;
        *guard = peer.into();
        Ok(())
    }

    /// Replaces the shareable session slug used by the local listen page.
    ///
    /// # Errors
    ///
    /// Returns [`ControlLockError`] if the name mutex is poisoned.
    pub fn set_session_name(&self, name: impl Into<String>) -> Result<(), ControlLockError> {
        let mut guard = self.session_name.lock().map_err(|_| ControlLockError)?;
        *guard = name.into();
        Ok(())
    }

    /// Copies queued 48 kHz stereo samples waiting for web fan-out.
    ///
    /// # Errors
    ///
    /// Returns [`ControlLockError`] if the PCM mutex is poisoned.
    pub fn last_pcm(&self) -> Result<Vec<f32>, ControlLockError> {
        self.pcm
            .lock()
            .map(|guard| guard.clone())
            .map_err(|_| ControlLockError)
    }

    /// Drains up to `max_samples` queued 48 kHz stereo samples.
    ///
    /// # Errors
    ///
    /// Returns [`ControlLockError`] if the PCM mutex is poisoned.
    pub fn take_pcm(&self, max_samples: usize) -> Result<Vec<f32>, ControlLockError> {
        self.take_pcm_live(max_samples, usize::MAX)
    }

    /// Drains up to `max_samples`, discarding older audio when the queue
    /// grows past `keep_max` so a stalled upload cannot play minutes late.
    ///
    /// # Errors
    ///
    /// Returns [`ControlLockError`] if the PCM mutex is poisoned.
    pub fn take_pcm_live(
        &self,
        max_samples: usize,
        keep_max: usize,
    ) -> Result<Vec<f32>, ControlLockError> {
        let mut guard = self.pcm.lock().map_err(|_| ControlLockError)?;
        if guard.len() > keep_max {
            let overflow = guard.len() - keep_max;
            guard.drain(..overflow);
        }
        let take = guard.len().min(max_samples);
        Ok(guard.drain(..take).collect())
    }

    /// Increments when [`Self::last_pcm`] is replaced with a new media frame.
    #[must_use]
    pub fn pcm_generation(&self) -> u64 {
        self.pcm_generation.load(Ordering::Acquire)
    }

    /// Last public-listen upload result from the fan-out thread.
    #[must_use]
    pub fn web_ok(&self) -> bool {
        self.web_ok.load(Ordering::Acquire)
    }

    /// Records whether the last public PCM POST succeeded.
    pub fn set_web_ok(&self, ok: bool) {
        self.web_ok.store(ok, Ordering::Release);
    }

    /// True while the public upload is connected but not sending audio (DTX).
    #[must_use]
    pub fn web_silent(&self) -> bool {
        self.web_silent.load(Ordering::Acquire)
    }

    /// Records whether the public upload is in silence hold.
    pub fn set_web_silent(&self, silent: bool) {
        self.web_silent.store(silent, Ordering::Release);
    }

    /// Ask the public upload to leave silence hold and start sending again.
    pub fn request_web_wake(&self) {
        self.web_wake.store(true, Ordering::Release);
    }

    /// Consumes a pending wake request from the editor.
    #[must_use]
    pub fn take_web_wake(&self) -> bool {
        self.web_wake.swap(false, Ordering::AcqRel)
    }

    /// True when the musician opted into the public listen page.
    #[must_use]
    pub fn web_wanted(&self) -> bool {
        self.web_wanted.load(Ordering::Acquire)
    }

    /// Enables or disables the Cloudflare listen path. Off means LAN only.
    pub fn set_web_wanted(&self, wanted: bool) {
        self.web_wanted.store(wanted, Ordering::Release);
        if !wanted {
            self.set_web_ok(false);
            self.set_web_silent(false);
            self.web_wake.store(false, Ordering::Release);
            self.set_web_listeners(0);
        }
    }

    /// Unlocked web listeners currently holding `/out`.
    #[must_use]
    pub fn web_listeners(&self) -> u32 {
        self.web_listeners.load(Ordering::Acquire)
    }

    /// Records how many unlocked browsers are listening.
    pub fn set_web_listeners(&self, n: u32) {
        self.web_listeners.store(n, Ordering::Release);
    }

    /// Local HTTP listen port bound by the plugin, or 0 if unbound.
    #[must_use]
    pub fn lan_http_port(&self) -> u16 {
        u16::try_from(self.lan_http_port.load(Ordering::Acquire)).unwrap_or(0)
    }

    /// Records the bound local listen HTTP port.
    pub fn set_lan_http_port(&self, port: u16) {
        self.lan_http_port.store(u32::from(port), Ordering::Release);
    }

    /// Browsers currently holding the local LAN `/out` socket.
    #[must_use]
    pub fn lan_listeners(&self) -> u32 {
        self.lan_listeners.load(Ordering::Acquire)
    }

    /// Records how many browsers are on the local listen socket.
    pub fn set_lan_listeners(&self, n: u32) {
        self.lan_listeners.store(n, Ordering::Release);
    }

    /// Host (DAW) sample rate in Hz.
    pub fn set_device_rate_hz(&self, rate: u32) {
        self.device_rate_hz
            .store(rate.clamp(8_000, 192_000), Ordering::Release);
    }

    /// Host (DAW) sample rate in Hz.
    #[must_use]
    pub fn device_rate_hz(&self) -> u32 {
        self.device_rate_hz.load(Ordering::Acquire).max(8_000)
    }

    /// Latest host callback block length in frames.
    pub fn set_block_frames(&self, frames: u32) {
        self.block_frames.store(frames, Ordering::Release);
    }

    /// Latest host callback block length in frames.
    #[must_use]
    pub fn block_frames(&self) -> u32 {
        self.block_frames.load(Ordering::Acquire)
    }

    pub(crate) fn publish_pcm(&self, samples: &[f32]) {
        if samples.is_empty() {
            return;
        }
        if let Ok(mut guard) = self.pcm.lock() {
            guard.extend_from_slice(samples);
            const MAX_QUEUED: usize = 48_000 * 2;
            if guard.len() > MAX_QUEUED {
                let overflow = guard.len() - MAX_QUEUED;
                guard.drain(..overflow);
            }
            self.pcm_generation.fetch_add(1, Ordering::Release);
        }
    }

    /// Audio arriving from a cloud join, for the worker to play out.
    ///
    /// Written by the listen thread, drained by the worker thread; the audio
    /// callback never touches it.
    pub fn push_rx_pcm(&self, samples: &[f32]) {
        if samples.is_empty() {
            return;
        }
        if let Ok(mut guard) = self.rx_pcm.lock() {
            guard.extend_from_slice(samples);
            // Half a second. A stall should resync to live, not play late.
            const MAX_QUEUED: usize = 48_000;
            if guard.len() > MAX_QUEUED {
                let overflow = guard.len() - MAX_QUEUED;
                guard.drain(..overflow);
            }
        }
    }

    pub(crate) fn take_rx_pcm(&self) -> Vec<f32> {
        self.rx_pcm
            .lock()
            .map(|mut guard| std::mem::take(&mut *guard))
            .unwrap_or_default()
    }

    /// True while a cloud join is delivering the host's audio.
    #[must_use]
    pub fn cloud_rx(&self) -> bool {
        self.cloud_rx.load(Ordering::Acquire)
    }

    /// Set by the listen thread when the join's data channel is live.
    pub fn set_cloud_rx(&self, live: bool) {
        self.cloud_rx.store(live, Ordering::Release);
    }

    /// Copies the current session slug.
    ///
    /// # Errors
    ///
    /// Returns [`ControlLockError`] if the name mutex is poisoned.
    pub fn session_name(&self) -> Result<String, ControlLockError> {
        self.session_name
            .lock()
            .map(|guard| guard.clone())
            .map_err(|_| ControlLockError)
    }

    /// Copies the current peer/hub address.
    ///
    /// # Errors
    ///
    /// Returns [`ControlLockError`] if the peer mutex is poisoned.
    pub fn peer(&self) -> Result<String, ControlLockError> {
        self.peer
            .lock()
            .map(|guard| guard.clone())
            .map_err(|_| ControlLockError)
    }

    /// Last worker-published snapshot.
    #[must_use]
    pub fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            mode: if self.snapshot_mode.load(Ordering::Acquire) == 1 {
                SessionMode::Stream
            } else {
                SessionMode::Connect
            },
            state: decode_state(self.snapshot_state.load(Ordering::Acquire)),
            route: if self.snapshot_mode.load(Ordering::Acquire) == 1 {
                MediaRoute::Sfu
            } else {
                MediaRoute::Direct
            },
            peers: self.snapshot_peers.load(Ordering::Acquire) as usize,
            local_port: {
                let port = u16::try_from(self.snapshot_port.load(Ordering::Acquire)).unwrap_or(0);
                if port == 0 { None } else { Some(port) }
            },
            bound: self.snapshot_bound.load(Ordering::Acquire),
            dropouts: self.snapshot_dropouts.load(Ordering::Acquire),
        }
    }

    /// Last bind/join error, or empty when the worker is healthy.
    ///
    /// # Errors
    ///
    /// Returns [`ControlLockError`] if the error mutex is poisoned.
    pub fn last_error(&self) -> Result<String, ControlLockError> {
        self.last_error
            .lock()
            .map(|guard| guard.clone())
            .map_err(|_| ControlLockError)
    }

    /// Records a bind/join failure that must survive the next snapshot publish.
    pub fn set_last_error(&self, message: impl Into<String>) {
        if let Ok(mut guard) = self.last_error.lock() {
            *guard = message.into();
        }
        self.snapshot_state
            .store(encode_state(ConnectionState::Failed), Ordering::Release);
    }

    /// Clears a previous bind/join failure.
    pub fn clear_last_error(&self) {
        if let Ok(mut guard) = self.last_error.lock() {
            guard.clear();
        }
    }

    /// Short "who is connected" line (peer IPs plus browser counts).
    ///
    /// # Errors
    ///
    /// Returns [`ControlLockError`] if the who mutex is poisoned.
    pub fn who(&self) -> Result<String, ControlLockError> {
        self.who
            .lock()
            .map(|guard| guard.clone())
            .map_err(|_| ControlLockError)
    }

    /// Replaces the "who is connected" line.
    pub fn set_who(&self, who: impl Into<String>) {
        if let Ok(mut guard) = self.who.lock() {
            *guard = who.into();
        }
    }

    fn publish(&self, snapshot: SessionSnapshot) {
        self.snapshot_state
            .store(encode_state(snapshot.state), Ordering::Release);
        self.snapshot_peers
            .store(snapshot.peers as u32, Ordering::Release);
        self.snapshot_port.store(
            u32::from(snapshot.local_port.unwrap_or(0)),
            Ordering::Release,
        );
        self.snapshot_mode.store(
            u8::from(snapshot.mode == SessionMode::Stream),
            Ordering::Release,
        );
        self.snapshot_bound.store(snapshot.bound, Ordering::Release);
        self.snapshot_dropouts
            .store(snapshot.dropouts, Ordering::Release);
    }

    fn publish_failed(&self, message: &str, snapshot: SessionSnapshot) {
        self.set_last_error(message);
        self.snapshot_peers
            .store(snapshot.peers as u32, Ordering::Release);
        self.snapshot_port.store(
            u32::from(snapshot.local_port.unwrap_or(0)),
            Ordering::Release,
        );
        self.snapshot_bound.store(snapshot.bound, Ordering::Release);
        self.snapshot_dropouts
            .store(snapshot.dropouts, Ordering::Release);
    }

    fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    /// Returns whether the worker (and any fan-out thread) should exit.
    #[must_use]
    pub fn should_stop(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }
}

/// The peer-address mutex was poisoned.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlLockError;

impl core::fmt::Display for ControlLockError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str("session peer lock poisoned")
    }
}

impl std::error::Error for ControlLockError {}

/// Callback face plus a dedicated worker thread.
pub struct SessionRuntime {
    face: CallbackFace,
    control: Arc<SessionControl>,
    join: Option<JoinHandle<()>>,
}

impl SessionRuntime {
    /// Prepares codec/rings and starts the worker thread. Not callback-safe.
    pub fn start(mut config: SessionConfig) -> Result<Self, crate::engine::EngineBuildError> {
        let control = Arc::new(SessionControl::default());
        config.mode = control.role().session_mode();
        let engine = SessionEngine::prepare(config)?;
        let (face, worker) = engine.into_parts();
        let join = spawn_worker(worker, Arc::clone(&control));
        Ok(Self {
            face,
            control,
            join: Some(join),
        })
    }

    /// Starts a runtime that shares an existing control cell.
    pub fn start_with(
        mut config: SessionConfig,
        control: Arc<SessionControl>,
    ) -> Result<Self, crate::engine::EngineBuildError> {
        control.stop.store(false, Ordering::Release);
        config.mode = control.role().session_mode();
        let engine = SessionEngine::prepare(config)?;
        let (face, worker) = engine.into_parts();
        let join = spawn_worker(worker, Arc::clone(&control));
        Ok(Self {
            face,
            control,
            join: Some(join),
        })
    }

    /// Shared control/status cell.
    #[must_use]
    pub fn control(&self) -> &Arc<SessionControl> {
        &self.control
    }

    /// Audio-thread mix policy.
    pub fn set_monitor(&mut self, monitor: MonitorMode) {
        self.face.set_monitor(monitor);
    }

    /// Playback ring target the host can report as latency, in device frames.
    #[must_use]
    pub fn playback_target_frames(&self) -> u32 {
        self.face.playback_target_frames()
    }

    /// Host-callback capture tap.
    #[must_use]
    pub fn process_capture(&mut self, interleaved: &[f32]) -> WriteOutcome {
        self.face.process_capture(interleaved)
    }

    /// Host-callback render.
    #[must_use]
    pub fn render(&mut self, output: &mut [f32], dry: &[f32]) -> RenderReport {
        self.face.render(output, dry)
    }

    /// Last published snapshot.
    #[must_use]
    pub fn snapshot(&self) -> SessionSnapshot {
        self.control.snapshot()
    }
}

impl Drop for SessionRuntime {
    fn drop(&mut self) {
        self.control.request_stop();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn spawn_worker(mut worker: SessionWorker, control: Arc<SessionControl>) -> JoinHandle<()> {
    thread::Builder::new()
        .name("relay-session".into())
        .spawn(move || run_worker(&mut worker, &control))
        .expect("session worker thread")
}

fn run_worker(worker: &mut SessionWorker, control: &SessionControl) {
    let mut applied: Option<Applied> = None;
    let mut hold_error: Option<String> = None;
    let mut last_claim = String::new();
    let mut posted_web = 0_u64;
    while !control.should_stop() {
        let desired = desired_from(control);
        if applied.as_ref() != Some(&desired) {
            match apply_desired(worker, &desired) {
                Ok(()) => {
                    applied = Some(desired);
                    hold_error = None;
                    control.clear_last_error();
                }
                Err(error) => {
                    hold_error = Some(error.to_string());
                }
            }
        }
        worker.apply_codec(control.codec_settings());
        worker.apply_password(control.password_token());
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker.drive())).is_err() {
            let _ = worker.apply(EngineCommand::Disconnect);
            applied = None;
            hold_error = Some("session worker recovered".into());
        }
        let inbound = control.take_rx_pcm();
        if !inbound.is_empty() {
            worker.push_cloud_pcm(&inbound);
        }
        if let Some((pcm, seq)) = worker.take_web_pcm()
            && seq != posted_web
        {
            control.publish_pcm(&pcm);
            posted_web = seq;
        }
        let snapshot = worker.snapshot();
        control.set_who(who_line(
            &worker.peer_addrs(),
            control.lan_listeners(),
            control.web_listeners(),
        ));
        if let Some(error) = hold_error.as_deref() {
            control.publish_failed(error, snapshot);
        } else {
            control.publish(snapshot);
        }
        if let Ok(name) = control.session_name() {
            let (bytes, len) = slug_bytes(&name);
            if len > 0 {
                let _ = worker.apply(EngineCommand::SetSlug { bytes, len });
            }
        }
        if let Some(key) = claim_key(control, snapshot)
            && last_claim != key
        {
            maybe_claim_session(control, snapshot);
            last_claim = key;
        }
        thread::sleep(Duration::from_millis(2));
    }
    let _ = worker.apply(EngineCommand::Disconnect);
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Applied {
    linked: bool,
    role: SessionRole,
    port: u16,
    peer: String,
}

fn desired_from(control: &SessionControl) -> Applied {
    Applied {
        linked: control.linked(),
        role: control.role(),
        port: control.bind_port(),
        peer: control.peer().unwrap_or_default(),
    }
}

fn apply_desired(worker: &mut SessionWorker, desired: &Applied) -> Result<(), PlaneError> {
    if !desired.linked {
        return worker.apply(EngineCommand::Disconnect);
    }
    match desired.role {
        SessionRole::ConnectListen => listen_with_fallback(worker, desired.port),
        SessionRole::ConnectJoin => {
            if let Some((bytes, len)) = lan_slug(&desired.peer) {
                worker.apply(EngineCommand::JoinLan { bytes, len })
            } else {
                let peer = parse_peer(&desired.peer)?;
                worker.apply(EngineCommand::Join(peer))
            }
        }
        SessionRole::StreamHub => worker.apply(EngineCommand::HostStream(SocketAddr::from((
            [0, 0, 0, 0],
            desired.port,
        )))),
        SessionRole::StreamPublish => {
            let hub = parse_peer(&desired.peer)?;
            worker.apply(EngineCommand::PublishStream(hub))
        }
        SessionRole::StreamListen => {
            let hub = parse_peer(&desired.peer)?;
            worker.apply(EngineCommand::ListenStream(hub))
        }
        SessionRole::Loopback => worker.apply(EngineCommand::Loopback),
    }
}

fn listen_with_fallback(worker: &mut SessionWorker, port: u16) -> Result<(), PlaneError> {
    let first = SocketAddr::from(([0, 0, 0, 0], port));
    match worker.apply(EngineCommand::Listen(first)) {
        Ok(()) => Ok(()),
        Err(error) => {
            if port != crate::DEFAULT_CONNECT_PORT {
                return Err(error);
            }
            for extra in 1_u16..=16 {
                let next = crate::DEFAULT_CONNECT_PORT.saturating_add(extra);
                let addr = SocketAddr::from(([0, 0, 0, 0], next));
                if worker.apply(EngineCommand::Listen(addr)).is_ok() {
                    return Ok(());
                }
            }
            Err(error)
        }
    }
}

fn parse_peer(value: &str) -> Result<SocketAddr, PlaneError> {
    SocketAddr::from_str(value.trim()).map_err(|_| PlaneError::InvalidRole)
}

fn who_line(peers: &[SocketAddr], _lan_browsers: u32, _web: u32) -> String {
    let mut bits = Vec::new();
    for peer in peers {
        bits.push(peer.ip().to_string());
    }
    bits.join(" · ")
}

fn slug_bytes(name: &str) -> ([u8; 48], u8) {
    let slug = normalize_slug(name);
    let mut bytes = [0_u8; 48];
    let len = slug.len().min(48);
    bytes[..len].copy_from_slice(slug.as_bytes());
    (bytes, u8::try_from(len).unwrap_or(0))
}

fn lan_slug(peer: &str) -> Option<([u8; 48], u8)> {
    let trimmed = peer.trim();
    let raw = trimmed.strip_prefix("lan:").unwrap_or(trimmed);
    if raw.is_empty() || raw.contains(':') || raw.contains('.') {
        return None;
    }
    let (bytes, len) = slug_bytes(raw);
    (len > 0).then_some((bytes, len))
}

fn claim_key(control: &SessionControl, snapshot: SessionSnapshot) -> Option<String> {
    if !control.linked() {
        return None;
    }
    if !matches!(
        control.role(),
        SessionRole::ConnectListen | SessionRole::StreamHub | SessionRole::StreamPublish
    ) {
        return None;
    }
    let name = normalize_slug(&control.session_name().ok()?);
    if name.is_empty() {
        return None;
    }
    let target = snapshot
        .local_port
        .map(|port| format!("127.0.0.1:{port}"))
        .or_else(|| control.peer().ok())?;
    Some(format!("{name}|{target}"))
}

fn maybe_claim_session(control: &SessionControl, snapshot: SessionSnapshot) {
    let Some(key) = claim_key(control, snapshot) else {
        return;
    };
    let Some((name, target)) = key.split_once('|') else {
        return;
    };
    advertise_slug(name, target);
}

/// Lowercases and keeps `[a-z0-9-]` only.
#[must_use]
pub fn normalize_slug(raw: &str) -> String {
    raw.chars()
        .flat_map(char::to_lowercase)
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-')
        .take(48)
        .collect()
}

fn advertise_slug(name: &str, target: &str) {
    let body = format!("{name}\n{target}\n");
    let request = format!(
        "POST /api/claim HTTP/1.1\r\nHost: 127.0.0.1:{http}\r\nContent-Type: text/plain\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
        http = crate::DEFAULT_LINK_HTTP_PORT,
        len = body.len(),
    );
    let addr = SocketAddr::from(([127, 0, 0, 1], crate::DEFAULT_LINK_HTTP_PORT));
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(40))
    else {
        return;
    };
    let _ = std::io::Write::write_all(&mut stream, request.as_bytes());
}

/// `http://<lan-ip>:<port>/<slug>` for same-network browser listen.
#[must_use]
pub fn lan_listen_url(name: &str, http_port: u16) -> Option<String> {
    if http_port == 0 {
        return None;
    }
    let slug = normalize_slug(name);
    if slug.is_empty() {
        return None;
    }
    let ip = local_ipv4_addrs().into_iter().next()?;
    Some(format!("http://{ip}:{http_port}/{slug}"))
}

/// True when both addresses share a /24 — same home/LAN in practice.
#[must_use]
pub fn same_ipv4_24(a: &str, b: &str) -> bool {
    let pa = parse_ipv4(a);
    let pb = parse_ipv4(b);
    match (pa, pb) {
        (Some(left), Some(right)) => {
            left[0] == right[0] && left[1] == right[1] && left[2] == right[2]
        }
        _ => false,
    }
}

fn parse_ipv4(value: &str) -> Option<[u8; 4]> {
    let mut out = [0_u8; 4];
    let mut parts = value.split('.');
    for slot in &mut out {
        *slot = parts.next()?.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(out)
}

/// Best-effort IPv4 addresses a LAN peer can join.
#[must_use]
pub fn local_ipv4_addrs() -> Vec<String> {
    let mut addrs = Vec::new();
    if let Ok(SocketAddr::V4(v4)) = std::net::UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0))
        .and_then(|socket| {
            socket.connect((std::net::Ipv4Addr::new(1, 1, 1, 1), 80))?;
            socket.local_addr()
        })
    {
        let ip = v4.ip().to_string();
        if ip != "127.0.0.1" {
            addrs.push(ip);
        }
    }
    addrs
}

/// Header pill shown by the plugin editor and the listen pages.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionPill {
    /// Live is off.
    Off,
    /// Bind or join failed.
    Failed,
    /// Public upload is holding silence.
    Asleep,
    /// UDP listen is bound and waiting for someone.
    Hosting,
    /// Join is in progress.
    Joining,
    /// Public upload is up but nobody is listening.
    Streaming,
    /// At least one plugin or browser is attached.
    Live,
}

impl SessionPill {
    /// Stable lowercase label for the editor chrome.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Failed => "failed",
            Self::Asleep => "asleep",
            Self::Hosting | Self::Streaming => "ready",
            Self::Joining => "join",
            Self::Live => "live",
        }
    }
}

/// Inputs the editor and tests use to classify the session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionView {
    /// Live toggle.
    pub linked: bool,
    /// Product role.
    pub role: SessionRole,
    /// Worker lifecycle.
    pub state: ConnectionState,
    /// UDP peers.
    pub peers: usize,
    /// Local HTTP browsers.
    pub lan_browsers: u32,
    /// Cloudflare `/out` browsers.
    pub web_listeners: u32,
    /// Public upload socket is open.
    pub web_ok: bool,
    /// Public upload is in DTX hold.
    pub web_silent: bool,
    /// Musician opted into the public page.
    pub web_wanted: bool,
    /// A UDP socket is bound.
    pub bound: bool,
    /// A cloud join is delivering the host's audio.
    pub cloud_rx: bool,
}

/// Maps a snapshot to the header pill.
#[must_use]
pub fn classify_session(view: SessionView) -> SessionPill {
    if !view.linked {
        return SessionPill::Off;
    }
    if view.state == ConnectionState::Failed {
        return SessionPill::Failed;
    }
    if view.web_ok && view.web_silent {
        return SessionPill::Asleep;
    }
    let audience = view.peers > 0 || view.lan_browsers > 0 || view.web_listeners > 0;
    if audience || view.cloud_rx || view.state == ConnectionState::Connected {
        return SessionPill::Live;
    }
    if view.web_ok {
        return SessionPill::Streaming;
    }
    let hosting = view.bound
        && matches!(
            view.role,
            SessionRole::ConnectListen | SessionRole::StreamHub | SessionRole::StreamPublish
        );
    if hosting {
        return SessionPill::Hosting;
    }
    SessionPill::Joining
}

/// One-line status under the meters.
#[must_use]
pub fn format_session_status(
    view: SessionView,
    _udp_port: Option<u16>,
    _lan_http: u16,
    dropouts: u32,
    who: &str,
    error: &str,
) -> String {
    if !view.linked {
        return "Off".into();
    }
    if view.state == ConnectionState::Failed {
        if error.is_empty() {
            return "Failed".into();
        }
        return format!("Failed · {error}");
    }
    let mut bits = Vec::new();
    match classify_session(view) {
        SessionPill::Asleep => bits.push("Asleep".into()),
        SessionPill::Hosting | SessionPill::Streaming => bits.push("Ready".into()),
        SessionPill::Joining => bits.push("Joining".into()),
        SessionPill::Live => bits.push("Live".into()),
        SessionPill::Off | SessionPill::Failed => {}
    }
    let listening = view.peers + view.lan_browsers as usize + view.web_listeners as usize;
    if listening > 0 {
        bits.push(format!("{listening} listening"));
    }
    if view.cloud_rx {
        bits.push("via cloud".into());
    }
    if view.web_wanted && !view.web_ok {
        bits.push("listen page offline".into());
    }
    if !who.is_empty() && view.peers > 0 {
        bits.push(who.to_owned());
    }
    if dropouts > 0 {
        bits.push(format!("{dropouts} dropouts"));
    }
    if bits.is_empty() {
        "Live".into()
    } else {
        bits.join(" · ")
    }
}

const fn encode_state(state: ConnectionState) -> u8 {
    match state {
        ConnectionState::Idle => 0,
        ConnectionState::Creating => 1,
        ConnectionState::Signaling => 2,
        ConnectionState::Connecting => 3,
        ConnectionState::Connected => 4,
        ConnectionState::Recovering => 5,
        ConnectionState::Closing => 6,
        ConnectionState::Closed => 7,
        ConnectionState::Failed => 8,
    }
}

fn decode_state(value: u8) -> ConnectionState {
    match value {
        1 => ConnectionState::Creating,
        2 => ConnectionState::Signaling,
        3 => ConnectionState::Connecting,
        4 => ConnectionState::Connected,
        5 => ConnectionState::Recovering,
        6 => ConnectionState::Closing,
        7 => ConnectionState::Closed,
        8 => ConnectionState::Failed,
        _ => ConnectionState::Idle,
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_slug;

    #[test]
    fn slug_keeps_safe_chars() {
        assert_eq!(normalize_slug("Late Night Mix!"), "latenightmix");
        assert_eq!(normalize_slug("room-AB-12"), "room-ab-12");
    }

    #[test]
    fn rx_pcm_drops_the_oldest_audio_rather_than_playing_late() {
        let control = super::SessionControl::default();
        control.push_rx_pcm(&[0.25; 1_000]);
        assert_eq!(control.take_rx_pcm().len(), 1_000);
        assert!(control.take_rx_pcm().is_empty(), "drained");

        // Half a second is the ceiling; a stall must resync to live.
        control.push_rx_pcm(&[0.5; 40_000]);
        control.push_rx_pcm(&[0.75; 40_000]);
        let queued = control.take_rx_pcm();
        assert_eq!(queued.len(), 48_000);
        assert!(
            (queued[queued.len() - 1] - 0.75).abs() < 1e-6,
            "newest audio survives"
        );
    }

    #[test]
    fn publish_pcm_bumps_generation() {
        let control = super::SessionControl::default();
        assert_eq!(control.pcm_generation(), 0);
        control.publish_pcm(&[0.1, -0.2, 0.3, -0.4]);
        assert_eq!(control.pcm_generation(), 1);
        let pcm = control.last_pcm().expect("pcm");
        assert_eq!(pcm.len(), 4);
        control.publish_pcm(&[0.5, 0.6]);
        assert_eq!(control.take_pcm(8).expect("take").len(), 6);
        assert!(control.last_pcm().expect("empty").is_empty());
        control.publish_pcm(&[]);
        assert_eq!(control.pcm_generation(), 2);
    }

    #[test]
    fn take_pcm_live_drops_old_audio() {
        let control = super::SessionControl::default();
        control.publish_pcm(&[1.0; 16]);
        let kept = control.take_pcm_live(8, 8).expect("take");
        assert_eq!(kept.len(), 8);
        assert!(kept.iter().all(|s| *s == 1.0));
        assert!(control.take_pcm(8).expect("empty").is_empty());
    }

    #[test]
    fn web_listeners_round_trip() {
        let control = super::SessionControl::default();
        assert_eq!(control.web_listeners(), 0);
        control.set_web_listeners(3);
        assert_eq!(control.web_listeners(), 3);
    }

    #[test]
    fn web_silent_round_trip() {
        let control = super::SessionControl::default();
        assert!(!control.web_silent());
        control.set_web_silent(true);
        assert!(control.web_silent());
        control.set_web_silent(false);
        assert!(!control.web_silent());
    }

    #[test]
    fn web_wake_is_consumed_once() {
        let control = super::SessionControl::default();
        assert!(!control.take_web_wake());
        control.request_web_wake();
        assert!(control.take_web_wake());
        assert!(!control.take_web_wake());
    }

    #[test]
    fn hosting_pill_is_bound_listen_without_audience() {
        let view = super::SessionView {
            linked: true,
            role: super::SessionRole::ConnectListen,
            state: super::ConnectionState::Connecting,
            peers: 0,
            lan_browsers: 0,
            web_listeners: 0,
            web_ok: false,
            web_silent: false,
            web_wanted: false,
            bound: true,
            cloud_rx: false,
        };
        assert_eq!(super::classify_session(view), super::SessionPill::Hosting);
        assert_eq!(super::classify_session(view).as_str(), "ready");
        assert!(
            super::format_session_status(view, Some(17_492), 8787, 0, "", "").starts_with("Ready")
        );
        assert!(
            super::format_session_status(view, Some(17_492), 8787, 3, "", "")
                .contains("3 dropouts")
        );
    }

    #[test]
    fn same_ipv4_24_matches_home_lan() {
        assert!(super::same_ipv4_24("192.168.1.20", "192.168.1.80"));
        assert!(!super::same_ipv4_24("192.168.1.20", "192.168.2.80"));
        assert!(!super::same_ipv4_24("192.168.1.20", "not-an-ip"));
    }

    #[test]
    fn lan_listen_url_needs_port_and_name() {
        assert!(super::lan_listen_url("mix", 0).is_none());
        assert!(super::lan_listen_url("", 8787).is_none());
    }

    #[test]
    fn claim_key_is_slug_and_bound_port() {
        let control = super::SessionControl::default();
        let _ = control.set_session_name("Late Night Mix");
        control.set_linked(true);
        control.set_role(super::SessionRole::ConnectListen);
        let snap = crate::engine::SessionSnapshot {
            mode: relay_domain::SessionMode::Connect,
            state: relay_domain::ConnectionState::Connecting,
            route: relay_domain::MediaRoute::Direct,
            peers: 0,
            local_port: Some(17_492),
            bound: true,
            dropouts: 0,
        };
        assert_eq!(
            super::claim_key(&control, snap).as_deref(),
            Some("latenightmix|127.0.0.1:17492")
        );
        control.set_linked(false);
        assert_eq!(super::claim_key(&control, snap), None);
    }
}
