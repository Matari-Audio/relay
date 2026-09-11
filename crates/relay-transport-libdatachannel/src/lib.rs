//! libdatachannel implementation of the RELAY [`NativeTransportProvider`] seam.
//!
//! ICE, DTLS, and SCTP stay inside libdatachannel. This crate only maps the
//! portable command/event pump onto that C API. The linked binary may be
//! libnice (TURN/TCP/TLS) or libjuice (STUN + TURN/UDP); capabilities are
//! reported from the loaded backend.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use core::task::{Context, Poll};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::task::Waker;

use relay_libdatachannel_sys::{
    self as sys, Configuration, IceBackend, PeerCallbacks, PeerConnection,
};
use relay_transport::{
    BinaryPayload, ChannelId, Command, DescriptionKind, EndOfCandidates, Event, IceCandidate,
    IceServer, IceTransport, NativeTransportProvider, NegotiationEpoch, OperationId, PeerConfig,
    PeerDriver, PeerState, ProviderCapabilities, RequiredCapabilities, SessionDescription,
    StatsReport, SubmitError, TransportError, ValidatedPeerConfig,
};

/// Cloudflare STUN used by the plugin listen path.
pub const STUN_HOST: &str = "stun.cloudflare.com";
/// Default STUN port.
pub const STUN_PORT: u16 = 3478;

/// libdatachannel factory. Safe to share across plugin peers.
#[derive(Clone, Copy, Debug, Default)]
pub struct LibdatachannelProvider;

impl LibdatachannelProvider {
    /// Constructs the provider and preloads the native worker pool.
    #[must_use]
    pub fn new() -> Self {
        sys::preload();
        Self
    }

    /// ICE backend of the loaded libdatachannel binary.
    #[must_use]
    pub fn ice_backend(self) -> IceBackend {
        sys::ice_backend()
    }
}

impl NativeTransportProvider for LibdatachannelProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        capabilities_for(sys::ice_backend())
    }

    fn create_peer(
        &self,
        config: ValidatedPeerConfig,
    ) -> Result<Box<dyn PeerDriver>, TransportError> {
        Ok(Box::new(LibdatachannelPeer::new(
            config.get(),
            self.capabilities(),
        )?))
    }
}

/// STUN server used for unpaid plugin → browser P2P.
///
/// # Errors
///
/// Returns [`TransportError::InvalidIceServer`] if the built-in host fails
/// validation (it does not).
pub fn default_stun_server() -> Result<IceServer, TransportError> {
    IceServer::stun(STUN_HOST, STUN_PORT, IceTransport::Udp)
}

/// Offerer configuration for one plugin → browser listen peer.
///
/// `ice_servers` comes from the room; entries this build's ICE backend cannot
/// dial are dropped rather than failing the whole peer. An empty list — no
/// relay configured, or the fetch failed — falls back to STUN alone, which
/// strands listeners behind symmetric NAT.
///
/// # Errors
///
/// Returns a construction error when the STUN server or capacities are invalid.
pub fn listen_offerer_config(ice_servers: &[IceServer]) -> Result<PeerConfig, TransportError> {
    let mut config = PeerConfig::offerer();
    let capabilities = capabilities_for(sys::ice_backend());
    let usable: Vec<IceServer> = ice_servers
        .iter()
        .filter(|server| ice_server_supported(server, &capabilities))
        .cloned()
        .collect();
    config.ice_servers = if usable.is_empty() {
        vec![default_stun_server()?]
    } else {
        usable
    };
    config.sendonly_opus = true;
    config.required_capabilities = RequiredCapabilities::default();
    Ok(config)
}

fn ice_server_supported(server: &IceServer, capabilities: &ProviderCapabilities) -> bool {
    match server {
        IceServer::Stun { transport, .. } => match transport {
            IceTransport::Udp => capabilities.stun_udp,
            IceTransport::Tcp => capabilities.stun_tcp,
            IceTransport::Tls => false,
        },
        IceServer::Turn { transport, .. } => match transport {
            IceTransport::Udp => capabilities.turn_udp,
            IceTransport::Tcp => capabilities.turn_tcp,
            IceTransport::Tls => capabilities.turn_tls,
        },
    }
}

/// 10 ms of 48 kHz Opus in RTP timestamp units.
const OPUS_TIMESTAMP_STEP: u32 = 480;

fn capabilities_for(backend: IceBackend) -> ProviderCapabilities {
    match backend {
        IceBackend::Nice => ProviderCapabilities {
            stun_udp: true,
            stun_tcp: true,
            turn_udp: true,
            turn_tcp: true,
            turn_tls: true,
            custom_tls_trust: false,
            ice_restart: false,
            reliable_ordered_data_channel: true,
            stats: true,
        },
        IceBackend::Juice | IceBackend::Unknown => ProviderCapabilities {
            stun_udp: true,
            stun_tcp: false,
            turn_udp: true,
            turn_tcp: false,
            turn_tls: false,
            custom_tls_trust: false,
            ice_restart: false,
            reliable_ordered_data_channel: true,
            stats: true,
        },
    }
}

fn ice_server_url(server: &IceServer) -> Option<String> {
    match server {
        IceServer::Stun {
            host,
            port,
            transport,
        } => match transport {
            IceTransport::Udp => Some(format!("stun:{host}:{port}")),
            IceTransport::Tcp => Some(format!("stun:{host}:{port}?transport=tcp")),
            IceTransport::Tls => None,
        },
        IceServer::Turn {
            host,
            port,
            transport,
            credentials,
            ..
        } => {
            let user = percent_encode(credentials.username());
            let pass = percent_encode(credentials.credential());
            let (scheme, query) = match transport {
                IceTransport::Udp => ("turn", "udp"),
                IceTransport::Tcp => ("turn", "tcp"),
                IceTransport::Tls => ("turns", "tcp"),
            };
            Some(format!(
                "{scheme}:{user}:{pass}@{host}:{port}?transport={query}"
            ))
        }
    }
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(char::from(byte));
            }
            _ => {
                let _ = std::fmt::Write::write_fmt(&mut out, format_args!("%{byte:02X}"));
            }
        }
    }
    out
}

struct Shared {
    events: VecDeque<Event>,
    waker: Option<Waker>,
    capacity: usize,
    overflowed: bool,
    inbound: VecDeque<NativeEvent>,
}

enum NativeEvent {
    LocalDescription { sdp: String, ty: String },
    LocalCandidate { candidate: String, mid: String },
    State(i32),
    Gathering(i32),
    DataChannel(i32),
    Open(i32),
    Closed(i32),
    Message { id: i32, data: Vec<u8> },
    BufferedLow(i32),
}

impl Shared {
    fn new(capacity: usize) -> Self {
        Self {
            events: VecDeque::with_capacity(capacity),
            waker: None,
            capacity,
            overflowed: false,
            inbound: VecDeque::with_capacity(capacity),
        }
    }

    fn push_native(&mut self, event: NativeEvent) {
        if self.inbound.len() >= self.capacity {
            self.overflowed = true;
        } else {
            self.inbound.push_back(event);
        }
        self.wake();
    }

    fn push_event(&mut self, event: Event) {
        if self.events.len() >= self.capacity {
            self.overflowed = true;
        } else {
            self.events.push_back(event);
        }
        self.wake();
    }

    fn wake(&mut self) {
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }
}

struct Sink(Arc<Mutex<Shared>>);

impl PeerCallbacks for Sink {
    fn on_local_description(&self, sdp: &str, ty: &str) {
        if let Ok(mut shared) = self.0.lock() {
            shared.push_native(NativeEvent::LocalDescription {
                sdp: sdp.to_owned(),
                ty: ty.to_owned(),
            });
        }
    }

    fn on_local_candidate(&self, candidate: &str, mid: &str) {
        if let Ok(mut shared) = self.0.lock() {
            shared.push_native(NativeEvent::LocalCandidate {
                candidate: candidate.to_owned(),
                mid: mid.to_owned(),
            });
        }
    }

    fn on_state(&self, state: i32) {
        if let Ok(mut shared) = self.0.lock() {
            shared.push_native(NativeEvent::State(state));
        }
    }

    fn on_gathering(&self, state: i32) {
        if let Ok(mut shared) = self.0.lock() {
            shared.push_native(NativeEvent::Gathering(state));
        }
    }

    fn on_data_channel(&self, dc: i32) {
        if let Ok(mut shared) = self.0.lock() {
            shared.push_native(NativeEvent::DataChannel(dc));
        }
    }

    fn on_open(&self, id: i32) {
        if let Ok(mut shared) = self.0.lock() {
            shared.push_native(NativeEvent::Open(id));
        }
    }

    fn on_closed(&self, id: i32) {
        if let Ok(mut shared) = self.0.lock() {
            shared.push_native(NativeEvent::Closed(id));
        }
    }

    fn on_message(&self, id: i32, data: &[u8]) {
        if let Ok(mut shared) = self.0.lock() {
            shared.push_native(NativeEvent::Message {
                id,
                data: data.to_vec(),
            });
        }
    }

    fn on_buffered_low(&self, id: i32) {
        if let Ok(mut shared) = self.0.lock() {
            shared.push_native(NativeEvent::BufferedLow(id));
        }
    }
}

struct LibdatachannelPeer {
    config: PeerConfig,
    connection: PeerConnection,
    shared: Arc<Mutex<Shared>>,
    highest_operation: Option<OperationId>,
    state: PeerState,
    epoch: Option<NegotiationEpoch>,
    local_description: Option<SessionDescription>,
    remote_description: Option<SessionDescription>,
    pending_create: Option<(OperationId, NegotiationEpoch, DescriptionKind)>,
    channel_id: Option<ChannelId>,
    dc_native: Option<i32>,
    channel_open: bool,
    messages_sent: u64,
    bytes_sent: u64,
    messages_received: u64,
    bytes_received: u64,
    stats_sequence: u64,
    shutdown: bool,
    shutdown_complete: bool,
    fatal: bool,
}

impl LibdatachannelPeer {
    fn new(config: PeerConfig, capabilities: ProviderCapabilities) -> Result<Self, TransportError> {
        let urls: Vec<String> = config
            .ice_servers
            .iter()
            .filter_map(ice_server_url)
            .collect();
        let enable_ice_tcp = capabilities.stun_tcp || capabilities.turn_tcp;
        let max_message_size = i32::try_from(config.max_message_bytes)
            .map_err(|_| TransportError::InvalidMessageCapacity)?;
        let shared = Arc::new(Mutex::new(Shared::new(config.event_capacity)));
        let connection = PeerConnection::create(
            &Configuration {
                ice_servers: urls,
                max_message_size,
                enable_ice_tcp,
                force_media_transport: config.sendonly_opus,
            },
            Box::new(Sink(Arc::clone(&shared))),
        )
        .map_err(|_| TransportError::ProviderFailure)?;
        Ok(Self {
            config,
            connection,
            shared,
            highest_operation: None,
            state: PeerState::New,
            epoch: None,
            local_description: None,
            remote_description: None,
            pending_create: None,
            channel_id: None,
            dc_native: None,
            channel_open: false,
            messages_sent: 0,
            bytes_sent: 0,
            messages_received: 0,
            bytes_received: 0,
            stats_sequence: 0,
            shutdown: false,
            shutdown_complete: false,
            fatal: false,
        })
    }

    fn emit(&mut self, event: Event) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.push_event(event);
        }
    }

    fn transition(&mut self, state: PeerState) {
        if self.state == state {
            return;
        }
        self.state = state;
        self.emit(Event::StateChanged { state });
    }

    fn complete(&mut self, operation_id: OperationId) {
        self.emit(Event::OperationCompleted { operation_id });
    }

    fn fail(&mut self, operation_id: OperationId, error: TransportError) {
        self.emit(Event::OperationFailed {
            operation_id,
            error,
        });
    }

    fn fail_fatal(&mut self, error: TransportError) {
        if self.fatal {
            return;
        }
        self.fatal = true;
        self.transition(PeerState::Failed);
        self.emit(Event::FatalError { error });
    }

    fn process(&mut self, command: Command) {
        match command {
            Command::CreateOffer {
                operation_id,
                epoch,
            } => self.create_local(operation_id, epoch, DescriptionKind::Offer),
            Command::CreateAnswer {
                operation_id,
                epoch,
            } => self.create_local(operation_id, epoch, DescriptionKind::Answer),
            Command::SetLocalDescription {
                operation_id,
                description,
            } => self.set_local(operation_id, description),
            Command::SetRemoteDescription {
                operation_id,
                description,
            } => self.set_remote(operation_id, description),
            Command::AddRemoteCandidate {
                operation_id,
                candidate,
            } => self.add_candidate(operation_id, &candidate),
            Command::EndRemoteCandidates { operation_id, .. } => self.complete(operation_id),
            Command::RestartIce { operation_id, .. } => {
                self.fail(operation_id, TransportError::UnsupportedCapability);
            }
            Command::OpenDataChannel {
                operation_id,
                channel_id,
            } => self.open_channel(operation_id, channel_id),
            Command::CloseDataChannel {
                operation_id,
                channel_id,
            } => self.close_channel(operation_id, channel_id),
            Command::Send {
                operation_id,
                channel_id,
                payload,
            } => self.send(operation_id, channel_id, payload),
            Command::RequestStats { operation_id } => self.stats(operation_id),
            Command::Shutdown { operation_id } => self.do_shutdown(operation_id),
        }
    }

    fn create_local(
        &mut self,
        operation_id: OperationId,
        epoch: NegotiationEpoch,
        kind: DescriptionKind,
    ) {
        let expected_role_ok = match kind {
            DescriptionKind::Offer => self.config.role == relay_transport::Role::Offerer,
            DescriptionKind::Answer => {
                self.config.role == relay_transport::Role::Answerer
                    && self.remote_description.as_ref().is_some_and(|remote| {
                        remote.epoch() == epoch && remote.kind() == DescriptionKind::Offer
                    })
            }
        };
        if !expected_role_ok {
            self.fail(operation_id, TransportError::InvalidState);
            return;
        }
        if kind == DescriptionKind::Offer {
            if self.epoch.is_some_and(|active| epoch <= active) {
                self.fail(operation_id, TransportError::StaleEpoch);
                return;
            }
            self.epoch = Some(epoch);
            self.local_description = None;
            self.remote_description = None;
        } else if self.epoch != Some(epoch) {
            self.fail(operation_id, TransportError::StaleEpoch);
            return;
        }
        if self.pending_create.is_some() {
            self.fail(operation_id, TransportError::InvalidState);
            return;
        }
        let ty = match kind {
            DescriptionKind::Offer => "offer",
            DescriptionKind::Answer => "answer",
        };
        self.pending_create = Some((operation_id, epoch, kind));
        if self.state == PeerState::New {
            self.transition(PeerState::Negotiating);
        }
        if self.connection.set_local_description(ty).is_err() {
            self.pending_create = None;
            self.fail(operation_id, TransportError::ProviderFailure);
        }
    }

    fn set_local(&mut self, operation_id: OperationId, description: SessionDescription) {
        if self.epoch != Some(description.epoch()) {
            self.fail(operation_id, TransportError::StaleEpoch);
            return;
        }
        if let Some(existing) = &self.local_description {
            if existing.sdp() == description.sdp() && existing.kind() == description.kind() {
                self.complete(operation_id);
            } else {
                self.fail(operation_id, TransportError::ConflictingDescription);
            }
            return;
        }
        let ty = match description.kind() {
            DescriptionKind::Offer => "offer",
            DescriptionKind::Answer => "answer",
        };
        if self.connection.set_local_description(ty).is_err() {
            self.fail(operation_id, TransportError::ProviderFailure);
            return;
        }
        self.local_description = Some(description);
        self.complete(operation_id);
    }

    fn set_remote(&mut self, operation_id: OperationId, description: SessionDescription) {
        if description.sdp().len() > self.config.max_sdp_bytes {
            self.fail(operation_id, TransportError::SdpTooLarge);
            return;
        }
        let expected = match self.config.role {
            relay_transport::Role::Offerer => DescriptionKind::Answer,
            relay_transport::Role::Answerer => DescriptionKind::Offer,
        };
        if description.kind() != expected {
            self.fail(operation_id, TransportError::InvalidState);
            return;
        }
        if self.config.role == relay_transport::Role::Answerer {
            if self
                .epoch
                .is_some_and(|active| description.epoch() < active)
            {
                self.fail(operation_id, TransportError::StaleEpoch);
                return;
            }
            self.epoch = Some(description.epoch());
            if self.state == PeerState::New {
                self.transition(PeerState::Negotiating);
            }
        } else if self.epoch != Some(description.epoch()) {
            self.fail(operation_id, TransportError::StaleEpoch);
            return;
        }
        let ty = match description.kind() {
            DescriptionKind::Offer => "offer",
            DescriptionKind::Answer => "answer",
        };
        if self
            .connection
            .set_remote_description(description.sdp(), ty)
            .is_err()
        {
            self.fail(operation_id, TransportError::ProviderFailure);
            return;
        }
        self.remote_description = Some(description);
        self.complete(operation_id);
    }

    fn add_candidate(&mut self, operation_id: OperationId, candidate: &IceCandidate) {
        if self.epoch != Some(candidate.epoch()) || self.remote_description.is_none() {
            self.fail(operation_id, TransportError::StaleEpoch);
            return;
        }
        let text = candidate.candidate().trim();
        if text.is_empty() || text == "candidate:" {
            self.complete(operation_id);
            return;
        }
        let prepared = if text.starts_with("candidate:") {
            text.to_owned()
        } else {
            format!("candidate:{text}")
        };
        if self
            .connection
            .add_remote_candidate(&prepared, candidate.sdp_mid())
            .is_err()
        {
            self.fail(operation_id, TransportError::ProviderFailure);
            return;
        }
        self.complete(operation_id);
    }

    fn open_channel(&mut self, operation_id: OperationId, channel_id: ChannelId) {
        if let Some(existing) = self.channel_id {
            if existing == channel_id && self.dc_native.is_some() {
                self.complete(operation_id);
                return;
            }
            self.fail(operation_id, TransportError::InvalidState);
            return;
        }
        if self.config.sendonly_opus {
            match self.connection.add_opus_sendonly_track() {
                Ok(tr) => {
                    self.channel_id = Some(channel_id);
                    self.dc_native = Some(tr);
                    // SCTP keepalives hold the ICE pair across NAT UDP timeouts
                    // that a sendonly RTP track alone often misses.
                    let _ = self.connection.create_data_channel("ka");
                    self.complete(operation_id);
                }
                Err(_) => self.fail(operation_id, TransportError::ProviderFailure),
            }
            return;
        }
        match self.connection.create_data_channel("relay") {
            Ok(dc) => {
                self.channel_id = Some(channel_id);
                self.dc_native = Some(dc);
                let low = i32::try_from(self.config.send_low_water_bytes).unwrap_or(i32::MAX);
                let _ = self.connection.set_buffered_amount_low(dc, low);
                if self.connection.is_open(dc) {
                    self.channel_open = true;
                    self.emit(Event::DataChannelOpened { channel_id });
                }
                self.complete(operation_id);
            }
            Err(_) => self.fail(operation_id, TransportError::ProviderFailure),
        }
    }

    fn close_channel(&mut self, operation_id: OperationId, channel_id: ChannelId) {
        if self.channel_id != Some(channel_id) {
            self.fail(operation_id, TransportError::InvalidState);
            return;
        }
        if let Some(dc) = self.dc_native {
            let _ = self.connection.close_channel(dc);
        }
        if self.channel_open {
            self.channel_open = false;
            self.emit(Event::DataChannelClosed { channel_id });
        }
        self.complete(operation_id);
    }

    fn maybe_open_sendonly_track(&mut self) {
        if !self.config.sendonly_opus || self.channel_open {
            return;
        }
        let Some(tr) = self.dc_native else {
            return;
        };
        if !self.connection.is_open(tr) {
            return;
        }
        let Some(channel_id) = self.channel_id else {
            return;
        };
        self.channel_open = true;
        self.emit(Event::DataChannelOpened { channel_id });
    }

    fn send(&mut self, operation_id: OperationId, channel_id: ChannelId, payload: BinaryPayload) {
        self.maybe_open_sendonly_track();
        if self.channel_id != Some(channel_id) || !self.channel_open {
            self.fail(operation_id, TransportError::InvalidState);
            return;
        }
        let Some(dc) = self.dc_native else {
            self.fail(operation_id, TransportError::InvalidState);
            return;
        };
        let buffered = usize::try_from(self.connection.buffered_amount(dc).max(0)).unwrap_or(0);
        if buffered.saturating_add(payload.len()) > self.config.send_buffer_bytes {
            self.fail(operation_id, TransportError::WouldBlock);
            return;
        }
        let sent = if self.config.sendonly_opus {
            self.connection
                .send_opus_frame(dc, payload.as_bytes(), OPUS_TIMESTAMP_STEP)
        } else {
            self.connection.send_binary(dc, payload.as_bytes())
        };
        if sent.is_err() {
            self.fail(operation_id, TransportError::ProviderFailure);
            return;
        }
        self.messages_sent = self.messages_sent.saturating_add(1);
        self.bytes_sent = self
            .bytes_sent
            .saturating_add(u64::try_from(payload.len()).unwrap_or(u64::MAX));
        self.complete(operation_id);
    }

    fn stats(&mut self, operation_id: OperationId) {
        self.stats_sequence = self.stats_sequence.saturating_add(1);
        let buffered = self
            .dc_native
            .map(|dc| u64::try_from(self.connection.buffered_amount(dc).max(0)).unwrap_or(0))
            .unwrap_or(0);
        self.emit(Event::Stats {
            operation_id,
            report: StatsReport {
                sequence: self.stats_sequence,
                messages_sent: self.messages_sent,
                bytes_sent: self.bytes_sent,
                messages_received: self.messages_received,
                bytes_received: self.bytes_received,
                buffered_send_bytes: buffered,
                buffered_send_messages: u64::from(buffered > 0),
            },
        });
        self.complete(operation_id);
    }

    fn do_shutdown(&mut self, operation_id: OperationId) {
        self.shutdown = true;
        self.transition(PeerState::Closing);
        if let Some(dc) = self.dc_native.take() {
            let _ = self.connection.close_channel(dc);
        }
        self.complete(operation_id);
        self.transition(PeerState::Closed);
        self.emit(Event::ShutdownComplete);
        self.shutdown_complete = true;
    }

    fn drain_native(&mut self) {
        let inbound = {
            let Ok(mut shared) = self.shared.lock() else {
                return;
            };
            let overflowed = shared.overflowed;
            shared.overflowed = false;
            let inbound: Vec<NativeEvent> = shared.inbound.drain(..).collect();
            (overflowed, inbound)
        };
        if inbound.0 {
            self.fail_fatal(TransportError::EventQueueOverflow);
        }
        for event in inbound.1 {
            self.handle_native(event);
        }
    }

    fn handle_native(&mut self, event: NativeEvent) {
        match event {
            NativeEvent::LocalDescription { sdp, ty } => {
                if sdp.len() > self.config.max_sdp_bytes {
                    if let Some((operation_id, _, _)) = self.pending_create.take() {
                        self.fail(operation_id, TransportError::SdpTooLarge);
                    }
                    return;
                }
                let kind = if ty.eq_ignore_ascii_case("answer") {
                    DescriptionKind::Answer
                } else {
                    DescriptionKind::Offer
                };
                let Some((_, epoch, pending_kind)) = self.pending_create else {
                    return;
                };
                if pending_kind != kind {
                    return;
                }
                let Ok(description) = SessionDescription::new(epoch, kind, sdp) else {
                    if let Some((operation_id, _, _)) = self.pending_create.take() {
                        self.fail(operation_id, TransportError::SdpTooLarge);
                    }
                    return;
                };
                self.local_description = Some(description.clone());
                self.emit(Event::LocalDescription { description });
                if let Some((operation_id, _, _)) = self.pending_create.take() {
                    self.complete(operation_id);
                }
            }
            NativeEvent::LocalCandidate { candidate, mid } => {
                let Some(epoch) = self.epoch else {
                    return;
                };
                let text = candidate
                    .strip_prefix("a=")
                    .unwrap_or(candidate.as_str())
                    .to_owned();
                let mid = if mid.is_empty() { None } else { Some(mid) };
                if let Ok(candidate) = IceCandidate::new(epoch, text, mid, Some(0), None) {
                    self.emit(Event::LocalCandidate { candidate });
                }
            }
            NativeEvent::State(state) => match state {
                sys::RTC_CONNECTING => self.transition(PeerState::Connecting),
                sys::RTC_CONNECTED => {
                    self.transition(PeerState::Connected);
                    self.maybe_open_sendonly_track();
                }
                sys::RTC_DISCONNECTED => self.transition(PeerState::Disconnected),
                sys::RTC_FAILED => self.fail_fatal(TransportError::ProviderFailure),
                sys::RTC_CLOSED if !self.shutdown => {
                    self.fail_fatal(TransportError::ProviderFailure);
                }
                _ => {}
            },
            NativeEvent::Gathering(sys::RTC_GATHERING_COMPLETE) => {
                if let Some(epoch) = self.epoch
                    && let Ok(end) = EndOfCandidates::new(epoch, None, Some(0), None)
                {
                    self.emit(Event::LocalCandidatesEnded { end });
                }
            }
            NativeEvent::Gathering(_) => {}
            NativeEvent::DataChannel(dc) => {
                if self.dc_native.is_some() {
                    return;
                }
                if self.connection.attach_channel(dc).is_err() {
                    return;
                }
                let channel_id = self.channel_id.unwrap_or(ChannelId(0));
                self.channel_id = Some(channel_id);
                self.dc_native = Some(dc);
                if self.connection.is_open(dc) {
                    self.channel_open = true;
                    self.emit(Event::DataChannelOpened { channel_id });
                }
            }
            NativeEvent::Open(id) => {
                if self.dc_native == Some(id)
                    && let Some(channel_id) = self.channel_id
                {
                    self.channel_open = true;
                    self.emit(Event::DataChannelOpened { channel_id });
                }
            }
            NativeEvent::Closed(id) => {
                if self.dc_native == Some(id)
                    && let Some(channel_id) = self.channel_id
                    && self.channel_open
                {
                    self.channel_open = false;
                    self.emit(Event::DataChannelClosed { channel_id });
                }
            }
            NativeEvent::Message { id, data } => {
                if self.dc_native != Some(id) {
                    return;
                }
                let Some(channel_id) = self.channel_id else {
                    return;
                };
                if data.len() > self.config.max_message_bytes {
                    return;
                }
                let Ok(payload) = BinaryPayload::new(data) else {
                    return;
                };
                self.messages_received = self.messages_received.saturating_add(1);
                self.bytes_received = self
                    .bytes_received
                    .saturating_add(u64::try_from(payload.len()).unwrap_or(u64::MAX));
                self.emit(Event::Message {
                    channel_id,
                    payload,
                });
            }
            NativeEvent::BufferedLow(id) => {
                if self.dc_native != Some(id) {
                    return;
                }
                let Some(channel_id) = self.channel_id else {
                    return;
                };
                let buffered =
                    usize::try_from(self.connection.buffered_amount(id).max(0)).unwrap_or(0);
                let available_bytes = self.config.send_buffer_bytes.saturating_sub(buffered);
                self.emit(Event::SendCapacity {
                    channel_id,
                    available_bytes,
                    available_messages: if available_bytes >= self.config.max_message_bytes {
                        1
                    } else {
                        0
                    },
                });
            }
        }
    }

    fn pop_event(&mut self) -> Option<Event> {
        self.shared
            .lock()
            .ok()
            .and_then(|mut shared| shared.events.pop_front())
    }
}

impl PeerDriver for LibdatachannelPeer {
    fn submit(&mut self, command: Command) -> Result<(), SubmitError> {
        if self.shutdown || self.shutdown_complete {
            return Err(SubmitError::new(TransportError::Shutdown, command));
        }
        if self.fatal && !matches!(command, Command::Shutdown { .. }) {
            return Err(SubmitError::new(TransportError::ProviderFailure, command));
        }
        let operation_id = command.operation_id();
        if operation_id == OperationId(u64::MAX) && !matches!(command, Command::Shutdown { .. }) {
            return Err(SubmitError::new(
                TransportError::OperationIdExhausted,
                command,
            ));
        }
        if self
            .highest_operation
            .is_some_and(|highest| operation_id <= highest)
        {
            return Err(SubmitError::new(
                TransportError::DuplicateOperation,
                command,
            ));
        }
        if let Command::Send {
            channel_id,
            payload,
            ..
        } = &command
        {
            self.maybe_open_sendonly_track();
            if self.channel_id != Some(*channel_id) || !self.channel_open {
                return Err(SubmitError::new(TransportError::InvalidState, command));
            }
            if payload.len() > self.config.max_message_bytes {
                return Err(SubmitError::new(TransportError::MessageTooLarge, command));
            }
            if let Some(dc) = self.dc_native {
                let buffered =
                    usize::try_from(self.connection.buffered_amount(dc).max(0)).unwrap_or(0);
                if buffered.saturating_add(payload.len()) > self.config.send_buffer_bytes {
                    return Err(SubmitError::new(TransportError::WouldBlock, command));
                }
            }
        }
        self.highest_operation = Some(operation_id);
        self.process(command);
        if let Ok(mut shared) = self.shared.lock() {
            shared.wake();
        }
        Ok(())
    }

    fn poll_event(&mut self, context: &mut Context<'_>) -> Poll<Option<Event>> {
        self.drain_native();
        self.maybe_open_sendonly_track();
        if let Some(event) = self.pop_event() {
            return Poll::Ready(Some(event));
        }
        if self.shutdown_complete {
            return Poll::Ready(None);
        }
        if let Ok(mut shared) = self.shared.lock() {
            shared.waker = Some(context.waker().clone());
        }
        Poll::Pending
    }
}

/// Drains every currently ready event without parking.
pub fn drain_ready(peer: &mut dyn PeerDriver) -> Vec<Event> {
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut events = Vec::new();
    while let Poll::Ready(Some(event)) = peer.poll_event(&mut context) {
        let terminal = matches!(event, Event::ShutdownComplete);
        events.push(event);
        if terminal {
            break;
        }
    }
    events
}

#[cfg(test)]
mod tests {
    use super::sys;
    use super::*;
    use std::time::{Duration, Instant};

    fn wait_until(mut pred: impl FnMut() -> bool, limit: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < limit {
            if pred() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        pred()
    }

    #[test]
    fn stun_url_is_cloudflare() {
        let server = default_stun_server().expect("built-in STUN");
        assert_eq!(
            ice_server_url(&server).as_deref(),
            Some("stun:stun.cloudflare.com:3478")
        );
    }

    #[test]
    fn turn_tls_url_uses_turns_443() {
        let credentials = relay_transport::TurnCredentials::new("u", "p").expect("creds");
        let tls = relay_transport::TurnTlsConfig::new(
            "turn.example",
            relay_transport::TlsTrust::Platform,
        )
        .expect("tls");
        let server = IceServer::turn(
            "turn.example",
            443,
            IceTransport::Tls,
            credentials,
            Some(tls),
        )
        .expect("turn");
        assert_eq!(
            ice_server_url(&server).as_deref(),
            Some("turns:u:p@turn.example:443?transport=tcp")
        );
    }

    #[test]
    fn juice_does_not_claim_turn_tls() {
        let caps = capabilities_for(sys::IceBackend::Juice);
        assert!(!caps.turn_tls);
        assert!(!caps.turn_tcp);
        assert!(caps.stun_udp);
        let nice = capabilities_for(sys::IceBackend::Nice);
        assert!(nice.turn_tls);
        assert!(nice.turn_tcp);
    }

    #[test]
    fn unsupported_relays_are_dropped_not_fatal() {
        let creds = relay_transport::TurnCredentials::new("u", "p").expect("creds");
        let udp = IceServer::turn(
            "turn.example",
            3478,
            IceTransport::Udp,
            creds.clone(),
            None,
        )
        .expect("turn udp");
        let tcp = IceServer::turn("turn.example", 80, IceTransport::Tcp, creds, None)
            .expect("turn tcp");
        let juice = capabilities_for(sys::IceBackend::Juice);
        assert!(ice_server_supported(&udp, &juice));
        assert!(
            !ice_server_supported(&tcp, &juice),
            "juice has no turn/tcp, so this entry must not reach validation"
        );
        // An all-unusable list still yields a dialable peer.
        let config = listen_offerer_config(&[tcp]).expect("config");
        assert_eq!(
            config.ice_servers,
            vec![default_stun_server().expect("stun")],
            "falls back to stun rather than shipping an empty ice list"
        );
    }

    #[test]
    fn listen_offer_sdp_contains_opus() {
        let provider = LibdatachannelProvider::new();
        let mut config = listen_offerer_config(&[]).expect("offerer config");
        config.ice_servers.clear();
        let validated = config.validate_for(provider.capabilities()).expect("cfg");
        let mut offerer = provider.create_peer(validated).expect("offerer");
        offerer
            .submit(Command::OpenDataChannel {
                operation_id: OperationId(1),
                channel_id: ChannelId(0),
            })
            .expect("open track");
        offerer
            .submit(Command::CreateOffer {
                operation_id: OperationId(2),
                epoch: NegotiationEpoch(1),
            })
            .expect("create offer");

        let mut offer = None;
        assert!(
            wait_until(
                || {
                    for event in drain_ready(offerer.as_mut()) {
                        if let Event::LocalDescription { description } = event {
                            offer = Some(description);
                        }
                    }
                    offer.is_some()
                },
                Duration::from_secs(5)
            ),
            "offerer produced no local description"
        );
        let sdp = offer.expect("offer").sdp().to_ascii_lowercase();
        assert!(sdp.contains("m=audio"), "sdp={sdp}");
        assert!(sdp.contains("opus"), "sdp={sdp}");
        assert!(sdp.contains("sendonly"), "sdp={sdp}");
        assert!(sdp.contains("m=application"), "sdp={sdp}");
        let _ = offerer.submit(Command::Shutdown {
            operation_id: OperationId(3),
        });
        let _ = drain_ready(offerer.as_mut());
    }

    #[test]
    fn listen_offerer_can_send_opus_after_ice() {
        let provider = LibdatachannelProvider::new();
        let mut offer_cfg = listen_offerer_config(&[]).expect("offerer config");
        offer_cfg.ice_servers.clear();
        let mut answer_cfg = PeerConfig::answerer();
        answer_cfg.sendonly_opus = true;
        answer_cfg.ice_servers.clear();
        let offer_validated = offer_cfg
            .validate_for(provider.capabilities())
            .expect("offer cfg");
        let answer_validated = answer_cfg
            .validate_for(provider.capabilities())
            .expect("answer cfg");
        let mut offerer = provider.create_peer(offer_validated).expect("offerer");
        let mut answerer = provider.create_peer(answer_validated).expect("answerer");

        offerer
            .submit(Command::OpenDataChannel {
                operation_id: OperationId(1),
                channel_id: ChannelId(0),
            })
            .expect("open track");
        offerer
            .submit(Command::CreateOffer {
                operation_id: OperationId(2),
                epoch: NegotiationEpoch(1),
            })
            .expect("create offer");

        let mut offer = None;
        let mut offer_ice = Vec::new();
        assert!(
            wait_until(
                || {
                    for event in drain_ready(offerer.as_mut()) {
                        match event {
                            Event::LocalDescription { description } => offer = Some(description),
                            Event::LocalCandidate { candidate } => offer_ice.push(candidate),
                            _ => {}
                        }
                    }
                    offer.is_some()
                },
                Duration::from_secs(5)
            ),
            "offerer produced no local description"
        );
        let offer = offer.expect("offer");

        answerer
            .submit(Command::SetRemoteDescription {
                operation_id: OperationId(1),
                description: offer,
            })
            .expect("set remote offer");
        answerer
            .submit(Command::CreateAnswer {
                operation_id: OperationId(2),
                epoch: NegotiationEpoch(1),
            })
            .expect("create answer");

        let mut answer = None;
        let mut answer_ice = Vec::new();
        assert!(
            wait_until(
                || {
                    for event in drain_ready(answerer.as_mut()) {
                        match event {
                            Event::LocalDescription { description } => answer = Some(description),
                            Event::LocalCandidate { candidate } => answer_ice.push(candidate),
                            _ => {}
                        }
                    }
                    answer.is_some()
                },
                Duration::from_secs(5)
            ),
            "answerer produced no local description"
        );
        let answer = answer.expect("answer");
        offerer
            .submit(Command::SetRemoteDescription {
                operation_id: OperationId(3),
                description: answer,
            })
            .expect("set remote answer");

        let mut next_offer = 4_u64;
        let mut next_answer = 3_u64;
        for candidate in offer_ice {
            answerer
                .submit(Command::AddRemoteCandidate {
                    operation_id: OperationId(next_answer),
                    candidate,
                })
                .ok();
            next_answer += 1;
        }
        for candidate in answer_ice {
            offerer
                .submit(Command::AddRemoteCandidate {
                    operation_id: OperationId(next_offer),
                    candidate,
                })
                .ok();
            next_offer += 1;
        }

        let mut offer_ready = false;
        let mut events = Vec::new();
        assert!(
            wait_until(
                || {
                    for event in drain_ready(offerer.as_mut()) {
                        match &event {
                            Event::LocalCandidate { candidate } => {
                                answerer
                                    .submit(Command::AddRemoteCandidate {
                                        operation_id: OperationId(next_answer),
                                        candidate: candidate.clone(),
                                    })
                                    .ok();
                                next_answer += 1;
                            }
                            Event::DataChannelOpened { .. } => offer_ready = true,
                            _ => {}
                        }
                        events.push(format!("{event:?}"));
                    }
                    for event in drain_ready(answerer.as_mut()) {
                        if let Event::LocalCandidate { candidate } = event {
                            offerer
                                .submit(Command::AddRemoteCandidate {
                                    operation_id: OperationId(next_offer),
                                    candidate,
                                })
                                .ok();
                            next_offer += 1;
                        }
                    }
                    offer_ready
                },
                Duration::from_secs(8)
            ),
            "listen offerer never became ready: {events:?}"
        );

        let send = offerer.submit(Command::Send {
            operation_id: OperationId(next_offer),
            channel_id: ChannelId(0),
            payload: BinaryPayload::new(vec![0xFC, 0xFF, 0xFE]).expect("payload"),
        });
        assert!(
            send.is_ok(),
            "send opus after ICE: {send:?} events={events:?}"
        );

        let _ = offerer.submit(Command::Shutdown {
            operation_id: OperationId(next_offer + 1),
        });
        let _ = answerer.submit(Command::Shutdown {
            operation_id: OperationId(next_answer + 1),
        });
        let _ = drain_ready(offerer.as_mut());
        let _ = drain_ready(answerer.as_mut());
    }

    #[test]
    fn two_peers_exchange_a_data_channel_message() {
        let provider = LibdatachannelProvider::new();
        let mut offer_cfg = PeerConfig::offerer();
        offer_cfg
            .required_capabilities
            .reliable_ordered_data_channel = true;
        let mut answer_cfg = PeerConfig::answerer();
        answer_cfg
            .required_capabilities
            .reliable_ordered_data_channel = true;
        let offer_validated = offer_cfg
            .validate_for(provider.capabilities())
            .expect("offer cfg");
        let answer_validated = answer_cfg
            .validate_for(provider.capabilities())
            .expect("answer cfg");
        let mut offerer = provider.create_peer(offer_validated).expect("offerer");
        let mut answerer = provider.create_peer(answer_validated).expect("answerer");

        offerer
            .submit(Command::OpenDataChannel {
                operation_id: OperationId(1),
                channel_id: ChannelId(0),
            })
            .expect("open dc");
        offerer
            .submit(Command::CreateOffer {
                operation_id: OperationId(2),
                epoch: NegotiationEpoch(1),
            })
            .expect("create offer");

        let mut offer = None;
        let mut offer_ice = Vec::new();
        assert!(
            wait_until(
                || {
                    for event in drain_ready(offerer.as_mut()) {
                        match event {
                            Event::LocalDescription { description } => offer = Some(description),
                            Event::LocalCandidate { candidate } => offer_ice.push(candidate),
                            _ => {}
                        }
                    }
                    offer.is_some()
                },
                Duration::from_secs(5)
            ),
            "offerer produced no local description"
        );
        let offer = offer.expect("offer");

        answerer
            .submit(Command::SetRemoteDescription {
                operation_id: OperationId(1),
                description: offer,
            })
            .expect("set remote offer");
        answerer
            .submit(Command::CreateAnswer {
                operation_id: OperationId(2),
                epoch: NegotiationEpoch(1),
            })
            .expect("create answer");

        let mut answer = None;
        let mut answer_ice = Vec::new();
        assert!(
            wait_until(
                || {
                    for event in drain_ready(answerer.as_mut()) {
                        match event {
                            Event::LocalDescription { description } => answer = Some(description),
                            Event::LocalCandidate { candidate } => answer_ice.push(candidate),
                            _ => {}
                        }
                    }
                    answer.is_some()
                },
                Duration::from_secs(5)
            ),
            "answerer produced no local description"
        );
        let answer = answer.expect("answer");

        offerer
            .submit(Command::SetRemoteDescription {
                operation_id: OperationId(3),
                description: answer,
            })
            .expect("set remote answer");

        let mut next_offer = 4_u64;
        let mut next_answer = 3_u64;
        for candidate in offer_ice {
            answerer
                .submit(Command::AddRemoteCandidate {
                    operation_id: OperationId(next_answer),
                    candidate,
                })
                .ok();
            next_answer += 1;
        }
        for candidate in answer_ice {
            offerer
                .submit(Command::AddRemoteCandidate {
                    operation_id: OperationId(next_offer),
                    candidate,
                })
                .ok();
            next_offer += 1;
        }

        let mut offer_open = false;
        let mut answer_open = false;
        assert!(
            wait_until(
                || {
                    for event in drain_ready(offerer.as_mut()) {
                        match event {
                            Event::LocalCandidate { candidate } => {
                                answerer
                                    .submit(Command::AddRemoteCandidate {
                                        operation_id: OperationId(next_answer),
                                        candidate,
                                    })
                                    .ok();
                                next_answer += 1;
                            }
                            Event::DataChannelOpened { .. } => offer_open = true,
                            _ => {}
                        }
                    }
                    for event in drain_ready(answerer.as_mut()) {
                        match event {
                            Event::LocalCandidate { candidate } => {
                                offerer
                                    .submit(Command::AddRemoteCandidate {
                                        operation_id: OperationId(next_offer),
                                        candidate,
                                    })
                                    .ok();
                                next_offer += 1;
                            }
                            Event::DataChannelOpened { .. } => answer_open = true,
                            _ => {}
                        }
                    }
                    offer_open && answer_open
                },
                Duration::from_secs(8)
            ),
            "data channel did not open"
        );

        offerer
            .submit(Command::Send {
                operation_id: OperationId(next_offer),
                channel_id: ChannelId(0),
                payload: BinaryPayload::new(b"ping".to_vec()).expect("payload"),
            })
            .expect("send");

        let mut got = None;
        assert!(
            wait_until(
                || {
                    for event in drain_ready(answerer.as_mut()) {
                        if let Event::Message { payload, .. } = event {
                            got = Some(payload.as_bytes().to_vec());
                        }
                    }
                    got.is_some()
                },
                Duration::from_secs(3)
            ),
            "answerer did not receive the message"
        );
        assert_eq!(got.as_deref(), Some(b"ping".as_ref()));

        let _ = offerer.submit(Command::Shutdown {
            operation_id: OperationId(next_offer + 1),
        });
        let _ = answerer.submit(Command::Shutdown {
            operation_id: OperationId(next_answer + 1),
        });
        let _ = drain_ready(offerer.as_mut());
        let _ = drain_ready(answerer.as_mut());
    }
}
