//! Plugin → browser WebRTC. The cloud only relays signaling.
//!
//! Off-LAN listeners get Opus over a data channel from a libdatachannel
//! peer per listener. STUN only; libjuice has no TURN/TLS.

use std::collections::HashMap;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use relay_opus::{
    Bitrate, Encoder, EncoderConfigV1, EncoderPolicyV1, FrameDuration, InbandFec, MAX_PACKET_BYTES,
    PacketLossPercent,
};
use relay_transport::{
    BinaryPayload, ChannelId, Command, DescriptionKind, Event, IceCandidate, IceServer,
    NativeTransportProvider, NegotiationEpoch, OperationId, PeerDriver, PeerState,
    SessionDescription, TransportError,
};
use relay_transport_libdatachannel::{LibdatachannelProvider, drain_ready, listen_offerer_config};

use crate::signal::{Outbound, Signal};

pub const MAX_PEERS: usize = 10;
/// 10 ms of 48 kHz stereo, interleaved.
const FRAME_SAMPLES: usize = 960;
const CHANNEL: ChannelId = ChannelId(0);
const EPOCH: NegotiationEpoch = NegotiationEpoch(1);
/// A peer that has not produced its offer by now never will. Reap it so the
/// listener's next `want` builds a fresh one instead of waiting forever.
const OFFER_GRACE: Duration = Duration::from_secs(2);

pub struct Hub {
    provider: LibdatachannelProvider,
    peers: HashMap<String, Peer>,
    encoder: Option<Encoder>,
    bitrate_kbps: u32,
    packet: Vec<u8>,
    leftover: Vec<f32>,
    frames_sent: u64,
    last_peak: f32,
    ice: Vec<IceServer>,
}

struct Peer {
    driver: Box<dyn PeerDriver>,
    next_op: u64,
    ready: bool,
    dead: bool,
    answered: bool,
    offer_sdp: Option<String>,
    pending_ice: Vec<(String, Option<String>)>,
    born: Instant,
}

impl Default for Hub {
    fn default() -> Self {
        Self {
            provider: LibdatachannelProvider::new(),
            peers: HashMap::new(),
            encoder: None,
            bitrate_kbps: 0,
            packet: vec![0; MAX_PACKET_BYTES],
            leftover: Vec::new(),
            frames_sent: 0,
            last_peak: 0.0,
            ice: Vec::new(),
        }
    }
}

impl Hub {
    /// Relay credentials from the room. Peers already up keep the servers
    /// they negotiated with; only new peers pick these up.
    pub fn set_ice_servers(&mut self, servers: Vec<IceServer>) {
        self.ice = servers;
    }

    pub fn peer_count(&self) -> u32 {
        u32::try_from(self.peers.len()).unwrap_or(u32::MAX)
    }

    pub fn ready_count(&self) -> u32 {
        let ready = self.peers.values().filter(|p| p.ready && !p.dead).count();
        u32::try_from(ready).unwrap_or(u32::MAX)
    }

    pub fn frames_sent(&self) -> u64 {
        self.frames_sent
    }

    pub fn last_peak(&self) -> f32 {
        self.last_peak
    }

    pub fn clear(&mut self) {
        for peer in self.peers.values_mut() {
            peer.shutdown();
        }
        self.peers.clear();
        self.leftover.clear();
        self.encoder = None;
        self.frames_sent = 0;
        self.last_peak = 0.0;
    }

    /// Byes first, so a reconnecting listener's `bye` + `want` for the same
    /// id yields a fresh peer rather than creating and deleting one.
    pub fn apply_all(&mut self, signals: &[Signal], outgoing: &mut Vec<String>) {
        for signal in signals.iter().filter(|s| s.is_bye()) {
            self.apply(signal, outgoing);
        }
        for signal in signals.iter().filter(|s| !s.is_bye()) {
            self.apply(signal, outgoing);
        }
    }

    pub fn apply(&mut self, signal: &Signal, outgoing: &mut Vec<String>) {
        match signal {
            Signal::Want { id } => self.want(id, outgoing),
            Signal::Answer { id, sdp } => self.answer(id, sdp),
            Signal::Ice { id, cand, mid } => self.remote_ice(id, cand, mid.clone()),
            Signal::Bye { id } => self.drop_peer(id),
        }
    }

    pub fn push_pcm(&mut self, pcm: &[f32], bitrate_kbps: u32) {
        self.last_peak = pcm.iter().fold(0.0_f32, |peak, s| peak.max(s.abs()));
        if self.peers.is_empty() {
            self.leftover.clear();
            return;
        }
        if self.encoder.is_none() || self.bitrate_kbps != bitrate_kbps {
            self.encoder = make_encoder(bitrate_kbps);
            self.bitrate_kbps = bitrate_kbps;
        }
        if self.encoder.is_none() {
            return;
        }
        self.leftover.extend_from_slice(pcm);
        let whole = self.leftover.len() / FRAME_SAMPLES * FRAME_SAMPLES;
        let frames = std::mem::take(&mut self.leftover);
        for frame in frames[..whole].chunks_exact(FRAME_SAMPLES) {
            self.send_frame(frame);
        }
        self.leftover = frames;
        self.leftover.drain(..whole);
    }

    /// Pump every peer's event queue; emits offers / ICE, reaps dead peers.
    pub fn drive(&mut self, outgoing: &mut Vec<String>) {
        for (id, peer) in &mut self.peers {
            for event in drain_ready(peer.driver.as_mut()) {
                match event {
                    Event::LocalDescription { description } => {
                        outgoing.push(
                            Outbound::Offer {
                                id,
                                sdp: description.sdp(),
                            }
                            .to_json(),
                        );
                        peer.offer_sdp = Some(description.sdp().to_owned());
                    }
                    Event::LocalCandidate { candidate } => {
                        outgoing.push(
                            Outbound::Ice {
                                id,
                                cand: candidate.candidate(),
                                mid: candidate.sdp_mid(),
                            }
                            .to_json(),
                        );
                    }
                    Event::DataChannelOpened { .. }
                    | Event::StateChanged {
                        state: PeerState::Connected,
                    } => peer.ready = true,
                    Event::DataChannelClosed { .. }
                    | Event::FatalError { .. }
                    | Event::ShutdownComplete
                    | Event::StateChanged {
                        state: PeerState::Failed | PeerState::Closed,
                    } => peer.dead = true,
                    _ => {}
                }
            }
        }
        self.peers.retain(|id, peer| {
            let stalled = peer.offer_sdp.is_none() && peer.born.elapsed() > OFFER_GRACE;
            if peer.dead || stalled {
                outgoing.push(Outbound::Bye { id }.to_json());
                peer.shutdown();
                return false;
            }
            true
        });
    }

    fn drop_peer(&mut self, id: &str) {
        if let Some(mut peer) = self.peers.remove(id) {
            peer.shutdown();
        }
    }

    fn want(&mut self, id: &str, outgoing: &mut Vec<String>) {
        if let Some(peer) = self.peers.get(id).filter(|peer| !peer.dead) {
            if let Some(sdp) = &peer.offer_sdp {
                outgoing.push(Outbound::Offer { id, sdp }.to_json());
            }
            return;
        }
        self.drop_peer(id);
        if self.peers.len() >= MAX_PEERS {
            let spare = self
                .peers
                .iter()
                .find(|(_, peer)| !peer.ready || peer.dead)
                .map(|(key, _)| key.clone());
            let Some(old) = spare else {
                outgoing.push(Outbound::Bye { id }.to_json());
                return;
            };
            self.drop_peer(&old);
        }
        match self.new_peer() {
            Some(peer) => {
                self.peers.insert(id.to_owned(), peer);
            }
            // No transport means no offer is ever coming. Say so, or the
            // listener retries `want` every four seconds forever.
            None => outgoing.push(Outbound::Bye { id }.to_json()),
        }
    }

    fn new_peer(&mut self) -> Option<Peer> {
        let config = listen_offerer_config(&self.ice).ok()?;
        let validated = config.validate_for(self.provider.capabilities()).ok()?;
        let driver = self.provider.create_peer(validated).ok()?;
        let mut peer = Peer {
            driver,
            next_op: 0,
            ready: false,
            dead: false,
            answered: false,
            offer_sdp: None,
            pending_ice: Vec::new(),
            born: Instant::now(),
        };
        peer.submit(|operation_id| Command::OpenDataChannel {
            operation_id,
            channel_id: CHANNEL,
        })
        .ok()?;
        peer.submit(|operation_id| Command::CreateOffer {
            operation_id,
            epoch: EPOCH,
        })
        .ok()?;
        Some(peer)
    }

    fn send_frame(&mut self, frame: &[f32]) {
        let Some(encoder) = self.encoder.as_mut() else {
            return;
        };
        let Ok(n) = encoder.encode(frame, &mut self.packet) else {
            return;
        };
        let packet = &self.packet[..n];
        for peer in self.peers.values_mut().filter(|p| p.ready && !p.dead) {
            let Ok(payload) = BinaryPayload::new(packet.to_vec()) else {
                continue;
            };
            match peer.submit(|operation_id| Command::Send {
                operation_id,
                channel_id: CHANNEL,
                payload,
            }) {
                Ok(()) => self.frames_sent = self.frames_sent.saturating_add(1),
                Err(TransportError::WouldBlock | TransportError::InvalidState) => {}
                Err(_) => peer.dead = true,
            }
        }
    }

    fn answer(&mut self, id: &str, sdp: &str) {
        let Some(peer) = self.peers.get_mut(id) else {
            return;
        };
        let Ok(description) =
            SessionDescription::new(EPOCH, DescriptionKind::Answer, sdp.to_owned())
        else {
            peer.dead = true;
            return;
        };
        if peer
            .submit(|operation_id| Command::SetRemoteDescription {
                operation_id,
                description,
            })
            .is_err()
        {
            peer.dead = true;
            return;
        }
        peer.answered = true;
        for (cand, mid) in std::mem::take(&mut peer.pending_ice) {
            peer.add_ice(cand, mid);
        }
    }

    fn remote_ice(&mut self, id: &str, cand: &str, mid: Option<String>) {
        if !usable_ice(cand) {
            return;
        }
        let Some(peer) = self.peers.get_mut(id) else {
            return;
        };
        if peer.answered {
            peer.add_ice(cand.to_owned(), mid);
        } else {
            peer.pending_ice.push((cand.to_owned(), mid));
        }
    }
}

impl Peer {
    fn submit(&mut self, make: impl FnOnce(OperationId) -> Command) -> Result<(), TransportError> {
        self.next_op = self.next_op.saturating_add(1);
        self.driver
            .submit(make(OperationId(self.next_op)))
            .map_err(|error| error.error())
    }

    fn add_ice(&mut self, cand: String, mid: Option<String>) {
        if let Ok(candidate) = IceCandidate::new(EPOCH, cand, mid, Some(0), None) {
            let _ = self.submit(|operation_id| Command::AddRemoteCandidate {
                operation_id,
                candidate,
            });
        }
    }

    fn shutdown(&mut self) {
        let _ = self.submit(|operation_id| Command::Shutdown { operation_id });
        let mut context = Context::from_waker(Waker::noop());
        while let Poll::Ready(Some(event)) = self.driver.poll_event(&mut context) {
            if matches!(event, Event::ShutdownComplete) {
                break;
            }
        }
    }
}

fn make_encoder(bitrate_kbps: u32) -> Option<Encoder> {
    let bps = i32::try_from(bitrate_kbps.clamp(64, 256).saturating_mul(1_000)).ok()?;
    let bitrate = Bitrate::try_new(bps).ok()?;
    let policy = EncoderPolicyV1::new(bitrate, InbandFec::Enabled, PacketLossPercent::ZERO);
    Encoder::new(EncoderConfigV1::stereo_48k(FrameDuration::Ms10, policy)).ok()
}

/// Browsers send an empty candidate as end-of-candidates; juice rejects it.
fn usable_ice(cand: &str) -> bool {
    let text = cand.trim();
    !text.is_empty() && text != "candidate:" && text != "a=candidate:"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn want(id: &str) -> Signal {
        Signal::Want { id: id.into() }
    }

    fn bye(id: &str) -> Signal {
        Signal::Bye { id: id.into() }
    }

    fn offer_sdp(msgs: &[String]) -> Option<String> {
        msgs.iter().find_map(|msg| {
            let value: serde_json::Value = serde_json::from_str(msg).ok()?;
            if value.get("t")?.as_str()? != "offer" {
                return None;
            }
            value.get("sdp")?.as_str().map(str::to_owned)
        })
    }

    fn wait_offer(hub: &mut Hub) -> Vec<String> {
        let start = std::time::Instant::now();
        let mut outgoing = Vec::new();
        while start.elapsed() < std::time::Duration::from_secs(5) {
            hub.drive(&mut outgoing);
            if offer_sdp(&outgoing).is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        outgoing
    }

    #[test]
    fn ice_before_want_is_ignored() {
        let mut hub = Hub::default();
        let mut outgoing = Vec::new();
        hub.apply(
            &Signal::Ice {
                id: "ab".into(),
                cand: "candidate:1 1 UDP 1 127.0.0.1 9 typ host".into(),
                mid: None,
            },
            &mut outgoing,
        );
        assert!(outgoing.is_empty());
        assert!(hub.peers.is_empty());
    }

    #[test]
    fn empty_end_of_candidates_is_not_usable() {
        assert!(!usable_ice(""));
        assert!(!usable_ice("   "));
        assert!(!usable_ice("candidate:"));
        assert!(usable_ice("candidate:1 1 UDP 1 127.0.0.1 9 typ host"));
    }

    #[test]
    fn want_on_existing_id_resends_the_same_offer() {
        let mut hub = Hub::default();
        let mut outgoing = Vec::new();
        hub.apply(&want("ab"), &mut outgoing);
        assert_eq!(hub.peer_count(), 1);
        let sdp = offer_sdp(&wait_offer(&mut hub)).expect("first want emits an offer");
        outgoing.clear();
        hub.apply(&want("ab"), &mut outgoing);
        assert_eq!(hub.peer_count(), 1);
        assert_eq!(offer_sdp(&outgoing).as_deref(), Some(sdp.as_str()));
    }

    #[test]
    fn bye_then_want_creates_a_fresh_peer() {
        let mut hub = Hub::default();
        let mut outgoing = Vec::new();
        hub.apply(&want("ab"), &mut outgoing);
        let first = offer_sdp(&wait_offer(&mut hub)).expect("first offer");
        outgoing.clear();
        hub.apply_all(&[bye("ab"), want("ab")], &mut outgoing);
        assert_eq!(hub.peer_count(), 1);
        let second = offer_sdp(&wait_offer(&mut hub)).expect("second offer");
        assert_ne!(first, second, "bye must drop the old ICE credentials");
    }

    #[test]
    fn an_offerless_peer_is_reaped_so_the_next_want_rebuilds() {
        let mut hub = Hub::default();
        let mut outgoing = Vec::new();
        hub.apply(&want("ab"), &mut outgoing);
        wait_offer(&mut hub);
        // Pretend offer generation never delivered.
        let peer = hub.peers.get_mut("ab").expect("peer");
        peer.offer_sdp = None;
        peer.born = Instant::now() - OFFER_GRACE - Duration::from_millis(1);
        outgoing.clear();
        hub.drive(&mut outgoing);
        assert_eq!(hub.peer_count(), 0, "a peer with no offer must not linger");
        assert!(
            outgoing.iter().any(|msg| msg.contains("\"t\":\"bye\"")),
            "the listener is told to stop waiting: {outgoing:?}"
        );
        outgoing.clear();
        hub.apply(&want("ab"), &mut outgoing);
        assert!(offer_sdp(&wait_offer(&mut hub)).is_some(), "want rebuilds");
    }

    #[test]
    fn apply_all_processes_bye_before_want() {
        let mut hub = Hub::default();
        let mut outgoing = Vec::new();
        hub.apply_all(&[want("ab"), bye("ab")], &mut outgoing);
        assert_eq!(hub.peer_count(), 1);
    }

    #[test]
    fn push_pcm_keeps_partial_frames_for_later() {
        let mut hub = Hub::default();
        hub.apply(&want("ab"), &mut Vec::new());
        hub.push_pcm(&[0.1; FRAME_SAMPLES + 10], 192);
        assert_eq!(hub.leftover.len(), 10);
        assert!((hub.last_peak() - 0.1).abs() < 1e-6);
    }
}
