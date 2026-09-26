//! WebRTC with str0m, sans-IO, driven from the link thread. [`Host`] offers
//! one sendonly Opus track to each peer and encodes once for all of them;
//! [`Guest`] answers, decodes and feeds the rx ring.

use std::net::{IpAddr, SocketAddr, SocketAddrV4, ToSocketAddrs, UdpSocket};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use opus_rs::{Application, OpusDecoder, OpusEncoder};
use rtrb::Producer;
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Async, FixedAsync, Resampler, SincInterpolationParameters};
use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::format::{Codec, FormatParams};
use str0m::media::{Direction, Frequency, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive, Transmit};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};

use crate::CHANNELS;
use crate::portmap::Mapping;

/// 10 ms at 48 kHz.
const FRAME: usize = 480;
const PT: Pt = Pt::new_with_value(111);
const STUN: &str = "stun.cloudflare.com:3478";

fn new_rtc(now: Instant) -> Rtc {
    let mut cfg = Rtc::builder().clear_codecs();
    let opus = FormatParams {
        min_p_time: Some(10),
        use_inband_fec: Some(true),
        stereo: Some(true),
        sprop_stereo: Some(true),
        ..Default::default()
    };
    let hz = Frequency::FORTY_EIGHT_KHZ;
    cfg.codec_config().add_config(PT, None, Codec::Opus, hz, Some(2), opus);
    cfg.build(now)
}

/// This machine's usable addresses: every up interface, loopback included
/// (same-machine peers), IPv6 link-local excluded (needs a scope id).
fn local_ips() -> Vec<IpAddr> {
    let mut ips = Vec::new();
    for i in netdev::get_interfaces().into_iter().filter(|i| i.is_up()) {
        ips.extend(i.ipv4.iter().map(|n| IpAddr::V4(n.addr())));
        ips.extend(i.ipv6.iter().map(|n| n.addr()).filter(|a| !a.is_unicast_link_local()).map(IpAddr::V6));
    }
    ips
}

/// One UDP socket per local address, so str0m always knows the destination,
/// plus the STUN and port-mapped addresses of the primary one.
struct Ice {
    socks: Vec<UdpSocket>,
    txid: [u8; 12],
    srflx: Option<(SocketAddr, SocketAddr)>,
    mapping: Arc<Mutex<Option<Mapping>>>,
    buf: Vec<u8>,
}

impl Ice {
    fn open(map: bool) -> Option<Self> {
        let socks: Vec<UdpSocket> = local_ips()
            .into_iter()
            .filter_map(|ip| UdpSocket::bind((ip, 0)).ok())
            .filter(|s| s.set_nonblocking(true).is_ok())
            .collect();
        let txid: [u8; 12] = crate::random_key().as_bytes()[..12].try_into().ok()?;
        let mapping = Arc::new(Mutex::new(None));
        let lan = crate::net::lan_ip();
        let primary = socks.iter().find(|s| s.local_addr().ok().map(|a| a.ip()) == lan);
        if let Some((sock, SocketAddr::V4(base))) =
            primary.and_then(|s| Some((s.try_clone().ok()?, s.local_addr().ok()?)))
        {
            let slot = Arc::downgrade(&mapping);
            let _ = thread::Builder::new().name("relay-gather".into()).spawn(move || {
                let req = stun_request(txid);
                if let Some(to) = STUN.to_socket_addrs().ok().and_then(|mut a| a.find(SocketAddr::is_ipv4)) {
                    for _ in 0..3 {
                        let _ = sock.send_to(&req, to);
                        thread::sleep(Duration::from_millis(500));
                    }
                }
                if !map {
                    return;
                }
                // If the session is gone by now, `m` drops here and unmaps.
                if let (Some(m), Some(slot)) = (crate::portmap::map(base), slot.upgrade()) {
                    *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(m);
                }
            });
        }
        Some(Self { socks, txid, srflx: None, mapping, buf: vec![0; 2048] })
    }

    /// ponytail: candidates are what we know when the offer/answer is made;
    /// a port mapping that lands later only helps the next peer.
    fn candidates(&self) -> Vec<Candidate> {
        let mut out: Vec<Candidate> = self
            .socks
            .iter()
            .filter_map(|s| Candidate::host(s.local_addr().ok()?, "udp").ok())
            .collect();
        let mapped = self.mapping.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|m| m.external);
        let base = self.srflx.map(|(_, b)| b).or_else(|| {
            let lan = crate::net::lan_ip();
            self.socks.iter().filter_map(|s| s.local_addr().ok()).find(|a| Some(a.ip()) == lan)
        });
        let public = [self.srflx.map(|(a, _)| a), mapped.filter(|m| Some(*m) != self.srflx.map(|(a, _)| a))];
        for (addr, base) in public.into_iter().flatten().zip(std::iter::repeat(base)) {
            if let Some(c) = base.and_then(|b| Candidate::server_reflexive(addr, b, "udp").ok()) {
                out.push(c);
            }
        }
        out
    }

    /// The next datagram for str0m: `(destination, source, len)`, bytes in
    /// `self.buf`. Our own STUN answers are consumed here.
    fn recv(&mut self) -> Option<(SocketAddr, SocketAddr, usize)> {
        for sock in &self.socks {
            while let Ok((n, from)) = sock.recv_from(&mut self.buf) {
                let local = sock.local_addr().ok()?;
                match stun_response(&self.buf[..n], self.txid) {
                    Some(public) => self.srflx = Some((public, local)),
                    None => return Some((local, from, n)),
                }
            }
        }
        None
    }

    fn send(&self, t: &Transmit) {
        if let Some(s) = self.socks.iter().find(|s| s.local_addr().ok() == Some(t.source)) {
            let _ = s.send_to(&t.contents, t.destination);
        }
    }
}

fn stun_request(txid: [u8; 12]) -> Vec<u8> {
    let mut p = vec![0, 1, 0, 0, 0x21, 0x12, 0xa4, 0x42];
    p.extend(txid);
    p
}

/// XOR-MAPPED-ADDRESS (IPv4) from a binding success with our transaction id.
fn stun_response(p: &[u8], txid: [u8; 12]) -> Option<SocketAddr> {
    if p.len() < 20 || p[..2] != [1, 1] || p[8..20] != txid {
        return None;
    }
    let mut at = 20;
    while at + 4 <= p.len() {
        let (kind, len) = (u16::from_be_bytes([p[at], p[at + 1]]), usize::from(u16::from_be_bytes([p[at + 2], p[at + 3]])));
        let v = p.get(at + 4..at + 4 + len)?;
        if kind == 0x20 && len == 8 && v[1] == 1 {
            let port = u16::from_be_bytes([v[2], v[3]]) ^ 0x2112;
            let ip = u32::from_be_bytes([v[4], v[5], v[6], v[7]]) ^ 0x2112_a442;
            return Some(SocketAddr::V4(SocketAddrV4::new(ip.into(), port)));
        }
        at += 4 + len.div_ceil(4) * 4;
    }
    None
}

/// Drain `rtc` to its next timeout. `None`: it failed and is done.
fn drain(rtc: &mut Rtc, ice: &Ice, mut on: impl FnMut(Event)) -> Option<Instant> {
    loop {
        match rtc.poll_output() {
            Ok(Output::Timeout(t)) => return Some(t),
            Ok(Output::Transmit(t)) => ice.send(&t),
            Ok(Output::Event(e)) => on(e),
            Err(_) => return None,
        }
    }
}

/// Interleaved stereo at one fixed rate to another.
struct Resample {
    rs: Async<f32>,
    input: Vec<f32>,
}

impl Resample {
    fn new(from: u32, to: u32) -> Option<Self> {
        let ratio = f64::from(to) / f64::from(from);
        let params = SincInterpolationParameters::default();
        let rs = Async::new_sinc(ratio, 1.0, &params, 256, CHANNELS, FixedAsync::Input).ok()?;
        Some(Self { rs, input: Vec::new() })
    }

    fn run(&mut self, input: &[f32], out: &mut Vec<f32>) {
        self.input.extend_from_slice(input);
        loop {
            let need = self.rs.input_frames_next();
            let max = self.rs.output_frames_max();
            if self.input.len() < need * CHANNELS {
                return;
            }
            let start = out.len();
            out.resize(start + max * CHANNELS, 0.0);
            let (Ok(i), Ok(mut o)) = (
                InterleavedSlice::new(&self.input[..need * CHANNELS], CHANNELS, need),
                InterleavedSlice::new_mut(&mut out[start..], CHANNELS, max),
            ) else {
                return;
            };
            let written = self.rs.process_into_buffer(&i, &mut o, None).map_or(0, |(_, w)| w);
            out.truncate(start + written * CHANNELS);
            self.input.drain(..need * CHANNELS);
        }
    }
}

/// To (or from) 48 kHz: `None` when already there.
fn resampler(slot: &mut Option<((u32, u32), Resample)>, from: u32, to: u32) -> Option<&mut Resample> {
    if from == to || from == 0 || to == 0 {
        *slot = None;
        return None;
    }
    if slot.as_ref().is_none_or(|(k, _)| *k != (from, to)) {
        *slot = Resample::new(from, to).map(|r| ((from, to), r));
    }
    slot.as_mut().map(|(_, r)| r)
}

struct Peer {
    id: String,
    rtc: Rtc,
    mid: Mid,
    pending: Option<SdpPendingOffer>,
    next: Instant,
    live: bool,
    dead: bool,
}

/// Share: every internet peer, one Opus encoder for all of them.
pub struct Host {
    ice: Ice,
    peers: Vec<Peer>,
    enc: OpusEncoder,
    up: Option<((u32, u32), Resample)>,
    pcm: Vec<f32>,
    time: u64,
    packet: Vec<u8>,
}

impl Host {
    pub fn new(map: bool) -> Option<Self> {
        let mut enc = OpusEncoder::new(48_000, CHANNELS, Application::RestrictedLowDelay).ok()?;
        enc.bitrate_bps = 510_000;
        enc.use_inband_fec = true;
        let ice = Ice::open(map)?;
        Some(Self { ice, peers: Vec::new(), enc, up: None, pcm: Vec::new(), time: 0, packet: vec![0; 1500] })
    }

    pub fn len(&self) -> usize {
        self.peers.len()
    }

    /// Peers with audio flowing.
    pub fn live(&self) -> usize {
        self.peers.iter().filter(|p| p.live).count()
    }

    /// A new peer said hello: its complete offer.
    pub fn offer(&mut self, id: &str, now: Instant) -> Option<String> {
        self.leave(id);
        let mut rtc = new_rtc(now);
        for c in self.ice.candidates() {
            rtc.add_local_candidate(c);
        }
        let mut api = rtc.sdp_api();
        let stream = Some("relay".to_owned());
        let mid = api.add_media(MediaKind::Audio, Direction::SendOnly, stream.clone(), stream, None);
        let (offer, pending) = api.apply()?;
        let next = drain(&mut rtc, &self.ice, |_| {})?;
        let sdp = offer.to_sdp_string();
        self.peers.push(Peer { id: id.to_owned(), rtc, mid, pending: Some(pending), next, live: false, dead: false });
        // str0m has no fmtp field for it; the browser's encoder reads it.
        Some(sdp.replace("useinbandfec=1", "useinbandfec=1;maxaveragebitrate=510000"))
    }

    pub fn answer(&mut self, id: &str, sdp: &str) {
        let Some(p) = self.peers.iter_mut().find(|p| p.id == id) else {
            return;
        };
        let (Some(pending), Ok(answer)) = (p.pending.take(), SdpAnswer::from_sdp_string(sdp)) else {
            return;
        };
        p.dead = p.rtc.sdp_api().accept_answer(pending, answer).is_err();
        p.next = drain(&mut p.rtc, &self.ice, |_| {}).unwrap_or(p.next);
    }

    pub fn leave(&mut self, id: &str) {
        self.peers.retain(|p| p.id != id);
    }

    /// Network in, timers, dead peers out.
    pub fn poll(&mut self, now: Instant) {
        while let Some((local, from, n)) = self.ice.recv() {
            let Ok(r) = Receive::new(Protocol::Udp, from, local, &self.ice.buf[..n]) else {
                continue;
            };
            let input = Input::Receive(now, r);
            if let Some(p) = self.peers.iter_mut().find(|p| p.rtc.accepts(&input)) {
                p.dead |= p.rtc.handle_input(input).is_err();
                p.next = Self::drain(p, &self.ice);
            }
        }
        for p in &mut self.peers {
            if now >= p.next {
                p.dead |= p.rtc.handle_input(Input::Timeout(now)).is_err();
                p.next = Self::drain(p, &self.ice);
            }
        }
        self.peers.retain(|p| !p.dead && p.rtc.is_alive());
    }

    fn drain(p: &mut Peer, ice: &Ice) -> Instant {
        let (live, dead) = (&mut p.live, &mut p.dead);
        let next = drain(&mut p.rtc, ice, |e| match e {
            Event::Connected => *live = true,
            Event::IceConnectionStateChange(IceConnectionState::Disconnected) => *dead = true,
            _ => {}
        });
        *dead |= next.is_none();
        next.unwrap_or_else(Instant::now)
    }

    /// Shared audio at `rate`, interleaved stereo. Encoded only while
    /// someone listens.
    pub fn audio(&mut self, samples: &[f32], rate: u32, now: Instant) {
        if self.live() == 0 {
            self.pcm.clear();
            return;
        }
        match resampler(&mut self.up, rate, 48_000) {
            Some(r) => r.run(samples, &mut self.pcm),
            None => self.pcm.extend_from_slice(samples),
        }
        while self.pcm.len() >= FRAME * CHANNELS {
            let n = self.enc.encode(&self.pcm[..FRAME * CHANNELS], FRAME, &mut self.packet).unwrap_or(0);
            self.pcm.drain(..FRAME * CHANNELS);
            let at = MediaTime::new(self.time, Frequency::FORTY_EIGHT_KHZ);
            self.time += FRAME as u64;
            if n == 0 {
                continue;
            }
            let data: Arc<[u8]> = self.packet[..n].into();
            for p in self.peers.iter_mut().filter(|p| p.live) {
                if let Some(w) = p.rtc.writer(p.mid) {
                    p.dead |= w.write(PT, now, at, Arc::clone(&data)).is_err();
                }
                p.next = Self::drain(p, &self.ice);
            }
        }
    }
}

/// Join over the internet: one Rtc, answering the host's offer.
pub struct Guest {
    ice: Ice,
    rtc: Option<(Rtc, Instant)>,
    dec: OpusDecoder,
    down: Option<((u32, u32), Resample)>,
    pcm: Vec<f32>,
    out: Vec<f32>,
    pub last_audio: Option<Instant>,
}

impl Guest {
    pub fn new() -> Option<Self> {
        Some(Self {
            ice: Ice::open(false)?,
            rtc: None,
            dec: OpusDecoder::new(48_000, CHANNELS).ok()?,
            down: None,
            pcm: vec![0.0; 5_760 * CHANNELS],
            out: Vec::new(),
            last_audio: None,
        })
    }

    pub fn offer(&mut self, sdp: &str, now: Instant) -> Option<String> {
        let mut rtc = new_rtc(now);
        for c in self.ice.candidates() {
            rtc.add_local_candidate(c);
        }
        let answer = rtc.sdp_api().accept_offer(SdpOffer::from_sdp_string(sdp).ok()?).ok()?;
        let next = drain(&mut rtc, &self.ice, |_| {})?;
        self.rtc = Some((rtc, next));
        Some(answer.to_sdp_string())
    }

    pub fn close(&mut self) {
        self.rtc = None;
    }

    /// Network in, decoded audio out to `rx` at `rate`.
    /// ponytail: no Opus PLC/FEC decode on loss; the playout buffer rides
    /// out the gap.
    pub fn poll(&mut self, now: Instant, rate: u32, rx: &mut Producer<f32>) {
        let Some((rtc, next)) = self.rtc.as_mut() else {
            return;
        };
        let mut packets: Vec<Arc<[u8]>> = Vec::new();
        let mut dead = false;
        let mut on = |e| match e {
            Event::MediaData(m) => packets.push(m.data),
            Event::IceConnectionStateChange(IceConnectionState::Disconnected) => dead = true,
            _ => {}
        };
        while let Some((local, from, n)) = self.ice.recv() {
            let Ok(r) = Receive::new(Protocol::Udp, from, local, &self.ice.buf[..n]) else {
                continue;
            };
            let input = Input::Receive(now, r);
            if rtc.accepts(&input) {
                dead |= rtc.handle_input(input).is_err();
                *next = drain(rtc, &self.ice, &mut on).unwrap_or(now);
            }
        }
        if now >= *next {
            dead |= rtc.handle_input(Input::Timeout(now)).is_err();
            *next = drain(rtc, &self.ice, &mut on).unwrap_or(now);
        }
        if dead || !rtc.is_alive() {
            self.rtc = None;
        }
        for p in packets {
            let Ok(n) = self.dec.decode(&p, 5_760, &mut self.pcm) else {
                continue;
            };
            self.last_audio = Some(now);
            let pcm = &self.pcm[..n * CHANNELS];
            self.out.clear();
            match resampler(&mut self.down, 48_000, rate) {
                Some(r) => r.run(pcm, &mut self.out),
                None => self.out.extend_from_slice(pcm),
            }
            // A full ring means playout is stalled; it trims itself on resume.
            if let Ok(chunk) = rx.write_chunk_uninit(self.out.len()) {
                chunk.fill_from_iter(self.out.iter().copied());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stun_binding_round_trip() {
        let txid = [3; 12];
        assert_eq!(stun_request(txid).len(), 20);
        // Success with XOR-MAPPED-ADDRESS 203.0.113.7:40000.
        let mut p = vec![1, 1, 0, 12, 0x21, 0x12, 0xa4, 0x42];
        p.extend(txid);
        p.extend([0, 0x20, 0, 8, 0, 1]);
        p.extend((40_000u16 ^ 0x2112).to_be_bytes());
        p.extend((u32::from_be_bytes([203, 0, 113, 7]) ^ 0x2112_a442).to_be_bytes());
        assert_eq!(stun_response(&p, txid), Some(SocketAddr::from(([203, 0, 113, 7], 40_000))));
        assert_eq!(stun_response(&p, [4; 12]), None);
    }
}
