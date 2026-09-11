//! Cloud join: the joining plugin as a listener on the host's room.
//!
//! Joining over a LAN slug or a direct `ip:port` needs both plugins on the
//! same network, or a forwarded port. This path instead speaks the exact
//! protocol the browser listen page speaks — `/out` socket, `want`, answer
//! the host's offer — so ICE does the NAT work and symmetric NAT falls back
//! to the room's TURN relay. The one difference from a browser is `dc` on the
//! `want`: a browser plays an RTP audio track, we want the Opus bytes.

use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use relay_opus::{Decoder, DecoderConfig, FrameDuration};
use relay_transport::{
    Command, DescriptionKind, Event, IceCandidate, IceServer, NativeTransportProvider,
    NegotiationEpoch, OperationId, PeerDriver, PeerState, SessionDescription, TransportError,
};
use relay_transport_libdatachannel::{LibdatachannelProvider, drain_ready, listen_answerer_config};

use crate::signal::{Ask, Incoming};

/// 10 ms of 48 kHz stereo, interleaved: one host packet.
const FRAME_SAMPLES: usize = 960;
const EPOCH: NegotiationEpoch = NegotiationEpoch(1);
/// Matches the listen page's offer watchdog, so a host that missed the first
/// `want` gets asked again instead of the join hanging.
const WANT_EVERY: Duration = Duration::from_secs(4);
/// A peer that has not connected by now never will; rebuild rather than wait.
const PEER_GRACE: Duration = Duration::from_secs(20);

/// The listener half of a cloud join.
pub struct Tap {
    provider: LibdatachannelProvider,
    peer: Option<Peer>,
    decoder: Option<Decoder>,
    ice: Vec<IceServer>,
    scratch: Vec<f32>,
    next_want: Instant,
    /// The offer the current peer is for, so a resend is not a rebuild.
    answering: String,
}

struct Peer {
    driver: Box<dyn PeerDriver>,
    next_op: u64,
    ready: bool,
    dead: bool,
    offered: bool,
    pending_ice: Vec<(String, Option<String>)>,
    born: Instant,
}

impl Default for Tap {
    fn default() -> Self {
        Self {
            provider: LibdatachannelProvider::new(),
            peer: None,
            decoder: None,
            ice: Vec::new(),
            scratch: vec![0.0; FRAME_SAMPLES],
            next_want: Instant::now(),
            answering: String::new(),
        }
    }
}

impl Tap {
    /// Relay credentials from the room. Applied to the next peer built, so an
    /// in-flight negotiation is never disturbed.
    pub fn set_ice_servers(&mut self, servers: Vec<IceServer>) {
        self.ice = servers;
    }

    /// True once the host's audio is arriving.
    pub fn connected(&self) -> bool {
        self.peer
            .as_ref()
            .is_some_and(|peer| peer.ready && !peer.dead)
    }

    /// Drop the peer and forget the room.
    pub fn clear(&mut self) {
        if let Some(mut peer) = self.peer.take() {
            peer.shutdown();
        }
        self.decoder = None;
        self.answering.clear();
        self.next_want = Instant::now();
    }

    /// One tick: apply room messages, emit listener messages, append decoded
    /// interleaved stereo PCM.
    pub fn step(&mut self, inbound: &[Incoming], outgoing: &mut Vec<String>, pcm: &mut Vec<f32>) {
        for message in inbound {
            self.apply(message);
        }
        self.drive(outgoing, pcm);
        // Only while there is no peer at all: asking again mid-negotiation
        // makes the host re-offer, and answering that offer would throw away
        // the connection that was about to come up.
        if self.peer.is_none() && Instant::now() >= self.next_want {
            outgoing.push(Ask::Want { dc: true }.to_json());
            self.next_want = Instant::now() + WANT_EVERY;
        }
    }

    fn apply(&mut self, message: &Incoming) {
        match message {
            Incoming::Offer { sdp } => self.offer(sdp),
            Incoming::Ice { cand, mid } => self.remote_ice(cand, mid.clone()),
            Incoming::Bye => self.clear(),
            // The host just came online: ask now instead of waiting out the
            // watchdog.
            Incoming::Room { host: true } => self.next_want = Instant::now(),
            Incoming::Room { host: false } => self.clear(),
            Incoming::Auth { ok: true } => self.next_want = Instant::now(),
            Incoming::Auth { ok: false } => {}
        }
    }

    /// A fresh offer replaces whatever peer we had: the host only re-offers
    /// after it gave up on the old one. A repeat of the offer we are already
    /// working on is the room being chatty, not a new session.
    fn offer(&mut self, sdp: &str) {
        // The room sends its own `want` the moment we connect, before ours
        // with `dc` gets there, so the first offer is usually the media one a
        // browser would take. Ignore it and let the next `want` re-offer.
        if !sdp.contains("m=application") {
            return;
        }
        if self.answering == sdp {
            return;
        }
        self.clear();
        sdp.clone_into(&mut self.answering);
        let Some(mut peer) = self.new_peer() else {
            return;
        };
        let Ok(description) =
            SessionDescription::new(EPOCH, DescriptionKind::Offer, sdp.to_owned())
        else {
            return;
        };
        if peer
            .submit(|operation_id| Command::SetRemoteDescription {
                operation_id,
                description,
            })
            .is_err()
        {
            return;
        }
        if peer
            .submit(|operation_id| Command::CreateAnswer {
                operation_id,
                epoch: EPOCH,
            })
            .is_err()
        {
            return;
        }
        peer.offered = true;
        self.peer = Some(peer);
    }

    fn new_peer(&mut self) -> Option<Peer> {
        let config = listen_answerer_config(&self.ice).ok()?;
        let validated = config.validate_for(self.provider.capabilities()).ok()?;
        let driver = self.provider.create_peer(validated).ok()?;
        Some(Peer {
            driver,
            next_op: 0,
            ready: false,
            dead: false,
            offered: false,
            pending_ice: Vec::new(),
            born: Instant::now(),
        })
    }

    fn remote_ice(&mut self, cand: &str, mid: Option<String>) {
        if cand.trim().is_empty() {
            return;
        }
        let Some(peer) = self.peer.as_mut() else {
            return;
        };
        if peer.offered {
            peer.add_ice(cand.to_owned(), mid);
        } else {
            peer.pending_ice.push((cand.to_owned(), mid));
        }
    }

    fn drive(&mut self, outgoing: &mut Vec<String>, pcm: &mut Vec<f32>) {
        let Some(peer) = self.peer.as_mut() else {
            return;
        };
        for event in drain_ready(peer.driver.as_mut()) {
            match event {
                Event::LocalDescription { description } => {
                    outgoing.push(
                        Ask::Answer {
                            sdp: description.sdp(),
                        }
                        .to_json(),
                    );
                    for (cand, mid) in std::mem::take(&mut peer.pending_ice) {
                        peer.add_ice(cand, mid);
                    }
                }
                Event::LocalCandidate { candidate } => {
                    outgoing.push(
                        Ask::Ice {
                            cand: candidate.candidate(),
                            mid: candidate.sdp_mid(),
                        }
                        .to_json(),
                    );
                }
                Event::Message { payload, .. } => {
                    Self::decode_into(
                        &mut self.decoder,
                        &mut self.scratch,
                        payload.as_bytes(),
                        pcm,
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
        let stalled = !peer.ready && peer.born.elapsed() > PEER_GRACE;
        if peer.dead || stalled {
            self.clear();
        }
    }

    /// Built lazily: a join that never gets audio never allocates libopus.
    fn decode_into(
        decoder: &mut Option<Decoder>,
        scratch: &mut [f32],
        packet: &[u8],
        pcm: &mut Vec<f32>,
    ) {
        if decoder.is_none() {
            *decoder = Decoder::new(DecoderConfig::stereo_48k(FrameDuration::Ms10)).ok();
        }
        let Some(decoder) = decoder.as_mut() else {
            return;
        };
        if let Ok(decoded) = decoder.decode(packet, scratch) {
            pcm.extend_from_slice(&scratch[..decoded.interleaved_samples()]);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asks_for_an_offer_until_one_arrives() {
        let mut tap = Tap::default();
        let mut outgoing = Vec::new();
        let mut pcm = Vec::new();
        tap.step(&[], &mut outgoing, &mut pcm);
        assert_eq!(outgoing, vec![r#"{"t":"want","dc":true}"#.to_owned()]);

        // The watchdog is on a timer: a second tick right away is quiet.
        outgoing.clear();
        tap.step(&[], &mut outgoing, &mut pcm);
        assert!(outgoing.is_empty(), "{outgoing:?}");

        // A host coming online resets it, so the join does not wait it out.
        tap.step(&[Incoming::Room { host: true }], &mut outgoing, &mut pcm);
        assert_eq!(outgoing, vec![r#"{"t":"want","dc":true}"#.to_owned()]);
        assert!(pcm.is_empty());
    }

    /// The room, in-process: it only ever adds or strips the listener id.
    fn to_listener(text: &str) -> Option<Incoming> {
        let mut value: serde_json::Value = serde_json::from_str(text).ok()?;
        value.as_object_mut()?.remove("id");
        Incoming::parse(&value.to_string())
    }

    fn to_host(text: &str) -> Option<crate::signal::Signal> {
        let mut value: serde_json::Value = serde_json::from_str(text).ok()?;
        value
            .as_object_mut()?
            .insert("id".into(), "listener".into());
        crate::signal::Signal::parse(&value.to_string())
    }

    /// The whole cloud-join road: the joining plugin asks, the host offers,
    /// ICE connects, and Opus comes back out as PCM. Local candidates carry
    /// it here — over the internet the same peers hole punch or relay.
    #[test]
    fn a_join_answers_the_host_and_gets_its_audio() {
        let mut hub = crate::p2p::Hub::default();
        let mut tap = Tap::default();
        let tone: Vec<f32> = (0..FRAME_SAMPLES)
            .map(|i| ((i / 2) as f32 * 0.05).sin() * 0.5)
            .collect();

        let mut inbound = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut pcm = Vec::new();
        while pcm.is_empty() && Instant::now() < deadline {
            let mut asks = Vec::new();
            tap.step(&inbound, &mut asks, &mut pcm);

            let signals: Vec<_> = asks.iter().filter_map(|ask| to_host(ask)).collect();
            let mut said = Vec::new();
            hub.apply_all(&signals, &mut said);
            hub.push_pcm(&tone, 128);
            hub.drive(&mut said);
            inbound = said.iter().filter_map(|text| to_listener(text)).collect();

            std::thread::sleep(Duration::from_millis(10));
        }

        assert!(tap.connected(), "the data channel never opened");
        assert!(!pcm.is_empty(), "no audio decoded");
        assert_eq!(pcm.len() % FRAME_SAMPLES, 0, "whole 10 ms frames only");
        assert!(
            pcm.iter().any(|sample| sample.abs() > 0.01),
            "decoded silence"
        );
    }

    #[test]
    fn a_bad_offer_does_not_leave_a_peer_behind() {
        let mut tap = Tap::default();
        let mut outgoing = Vec::new();
        let mut pcm = Vec::new();
        tap.step(
            &[Incoming::Offer {
                sdp: "not an sdp".into(),
            }],
            &mut outgoing,
            &mut pcm,
        );
        assert!(!tap.connected());
    }
}
