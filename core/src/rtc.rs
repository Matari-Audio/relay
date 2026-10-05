//! WebRTC with str0m, sans-IO, driven from the link thread. [`Host`] offers
//! one Opus track to each peer; browser listeners hear the plugin mix while
//! plugin peers hear the host. [`Guest`] sends and receives on that track.

use std::collections::VecDeque;

use std::net::{IpAddr, SocketAddr, SocketAddrV4, ToSocketAddrs, UdpSocket};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use opus_rs::{Application, OpusDecoder, OpusEncoder};
use rtrb::Producer;
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Async, FixedAsync, Resampler, SincInterpolationParameters};
use str0m::bwe::{Bitrate, BweKind};
use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::format::{Codec, FormatParams};
use str0m::media::{Direction, Frequency, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive, Transmit};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};

use crate::portmap::Mapping;
use crate::{CHANNELS, Talker};

/// 10 ms at 48 kHz.
const FRAME: usize = 480;
/// Talkback: a voice starts playing once this much is queued, 40 ms…
const PRIME: usize = 1_920 * CHANNELS;
/// …and a backlog past 200 ms is cut back to that.
const BACKLOG: usize = 9_600 * CHANNELS;
/// Adaptive bitrate: the most Opus is asked for, and the least it falls to.
pub const MAX_BPS: u32 = 510_000;
const MIN_BPS: u32 = 32_000;
/// Silence gate: after this many silent 10 ms frames (300 ms, room for a
/// reverb tail) nothing is sent until sound returns. Relayed (TURN) traffic
/// is billed per byte, and silence is most of a session.
const GATE_AFTER: u32 = 30;
/// Below this a sample counts as silent: -80 dBFS.
const SILENT: f32 = 1e-4;
const PT: Pt = Pt::new_with_value(111);
const STUN: &str = "stun.cloudflare.com:3478";
const RECONNECT_AFTER: Duration = Duration::from_secs(5);

/// `bwe`: estimate the path to the peer, so the host can size its bitrate.
fn new_rtc(now: Instant, bwe: bool) -> Rtc {
    let mut cfg = Rtc::builder()
        .clear_codecs()
        .enable_bwe(bwe.then(|| Bitrate::bps(MAX_BPS.into())));
    let opus = FormatParams {
        min_p_time: Some(10),
        use_inband_fec: Some(true),
        stereo: Some(true),
        sprop_stereo: Some(true),
        ..Default::default()
    };
    let hz = Frequency::FORTY_EIGHT_KHZ;
    cfg.codec_config()
        .add_config(PT, None, Codec::Opus, hz, Some(2), opus);
    cfg.build(now)
}

/// This machine's usable addresses: every up interface, loopback included
/// (same-machine peers), IPv6 link-local excluded (needs a scope id).
fn local_ips() -> Vec<IpAddr> {
    let mut ips = Vec::new();
    for i in netdev::get_interfaces().into_iter().filter(|i| i.is_up()) {
        ips.extend(i.ipv4.iter().map(|n| IpAddr::V4(n.addr())));
        ips.extend(
            i.ipv6
                .iter()
                .map(|n| n.addr())
                .filter(|a| !a.is_unicast_link_local())
                .map(IpAddr::V6),
        );
    }
    ips
}

/// One UDP socket per local address, so str0m always knows the destination,
/// plus STUN addresses for both families and a primary IPv4 port mapping.
struct Ice {
    socks: Vec<UdpSocket>,
    txid: [u8; 12],
    /// The primary IPv4 socket used for router port mapping.
    primary: Option<SocketAddr>,
    srflx: Vec<(SocketAddr, SocketAddr)>,
    announced_mapping: Option<SocketAddr>,
    discoveries: Vec<Candidate>,
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
        let primary = socks
            .iter()
            .find(|s| s.local_addr().ok().map(|a| a.ip()) == lan);
        let primary_addr = primary.and_then(|s| s.local_addr().ok());
        let probes: Vec<_> = socks.iter().filter_map(|s| s.try_clone().ok()).collect();
        let _ = thread::Builder::new()
            .name("relay-stun".into())
            .spawn(move || {
                let destinations: Vec<_> = STUN
                    .to_socket_addrs()
                    .map(Iterator::collect)
                    .unwrap_or_default();
                let req = stun_request(txid);
                for _ in 0..3 {
                    for sock in &probes {
                        if let Ok(local) = sock.local_addr()
                            && let Some(to) =
                                destinations.iter().find(|a| a.is_ipv4() == local.is_ipv4())
                        {
                            let _ = sock.send_to(&req, to);
                        }
                    }
                    thread::sleep(Duration::from_millis(500));
                }
            });
        if map && let Some(SocketAddr::V4(base)) = primary_addr {
            let slot = Arc::downgrade(&mapping);
            let _ = thread::Builder::new()
                .name("relay-portmap".into())
                .spawn(move || {
                    // If the session is gone by now, `m` drops here and unmaps.
                    if let (Some(m), Some(slot)) = (crate::portmap::map(base), slot.upgrade()) {
                        *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(m);
                    }
                });
        }
        Some(Self {
            socks,
            txid,
            primary: primary_addr,
            srflx: Vec::new(),
            announced_mapping: None,
            discoveries: Vec::new(),
            mapping,
            buf: vec![0; 2048],
        })
    }

    fn candidates(&self) -> Vec<Candidate> {
        let mut out: Vec<Candidate> = self
            .socks
            .iter()
            .filter_map(|s| Candidate::host(s.local_addr().ok()?, "udp").ok())
            .collect();
        let mapped = self
            .mapping
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|m| m.external);
        for &(base, addr) in &self.srflx {
            out.extend(Candidate::server_reflexive(addr, base, "udp").ok());
        }
        if let (Some(base), Some(addr)) = (self.primary, mapped) {
            out.extend(Candidate::server_reflexive(addr, base, "udp").ok());
        }
        out
    }

    fn discoveries(&mut self) -> Vec<Candidate> {
        let mapped = self
            .mapping
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|m| m.external);
        if let (Some(base), Some(addr)) = (self.primary, mapped)
            && self.announced_mapping != Some(addr)
        {
            self.announced_mapping = Some(addr);
            self.discoveries
                .extend(Candidate::server_reflexive(addr, base, "udp").ok());
        }
        std::mem::take(&mut self.discoveries)
    }

    /// The next datagram for str0m: `(destination, source, len)`, bytes in
    /// `self.buf`. Our own STUN answers are consumed here.
    fn recv(&mut self) -> Option<(SocketAddr, SocketAddr, usize)> {
        for sock in &self.socks {
            while let Ok((n, from)) = sock.recv_from(&mut self.buf) {
                let local = sock.local_addr().ok()?;
                match stun_response(&self.buf[..n], self.txid) {
                    Some(public) => {
                        if !self.srflx.contains(&(local, public)) {
                            self.srflx.push((local, public));
                            self.discoveries
                                .extend(Candidate::server_reflexive(public, local, "udp").ok());
                        }
                    }
                    None => return Some((local, from, n)),
                }
            }
        }
        None
    }

    fn send(&self, t: &Transmit) {
        if let Some(s) = self
            .socks
            .iter()
            .find(|s| s.local_addr().ok() == Some(t.source))
        {
            let _ = s.send_to(&t.contents, t.destination);
        }
    }
}

fn stun_request(txid: [u8; 12]) -> Vec<u8> {
    let mut p = vec![0, 1, 0, 0, 0x21, 0x12, 0xa4, 0x42];
    p.extend(txid);
    p
}

/// XOR-MAPPED-ADDRESS from a binding success with our transaction id.
fn stun_response(p: &[u8], txid: [u8; 12]) -> Option<SocketAddr> {
    if p.len() < 20 || p[..2] != [1, 1] || p[4..8] != [0x21, 0x12, 0xa4, 0x42] || p[8..20] != txid {
        return None;
    }
    let end = 20 + usize::from(u16::from_be_bytes([p[2], p[3]]));
    let p = p.get(..end)?;
    let mut at = 20;
    while at + 4 <= p.len() {
        let (kind, len) = (
            u16::from_be_bytes([p[at], p[at + 1]]),
            usize::from(u16::from_be_bytes([p[at + 2], p[at + 3]])),
        );
        let v = p.get(at + 4..at + 4 + len)?;
        if kind == 0x20 && len == 8 && v[1] == 1 {
            let port = u16::from_be_bytes([v[2], v[3]]) ^ 0x2112;
            let ip = u32::from_be_bytes([v[4], v[5], v[6], v[7]]) ^ 0x2112_a442;
            return Some(SocketAddr::V4(SocketAddrV4::new(ip.into(), port)));
        }
        if kind == 0x20 && len == 20 && v[1] == 2 {
            let port = u16::from_be_bytes([v[2], v[3]]) ^ 0x2112;
            let mask = p.get(4..20)?;
            let mut ip = [0; 16];
            for i in 0..16 {
                ip[i] = v[i + 4] ^ mask[i];
            }
            return Some(SocketAddr::new(std::net::Ipv6Addr::from(ip).into(), port));
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
        Some(Self {
            rs,
            input: Vec::new(),
        })
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
            let written = self
                .rs
                .process_into_buffer(&i, &mut o, None)
                .map_or(0, |(_, w)| w);
            out.truncate(start + written * CHANNELS);
            self.input.drain(..need * CHANNELS);
        }
    }
}

/// To (or from) 48 kHz: `None` when already there.
fn resampler(
    slot: &mut Option<((u32, u32), Resample)>,
    from: u32,
    to: u32,
) -> Option<&mut Resample> {
    if from == to || from == 0 || to == 0 {
        *slot = None;
        return None;
    }
    if slot.as_ref().is_none_or(|(k, _)| *k != (from, to)) {
        *slot = Resample::new(from, to).map(|r| ((from, to), r));
    }
    slot.as_mut().map(|(_, r)| r)
}

/// Decode short losses without waiting for retransmissions. Longer timestamp
/// gaps are silence/DTX and must not turn into seconds of queued concealment.
struct Decode {
    dec: OpusDecoder,
    channels: usize,
    pcm: Vec<f32>,
    out: Vec<f32>,
    next: Option<u64>,
    toc: u8,
    duration: usize,
}

impl Decode {
    fn new() -> Option<Self> {
        Some(Self {
            dec: OpusDecoder::new(48_000, CHANNELS).ok()?,
            channels: CHANNELS,
            pcm: vec![0.0; 5_760 * CHANNELS],
            out: Vec::new(),
            next: None,
            toc: 0,
            duration: 0,
        })
    }

    fn append(&mut self, n: usize) {
        if self.channels == 1 {
            self.out.extend(self.pcm[..n].iter().flat_map(|&s| [s, s]));
        } else {
            self.out.extend_from_slice(&self.pcm[..n * CHANNELS]);
        }
    }

    fn run(&mut self, packet: &[u8], time: u64) -> &[f32] {
        self.out.clear();
        let Some(&toc) = packet.first() else {
            return &self.out;
        };
        if self.next.is_some_and(|next| time < next) {
            return &self.out;
        }
        let channels = if toc & 4 == 0 { 1 } else { 2 };
        if channels != self.channels {
            let Ok(dec) = OpusDecoder::new(48_000, channels) else {
                return &self.out;
            };
            self.dec = dec;
            self.channels = channels;
            self.next = None;
        }
        let gap = self.next.map_or(0, |next| time.saturating_sub(next)) as usize;
        // The native decoder accepts a single TOC byte as a lost frame.
        // ponytail: PLC for single-frame packets; multi-frame loss stays with playout.
        if gap > 0
            && gap <= 2_880
            && self.duration > 0
            && self.toc & 3 == 0
            && gap.is_multiple_of(self.duration)
        {
            for _ in 0..gap / self.duration {
                let Ok(n) = self.dec.decode(&[self.toc], self.duration, &mut self.pcm) else {
                    break;
                };
                self.append(n);
            }
        }
        if let Ok(n) = self.dec.decode(packet, 5_760, &mut self.pcm) {
            self.append(n);
            self.next = Some(time + n as u64);
            self.toc = toc;
            self.duration = n;
        }
        &self.out
    }
}

/// A peer's input, decoded and queued at 48 kHz until the host's clock
/// mixes it.
struct Voice {
    decode: Decode,
    queue: VecDeque<f32>,
    primed: bool,
    stereo: bool,
    peak: [f32; CHANNELS],
}

impl Voice {
    fn new() -> Option<Self> {
        Some(Self {
            decode: Decode::new()?,
            queue: VecDeque::new(),
            primed: false,
            stereo: false,
            peak: [0.0; CHANNELS],
        })
    }

    fn push(&mut self, packet: &[u8], time: u64) {
        self.queue.extend(self.decode.run(packet, time));
        if self.queue.len() > BACKLOG {
            self.queue.drain(..self.queue.len() - PRIME);
        }
        self.primed |= self.queue.len() >= PRIME;
    }

    /// Add what is due into `out`. Running dry re-primes.
    fn mix_into(&mut self, out: &mut [f32], gain: f32) {
        if !self.primed {
            return;
        }
        let n = out.len().min(self.queue.len());
        let mut left = 0.0;
        for (i, (o, s)) in out.iter_mut().zip(self.queue.drain(..n)).enumerate() {
            if i % CHANNELS == 0 {
                left = s;
            } else {
                self.stereo |= (s - left).abs() > 1e-4;
            }
            self.peak[i % CHANNELS] = self.peak[i % CHANNELS].max(s.abs());
            *o += s * gain;
        }
        self.primed = !self.queue.is_empty();
    }
}

struct Peer {
    id: String,
    web: bool,
    slot: usize,
    name: String,
    rtc: Rtc,
    mid: Mid,
    pending: Option<SdpPendingOffer>,
    next: Instant,
    live: bool,
    dead: bool,
    disconnected: Option<Instant>,
    voice: Voice,
    gain: f32,
    /// What the path to this peer carries, bits/s, from TWCC or REMB.
    estimate: u32,
}

/// Share: every internet peer, one Opus encoder for all of them.
pub struct Host {
    ice: Ice,
    peers: Vec<Peer>,
    enc: OpusEncoder,
    web_enc: OpusEncoder,
    up: Option<((u32, u32), Resample)>,
    pcm: Vec<f32>,
    time: u64,
    packet: Vec<u8>,
    web_packet: Vec<u8>,
    /// Talkback: 48 kHz frames owed, times the host rate.
    owed: u64,
    mix: Vec<f32>,
    remote: Vec<f32>,
    browser_pcm: Vec<f32>,
    lan_up: Option<((u32, u32), Resample)>,
    lan_pcm: Vec<f32>,
    down: Option<((u32, u32), Resample)>,
    talk: Vec<f32>,
    /// The user's ceiling, bits/s. The encoder runs at the lowest peer
    /// estimate under it.
    pub cap: u32,
    /// Silent frames in a row; at [`GATE_AFTER`] the sender goes quiet.
    silent: u32,
}

impl Host {
    pub fn new(map: bool) -> Option<Self> {
        let mut enc = OpusEncoder::new(48_000, CHANNELS, Application::RestrictedLowDelay).ok()?;
        enc.bitrate_bps = 510_000;
        let mut web_enc =
            OpusEncoder::new(48_000, CHANNELS, Application::RestrictedLowDelay).ok()?;
        web_enc.bitrate_bps = 510_000;
        let ice = Ice::open(map)?;
        Some(Self {
            ice,
            peers: Vec::new(),
            enc,
            web_enc,
            up: None,
            pcm: Vec::new(),
            time: 0,
            packet: vec![0; 1500],
            web_packet: vec![0; 1500],
            owed: 0,
            mix: Vec::new(),
            remote: Vec::new(),
            browser_pcm: Vec::new(),
            lan_up: None,
            lan_pcm: Vec::new(),
            down: None,
            talk: Vec::new(),
            cap: MAX_BPS,
            silent: 0,
        })
    }

    pub fn len(&self) -> usize {
        self.peers.len()
    }

    /// Peers with audio flowing.
    pub fn live(&self) -> usize {
        self.peers.iter().filter(|p| p.live).count()
    }

    /// An offer to `id` is out and unanswered: a second hello from it
    /// crossed the first and must not replace it.
    pub fn awaiting(&self, id: &str) -> bool {
        self.peers.iter().any(|p| p.id == id && p.pending.is_some())
    }

    /// A new peer said hello: its complete offer.
    pub fn offer(&mut self, id: &str, web: bool, now: Instant) -> Option<String> {
        self.leave(id);
        let slot = self.peers.len() + 1;
        let mut rtc = new_rtc(now, true);
        rtc.bwe().set_desired_bitrate(Bitrate::bps(self.cap.into()));
        for c in self.ice.candidates() {
            rtc.add_local_candidate(c);
        }
        let mut api = rtc.sdp_api();
        let stream = Some("relay".to_owned());
        let mid = api.add_media(
            MediaKind::Audio,
            Direction::SendRecv,
            stream.clone(),
            stream,
            None,
        );
        let (offer, pending) = api.apply()?;
        let next = drain(&mut rtc, &self.ice, |_| {})?;
        let sdp = offer.to_sdp_string();
        self.peers.push(Peer {
            id: id.to_owned(),
            web,
            slot,
            name: String::new(),
            rtc,
            mid,
            pending: Some(pending),
            next,
            live: false,
            dead: false,
            disconnected: None,
            voice: Voice::new()?,
            gain: 1.0,
            estimate: MAX_BPS,
        });
        // str0m has no fmtp fields for these. A browser reads usedtx as
        // "send nothing while your mic is silent".
        Some(sdp.replace(
            "useinbandfec=1",
            "useinbandfec=1;maxaveragebitrate=510000;usedtx=1",
        ))
    }

    pub fn answer(&mut self, id: &str, sdp: &str) {
        let Some(p) = self.peers.iter_mut().find(|p| p.id == id) else {
            return;
        };
        let Ok(answer) = SdpAnswer::from_sdp_string(sdp) else {
            return;
        };
        let Some(pending) = p.pending.take() else {
            return;
        };
        p.dead = p.rtc.sdp_api().accept_answer(pending, answer).is_err();
        p.next = drain(&mut p.rtc, &self.ice, |_| {}).unwrap_or(p.next);
    }

    /// Browser candidates arrive after the answer. The offer's ICE username
    /// keeps late candidates from an earlier connection out of a replacement.
    pub fn candidate(&mut self, id: &str, candidate: &str, ufrag: &str) {
        let Some(p) = self.peers.iter_mut().find(|p| p.id == id) else {
            return;
        };
        if p.pending.is_some() || p.rtc.direct_api().local_ice_credentials().ufrag != ufrag {
            return;
        }
        if let Ok(c) = Candidate::from_sdp_string(candidate) {
            p.rtc.add_remote_candidate(c);
            p.next = Self::drain(p, &self.ice);
        }
    }

    pub fn leave(&mut self, id: &str) {
        self.peers.retain(|p| p.id != id);
    }

    /// Announce discoveries to peers already negotiating or playing.
    pub fn trickle(&mut self) -> Vec<(String, String, String)> {
        let mut messages = Vec::new();
        for c in self.ice.discoveries() {
            for p in &mut self.peers {
                if let Some(candidate) = p
                    .rtc
                    .add_local_candidate(c.clone())
                    .map(Candidate::to_sdp_string)
                {
                    let ufrag = p.rtc.direct_api().local_ice_credentials().ufrag;
                    messages.push((p.id.clone(), candidate, ufrag));
                    p.next = Self::drain(p, &self.ice);
                }
            }
        }
        messages
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
        self.peers.retain(|p| {
            !p.dead
                && p.rtc.is_alive()
                && p.disconnected
                    .is_none_or(|at| now.saturating_duration_since(at) < RECONNECT_AFTER)
        });
    }

    fn drain(p: &mut Peer, ice: &Ice) -> Instant {
        let (live, dead, voice, estimate, disconnected) = (
            &mut p.live,
            &mut p.dead,
            &mut p.voice,
            &mut p.estimate,
            &mut p.disconnected,
        );
        let next = drain(&mut p.rtc, ice, |e| match e {
            Event::Connected => *live = true,
            Event::MediaData(m) => voice.push(&m.data, m.time.numer()),
            Event::EgressBitrateEstimate(
                BweKind::Twcc { estimate: b, .. } | BweKind::Remb { estimate: b, .. },
            ) => *estimate = b.as_u64().min(u64::from(MAX_BPS)) as u32,
            Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                disconnected.get_or_insert_with(Instant::now);
            }
            Event::IceConnectionStateChange(
                IceConnectionState::Connected | IceConnectionState::Completed,
            ) => *disconnected = None,
            _ => {}
        });
        *dead |= next.is_none();
        next.unwrap_or_else(Instant::now)
    }

    /// What Opus encodes at, bits/s: the slowest listener's path, less a
    /// margin for headers and cross traffic, under the user's cap.
    pub fn bitrate(&self) -> u32 {
        let path = self
            .peers
            .iter()
            .filter(|p| p.live)
            .map(|p| p.estimate)
            .min();
        path.map_or(self.cap, |b| b / 10 * 8)
            .min(self.cap)
            .max(MIN_BPS)
    }

    /// Set the user's ceiling; the estimators probe up to it.
    pub fn set_cap(&mut self, cap: u32) {
        if cap != self.cap {
            self.cap = cap;
            for p in &mut self.peers {
                p.rtc.bwe().set_desired_bitrate(Bitrate::bps(cap.into()));
            }
        }
    }

    /// Peers whose mic is playing.
    pub fn talking(&self) -> usize {
        self.peers
            .iter()
            .filter(|p| p.web && p.voice.primed)
            .count()
    }

    /// The worker's roster order is what browser listeners see on their page.
    pub fn roster(&mut self, slots: &[(String, usize, String)]) {
        for p in &mut self.peers {
            if let Some((_, slot, name)) = slots.iter().find(|(id, _, _)| id == &p.id) {
                p.slot = *slot;
                p.name.clone_from(name);
            }
        }
    }

    /// Keep the editor's source list in peer order, retaining its gain edits.
    pub fn sync_talkers(&mut self, talkers: &mut Vec<Talker>) {
        talkers.retain(|t| {
            t.id.starts_with("lan:") || self.peers.iter().any(|p| p.live && p.id == t.id)
        });
        for p in self.peers.iter_mut().filter(|p| p.live) {
            let t = match talkers.iter_mut().find(|t| t.id == p.id) {
                Some(t) => t,
                None => {
                    talkers.push(Talker {
                        id: p.id.clone(),
                        slot: p.slot,
                        name: if p.name.is_empty() && !p.web {
                            format!("Plugin {}", p.slot)
                        } else {
                            p.name.clone()
                        },
                        plugin: !p.web,
                        stereo: false,
                        active: false,
                        peak: [0.0; CHANNELS],
                        gain_db: 0.0,
                        muted: false,
                    });
                    talkers.last_mut().unwrap()
                }
            };
            t.slot = p.slot;
            t.name = if p.name.is_empty() && !p.web {
                format!("Plugin {}", p.slot)
            } else {
                p.name.clone()
            };
            t.plugin = !p.web;
            t.stereo = !p.web && p.voice.stereo;
            t.active = p.voice.primed;
            for (peak, next) in t.peak.iter_mut().zip(std::mem::take(&mut p.voice.peak)) {
                *peak = peak.max(next);
            }
            p.gain = if t.muted {
                0.0
            } else if t.gain_db.is_finite() {
                10.0_f32.powf(t.gain_db.clamp(-24.0, 12.0) / 20.0)
            } else {
                1.0
            };
        }
    }

    /// Mix peer inputs on the host's clock; plugin inputs also feed browsers.
    pub fn talk_back(&mut self, frames: usize, rate: u32, rx: &mut Producer<f32>, lan: &[f32]) {
        if rate == 0 {
            return;
        }
        self.owed += frames as u64 * 48_000;
        let n = (self.owed / u64::from(rate)) as usize;
        self.owed %= u64::from(rate);
        self.remote.resize(n * CHANNELS, 0.0);
        self.remote.fill(0.0);
        for p in self.peers.iter_mut().filter(|p| !p.web) {
            p.voice.mix_into(&mut self.remote, p.gain);
        }
        self.browser_pcm.extend_from_slice(&self.remote);
        self.mix.clear();
        self.mix.extend_from_slice(&self.remote);
        for p in self.peers.iter_mut().filter(|p| p.web) {
            p.voice.mix_into(&mut self.mix, p.gain);
        }
        if !self.mix.iter().any(|s| *s != 0.0) && !lan.iter().any(|s| *s != 0.0) {
            return;
        }
        self.talk.clear();
        match resampler(&mut self.down, 48_000, rate) {
            Some(r) => r.run(&self.mix, &mut self.talk),
            None => self.talk.extend_from_slice(&self.mix),
        }
        self.talk.resize(lan.len(), 0.0);
        for (dst, src) in self.talk.iter_mut().zip(lan) {
            *dst += src;
        }
        if let Ok(chunk) = rx.write_chunk_uninit(self.talk.len()) {
            chunk.fill_from_iter(self.talk.iter().copied());
        }
    }

    /// Shared audio at `rate`, interleaved stereo. Encoded only while
    /// someone listens.
    pub fn audio(&mut self, samples: &[f32], lan: &[f32], rate: u32, now: Instant) {
        if self.live() == 0 {
            self.pcm.clear();
            self.browser_pcm.clear();
            self.lan_pcm.clear();
            return;
        }
        match resampler(&mut self.up, rate, 48_000) {
            Some(r) => r.run(samples, &mut self.pcm),
            None => self.pcm.extend_from_slice(samples),
        }
        match resampler(&mut self.lan_up, rate, 48_000) {
            Some(r) => r.run(lan, &mut self.lan_pcm),
            None => self.lan_pcm.extend_from_slice(lan),
        }
        self.enc.bitrate_bps = self.bitrate() as i32;
        self.web_enc.bitrate_bps = self.enc.bitrate_bps;
        let has_web = self.peers.iter().any(|p| p.live && p.web);
        while self.pcm.len() >= FRAME * CHANNELS {
            let n = self
                .enc
                .encode(&self.pcm[..FRAME * CHANNELS], FRAME, &mut self.packet)
                .unwrap_or(0);
            self.mix.clear();
            self.mix.extend_from_slice(&self.pcm[..FRAME * CHANNELS]);
            let available = self.browser_pcm.len().min(FRAME * CHANNELS);
            for (dst, src) in self.mix.iter_mut().zip(self.browser_pcm.drain(..available)) {
                *dst += src;
            }
            let lan_available = self.lan_pcm.len().min(FRAME * CHANNELS);
            for (dst, src) in self.mix.iter_mut().zip(self.lan_pcm.drain(..lan_available)) {
                *dst += src;
            }
            let web_n = if has_web {
                self.web_enc
                    .encode(&self.mix, FRAME, &mut self.web_packet)
                    .unwrap_or(0)
            } else {
                0
            };
            let quiet = (if has_web {
                &self.mix[..]
            } else {
                &self.pcm[..FRAME * CHANNELS]
            })
            .iter()
            .all(|s| s.abs() < SILENT);
            self.pcm.drain(..FRAME * CHANNELS);
            let at = MediaTime::new(self.time, Frequency::FORTY_EIGHT_KHZ);
            self.time += FRAME as u64;
            // Still encoded while gated, so the codec state is warm when
            // sound returns; the receiver hears the gap as DTX silence.
            let was = self.silent >= GATE_AFTER;
            self.silent = if quiet {
                self.silent.saturating_add(1)
            } else {
                0
            };
            let gated = self.silent >= GATE_AFTER;
            if gated != was {
                // No bandwidth probing (padding bytes) while there is nothing to send.
                let want = if gated { MIN_BPS } else { self.cap };
                for p in &mut self.peers {
                    p.rtc.bwe().set_desired_bitrate(Bitrate::bps(want.into()));
                }
            }
            if self.silent >= GATE_AFTER {
                continue;
            }
            let data: Arc<[u8]> = self.packet[..n].into();
            let web_data: Arc<[u8]> = self.web_packet[..web_n].into();
            for p in self.peers.iter_mut().filter(|p| p.live) {
                if let Some(w) = p.rtc.writer(p.mid) {
                    let packet = if p.web { &web_data } else { &data };
                    if !packet.is_empty() {
                        p.dead |= w.write(PT, now, at, Arc::clone(packet)).is_err();
                    }
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
    mid: Option<Mid>,
    enc: OpusEncoder,
    up: Option<((u32, u32), Resample)>,
    send_pcm: Vec<f32>,
    packet: Vec<u8>,
    time: u64,
    decode: Decode,
    down: Option<((u32, u32), Resample)>,
    out: Vec<f32>,
    pub last_audio: Option<Instant>,
    generation: Option<String>,
    disconnected: Option<Instant>,
    silent: u32,
}

impl Guest {
    pub fn new() -> Option<Self> {
        Some(Self {
            ice: Ice::open(false)?,
            rtc: None,
            mid: None,
            enc: OpusEncoder::new(48_000, CHANNELS, Application::RestrictedLowDelay).ok()?,
            up: None,
            send_pcm: Vec::new(),
            packet: vec![0; 1500],
            time: 0,
            decode: Decode::new()?,
            down: None,
            out: Vec::new(),
            last_audio: None,
            generation: None,
            disconnected: None,
            silent: 0,
        })
    }

    pub fn offer(&mut self, sdp: &str, now: Instant) -> Option<String> {
        let mid = sdp
            .lines()
            .find_map(|line| line.strip_prefix("a=mid:"))?
            .trim();
        let mut rtc = new_rtc(now, false);
        for c in self.ice.candidates() {
            rtc.add_local_candidate(c);
        }
        let answer = rtc
            .sdp_api()
            .accept_offer(SdpOffer::from_sdp_string(sdp).ok()?)
            .ok()?;
        let next = drain(&mut rtc, &self.ice, |_| {})?;
        self.rtc = Some((rtc, next));
        self.mid = Some(Mid::from(mid));
        self.generation = sdp
            .lines()
            .find_map(|l| l.strip_prefix("a=ice-ufrag:"))
            .map(|s| s.trim().to_owned());
        self.disconnected = None;
        self.silent = 0;
        self.last_audio = None;
        self.decode = Decode::new()?;
        self.send_pcm.clear();
        self.time = 0;
        Some(answer.to_sdp_string())
    }

    pub fn close(&mut self) {
        self.rtc = None;
        self.mid = None;
        self.last_audio = None;
        self.disconnected = None;
    }

    pub fn live(&self) -> bool {
        self.rtc.as_ref().is_some_and(|(rtc, _)| rtc.is_connected())
    }

    pub fn set_cap(&mut self, cap: u32) -> u32 {
        self.enc.bitrate_bps = cap.clamp(MIN_BPS, MAX_BPS) as i32;
        self.enc.bitrate_bps as u32
    }

    pub fn candidate(&mut self, candidate: &str, ufrag: &str) {
        if self.generation.as_deref() != Some(ufrag) {
            return;
        }
        if let Some((rtc, next)) = self.rtc.as_mut()
            && let Ok(c) = Candidate::from_sdp_string(candidate)
        {
            rtc.add_remote_candidate(c);
            *next = drain(rtc, &self.ice, |_| {}).unwrap_or(*next);
        }
    }

    pub fn trickle(&mut self) -> Vec<(String, String)> {
        let candidates = self.ice.discoveries();
        let (Some((rtc, next)), Some(ufrag)) = (self.rtc.as_mut(), self.generation.as_ref()) else {
            return Vec::new();
        };
        let mut messages = Vec::new();
        for c in candidates {
            if let Some(candidate) = rtc.add_local_candidate(c).map(Candidate::to_sdp_string) {
                messages.push((candidate, ufrag.clone()));
            }
        }
        if !messages.is_empty() {
            *next = drain(rtc, &self.ice, |_| {}).unwrap_or(*next);
        }
        messages
    }

    /// Join's DAW input travels upstream on the same sendrecv track.
    pub fn audio(&mut self, samples: &[f32], rate: u32, now: Instant) {
        let (Some((rtc, next)), Some(mid)) = (self.rtc.as_mut(), self.mid) else {
            self.send_pcm.clear();
            return;
        };
        if !rtc.is_connected() {
            self.send_pcm.clear();
            return;
        }
        match resampler(&mut self.up, rate, 48_000) {
            Some(r) => r.run(samples, &mut self.send_pcm),
            None => self.send_pcm.extend_from_slice(samples),
        }
        while self.send_pcm.len() >= FRAME * CHANNELS {
            let n = self
                .enc
                .encode(&self.send_pcm[..FRAME * CHANNELS], FRAME, &mut self.packet)
                .unwrap_or(0);
            let quiet = self.send_pcm[..FRAME * CHANNELS]
                .iter()
                .all(|s| s.abs() < SILENT);
            self.silent = if quiet {
                self.silent.saturating_add(1)
            } else {
                0
            };
            self.send_pcm.drain(..FRAME * CHANNELS);
            let at = MediaTime::new(self.time, Frequency::FORTY_EIGHT_KHZ);
            self.time += FRAME as u64;
            if n > 0
                && self.silent < GATE_AFTER
                && let Some(w) = rtc.writer(mid)
            {
                let data: Arc<[u8]> = self.packet[..n].into();
                let _ = w.write(PT, now, at, data);
                *next = drain(rtc, &self.ice, |_| {}).unwrap_or(*next);
            }
        }
    }

    /// Network in, decoded audio out to `rx` at `rate`.
    /// Returns true when a failed connection needs a fresh hello.
    pub fn poll(&mut self, now: Instant, rate: u32, rx: &mut Producer<f32>) -> bool {
        let Some((rtc, next)) = self.rtc.as_mut() else {
            return false;
        };
        let mut packets = Vec::new();
        let mut dead = false;
        let disconnected = &mut self.disconnected;
        let mut on = |e| match e {
            Event::MediaData(m) => packets.push((m.data, m.time.numer())),
            Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                disconnected.get_or_insert(now);
            }
            Event::IceConnectionStateChange(
                IceConnectionState::Connected | IceConnectionState::Completed,
            ) => *disconnected = None,
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
        let retry = dead
            || !rtc.is_alive()
            || self
                .disconnected
                .is_some_and(|at| now.saturating_duration_since(at) >= RECONNECT_AFTER);
        if retry {
            self.rtc = None;
            self.last_audio = None;
        }
        for (p, time) in packets {
            let pcm = self.decode.run(&p, time);
            if pcm.is_empty() {
                continue;
            }
            self.last_audio = Some(now);
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
        retry
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_candidates_require_an_answer_and_current_offer_generation() {
        let now = Instant::now();
        let mut host = Host::new(false).unwrap();
        let mut guest = Guest::new().unwrap();
        let offer = host.offer("phone", true, now).unwrap();
        let ufrag = host.peers[0].rtc.direct_api().local_ice_credentials().ufrag;
        let c = guest.ice.candidates().remove(0);
        let sdp = c.to_sdp_string();
        let absent = |host: &mut Host| !host.peers[0].rtc.direct_api().invalidate_candidate(&c);
        host.candidate("phone", &sdp, &ufrag);
        assert!(absent(&mut host), "candidate before answer is ignored");
        let without_candidates = |sdp: &str| {
            sdp.lines()
                .filter(|line| !line.starts_with("a=candidate:"))
                .collect::<Vec<_>>()
                .join("\r\n")
                + "\r\n"
        };
        let answer = guest.offer(&without_candidates(&offer), now).unwrap();
        host.answer("phone", "invalid SDP");
        assert!(
            host.awaiting("phone"),
            "invalid SDP cannot consume the offer"
        );
        host.answer("phone", &without_candidates(&answer));
        host.candidate("phone", &sdp, "oldGeneration");
        assert!(absent(&mut host), "stale candidate is ignored");
        host.candidate("phone", "malformed candidate", &ufrag);
        assert!(absent(&mut host));
        host.candidate("phone", &sdp, &ufrag);
        assert!(!absent(&mut host), "late candidate reaches the ICE agent");
        host.candidate("phone", &sdp, &ufrag);
        let (_, (mut rx, _)) = crate::rings();
        let start = Instant::now();
        while (host.live() == 0 || !guest.live()) && start.elapsed() < Duration::from_secs(5) {
            host.poll(Instant::now());
            guest.poll(Instant::now(), 48_000, &mut rx);
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            host.live(),
            1,
            "candidate-free SDP connects through trickle ICE"
        );
        assert!(guest.live());
        assert_eq!(guest.set_cap(192_000), 192_000);
        for _ in 0..40 {
            guest.audio(&[0.0; FRAME * CHANNELS], 48_000, Instant::now());
        }
        assert_eq!(guest.silent, 40);
        assert_eq!(
            guest.time,
            40 * FRAME as u64,
            "silence preserves the RTP clock"
        );
        guest.audio(&[0.1; FRAME * CHANNELS], 48_000, Instant::now());
        assert_eq!(guest.silent, 0, "audio resumes immediately after silence");
        let answer = guest
            .offer(&host.offer("phone", true, now).unwrap(), now)
            .unwrap();
        host.answer("phone", &without_candidates(&answer));
        host.candidate("phone", &sdp, &ufrag);
        assert!(absent(&mut host), "previous offer cannot alter replacement");
        guest.poll(Instant::now(), 48_000, &mut rx);
        guest.disconnected = Some(Instant::now());
        assert!(
            !guest.poll(Instant::now(), 48_000, &mut rx),
            "brief outages can recover"
        );
        guest.disconnected = Some(Instant::now() - RECONNECT_AFTER);
        assert!(
            guest.poll(Instant::now(), 48_000, &mut rx),
            "persistent outages request a new offer"
        );
        assert!(
            !guest.poll(Instant::now(), 48_000, &mut rx),
            "retry is requested once"
        );
    }

    #[test]
    fn late_host_addresses_reach_native_peers_with_current_generation() {
        let now = Instant::now();
        let mut host = Host::new(false).unwrap();
        let mut guest = Guest::new().unwrap();
        let offer = host.offer("plugin", false, now).unwrap();
        let answer = guest.offer(&offer, now).unwrap();
        host.answer("plugin", &answer);
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        let c = Candidate::host(socket.local_addr().unwrap(), "udp").unwrap();
        host.ice.socks.push(socket);
        host.ice.discoveries.push(c.clone());
        let messages = host.trickle();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].0, "plugin");
        assert!(host.trickle().is_empty(), "discoveries are announced once");
        guest.candidate(&messages[0].1, "previousOffer");
        assert!(
            !guest
                .rtc
                .as_mut()
                .unwrap()
                .0
                .direct_api()
                .invalidate_candidate(&c)
        );
        guest.candidate(&messages[0].1, &messages[0].2);
        assert!(
            guest
                .rtc
                .as_mut()
                .unwrap()
                .0
                .direct_api()
                .invalidate_candidate(&c)
        );
    }

    #[test]
    fn mono_decode_conceals_short_loss_without_queueing_dtx_silence() {
        let mut enc = OpusEncoder::new(48_000, 1, Application::RestrictedLowDelay).unwrap();
        let input: Vec<_> = (0..FRAME)
            .map(|i| (i as f32 * std::f32::consts::TAU / 48.0).sin() * 0.2)
            .collect();
        let mut packet = vec![0; 1500];
        let n = enc.encode(&input, FRAME, &mut packet).unwrap();
        packet.truncate(n);
        let mut decode = Decode::new().unwrap();
        let pcm = decode.run(&packet, 0);
        assert_eq!(pcm.len(), FRAME * CHANNELS);
        assert!(pcm.as_chunks::<2>().0.iter().all(|pair| pair[0] == pair[1]));
        let pcm = decode.run(&packet, 2 * FRAME as u64);
        assert_eq!(
            pcm.len(),
            2 * FRAME * CHANNELS,
            "conceal one missing 10 ms frame"
        );
        assert!(pcm.iter().all(|s| s.is_finite()));
        assert!(
            decode.run(&packet, 2 * FRAME as u64).is_empty(),
            "drop duplicate timestamps"
        );
        assert_eq!(
            decode.run(&packet, 24_000).len(),
            FRAME * CHANNELS,
            "long silence adds no concealment backlog"
        );
    }

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
        assert_eq!(
            stun_response(&p, txid),
            Some(SocketAddr::from(([203, 0, 113, 7], 40_000)))
        );
        assert_eq!(stun_response(&p, [4; 12]), None);
        p[4] = 0;
        assert_eq!(stun_response(&p, txid), None, "validate the STUN cookie");
        let addr: std::net::Ipv6Addr = "2001:db8::7".parse().unwrap();
        let mut p = vec![1, 1, 0, 24, 0x21, 0x12, 0xa4, 0x42];
        p.extend(txid);
        let mask: Vec<_> = p[4..20].to_vec();
        p.extend([0, 0x20, 0, 20, 0, 2]);
        p.extend((40_000u16 ^ 0x2112).to_be_bytes());
        p.extend(
            addr.octets()
                .iter()
                .zip(mask)
                .map(|(byte, mask)| byte ^ mask),
        );
        assert_eq!(
            stun_response(&p, txid),
            Some(SocketAddr::new(addr.into(), 40_000))
        );
        assert_eq!(
            stun_response(&p[..p.len() - 1], txid),
            None,
            "reject truncated IPv6 responses"
        );
    }

    #[test]
    fn browser_voice_gain_applies_per_source() {
        let mut loud = Voice::new().unwrap();
        let mut quiet = Voice::new().unwrap();
        loud.queue.extend([0.4, -0.4]);
        quiet.queue.extend([0.4, -0.4]);
        loud.primed = true;
        quiet.primed = true;
        let mut out = [0.0; 2];
        loud.mix_into(&mut out, 2.0);
        quiet.mix_into(&mut out, 0.5);
        assert_eq!(out, [1.0, -1.0]);
        assert_eq!(loud.peak, [0.4, 0.4]);
        assert!(loud.stereo);
    }

    #[test]
    fn browser_mix_excludes_its_own_microphone() {
        let mut host = Host::new(false).unwrap();
        let now = Instant::now();
        host.offer("plugin", false, now).unwrap();
        host.offer("browser", true, now).unwrap();
        for (peer, sample) in host.peers.iter_mut().zip([0.2, 0.3]) {
            peer.voice
                .queue
                .extend(std::iter::repeat_n(sample, FRAME * CHANNELS));
            peer.voice.primed = true;
        }
        let ((_, _), (mut rx, mut heard)) = crate::rings();
        host.talk_back(FRAME, 48_000, &mut rx, &[0.0; FRAME * CHANNELS]);
        assert!((host.browser_pcm[0] - 0.2).abs() < 1e-6);
        let chunk = heard.read_chunk(heard.slots()).unwrap();
        assert!((chunk.as_slices().0[0] - 0.5).abs() < 1e-6);
        for peer in &mut host.peers {
            peer.live = true;
        }
        host.audio(
            &vec![0.1; FRAME * CHANNELS],
            &[0.0; FRAME * CHANNELS],
            48_000,
            now,
        );
        assert!(
            (host.mix[0] - 0.3).abs() < 1e-6,
            "browser gets host and plugin, not its mic"
        );
    }

    #[test]
    fn talkers_follow_sources_and_keep_gain() {
        let mut host = Host::new(false).unwrap();
        let now = Instant::now();
        host.offer("plugin", false, now).unwrap();
        host.offer("browser", true, now).unwrap();
        for peer in &mut host.peers {
            peer.live = true;
            peer.voice.primed = true;
        }
        let mut talkers = Vec::new();
        host.sync_talkers(&mut talkers);
        assert_eq!(talkers.len(), 2);
        assert!(talkers[0].plugin);
        assert_eq!(talkers[1].slot, 2);
        assert_eq!(host.talking(), 1);
        host.roster(&[
            ("browser".into(), 1, "Maya".into()),
            ("plugin".into(), 2, String::new()),
        ]);
        host.sync_talkers(&mut talkers);
        assert_eq!(talkers[1].slot, 1);
        assert_eq!(talkers[1].name, "Maya");
        talkers[1].gain_db = -6.0;
        host.sync_talkers(&mut talkers);
        assert!((host.peers[1].gain - 0.501_187_2).abs() < 1e-6);
        talkers[1].muted = true;
        host.sync_talkers(&mut talkers);
        assert_eq!(host.peers[1].gain, 0.0);
        assert_eq!(talkers[1].gain_db, -6.0);
        host.peers[1].voice.queue.extend([0.25, -0.5]);
        host.peers[1].voice.mix_into(&mut [0.0; 2], 0.0);
        host.sync_talkers(&mut talkers);
        assert_eq!(
            talkers[1].peak,
            [0.25, 0.5],
            "muted mics still show incoming level"
        );
        host.sync_talkers(&mut talkers);
        assert_eq!(
            talkers[1].peak,
            [0.25, 0.5],
            "peak waits for the editor to read it"
        );
        talkers[1].peak = [0.0; CHANNELS];
        host.sync_talkers(&mut talkers);
        assert_eq!(talkers[1].peak, [0.0; CHANNELS]);
        host.leave("browser");
        host.sync_talkers(&mut talkers);
        assert_eq!(talkers.len(), 1);
        assert!(talkers[0].plugin);
    }
}
