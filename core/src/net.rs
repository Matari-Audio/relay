//! The network thread. Share: accept tagged HELLOs, stream the tx ring to
//! every live peer, and offer the same audio over WebRTC to peers that say
//! hello through signaling. Join: find the host (typed address, else mDNS),
//! HELLO it once a second and write its audio into the rx ring by frame
//! index; with no LAN host after 1.5 s, answer its WebRTC offer instead.

use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rtrb::{Consumer, Producer};
use serde_json::{Value, json};

use crate::mdns::{Mdns, hex};
use crate::rtc::{Guest, Host};
use crate::signal::{Msg, Signal};
use crate::{CHANNELS, Net, PORT, Role, SITE, Shared, tag, wire};

/// A peer that has not said HELLO for this long is dropped.
const PEER_TIMEOUT: Duration = Duration::from_secs(3);
const HELLO_EVERY: Duration = Duration::from_secs(1);
const MAX_PEERS: usize = 16;
/// Join: how long mDNS gets before we go to the internet.
const LAN_FIRST: Duration = Duration::from_millis(1_500);
/// Poll period while audio flows: the most the link adds to latency.
const TICK: Duration = Duration::from_micros(500);
/// Poll period while nobody listens: 20x fewer wakeups.
const IDLE: Duration = Duration::from_millis(10);
/// Share: the longest a part-filled datagram waits for more samples. Small
/// host blocks then travel as full datagrams, not one tiny one each.
const COALESCE: Duration = Duration::from_millis(2);

/// Owns the network thread; dropping it stops and joins the thread.
pub struct Link {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Link {
    /// `tx`: audio the plugin shares. `rx`: audio the plugin plays.
    pub fn spawn(shared: Arc<Shared>, tx: Consumer<f32>, rx: Producer<f32>) -> Self {
        let s = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("relay-link".into())
            .spawn(move || run(&s, tx, rx))
            .ok();
        Self { shared, thread }
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.shared.stop.store(true, Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(shared: &Shared, mut tx: Consumer<f32>, mut rx: Producer<f32>) {
    let mut session: Option<Session> = None;
    let mut key = (Role::Off, u32::MAX);
    while !shared.stop.load(Relaxed) {
        let now = (shared.role(), shared.config.load(Relaxed));
        if now != key {
            key = now;
            session = Session::open(shared, now.0);
        }
        match session.as_mut() {
            Some(s) => s.step(shared, &mut tx, &mut rx),
            None => {
                // Nothing to send to: keep the ring from filling.
                let n = tx.slots();
                if let Ok(chunk) = tx.read_chunk(n) {
                    chunk.commit_all();
                }
                if key.0 != Role::Off {
                    // Open failed (no free port, bad host): retry soon.
                    key.1 = u32::MAX;
                    thread::sleep(Duration::from_millis(250));
                } else {
                    thread::sleep(Duration::from_millis(20));
                }
                continue;
            }
        }
        let live = shared.peers.load(Relaxed) > 0;
        thread::sleep(if live { TICK } else { IDLE });
    }
}

/// The address other machines reach us on: the one the OS would route a
/// packet out of. `connect` on UDP sends nothing.
pub(crate) fn lan_ip() -> Option<std::net::IpAddr> {
    let probe = UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    probe.connect(("192.0.2.1", 9)).ok()?;
    Some(probe.local_addr().ok()?.ip())
}

struct Session {
    sock: UdpSocket,
    role: Role,
    tag: [u8; 8],
    /// Share: listeners and when each last said HELLO.
    peers: Vec<(SocketAddr, Instant)>,
    /// Join: the host.
    host: Option<SocketAddr>,
    last_hello: Option<Instant>,
    last_audio: Option<Instant>,
    /// Share: frames sent. Join: next frame expected.
    frame: u64,
    synced: bool,
    buf: Vec<u8>,
    samples: Vec<f32>,
    /// Share: since when a part-filled datagram has been waiting.
    waiting: Option<Instant>,
    /// Share: advertises the room. Join: browses for it.
    mdns: Option<Mdns>,
    /// Join: the host came from mDNS, so drop it when it goes quiet.
    found: bool,
    signal: Option<Signal>,
    /// Share: internet peers.
    rtc: Option<Host>,
    /// Join: the internet path.
    guest: Option<Guest>,
    up: bool,
    hello_sent: bool,
    /// Signaling trouble, shown while no audio flows.
    trouble: Option<Net>,
    opened: Instant,
}

impl Session {
    fn open(shared: &Shared, role: Role) -> Option<Self> {
        let mut s = Self::build(shared, role, true)?;
        let room = crate::slug(&Shared::text(&shared.room));
        let path = match role {
            Role::Share => {
                let mut key = shared.host_key.lock().unwrap_or_else(|e| e.into_inner());
                if key.is_empty() {
                    // Not via set_text: that would rebuild this session.
                    *key = crate::random_key();
                }
                format!("/{room}/host?key={key}")
            }
            _ if s.guest.is_some() => format!("/{room}/peer"),
            _ => return Some(s),
        };
        if !room.is_empty() {
            s.signal = Some(Signal::connect(SITE, &path));
        }
        Some(s)
    }

    /// `discover`: advertise/browse mDNS and map a router port. Off in tests.
    fn build(shared: &Shared, role: Role, discover: bool) -> Option<Self> {
        shared.peers.store(0, Relaxed);
        shared.port.store(0, Relaxed);
        shared.set_net(if role == Role::Off {
            Net::Idle
        } else {
            Net::Waiting
        });
        let (sock, host) = match role {
            Role::Off => return None,
            // Several instances on one machine each take the next port.
            Role::Share => (
                (PORT..PORT + 16).find_map(|p| UdpSocket::bind(("0.0.0.0", p)).ok())?,
                None,
            ),
            Role::Join => {
                let peer = Shared::text(&shared.peer);
                let peer = peer.trim();
                let addr = if peer.is_empty() {
                    None
                } else if peer.contains(':') {
                    Some(peer.to_socket_addrs().ok()?.next()?)
                } else {
                    Some((peer, PORT).to_socket_addrs().ok()?.next()?)
                };
                (UdpSocket::bind(("0.0.0.0", 0)).ok()?, addr)
            }
        };
        sock.set_nonblocking(true).ok()?;
        if role == Role::Share {
            let port = sock.local_addr().ok()?.port();
            shared.port.store(u32::from(port), Relaxed);
            let ip = lan_ip().map_or_else(|| "this machine".to_owned(), |ip| ip.to_string());
            let address = if port == PORT {
                ip
            } else {
                format!("{ip}:{port}")
            };
            *shared.address.lock().unwrap_or_else(|e| e.into_inner()) = address;
        }
        let room = crate::slug(&Shared::text(&shared.room));
        let tag = tag(&room, &Shared::text(&shared.password));
        let port = sock.local_addr().ok()?.port();
        let (mdns, rtc, guest) = match (role, host) {
            (Role::Share, _) => (
                discover
                    .then(|| Mdns::advertise(&room, tag, port))
                    .flatten(),
                Host::new(discover),
                None,
            ),
            (_, None) => (
                discover.then(|| Mdns::browse(&room, tag)).flatten(),
                None,
                Guest::new(),
            ),
            _ => (None, None, None),
        };
        Some(Self {
            sock,
            role,
            tag,
            peers: Vec::new(),
            host,
            last_hello: None,
            last_audio: None,
            frame: 0,
            synced: false,
            buf: vec![0; 2048],
            samples: vec![0.0; wire::MAX_FRAMES * CHANNELS],
            waiting: None,
            mdns,
            found: false,
            signal: None,
            rtc,
            guest,
            up: false,
            hello_sent: false,
            trouble: None,
            opened: Instant::now(),
        })
    }

    fn step(&mut self, shared: &Shared, tx: &mut Consumer<f32>, rx: &mut Producer<f32>) {
        let now = Instant::now();
        let mut recv = [0u8; 2048];
        while let Ok((n, from)) = self.sock.recv_from(&mut recv) {
            let Some(p) = wire::decode(&recv[..n]) else {
                continue;
            };
            if p.tag != self.tag {
                continue;
            }
            match (self.role, p.kind) {
                (Role::Share, wire::HELLO) => {
                    if let Some(peer) = self.peers.iter_mut().find(|(a, _)| *a == from) {
                        peer.1 = now;
                    } else if self.peers.len() + self.rtc.as_ref().map_or(0, Host::len) < MAX_PEERS
                    {
                        self.peers.push((from, now));
                    }
                }
                (Role::Share, wire::BYE) => self.peers.retain(|(a, _)| *a != from),
                (Role::Join, wire::AUDIO) if Some(from) == self.host => {
                    self.last_audio = Some(now);
                    self.receive(shared, p.rate, p.frame, p.samples, rx);
                }
                _ => {}
            }
        }
        self.signaling(now);
        match self.role {
            Role::Share => {
                self.peers.retain(|(_, seen)| now - *seen < PEER_TIMEOUT);
                let internet = self.rtc.as_mut().map_or(0, |h| {
                    h.poll(now);
                    h.live()
                });
                shared
                    .peers
                    .store((self.peers.len() + internet) as u32, Relaxed);
                shared.set_net(if !self.peers.is_empty() {
                    Net::Lan
                } else if internet > 0 {
                    Net::Internet
                } else {
                    self.trouble.unwrap_or(Net::Waiting)
                });
                shared
                    .talking
                    .store(self.rtc.as_ref().map_or(0, Host::talking) as u32, Relaxed);
                self.send(shared, tx, rx, now);
            }
            Role::Join => {
                if self.host.is_none()
                    && let Some(addr) = self.mdns.as_ref().and_then(Mdns::found)
                {
                    // The LAN wins: lossless, and no double audio.
                    self.host = Some(addr);
                    self.found = true;
                    self.last_audio = Some(now);
                    if let Some(g) = self.guest.as_mut() {
                        g.close();
                    }
                }
                if self.found && self.last_audio.is_some_and(|t| now - t > PEER_TIMEOUT) {
                    // Found host went quiet: back to looking, internet included.
                    (self.host, self.found, self.hello_sent) = (None, false, false);
                }
                if self.last_hello.is_none_or(|t| now - t >= HELLO_EVERY) {
                    self.last_hello = Some(now);
                    self.control(wire::HELLO);
                }
                let fresh =
                    |t: Option<Instant>| t.is_some_and(|t| now - t < Duration::from_millis(500));
                let lan = self.host.is_some() && fresh(self.last_audio);
                let mut internet = false;
                if let (None, Some(g)) = (self.host, self.guest.as_mut()) {
                    g.poll(now, shared.rate.load(Relaxed), rx);
                    internet = fresh(g.last_audio);
                }
                shared.peers.store(u32::from(lan || internet), Relaxed);
                if shared.net() != Net::RateMismatch || !lan {
                    shared.set_net(if lan {
                        Net::Lan
                    } else if internet {
                        Net::Internet
                    } else {
                        self.trouble.unwrap_or(Net::Waiting)
                    });
                }
                // Nothing to share while joined.
                if let Ok(chunk) = tx.read_chunk(tx.slots()) {
                    chunk.commit_all();
                }
            }
            Role::Off => {}
        }
    }

    /// Handle what signaling says; Join also says hello when it is time.
    fn signaling(&mut self, now: Instant) {
        let Some(signal) = self.signal.as_ref() else {
            return;
        };
        let auth = hex(self.tag);
        for msg in signal.inbox.try_iter() {
            let v: Value = match msg {
                Msg::Up => {
                    if self.trouble == Some(Net::Offline) {
                        self.trouble = None;
                    }
                    continue;
                }
                Msg::Down => {
                    self.up = false;
                    self.trouble = Some(self.trouble.unwrap_or(Net::Offline));
                    continue;
                }
                Msg::Text(t) => serde_json::from_str(&t).unwrap_or_default(),
            };
            let s = |k: &str| v[k].as_str().unwrap_or_default();
            // Peer ids are numbers; we only compare them.
            let id = v["id"].to_string();
            match (s("t"), self.rtc.as_mut(), self.guest.as_mut()) {
                // Each connect starts with `ice`: whatever went wrong before
                // is over, and a joiner says hello.
                // ponytail: TURN servers in it are unused; str0m has no TURN client.
                ("ice", ..) => (self.trouble, self.up, self.hello_sent) = (None, true, false),
                ("error", ..) => match s("code") {
                    "taken" => self.trouble = Some(Net::Taken),
                    "denied" => self.trouble = Some(Net::Denied),
                    // Hold the hello until `host`: one sent now would reach
                    // a host that arrives meanwhile, and so would the one
                    // after `host`, and two offers cross.
                    "no-host" => self.up = false,
                    // full: wait for a reconnect's `ice`.
                    _ => {}
                },
                ("hello", Some(h), _) if h.awaiting(&id) => {}
                ("hello", Some(h), _) => {
                    let full = self.peers.len() + h.len() >= MAX_PEERS;
                    let offer = (s("auth") == auth && !full)
                        .then(|| h.offer(&id, now))
                        .flatten();
                    signal.send(&match offer {
                        Some(sdp) => json!({"t": "offer", "to": v["id"], "sdp": sdp}),
                        None => json!({"t": "deny", "to": v["id"]}),
                    });
                }
                ("answer", Some(h), _) => h.answer(&id, s("sdp")),
                ("leave", Some(h), _) => h.leave(&id),
                ("host", ..) => (self.up, self.hello_sent) = (true, false),
                ("offer", _, Some(g)) if self.host.is_none() => {
                    if let Some(sdp) = g.offer(s("sdp"), now) {
                        signal.send(&json!({"t": "answer", "sdp": sdp}));
                    }
                }
                _ => {}
            }
        }
        let lan_had_its_chance = now - self.opened >= LAN_FIRST;
        if self.role == Role::Join
            && self.up
            && !self.hello_sent
            && self.host.is_none()
            && lan_had_its_chance
        {
            self.hello_sent = true;
            signal.send(&json!({"t": "hello", "kind": "plugin", "auth": auth}));
        }
    }

    fn control(&mut self, kind: u8) {
        if let Some(host) = self.host {
            wire::encode(&mut self.buf, kind, self.tag, 0, 0, &[]);
            let _ = self.sock.send_to(&self.buf, host);
        }
    }

    /// Share: drain the tx ring into full datagrams, one copy per peer. A
    /// part-filled one goes once it has waited [`COALESCE`].
    fn send(
        &mut self,
        shared: &Shared,
        tx: &mut Consumer<f32>,
        rx: &mut Producer<f32>,
        now: Instant,
    ) {
        let rate = shared.rate.load(Relaxed);
        loop {
            let n = (tx.slots() / CHANNELS).min(wire::MAX_FRAMES);
            if n == 0 {
                self.waiting = None;
                return;
            }
            if n < wire::MAX_FRAMES && now - *self.waiting.get_or_insert(now) < COALESCE {
                return;
            }
            self.waiting = None;
            let Ok(chunk) = tx.read_chunk(n * CHANNELS) else {
                return;
            };
            let (a, b) = chunk.as_slices();
            self.samples[..a.len()].copy_from_slice(a);
            self.samples[a.len()..a.len() + b.len()].copy_from_slice(b);
            chunk.commit_all();
            if let Some(h) = self.rtc.as_mut() {
                h.audio(&self.samples[..n * CHANNELS], rate, now);
                h.talk_back(n, rate, rx);
            }
            if !self.peers.is_empty() {
                wire::encode(
                    &mut self.buf,
                    wire::AUDIO,
                    self.tag,
                    rate,
                    self.frame,
                    &self.samples[..n * CHANNELS],
                );
                for (peer, _) in &self.peers {
                    let _ = self.sock.send_to(&self.buf, peer);
                }
            }
            self.frame += n as u64;
        }
    }

    /// Join: place a packet at its frame index. Late packets are dropped,
    /// small gaps become silence, a big jump (host restarted) resyncs.
    fn receive(
        &mut self,
        shared: &Shared,
        rate: u32,
        frame: u64,
        bytes: &[u8],
        rx: &mut Producer<f32>,
    ) {
        let mismatch = rate != shared.rate.load(Relaxed);
        if mismatch {
            shared.set_net(Net::RateMismatch);
        }
        // ponytail: equal rates only; resample in the link if mixed rates matter.
        if mismatch {
            return;
        }
        let n = (bytes.len() / (4 * CHANNELS)) as u64;
        let max_gap = u64::from(rate / 10);
        if !self.synced || frame > self.frame + max_gap || frame + u64::from(rate) < self.frame {
            self.synced = true;
            self.frame = frame;
        }
        if frame + n <= self.frame {
            return;
        }
        let gap = frame.saturating_sub(self.frame) as usize * CHANNELS;
        let skip = self.frame.saturating_sub(frame) as usize * CHANNELS;
        let fresh = wire::samples(bytes).skip(skip);
        let len = gap + (n as usize * CHANNELS - skip);
        // A full ring means playout is stalled; it trims itself when it resumes.
        if let Ok(chunk) = rx.write_chunk_uninit(len) {
            chunk.fill_from_iter(std::iter::repeat_n(0.0, gap).chain(fresh));
        }
        self.frame = frame + n;
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.role == Role::Join {
            self.control(wire::BYE);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(
        role: Role,
        room: &str,
        peer: &str,
    ) -> (Arc<Shared>, Link, Producer<f32>, Consumer<f32>) {
        let shared = Shared::new();
        shared.rate.store(48_000, Relaxed);
        shared.set_text(&shared.room, room);
        shared.set_text(&shared.peer, peer);
        shared.set_role(role);
        let ((tx_in, tx_out), (rx_in, rx_out)) = crate::rings();
        let link = Link::spawn(Arc::clone(&shared), tx_out, rx_in);
        (shared, link, tx_in, rx_out)
    }

    fn wait(mut done: impl FnMut() -> bool) -> bool {
        let end = Instant::now() + Duration::from_secs(3);
        while Instant::now() < end {
            if done() {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        false
    }

    /// Share → Join over loopback: the joiner gets the exact samples, a
    /// stranger with the wrong room gets nothing.
    #[test]
    fn share_to_join_on_loopback() {
        let (host, _h, mut send, _) = link(Role::Share, "room-a", "");
        assert!(wait(|| host.port.load(Relaxed) != 0));
        let port = host.port.load(Relaxed);
        let (_, _j, _, mut hear) = link(Role::Join, "room-a", &format!("127.0.0.1:{port}"));
        let (_, _x, _, stranger) = link(Role::Join, "room-b", &format!("127.0.0.1:{port}"));
        assert!(
            wait(|| host.peers.load(Relaxed) == 1),
            "joiner registered, stranger not"
        );

        let sent: Vec<f32> = (0..4_800).map(|i| (i as f32 * 0.01).sin()).collect();
        let chunk = send.write_chunk_uninit(sent.len()).unwrap();
        chunk.fill_from_iter(sent.iter().copied());
        assert!(wait(|| hear.slots() >= sent.len()));
        let got = hear.read_chunk(sent.len()).unwrap();
        let (a, b) = got.as_slices();
        assert_eq!([a, b].concat(), sent);
        assert_eq!(stranger.slots(), 0);
    }

    /// Share → Join over WebRTC, both at 44.1 kHz (so both resamplers run),
    /// signaling relayed in-process the way the Durable Object would. The
    /// joiner hears the host's sine at its level.
    #[test]
    fn share_to_join_over_webrtc() {
        use crate::signal::Msg;
        use std::sync::mpsc;

        let side = |role| {
            let shared = Shared::new();
            shared.rate.store(44_100, Relaxed);
            shared.set_text(&shared.room, "rtc-room");
            shared.set_role(role);
            let mut s = Session::build(&shared, role, false).unwrap();
            let (out, from) = mpsc::channel();
            let (to, inbox) = mpsc::channel();
            s.signal = Some(Signal { out, inbox });
            to.send(Msg::Text(r#"{"t":"ice","servers":[]}"#.into()))
                .unwrap();
            (shared, s, from, to)
        };
        let (hs, mut host, from_host, to_host) = side(Role::Share);
        let (js, mut join, from_join, to_join) = side(Role::Join);
        join.opened -= LAN_FIRST;
        let ((mut send, mut htx), (mut hrx, _)) = crate::rings();
        let ((_, mut jtx), (mut jrx, mut hear)) = crate::rings();

        let start = Instant::now();
        let (mut pushed, mut heard) = (0usize, Vec::<f32>::new());
        // Debug builds run the codec well below real time; give them room.
        while start.elapsed() < Duration::from_secs(20) && heard.len() < 44_100 {
            host.step(&hs, &mut htx, &mut hrx);
            join.step(&js, &mut jtx, &mut jrx);
            for m in from_join.try_iter() {
                let mut v: Value = serde_json::from_str(&m).unwrap();
                v["id"] = json!(7);
                to_host.send(Msg::Text(v.to_string())).unwrap();
            }
            for m in from_host.try_iter() {
                let v: Value = serde_json::from_str(&m).unwrap();
                assert_eq!(v["t"], "offer", "host denied the joiner: {v}");
                assert_eq!(v["to"], 7);
                assert!(
                    v["sdp"]
                        .as_str()
                        .unwrap()
                        .contains("maxaveragebitrate=510000")
                );
                let fwd = json!({"t": "offer", "sdp": v["sdp"]});
                to_join.send(Msg::Text(fwd.to_string())).unwrap();
            }
            // Real time: a 1 kHz sine at 0.5.
            let due = (start.elapsed().as_secs_f64() * 44_100.0) as usize;
            if due > pushed {
                let n = (due - pushed).min(send.slots() / 2);
                let chunk = send.write_chunk_uninit(n * 2).unwrap();
                chunk.fill_from_iter((pushed..pushed + n).flat_map(|i| {
                    let x = (i as f32 * 1_000.0 * std::f32::consts::TAU / 44_100.0).sin() * 0.5;
                    [x, x]
                }));
                pushed += n;
            }
            if let Ok(chunk) = hear.read_chunk(hear.slots()) {
                let (a, b) = chunk.as_slices();
                heard.extend(a.iter().chain(b).step_by(2));
                chunk.commit_all();
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!((hs.net(), hs.peers.load(Relaxed)), (Net::Internet, 1));
        assert_eq!(js.net(), Net::Internet);
        assert!(heard.len() >= 44_100, "heard {} frames", heard.len());
        let tail = &heard[heard.len() - 11_025..];
        let rms = (tail.iter().map(|x| x * x).sum::<f32>() / tail.len() as f32).sqrt();
        assert!((0.3..0.4).contains(&rms), "rms {rms}, want 0.354");
    }

    /// Share → Join through the deployed signaling worker. Run with
    /// `cargo test -p relay-core live -- --ignored` after a deploy.
    #[test]
    #[ignore = "needs the network and the deployed worker"]
    fn live_share_to_join() {
        let room = format!("live-{}", crate::random_key());
        let side = |role, path: String| {
            let shared = Shared::new();
            shared.rate.store(48_000, Relaxed);
            shared.set_text(&shared.room, &room);
            shared.set_role(role);
            let mut s = Session::build(&shared, role, false).unwrap();
            s.signal = Some(Signal::connect(SITE, &path));
            (shared, s)
        };
        let key = crate::random_key();
        let (hs, mut host) = side(Role::Share, format!("/{room}/host?key={key}"));
        let (js, mut join) = side(Role::Join, format!("/{room}/peer"));
        join.opened -= LAN_FIRST;
        let ((mut send, mut htx), (mut hrx, _)) = crate::rings();
        let ((_, mut jtx), (mut jrx, mut hear)) = crate::rings();
        let start = Instant::now();
        let mut heard = 0;
        while start.elapsed() < Duration::from_secs(30) && heard < 48_000 {
            host.step(&hs, &mut htx, &mut hrx);
            join.step(&js, &mut jtx, &mut jrx);
            if let Ok(chunk) = send.write_chunk_uninit(send.slots().min(96)) {
                chunk.fill_from_iter(std::iter::repeat(0.25));
            }
            if let Ok(chunk) = hear.read_chunk(hear.slots()) {
                heard += chunk.len() / 2;
                chunk.commit_all();
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!((hs.net(), js.net()), (Net::Internet, Net::Internet));
        assert!(heard >= 48_000, "heard {heard} frames");
    }

    /// A browser on the deployed listen page talks back. Needs a browser
    /// driven at `https://{SITE}/$RELAY_TALK_ROOM` with a mic; see
    /// `apps/relay-web/scripts/talk-e2e.mjs`.
    #[test]
    #[ignore = "needs the network, the deployed worker and a browser"]
    fn live_browser_talks_back() {
        let room = std::env::var("RELAY_TALK_ROOM").expect("RELAY_TALK_ROOM");
        let shared = Shared::new();
        shared.rate.store(44_100, Relaxed);
        shared.set_text(&shared.room, &room);
        shared.set_role(Role::Share);
        let mut host = Session::build(&shared, Role::Share, false).unwrap();
        let key = crate::random_key();
        host.signal = Some(Signal::connect(SITE, &format!("/{room}/host?key={key}")));
        let ((mut send, mut tx), (mut rx, mut talk)) = crate::rings();
        let start = Instant::now();
        let (mut pushed, mut heard, mut energy, mut talking) = (0, 0usize, 0.0f64, 0);
        while start.elapsed() < Duration::from_secs(60) && heard < 44_100 {
            host.step(&shared, &mut tx, &mut rx);
            talking = talking.max(shared.talking.load(Relaxed));
            // The host's clock: real-time silence, which also clocks talkback.
            let due = (start.elapsed().as_secs_f64() * 44_100.0) as usize * 2;
            let n = due.saturating_sub(pushed).min(send.slots());
            if let Ok(chunk) = send.write_chunk_uninit(n) {
                chunk.fill_from_iter(std::iter::repeat(0.0));
                pushed += n;
            }
            if let Ok(chunk) = talk.read_chunk(talk.slots()) {
                let (a, b) = chunk.as_slices();
                heard += (a.len() + b.len()) / 2;
                energy += a.iter().chain(b).map(|x| f64::from(x * x)).sum::<f64>();
                chunk.commit_all();
            }
            thread::sleep(Duration::from_millis(1));
        }
        let rms = (energy / (heard.max(1) * 2) as f64).sqrt();
        eprintln!("talking {talking}, heard {heard} frames, rms {rms:.4}");
        assert!(talking >= 1 && heard >= 44_100 && rms > 0.005);
    }
}
