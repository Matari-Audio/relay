//! Off-audio-thread worker: mirrors session strings into the engine, serves
//! same-LAN browsers, claims the public room, and feeds WebRTC listeners.
//!
//! One thread, one 2–8 ms tick. Anything that can block (DNS, TLS, the
//! claim POST) runs on a short-lived dial thread and is polled here, so LAN
//! audio never stalls behind the internet.

use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use relay_session::{
    ConnectionState, PUBLIC_LINK_ORIGIN, SessionControl, SessionRole, normalize_slug,
};
use relay_transport::{IceServer, TurnCredentials};
use tungstenite::client::IntoClientRequest;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use crate::local_listen::LocalHub;
use crate::signal::{ClaimBody, Incoming, Outbound, Signal};
use crate::ws;
use crate::{SessionPersist, SessionStore, default_peer};

type CloudSocket = WebSocket<MaybeTlsStream<TcpStream>>;

/// 20 ms of 48 kHz stereo: one wire batch.
const WEB_BATCH_SAMPLES: usize = 48_000 / 50 * 2;
/// Keep at most 80 ms queued so a stall jumps to live instead of playing late.
const KEEP_SAMPLES: usize = 48_000 / 5 * 2;
/// Smallest batch worth a frame (5 ms). Shorter fragments accumulate.
const MIN_EMIT_SAMPLES: usize = 480;
/// Protocol pings keep the idle `/in` socket alive through Cloudflare.
const PING_EVERY: Duration = Duration::from_secs(12);
/// Per-address TCP timeout; a black-holed IPv6 must not eat the dial.
const CONNECT_WAIT: Duration = Duration::from_secs(2);
/// Empty takes before we declare the DAW stopped and hold the room.
const STARVE_EMPTY: u32 = 12;
/// While held, tick the Opus clock so browsers keep their jitter buffer.
const RTP_KEEP_EVERY: Duration = Duration::from_millis(20);
const ROOM_EVERY: Duration = Duration::from_millis(400);
const IDLE_TICK: Duration = Duration::from_millis(80);
const STARVED_TICK: Duration = Duration::from_millis(8);
const USER_AGENT: &str = "Mozilla/5.0 RELAY/0.1";
/// How long a join gets to find the host directly before the cloud tap
/// opens. LAN is free and lower latency, so it goes first.
const LAN_GRACE: Duration = Duration::from_secs(3);

/// Owns the worker thread; dropping stops and joins it.
pub struct Fanout {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Fanout {
    pub fn spawn(control: Arc<SessionControl>, session: SessionStore) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("relay-fanout".into())
            .spawn(move || supervise(&control, &session, &thread_stop))
            .ok();
        Self { stop, thread }
    }

    pub fn is_alive(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }
}

impl Drop for Fanout {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Restart the worker if it ever panics; the DAW must never lose audio
/// because the network side tripped.
fn supervise(control: &Arc<SessionControl>, session: &SessionStore, stop: &Arc<AtomicBool>) {
    while !stop.load(Ordering::Acquire) {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Worker::new(Arc::clone(control), session.clone(), Arc::clone(stop)).run();
        }));
        if outcome.is_err() && !stop.load(Ordering::Acquire) {
            control.set_last_error("listen thread restarted");
            thread::sleep(IDLE_TICK);
        }
    }
}

fn is_sender(role: SessionRole) -> bool {
    matches!(
        role,
        SessionRole::ConnectListen | SessionRole::StreamHub | SessionRole::StreamPublish
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaEdge {
    Speak,
    Resume,
    HoldStart,
    KeepAlive,
    Idle,
}

fn media_edge(held: bool, has_pcm: bool, empty_runs: u32, has_listeners: bool) -> MediaEdge {
    match (has_pcm, held, has_listeners) {
        (true, true, _) => MediaEdge::Resume,
        (true, false, _) => MediaEdge::Speak,
        (false, true, true) => MediaEdge::KeepAlive,
        (false, false, true) if empty_runs >= STARVE_EMPTY => MediaEdge::HoldStart,
        (false, _, _) => MediaEdge::Idle,
    }
}

struct Worker {
    control: Arc<SessionControl>,
    session: SessionStore,
    stop: Arc<AtomicBool>,
    lan: Arc<LocalHub>,
    p2p: crate::p2p::Hub,
    tap: crate::tap::Tap,
    cloud: Cloud,
    synced: Option<SessionPersist>,
    lan_seq: u32,
    next_room: Instant,
    /// DTX hold: the DAW stopped delivering PCM.
    held: bool,
    empty_runs: u32,
    /// Last emitted stereo sample, for a click-free fade into a hold.
    tail: (f32, f32),
    last_keep: Instant,
    /// Sub-batch PCM waiting for enough samples to emit.
    pending: Vec<f32>,
    /// When the current join attempt started looking for a direct route.
    lan_wait: Option<Instant>,
    silence: Vec<f32>,
}

impl Worker {
    fn new(control: Arc<SessionControl>, session: SessionStore, stop: Arc<AtomicBool>) -> Self {
        let lan = LocalHub::start(Arc::clone(&control), Arc::clone(&stop));
        Self {
            control,
            session,
            stop,
            lan,
            p2p: crate::p2p::Hub::default(),
            tap: crate::tap::Tap::default(),
            cloud: Cloud::new(),
            synced: None,
            lan_seq: 0,
            next_room: Instant::now(),
            held: false,
            empty_runs: 0,
            tail: (0.0, 0.0),
            last_keep: Instant::now(),
            pending: Vec::with_capacity(KEEP_SAMPLES),
            lan_wait: None,
            silence: vec![0.0; WEB_BATCH_SAMPLES],
        }
    }

    fn run(&mut self) {
        while !self.stop.load(Ordering::Acquire) {
            self.tick();
        }
        self.cloud.reset(&self.control);
        self.p2p.clear();
    }

    fn tick(&mut self) {
        self.sync_session();
        let lan_n = self.lan.prune_and_count();
        self.control.set_lan_listeners(lan_n);
        if Instant::now() >= self.next_room {
            self.lan.broadcast_text(&self.room_json(lan_n));
            self.next_room = Instant::now() + ROOM_EVERY;
        }

        let role = self.control.role();
        if self.control.linked() && role == SessionRole::ConnectJoin {
            self.join_tick();
            // Tick fast while audio is arriving; a data channel packet is
            // 10 ms and IDLE_TICK would batch eight of them.
            thread::sleep(if self.tap.connected() {
                STARVED_TICK
            } else {
                IDLE_TICK
            });
            return;
        }
        if !self.control.linked() || !is_sender(role) {
            self.go_idle();
            thread::sleep(IDLE_TICK);
            return;
        }
        let Ok(name) = self.control.session_name() else {
            thread::sleep(IDLE_TICK);
            return;
        };
        let slug = normalize_slug(&name);
        if slug.is_empty() {
            thread::sleep(IDLE_TICK);
            return;
        }

        if self.control.web_wanted() {
            let signals = self.cloud.maintain(&self.control, &slug, self.lan.port());
            if let Some(servers) = self.cloud.poll_ice(&slug) {
                self.p2p.set_ice_servers(servers);
            }
            self.pump_p2p(&signals, lan_n);
        } else {
            self.cloud.reset(&self.control);
            self.p2p.clear();
        }
        self.step_audio(lan_n);
    }

    /// Mirror the editor's strings into the engine; only on change.
    fn sync_session(&mut self) {
        let now = self.session.read();
        if self.synced.as_ref() == Some(&now) {
            return;
        }
        let name = normalize_slug(&now.name);
        if !name.is_empty() {
            let _ = self.control.set_session_name(name);
        }
        let peer = now.peer.trim();
        let _ = self.control.set_peer(if peer.is_empty() {
            default_peer()
        } else {
            peer.to_owned()
        });
        let _ = self.control.set_password(now.password.clone());
        self.synced = Some(now);
    }

    fn go_idle(&mut self) {
        self.drop_tap();
        self.cloud.reset(&self.control);
        self.p2p.clear();
        self.held = false;
        self.empty_runs = 0;
        self.pending.clear();
    }

    /// Join over the cloud when the direct route cannot reach the host.
    ///
    /// The joining plugin becomes an ordinary listener on the host's room, so
    /// ICE handles the NAT traversal that a raw `ip:port` cannot. Only opened
    /// once the direct attempt has had `LAN_GRACE`, and torn down the moment
    /// the direct route connects.
    fn join_tick(&mut self) {
        let Some(slug) = self.control.peer().ok().and_then(|peer| host_slug(&peer)) else {
            self.drop_tap();
            return;
        };
        if self.control.snapshot().state == ConnectionState::Connected {
            self.drop_tap();
            return;
        }
        let since = *self.lan_wait.get_or_insert_with(Instant::now);
        if since.elapsed() < LAN_GRACE {
            return;
        }

        let pass = self.control.password().unwrap_or_default();
        let inbound = self.cloud.maintain_out(&slug, &pass);
        if let Some(servers) = self.cloud.poll_ice(&slug) {
            self.tap.set_ice_servers(servers);
        }
        let mut outgoing = Vec::new();
        let mut pcm = Vec::new();
        self.tap.step(&inbound, &mut outgoing, &mut pcm);
        self.cloud.send_all(outgoing);
        if !pcm.is_empty() {
            self.control.push_rx_pcm(&pcm);
        }
        self.control.set_cloud_rx(self.tap.connected());
    }

    fn drop_tap(&mut self) {
        self.lan_wait = None;
        if self.cloud.leg == "out" {
            self.tap.clear();
            self.cloud.reset(&self.control);
        }
        self.control.set_cloud_rx(false);
    }

    fn pump_p2p(&mut self, signals: &[Signal], lan_n: u32) {
        let mut outgoing = Vec::new();
        self.p2p.apply_all(signals, &mut outgoing);
        self.p2p.drive(&mut outgoing);
        self.cloud.send_all(outgoing);
        self.control.set_web_listeners(self.p2p.peer_count());
        self.control.set_web_ok(self.cloud.socket.is_some());
        let stat = self.stat_json(lan_n);
        self.cloud.send_stat(&stat);
    }

    fn step_audio(&mut self, lan_n: u32) {
        let wake = self.control.take_web_wake();
        let listeners = self.p2p.peer_count();
        let has_out = listeners > 0 || lan_n > 0;

        if let Ok(pcm) = self.control.take_pcm_live(WEB_BATCH_SAMPLES, KEEP_SAMPLES) {
            self.pending.extend_from_slice(&pcm);
        }
        if self.pending.len() >= MIN_EMIT_SAMPLES {
            let batch = std::mem::take(&mut self.pending);
            self.empty_runs = 0;
            if self.held || wake {
                self.tell_live(true);
            }
            self.held = false;
            if let [.., l, r] = batch[..] {
                self.tail = (l, r);
            }
            self.emit(&batch, lan_n, listeners);
            self.control.set_web_silent(false);
            self.pending = batch;
            self.pending.clear();
            return;
        }

        if wake && self.held {
            self.held = false;
            self.empty_runs = 0;
            self.tell_live(true);
            self.control.set_web_silent(false);
        }
        self.empty_runs = self.empty_runs.saturating_add(1);
        match media_edge(self.held, false, self.empty_runs, has_out) {
            MediaEdge::HoldStart => {
                let fade = fade_from_last(self.tail.0, self.tail.1);
                self.tail = (0.0, 0.0);
                self.emit(&fade, lan_n, listeners);
                self.tell_live(false);
                self.held = true;
                self.control.set_web_silent(true);
                self.last_keep = Instant::now();
            }
            MediaEdge::KeepAlive if self.last_keep.elapsed() >= RTP_KEEP_EVERY => {
                let silence = std::mem::take(&mut self.silence);
                self.emit(&silence, lan_n, listeners);
                self.silence = silence;
                self.last_keep = Instant::now();
            }
            _ => {}
        }
        thread::sleep(STARVED_TICK);
    }

    fn emit(&mut self, pcm: &[f32], lan_n: u32, listeners: u32) {
        if lan_n > 0 {
            self.lan_seq = self.lan_seq.wrapping_add(1);
            self.lan.broadcast_bin(&encode_frame(self.lan_seq, pcm));
        }
        if listeners > 0 {
            self.p2p.push_pcm(pcm, self.control.bitrate_kbps());
        }
    }

    fn tell_live(&mut self, live: bool) {
        let msg = if live { Outbound::Go } else { Outbound::Dtx }.to_json();
        self.cloud.send_text(&msg);
        self.lan.broadcast_text(&msg);
    }

    fn room_json(&self, lan_n: u32) -> String {
        let snap = self.control.snapshot();
        let silent = self.control.web_silent();
        Outbound::Room {
            host: true,
            live: lan_n > 0 || snap.peers > 0 || self.control.web_listeners() > 0,
            silent,
            listeners: lan_n,
            peers: snap.peers,
            dropouts: snap.dropouts,
            port: self.lan.port(),
            asleep: silent,
        }
        .to_json()
    }

    fn stat_json(&self, lan_n: u32) -> String {
        let snap = self.control.snapshot();
        Outbound::Stat {
            dropouts: snap.dropouts,
            peers: snap.peers,
            lan: lan_n,
            web: self.p2p.peer_count(),
            ready: self.p2p.ready_count(),
            sent: self.p2p.frames_sent(),
            peak: (self.p2p.last_peak() * 1000.0).round() / 1000.0,
            port: snap.local_port.unwrap_or(0),
        }
        .to_json()
    }
}

/// Result of one background dial attempt.
struct DialOutcome {
    /// Room identity that was claimed, if a claim was attempted and accepted.
    claimed: Option<String>,
    claim_failed: bool,
    socket: Option<CloudSocket>,
}

/// The public room: claim + `/in` signaling socket.
struct Cloud {
    agent: ureq::Agent,
    socket: Option<CloudSocket>,
    /// Which leg of the room this socket is on: `in` to host, `out` to listen.
    leg: &'static str,
    ice: Option<Receiver<Vec<IceServer>>>,
    next_ice_try: Instant,
    claimed: String,
    sent_cfg: String,
    sent_stat: String,
    last_ping: Instant,
    ws_backoff: Duration,
    next_ws_try: Instant,
    claim_backoff: Duration,
    next_claim_try: Instant,
    dial: Option<Receiver<DialOutcome>>,
}

impl Cloud {
    const WS_BACKOFF: (Duration, Duration) = (Duration::from_secs(1), Duration::from_secs(30));
    /// The room mints two-hour TURN credentials; refresh well inside that so a
    /// listener arriving late still gets a relay that will authenticate.
    const ICE_GOOD_FOR: Duration = Duration::from_secs(45 * 60);
    const ICE_RETRY: Duration = Duration::from_secs(60);
    const CLAIM_BACKOFF: (Duration, Duration) = (Duration::from_secs(2), Duration::from_secs(60));

    fn new() -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(2))
                .user_agent(USER_AGENT)
                .build(),
            socket: None,
            leg: "in",
            ice: None,
            next_ice_try: Instant::now(),
            claimed: String::new(),
            sent_cfg: String::new(),
            sent_stat: String::new(),
            last_ping: Instant::now(),
            ws_backoff: Self::WS_BACKOFF.0,
            next_ws_try: Instant::now(),
            claim_backoff: Self::CLAIM_BACKOFF.0,
            next_claim_try: Instant::now(),
            dial: None,
        }
    }

    /// Forget the room entirely (Live off, Join mode, or shutdown).
    fn reset(&mut self, control: &SessionControl) {
        self.drop_socket();
        self.claimed.clear();
        self.dial = None;
        control.set_web_ok(false);
        control.set_web_silent(false);
        control.set_web_listeners(0);
    }

    fn drop_socket(&mut self) {
        if self.socket.take().is_some() {
            self.next_ws_try = Instant::now() + self.ws_backoff;
            self.ws_backoff = (self.ws_backoff * 2).min(Self::WS_BACKOFF.1);
        }
        self.sent_cfg.clear();
        self.sent_stat.clear();
    }

    /// One tick of room upkeep. Returns inbound WebRTC signals.
    fn maintain(&mut self, control: &SessionControl, slug: &str, lan_http: u16) -> Vec<Signal> {
        self.take_leg(control, "in");
        self.poll_dial();
        let port = control
            .snapshot()
            .local_port
            .unwrap_or_else(|| control.bind_port());
        let settings = control.codec_settings();
        let pass = control.password_hex();
        let key = format!("{slug}|{lan_http}|{pass}");
        let need_claim = key != self.claimed;
        let now = Instant::now();
        let due = if need_claim {
            now >= self.next_claim_try
        } else {
            self.socket.is_none() && now >= self.next_ws_try
        };
        if self.dial.is_none() && due {
            let body = need_claim.then(|| {
                ClaimBody::new(
                    slug,
                    port,
                    settings,
                    &pass,
                    control.device_rate_hz(),
                    control.block_frames(),
                    lan_http,
                )
                .to_json()
            });
            self.start_dial(slug.to_owned(), key, body, "in");
        }

        let mut signals = Vec::new();
        let Some(ws) = self.socket.as_mut() else {
            return signals;
        };
        let cfg = Outbound::cfg(settings, port, lan_http).to_json();
        if cfg != self.sent_cfg {
            if !ws::send_keep(ws, Message::Text(cfg.clone().into())) {
                self.drop_socket();
                return signals;
            }
            self.sent_cfg = cfg;
        }
        if self.last_ping.elapsed() >= PING_EVERY {
            if !ws::send_keep(ws, Message::Ping(Vec::new().into())) {
                self.drop_socket();
                return signals;
            }
            self.last_ping = Instant::now();
        }
        let open = ws::drain(ws, |text| signals.extend(Signal::parse(text)));
        if !open {
            self.drop_socket();
        }
        signals
    }

    /// One tick of the listener leg: `/out` on the host's room, exactly what
    /// the browser listen page opens. Returns inbound room messages.
    fn maintain_out(&mut self, slug: &str, pass: &str) -> Vec<Incoming> {
        self.take_leg_out();
        self.poll_dial();
        if self.dial.is_none() && self.socket.is_none() && Instant::now() >= self.next_ws_try {
            self.start_dial(slug.to_owned(), String::new(), None, "out");
        }

        let mut inbound = Vec::new();
        let Some(ws) = self.socket.as_mut() else {
            return inbound;
        };
        // The room takes the listener password as a bare text frame and
        // answers `auth`. Unlocked rooms accept it too, so this needs no
        // knowledge of whether the host set one.
        if self.sent_cfg.is_empty() {
            if !ws::send_keep(ws, Message::Text(pass.to_owned().into())) {
                self.drop_socket();
                return inbound;
            }
            "sent".clone_into(&mut self.sent_cfg);
        }
        if self.last_ping.elapsed() >= PING_EVERY {
            if !ws::send_keep(ws, Message::Ping(Vec::new().into())) {
                self.drop_socket();
                return inbound;
            }
            self.last_ping = Instant::now();
        }
        let open = ws::drain(ws, |text| inbound.extend(Incoming::parse(text)));
        if !open {
            self.drop_socket();
        }
        inbound
    }

    /// Switching roles switches legs; the old socket is for the wrong one.
    fn take_leg(&mut self, control: &SessionControl, leg: &'static str) {
        if self.leg != leg {
            self.reset(control);
            self.leg = leg;
            self.next_ws_try = Instant::now();
        }
    }

    fn take_leg_out(&mut self) {
        if self.leg != "out" {
            self.drop_socket();
            self.claimed.clear();
            self.dial = None;
            self.leg = "out";
            self.next_ws_try = Instant::now();
        }
    }

    fn start_dial(
        &mut self,
        slug: String,
        key: String,
        claim_body: Option<String>,
        leg: &'static str,
    ) {
        let (tx, rx) = mpsc::channel();
        let agent = self.agent.clone();
        let spawned = thread::Builder::new()
            .name("relay-dial".into())
            .spawn(move || {
                let outcome = match claim_body {
                    Some(body) if claim(&agent, &body).is_err() => DialOutcome {
                        claimed: None,
                        claim_failed: true,
                        socket: None,
                    },
                    Some(_) => DialOutcome {
                        claimed: Some(key),
                        claim_failed: false,
                        socket: open_room(&slug, leg),
                    },
                    None => DialOutcome {
                        claimed: None,
                        claim_failed: false,
                        socket: open_room(&slug, leg),
                    },
                };
                let _ = tx.send(outcome);
            });
        if spawned.is_ok() {
            self.dial = Some(rx);
        }
    }

    /// Relay credentials, refreshed on a slow timer off the worker thread.
    /// `None` until one lands; the caller keeps whatever it already had.
    fn poll_ice(&mut self, slug: &str) -> Option<Vec<IceServer>> {
        if let Some(rx) = self.ice.as_ref() {
            match rx.try_recv() {
                Ok(servers) => {
                    self.ice = None;
                    self.next_ice_try = Instant::now()
                        + if servers.is_empty() {
                            Self::ICE_RETRY
                        } else {
                            Self::ICE_GOOD_FOR
                        };
                    return (!servers.is_empty()).then_some(servers);
                }
                Err(TryRecvError::Empty) => return None,
                Err(TryRecvError::Disconnected) => {
                    self.ice = None;
                    self.next_ice_try = Instant::now() + Self::ICE_RETRY;
                    return None;
                }
            }
        }
        if Instant::now() < self.next_ice_try {
            return None;
        }
        let (tx, rx) = mpsc::channel();
        let agent = self.agent.clone();
        let room = slug.to_owned();
        let spawned = thread::Builder::new()
            .name("relay-ice".into())
            .spawn(move || {
                let _ = tx.send(fetch_ice(&agent, &room));
            });
        if spawned.is_ok() {
            self.ice = Some(rx);
        } else {
            self.next_ice_try = Instant::now() + Self::ICE_RETRY;
        }
        None
    }

    fn poll_dial(&mut self) {
        let Some(rx) = self.dial.as_ref() else {
            return;
        };
        let outcome = match rx.try_recv() {
            Ok(outcome) => outcome,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.dial = None;
                return;
            }
        };
        self.dial = None;
        if outcome.claim_failed {
            self.next_claim_try = Instant::now() + self.claim_backoff;
            self.claim_backoff = (self.claim_backoff * 2).min(Self::CLAIM_BACKOFF.1);
            return;
        }
        if let Some(key) = outcome.claimed {
            self.claimed = key;
            self.claim_backoff = Self::CLAIM_BACKOFF.0;
            self.drop_socket();
        }
        if let Some(socket) = outcome.socket {
            self.socket = Some(socket);
            self.ws_backoff = Self::WS_BACKOFF.0;
            self.last_ping = Instant::now();
            self.sent_cfg.clear();
            self.sent_stat.clear();
        } else {
            self.next_ws_try = Instant::now() + self.ws_backoff;
            self.ws_backoff = (self.ws_backoff * 2).min(Self::WS_BACKOFF.1);
        }
    }

    fn send_text(&mut self, text: &str) {
        if let Some(ws) = self.socket.as_mut()
            && !ws::send_keep(ws, Message::Text(text.to_owned().into()))
        {
            self.drop_socket();
        }
    }

    fn send_all(&mut self, messages: Vec<String>) {
        for message in messages {
            if self.socket.is_none() {
                return;
            }
            self.send_text(&message);
        }
    }

    fn send_stat(&mut self, stat: &str) {
        if self.socket.is_some() && stat != self.sent_stat {
            self.send_text(stat);
            if self.socket.is_some() {
                stat.clone_into(&mut self.sent_stat);
            }
        }
    }
}

/// Blocking: `GET /api/ice`. An empty result means STUN only — the peer
/// falls back on its own, so a failure here costs a retry, not a session.
/// The room is named because the server only grants a relay to a room whose
/// host is connected; before the `/in` socket is up this returns STUN.
fn fetch_ice(agent: &ureq::Agent, slug: &str) -> Vec<IceServer> {
    let Some(body) = agent
        .get(&format!("{PUBLIC_LINK_ORIGIN}/api/ice?room={slug}"))
        .call()
        .ok()
        .and_then(|response| response.into_string().ok())
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
    else {
        return Vec::new();
    };
    parse_ice(&body)
}

/// `{"iceServers":[{"urls":[...],"username":..,"credential":..}]}`, where
/// `urls` is a string or an array. Unusable entries are dropped, not fatal.
fn parse_ice(body: &serde_json::Value) -> Vec<IceServer> {
    let mut servers = Vec::new();
    for entry in body["iceServers"].as_array().into_iter().flatten() {
        let credentials = match (entry["username"].as_str(), entry["credential"].as_str()) {
            (Some(user), Some(pass)) => TurnCredentials::new(user, pass).ok(),
            _ => None,
        };
        let urls = match &entry["urls"] {
            serde_json::Value::String(one) => vec![one.as_str()],
            serde_json::Value::Array(many) => many.iter().filter_map(|u| u.as_str()).collect(),
            _ => continue,
        };
        servers.extend(
            urls.into_iter()
                .filter_map(|url| IceServer::parse_url(url, credentials.clone()).ok()),
        );
    }
    servers
}

fn claim(agent: &ureq::Agent, body: &str) -> Result<(), Box<ureq::Error>> {
    agent
        .post(&format!("{PUBLIC_LINK_ORIGIN}/api/claim"))
        .set("content-type", "application/json")
        .send_string(body)
        .map(drop)
        .map_err(Box::new)
}

/// Blocking: DNS + TCP + TLS + WebSocket handshake for `wss://…/<slug>/<leg>`.
fn open_room(slug: &str, leg: &str) -> Option<CloudSocket> {
    let host = PUBLIC_LINK_ORIGIN
        .strip_prefix("https://")
        .unwrap_or(PUBLIC_LINK_ORIGIN);
    let mut request = format!("wss://{host}/{slug}/{leg}")
        .into_client_request()
        .ok()?;
    let headers = request.headers_mut();
    headers.insert(
        tungstenite::http::header::USER_AGENT,
        tungstenite::http::HeaderValue::from_static(USER_AGENT),
    );
    headers.insert(
        tungstenite::http::header::ORIGIN,
        tungstenite::http::HeaderValue::from_static(PUBLIC_LINK_ORIGIN),
    );
    for addr in ordered_addrs(host, 443) {
        let Ok(stream) = TcpStream::connect_timeout(&addr, CONNECT_WAIT) else {
            continue;
        };
        let _ = stream.set_nodelay(true);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));
        if let Ok((mut ws, _)) = tungstenite::client_tls(request.clone(), stream) {
            match ws.get_mut() {
                MaybeTlsStream::Plain(tcp) => ws::tune_tcp(tcp),
                MaybeTlsStream::Rustls(tls) => ws::tune_tcp(tls.get_ref()),
                _ => {}
            }
            return Some(ws);
        }
    }
    None
}

/// The room name behind a join target, or `None` when the user typed an
/// address. Mirrors `lan_slug` in relay-session: anything with a `:` or `.`
/// is a host, not a room.
fn host_slug(peer: &str) -> Option<String> {
    let trimmed = peer.trim();
    let raw = trimmed.strip_prefix("lan:").unwrap_or(trimmed);
    if raw.is_empty() || raw.contains(':') || raw.contains('.') {
        return None;
    }
    let slug = normalize_slug(raw);
    (!slug.is_empty()).then_some(slug)
}

/// IPv4 first: a black-holed IPv6 route is common on Linux DAW hosts.
fn ordered_addrs(host: &str, port: u16) -> Vec<SocketAddr> {
    let mut addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map(Iterator::collect)
        .unwrap_or_default();
    addrs.sort_by_key(SocketAddr::is_ipv6);
    addrs
}

/// LAN wire frame: `RLY1` + LE u32 sequence + s16le interleaved PCM.
fn encode_frame(seq: u32, pcm: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(8 + pcm.len() * 2);
    bytes.extend_from_slice(b"RLY1");
    bytes.extend_from_slice(&seq.to_le_bytes());
    for sample in pcm {
        let quant = (sample.clamp(-1.0, 1.0) * 32_767.0) as i16;
        bytes.extend_from_slice(&quant.to_le_bytes());
    }
    bytes
}

/// One batch that ramps the last live sample down to zero.
fn fade_from_last(last_l: f32, last_r: f32) -> Vec<f32> {
    let frames = WEB_BATCH_SAMPLES / 2;
    let mut out = vec![0.0; WEB_BATCH_SAMPLES];
    for (i, pair) in out.chunks_exact_mut(2).enumerate() {
        let t = (i + 1) as f32 / frames as f32;
        let gain = 0.5 + 0.5 * (core::f32::consts::PI * t).cos();
        pair[0] = last_l * gain;
        pair[1] = last_r * gain;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use relay_session::CodecSettings;

    #[test]
    fn parse_ice_reads_a_cloudflare_response() {
        let body = serde_json::json!({
            "iceServers": [
                { "urls": ["stun:stun.cloudflare.com:3478", "stun:stun.cloudflare.com:53"] },
                {
                    "urls": [
                        "turn:turn.cloudflare.com:3478?transport=udp",
                        "turns:turn.cloudflare.com:443?transport=tcp"
                    ],
                    "username": "u",
                    "credential": "c"
                }
            ]
        });
        let servers = parse_ice(&body);
        assert_eq!(servers.len(), 4, "{servers:?}");
        assert_eq!(
            servers
                .iter()
                .filter(|s| matches!(s, IceServer::Turn { .. }))
                .count(),
            2,
            "both relay urls survive: {servers:?}"
        );
    }

    #[test]
    fn parse_ice_drops_what_it_cannot_use_instead_of_giving_up() {
        // A turn url with no credentials and a junk scheme sit beside a good one.
        let body = serde_json::json!({
            "iceServers": [
                { "urls": "turn:turn.example.com:3478?transport=udp" },
                { "urls": ["nonsense", "stun:stun.example.com:3478"] },
                { "urls": 7 }
            ]
        });
        let servers = parse_ice(&body);
        assert_eq!(servers.len(), 1, "{servers:?}");
        assert!(matches!(servers[0], IceServer::Stun { .. }));
        assert!(parse_ice(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn media_edge_resume_is_an_event() {
        assert_eq!(media_edge(true, true, 0, true), MediaEdge::Resume);
        assert_eq!(media_edge(false, true, 0, true), MediaEdge::Speak);
        assert_eq!(media_edge(false, true, 99, false), MediaEdge::Speak);
    }

    #[test]
    fn media_edge_keeps_rtp_warm_after_hold() {
        assert_eq!(
            media_edge(false, false, STARVE_EMPTY - 1, true),
            MediaEdge::Idle
        );
        assert_eq!(
            media_edge(false, false, STARVE_EMPTY, true),
            MediaEdge::HoldStart
        );
        assert_eq!(
            media_edge(true, false, STARVE_EMPTY + 8, true),
            MediaEdge::KeepAlive
        );
        assert_eq!(media_edge(true, false, 3, false), MediaEdge::Idle);
    }

    #[test]
    fn encode_frame_has_magic_and_seq() {
        let bytes = encode_frame(7, &[0.0, 0.5]);
        assert_eq!(&bytes[..4], b"RLY1");
        assert_eq!(&bytes[4..8], 7_u32.to_le_bytes());
        assert_eq!(bytes.len(), 12);
        assert_eq!(i16::from_le_bytes([bytes[10], bytes[11]]), 16_383);
    }

    #[test]
    fn fade_from_last_starts_near_tail_and_ends_silent() {
        let pcm = fade_from_last(0.8, -0.4);
        assert_eq!(pcm.len(), WEB_BATCH_SAMPLES);
        assert!((pcm[0] - 0.8).abs() < 0.05);
        assert!((pcm[1] + 0.4).abs() < 0.05);
        assert!(pcm[pcm.len() - 2].abs() < 0.05);
        assert!(pcm[pcm.len() - 1].abs() < 0.05);
    }

    #[test]
    fn host_slug_only_accepts_room_names() {
        assert_eq!(host_slug("Studio Mix"), Some("studiomix".into()));
        assert_eq!(host_slug("lan:studio-mix"), Some("studio-mix".into()));
        // An address is for the direct route; there is no room by that name.
        assert_eq!(host_slug("192.168.1.5:17492"), None);
        assert_eq!(host_slug("relay.example.com"), None);
        assert_eq!(host_slug("   "), None);
    }

    #[test]
    fn ordered_addrs_puts_ipv4_first() {
        let addrs = ordered_addrs("localhost", 443);
        let first_v6 = addrs.iter().position(SocketAddr::is_ipv6);
        let last_v4 = addrs.iter().rposition(SocketAddr::is_ipv4);
        if let (Some(v6), Some(v4)) = (first_v6, last_v4) {
            assert!(v4 < v6);
        }
    }

    #[test]
    fn dropping_fanout_joins_the_thread() {
        let control = Arc::new(SessionControl::default());
        let fanout = Fanout::spawn(control, SessionStore::default());
        assert!(fanout.is_alive());
        let start = Instant::now();
        drop(fanout);
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    fn diag_slug(prefix: &str) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |elapsed| elapsed.as_nanos());
        format!("{prefix}-{nanos}")
    }

    fn diag_claim(slug: &str) {
        let body = ClaimBody::new(slug, 17_492, CodecSettings::live(), "", 48_000, 128, 8_787);
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(4))
            .user_agent("Mozilla/5.0 RELAY/diag")
            .build();
        claim(&agent, &body.to_json()).expect("claim against relay.matari-audio.com");
    }

    #[test]
    #[ignore = "hits production relay.matari-audio.com"]
    fn live_cloud_claim_and_in_socket() {
        let slug = diag_slug("diag");
        diag_claim(&slug);
        assert!(
            open_room(&slug, "in").is_some(),
            "/in must open with the plugin's TLS stack"
        );
    }

    #[test]
    #[ignore = "hits production relay.matari-audio.com"]
    fn live_in_socket_receives_want_from_waiting_out() {
        let slug = diag_slug("want");
        diag_claim(&slug);
        let host = PUBLIC_LINK_ORIGIN
            .strip_prefix("https://")
            .expect("https origin");
        let (mut listener, _) =
            tungstenite::connect(format!("wss://{host}/{slug}/out")).expect("listener /out");
        listener
            .send(Message::Text(r#"{"t":"want"}"#.to_owned().into()))
            .expect("listener want");
        let mut socket = open_room(&slug, "in").expect("host /in after waiting listener");
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut signals = Vec::new();
        while Instant::now() < deadline {
            assert!(ws::drain(&mut socket, |text| signals.extend(Signal::parse(text))));
            if signals.iter().any(|s| matches!(s, Signal::Want { .. })) {
                return;
            }
            thread::sleep(Duration::from_millis(40));
        }
        panic!("host /in got no want after waiting /out; signals={signals:?}");
    }
}
