//! The network thread. Share: accept tagged HELLOs, stream the tx ring to
//! every live peer. Join: HELLO the host once a second, write its audio into
//! the rx ring by frame index, zero-filling small gaps.

use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rtrb::{Consumer, Producer};

use crate::{CHANNELS, PORT, Role, Shared, tag, wire};

/// A peer that has not said HELLO for this long is dropped.
const PEER_TIMEOUT: Duration = Duration::from_secs(3);
const HELLO_EVERY: Duration = Duration::from_secs(1);
const MAX_PEERS: usize = 16;
/// Poll period: the most the link adds to latency.
const TICK: Duration = Duration::from_micros(500);

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
        thread::sleep(TICK);
    }
}

/// The address other machines reach us on: the one the OS would route a
/// packet out of. `connect` on UDP sends nothing.
fn lan_ip() -> Option<std::net::IpAddr> {
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
}

impl Session {
    fn open(shared: &Shared, role: Role) -> Option<Self> {
        shared.peers.store(0, Relaxed);
        shared.port.store(0, Relaxed);
        shared.set_net(if role == Role::Off {
            crate::Net::Idle
        } else {
            crate::Net::Waiting
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
                let addr = if peer.contains(':') {
                    peer.to_socket_addrs().ok()?.next()?
                } else {
                    (peer, PORT).to_socket_addrs().ok()?.next()?
                };
                (UdpSocket::bind(("0.0.0.0", 0)).ok()?, Some(addr))
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
        Some(Self {
            sock,
            role,
            tag: tag(&Shared::text(&shared.room), &Shared::text(&shared.password)),
            peers: Vec::new(),
            host,
            last_hello: None,
            last_audio: None,
            frame: 0,
            synced: false,
            buf: vec![0; 2048],
            samples: vec![0.0; wire::MAX_FRAMES * CHANNELS],
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
                    } else if self.peers.len() < MAX_PEERS {
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
        match self.role {
            Role::Share => {
                self.peers.retain(|(_, seen)| now - *seen < PEER_TIMEOUT);
                shared.peers.store(self.peers.len() as u32, Relaxed);
                shared.set_net(if self.peers.is_empty() {
                    crate::Net::Waiting
                } else {
                    crate::Net::Lan
                });
                self.send(shared, tx);
            }
            Role::Join => {
                if self.last_hello.is_none_or(|t| now - t >= HELLO_EVERY) {
                    self.last_hello = Some(now);
                    self.control(wire::HELLO);
                }
                let live = self
                    .last_audio
                    .is_some_and(|t| now - t < Duration::from_millis(500));
                shared.peers.store(u32::from(live), Relaxed);
                if shared.net() != crate::Net::RateMismatch {
                    shared.set_net(if live {
                        crate::Net::Lan
                    } else {
                        crate::Net::Waiting
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

    fn control(&mut self, kind: u8) {
        if let Some(host) = self.host {
            wire::encode(&mut self.buf, kind, self.tag, 0, 0, &[]);
            let _ = self.sock.send_to(&self.buf, host);
        }
    }

    /// Share: drain the tx ring into datagrams, one copy per peer.
    fn send(&mut self, shared: &Shared, tx: &mut Consumer<f32>) {
        let rate = shared.rate.load(Relaxed);
        loop {
            let n = (tx.slots() / CHANNELS).min(wire::MAX_FRAMES);
            if n == 0 {
                return;
            }
            let Ok(chunk) = tx.read_chunk(n * CHANNELS) else {
                return;
            };
            let (a, b) = chunk.as_slices();
            self.samples[..a.len()].copy_from_slice(a);
            self.samples[a.len()..a.len() + b.len()].copy_from_slice(b);
            chunk.commit_all();
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
            shared.set_net(crate::Net::RateMismatch);
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
}
