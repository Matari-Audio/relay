//! Bounded, provider-neutral negotiation primitives for native transports.
//!
//! The public seam is deliberately a single-owner command/event pump. Native
//! callbacks, runtimes, threads, and provider objects stay behind [`PeerDriver`].
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use core::fmt;
use core::task::{Context, Poll};
use std::collections::VecDeque;
use std::net::IpAddr;
use std::str::FromStr;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Largest command or event queue accepted by configuration.
pub const MAX_QUEUE_CAPACITY: usize = 4_096;
/// Absolute bound for an SDP value owned by this crate.
pub const MAX_SDP_BYTES: usize = 64 * 1_024;
/// Absolute aggregate bound for one ICE candidate value.
pub const MAX_CANDIDATE_BYTES: usize = 4 * 1_024;
/// Absolute bound for one reliable ordered data-channel message.
pub const MAX_MESSAGE_BYTES: usize = 256 * 1_024;
/// Largest aggregate provider send budget accepted by configuration.
pub const MAX_SEND_BUFFER_BYTES: usize = 4 * 1_024 * 1_024;
/// Largest aggregate provider send-message budget accepted by configuration.
pub const MAX_SEND_BUFFER_MESSAGES: usize = 4_096;
/// Largest number of configured ICE servers.
pub const MAX_ICE_SERVERS: usize = 16;
/// Absolute bound for an ICE host, user name, credential, or TLS server name.
pub const MAX_ICE_TEXT_BYTES: usize = 1_024;
/// Absolute aggregate bound for custom DER trust anchors.
pub const MAX_CUSTOM_TRUST_BYTES: usize = 256 * 1_024;
/// Largest number of custom DER trust anchors.
pub const MAX_CUSTOM_TRUST_ANCHORS: usize = 64;
/// Largest portable operation or shutdown timeout.
pub const MAX_TIMEOUT_MILLIS: u64 = 300_000;

const MIN_EVENT_CAPACITY: usize = 5;
const FAKE_OFFER_SDP: &str = "v=0\r\no=- 20002 1 IN IP4 127.0.0.1\r\ns=RELAY native offer fixture\r\nt=0 0\r\na=ice-options:trickle\r\na=ice-ufrag:native-base-v1\r\na=setup:actpass\r\n";
const FAKE_ANSWER_SDP: &str = "v=0\r\no=- 20001 1 IN IP4 127.0.0.1\r\ns=RELAY native answer fixture\r\nt=0 0\r\na=ice-options:trickle\r\na=ice-ufrag:native-base-v1\r\na=setup:active\r\n";
const FAKE_CANDIDATE: &str =
    "candidate:1 1 UDP 2122260223 198.51.100.20 50002 typ host generation 0 ufrag native-base-v1";

/// Correlates an accepted command with its terminal operation event.
///
/// A peer accepts operation IDs in strictly increasing order. This permits
/// duplicate detection with bounded memory. [`OperationId(u64::MAX)`](OperationId)
/// is reserved for [`Command::Shutdown`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OperationId(pub u64);

/// Identifies one local negotiation generation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NegotiationEpoch(pub u64);

/// The native peer's signalling role.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    /// The native peer initiates with an offer.
    Offerer,
    /// The native peer receives an offer and creates an answer.
    Answerer,
}

/// The kind of an opaque session description.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DescriptionKind {
    /// An SDP offer.
    Offer,
    /// An SDP answer.
    Answer,
}

/// An owned SDP string with a crate-wide absolute bound.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionDescription {
    epoch: NegotiationEpoch,
    kind: DescriptionKind,
    sdp: String,
}

impl SessionDescription {
    /// Constructs an opaque SDP value.
    pub fn new(
        epoch: NegotiationEpoch,
        kind: DescriptionKind,
        sdp: impl Into<String>,
    ) -> Result<Self, TransportError> {
        let sdp = sdp.into();
        if sdp.len() > MAX_SDP_BYTES {
            return Err(TransportError::SdpTooLarge);
        }
        let sdp = sdp.into_boxed_str().into_string();
        Ok(Self { epoch, kind, sdp })
    }

    /// Returns the negotiation epoch.
    #[must_use]
    pub const fn epoch(&self) -> NegotiationEpoch {
        self.epoch
    }

    /// Returns the description kind.
    #[must_use]
    pub const fn kind(&self) -> DescriptionKind {
        self.kind
    }

    /// Returns the opaque SDP text.
    #[must_use]
    pub fn sdp(&self) -> &str {
        &self.sdp
    }
}

/// A bounded end-of-candidates marker using the canonical empty-candidate shape.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EndOfCandidates {
    epoch: NegotiationEpoch,
    sdp_mid: Option<String>,
    sdp_mline_index: Option<u16>,
    username_fragment: Option<String>,
}

impl EndOfCandidates {
    /// Constructs an end marker and enforces the absolute aggregate text bound.
    pub fn new(
        epoch: NegotiationEpoch,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
        username_fragment: Option<String>,
    ) -> Result<Self, TransportError> {
        let text_bytes =
            optional_candidate_text_bytes(sdp_mid.as_deref(), username_fragment.as_deref());
        if text_bytes > MAX_CANDIDATE_BYTES {
            return Err(TransportError::CandidateTooLarge);
        }
        let sdp_mid = sdp_mid.map(normalize_string);
        let username_fragment = username_fragment.map(normalize_string);
        Ok(Self {
            epoch,
            sdp_mid,
            sdp_mline_index,
            username_fragment,
        })
    }

    /// Returns the negotiation epoch.
    #[must_use]
    pub const fn epoch(&self) -> NegotiationEpoch {
        self.epoch
    }

    /// Returns the media-section identifier, when present.
    #[must_use]
    pub fn sdp_mid(&self) -> Option<&str> {
        self.sdp_mid.as_deref()
    }

    /// Returns the media-section index, when present.
    #[must_use]
    pub const fn sdp_mline_index(&self) -> Option<u16> {
        self.sdp_mline_index
    }

    /// Returns the ICE username fragment, when present.
    #[must_use]
    pub fn username_fragment(&self) -> Option<&str> {
        self.username_fragment.as_deref()
    }

    fn text_bytes(&self) -> usize {
        optional_candidate_text_bytes(self.sdp_mid.as_deref(), self.username_fragment.as_deref())
    }
}

fn normalize_string(value: String) -> String {
    value.into_boxed_str().into_string()
}

fn optional_candidate_text_bytes(sdp_mid: Option<&str>, username_fragment: Option<&str>) -> usize {
    sdp_mid
        .map_or(0, str::len)
        .saturating_add(username_fragment.map_or(0, str::len))
}

/// An owned trickle ICE candidate with V1-carrier-compatible fields.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IceCandidate {
    epoch: NegotiationEpoch,
    candidate: String,
    sdp_mid: Option<String>,
    sdp_mline_index: Option<u16>,
    username_fragment: Option<String>,
}

impl IceCandidate {
    /// Constructs a candidate and enforces the absolute aggregate text bound.
    pub fn new(
        epoch: NegotiationEpoch,
        candidate: impl Into<String>,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
        username_fragment: Option<String>,
    ) -> Result<Self, TransportError> {
        let candidate = candidate.into();
        let text_bytes = candidate
            .len()
            .saturating_add(sdp_mid.as_deref().map_or(0, str::len))
            .saturating_add(username_fragment.as_deref().map_or(0, str::len));
        if text_bytes > MAX_CANDIDATE_BYTES {
            return Err(TransportError::CandidateTooLarge);
        }
        let candidate = candidate.into_boxed_str().into_string();
        let sdp_mid = sdp_mid.map(|value| value.into_boxed_str().into_string());
        let username_fragment = username_fragment.map(|value| value.into_boxed_str().into_string());
        Ok(Self {
            epoch,
            candidate,
            sdp_mid,
            sdp_mline_index,
            username_fragment,
        })
    }

    /// Returns the negotiation epoch.
    #[must_use]
    pub const fn epoch(&self) -> NegotiationEpoch {
        self.epoch
    }

    /// Returns the candidate attribute text without an `a=` prefix.
    #[must_use]
    pub fn candidate(&self) -> &str {
        &self.candidate
    }

    /// Returns the media-section identifier, when present.
    #[must_use]
    pub fn sdp_mid(&self) -> Option<&str> {
        self.sdp_mid.as_deref()
    }

    /// Returns the media-section index, when present.
    #[must_use]
    pub const fn sdp_mline_index(&self) -> Option<u16> {
        self.sdp_mline_index
    }

    /// Returns the ICE username fragment, when present.
    #[must_use]
    pub fn username_fragment(&self) -> Option<&str> {
        self.username_fragment.as_deref()
    }

    fn text_bytes(&self) -> usize {
        self.candidate
            .len()
            .saturating_add(self.sdp_mid.as_deref().map_or(0, str::len))
            .saturating_add(self.username_fragment.as_deref().map_or(0, str::len))
    }
}

/// Provider-neutral transport used to contact an ICE server.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IceTransport {
    /// UDP datagrams.
    Udp,
    /// A TCP connection without TLS.
    Tcp,
    /// A TLS-protected TCP connection.
    Tls,
}

/// Trust roots used to authenticate a TURN-over-TLS server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TlsTrust {
    /// Use the target platform's authenticated root store.
    Platform,
    /// Use only the supplied DER-encoded trust anchors.
    Custom(Vec<Vec<u8>>),
}

/// Fail-closed TURN TLS configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnTlsConfig {
    server_name: String,
    trust: TlsTrust,
}

impl TurnTlsConfig {
    /// Constructs TLS configuration. Certificate and server-name verification
    /// are always enabled; this API deliberately has no insecure bypass.
    pub fn new(server_name: impl Into<String>, trust: TlsTrust) -> Result<Self, TransportError> {
        let server_name = server_name.into();
        if !valid_dns_name(&server_name) {
            return Err(TransportError::InvalidTlsServerName);
        }
        let trust = normalize_trust(trust)?;
        Ok(Self {
            server_name: normalize_string(server_name),
            trust,
        })
    }

    /// Returns the authenticated TLS server name.
    #[must_use]
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// Returns the fail-closed trust source.
    #[must_use]
    pub const fn trust(&self) -> &TlsTrust {
        &self.trust
    }
}

/// Credentials for a TURN server.
#[derive(Clone, Eq, PartialEq)]
pub struct TurnCredentials {
    username: String,
    credential: String,
}

impl TurnCredentials {
    /// Constructs non-empty bounded long-term TURN credentials.
    pub fn new(
        username: impl Into<String>,
        credential: impl Into<String>,
    ) -> Result<Self, TransportError> {
        let username = username.into();
        let credential = credential.into();
        if username.is_empty()
            || credential.is_empty()
            || username.len() > MAX_ICE_TEXT_BYTES
            || credential.len() > MAX_ICE_TEXT_BYTES
        {
            return Err(TransportError::InvalidIceServer);
        }
        Ok(Self {
            username: normalize_string(username),
            credential: normalize_string(credential),
        })
    }

    /// Returns the TURN username.
    #[must_use]
    pub fn username(&self) -> &str {
        &self.username
    }

    /// Returns the TURN credential.
    #[must_use]
    pub fn credential(&self) -> &str {
        &self.credential
    }
}

impl fmt::Debug for TurnCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TurnCredentials")
            .field("username", &"<redacted>")
            .field("credential", &"<redacted>")
            .finish()
    }
}

/// One validated provider-neutral STUN or TURN endpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IceServer {
    /// An unauthenticated STUN endpoint.
    Stun {
        /// DNS name or numeric address.
        host: String,
        /// Non-zero server port.
        port: u16,
        /// UDP or TCP transport. TLS is not valid for STUN in this seam.
        transport: IceTransport,
    },
    /// An authenticated TURN endpoint.
    Turn {
        /// DNS name or numeric address.
        host: String,
        /// Non-zero server port.
        port: u16,
        /// UDP, TCP, or TLS transport.
        transport: IceTransport,
        /// Authentication material, redacted from `Debug` output.
        credentials: TurnCredentials,
        /// Required exactly when `transport` is [`IceTransport::Tls`].
        tls: Option<TurnTlsConfig>,
    },
}

impl IceServer {
    /// Constructs and validates a STUN endpoint.
    pub fn stun(
        host: impl Into<String>,
        port: u16,
        transport: IceTransport,
    ) -> Result<Self, TransportError> {
        let host = host.into();
        if !valid_ice_host(&host) || port == 0 || transport == IceTransport::Tls {
            return Err(TransportError::InvalidIceServer);
        }
        Ok(Self::Stun {
            host: normalize_string(host),
            port,
            transport,
        })
    }

    /// Constructs and validates a TURN endpoint.
    pub fn turn(
        host: impl Into<String>,
        port: u16,
        transport: IceTransport,
        credentials: TurnCredentials,
        tls: Option<TurnTlsConfig>,
    ) -> Result<Self, TransportError> {
        let host = host.into();
        if !valid_ice_host(&host) || port == 0 || (transport == IceTransport::Tls) != tls.is_some()
        {
            return Err(TransportError::InvalidIceServer);
        }
        Ok(Self::Turn {
            host: normalize_string(host),
            port,
            transport,
            credentials,
            tls,
        })
    }

    /// Parses one RFC 7064 / RFC 7065 ICE URL as a signaling server hands it
    /// over: `stun:host:port`, `turn:host:port?transport=udp`,
    /// `turns:host:port?transport=tcp`.
    ///
    /// # Errors
    ///
    /// Returns [`TransportError::InvalidIceServer`] for an unknown scheme, a
    /// malformed authority, or TURN without credentials.
    pub fn parse_url(
        url: &str,
        credentials: Option<TurnCredentials>,
    ) -> Result<Self, TransportError> {
        let url = url.trim();
        let (scheme, rest) = url.split_once(':').ok_or(TransportError::InvalidIceServer)?;
        let (authority, query) = rest.split_once('?').unwrap_or((rest, ""));
        let transport = match query
            .split('&')
            .find_map(|pair| pair.strip_prefix("transport="))
        {
            None => None,
            Some("udp") => Some(IceTransport::Udp),
            Some("tcp") => Some(IceTransport::Tcp),
            Some(_) => return Err(TransportError::InvalidIceServer),
        };
        let (host, port) = match authority.rsplit_once(':') {
            // A bare IPv6 literal has colons but no port; RFC 7064 has no
            // bracket form, so treat anything unparseable as the whole host.
            Some((head, tail)) => match tail.parse::<u16>() {
                Ok(port) => (head, Some(port)),
                Err(_) => (authority, None),
            },
            None => (authority, None),
        };
        match scheme.to_ascii_lowercase().as_str() {
            "stun" => Self::stun(host, port.unwrap_or(3478), transport.unwrap_or(IceTransport::Udp)),
            "turn" => Self::turn(
                host,
                port.unwrap_or(3478),
                transport.unwrap_or(IceTransport::Udp),
                credentials.ok_or(TransportError::InvalidIceServer)?,
                None,
            ),
            "turns" => Self::turn(
                host,
                port.unwrap_or(5349),
                IceTransport::Tls,
                credentials.ok_or(TransportError::InvalidIceServer)?,
                Some(TurnTlsConfig::new(host, TlsTrust::Platform)?),
            ),
            _ => Err(TransportError::InvalidIceServer),
        }
    }
}

fn valid_ice_host(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ICE_TEXT_BYTES
        && value.is_ascii()
        && (IpAddr::from_str(value).is_ok() || valid_dns_name(value))
}

fn valid_dns_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 253
        && value.is_ascii()
        && value.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

fn der_value(input: &[u8], expected_tag: u8) -> Option<(&[u8], &[u8])> {
    if input.first().copied()? != expected_tag || expected_tag == 0 || expected_tag & 0x1f == 0x1f {
        return None;
    }
    let first_length = *input.get(1)?;
    let (header, length) = if first_length & 0x80 == 0 {
        (2_usize, usize::from(first_length))
    } else {
        let width = usize::from(first_length & 0x7f);
        if width == 0 || width > core::mem::size_of::<usize>() || input.get(2) == Some(&0) {
            return None;
        }
        let mut length = 0_usize;
        for byte in input.get(2..2 + width)? {
            length = length.checked_mul(256)?.checked_add(usize::from(*byte))?;
        }
        if length < 128 {
            return None;
        }
        (2 + width, length)
    };
    let end = header.checked_add(length)?;
    Some((input.get(header..end)?, input.get(end..)?))
}

fn take_der_value<'a>(input: &mut &'a [u8], expected_tag: u8) -> Option<(&'a [u8], &'a [u8])> {
    let original = *input;
    let (value, trailing) = der_value(original, expected_tag)?;
    let encoded_length = original.len().checked_sub(trailing.len())?;
    let encoded = original.get(..encoded_length)?;
    *input = trailing;
    Some((value, encoded))
}

fn take_any_der_value<'a>(input: &mut &'a [u8]) -> Option<(u8, &'a [u8], &'a [u8])> {
    let tag = input.first().copied()?;
    let (value, encoded) = take_der_value(input, tag)?;
    Some((tag, value, encoded))
}

fn valid_der_oid(oid: &[u8]) -> bool {
    if oid.is_empty() {
        return false;
    }
    let mut at_subidentifier_start = true;
    let mut subidentifier_bytes = 0_usize;
    for byte in oid {
        if at_subidentifier_start && *byte == 0x80 {
            return false;
        }
        subidentifier_bytes += 1;
        if subidentifier_bytes > 10 {
            return false;
        }
        at_subidentifier_start = byte & 0x80 == 0;
        if at_subidentifier_start {
            subidentifier_bytes = 0;
        }
    }
    at_subidentifier_start
}

fn valid_der_integer(integer: &[u8], max_bytes: usize) -> bool {
    !integer.is_empty()
        && integer.len() <= max_bytes
        && integer[0] & 0x80 == 0
        && (integer.len() == 1 || integer[0] != 0 || integer[1] & 0x80 != 0)
}

fn valid_der_bit_string(bit_string: &[u8]) -> bool {
    let Some((&unused_bits, bytes)) = bit_string.split_first() else {
        return false;
    };
    if unused_bits > 7 || bytes.is_empty() {
        return false;
    }
    unused_bits == 0
        || bytes
            .last()
            .is_some_and(|last| last & ((1_u8 << unused_bits) - 1) == 0)
}

fn valid_algorithm_identifier(algorithm: &[u8]) -> bool {
    let mut fields = algorithm;
    let Some((oid, _)) = take_der_value(&mut fields, 0x06) else {
        return false;
    };
    if !valid_der_oid(oid) {
        return false;
    }
    if fields.is_empty() {
        return true;
    }
    take_any_der_value(&mut fields).is_some() && fields.is_empty()
}

const OID_DOMAIN_COMPONENT: &[u8] = &[0x09, 0x92, 0x26, 0x89, 0x93, 0xf2, 0x2c, 0x64, 0x01, 0x19];
const OID_EMAIL_ADDRESS: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x09, 0x01];

#[derive(Clone, Copy)]
enum NameAttributeSyntax {
    Directory {
        max_characters: usize,
    },
    Printable {
        min_characters: usize,
        max_characters: usize,
    },
    Ia5 {
        max_characters: usize,
    },
}

fn name_attribute_syntax(oid: &[u8]) -> Option<NameAttributeSyntax> {
    if oid == OID_DOMAIN_COMPONENT {
        return Some(NameAttributeSyntax::Ia5 { max_characters: 63 });
    }
    if oid == OID_EMAIL_ADDRESS {
        return Some(NameAttributeSyntax::Ia5 {
            max_characters: 255,
        });
    }

    let &[0x55, 0x04, attribute] = oid else {
        return None;
    };
    let max_characters = match attribute {
        3 | 4 | 10 | 11 | 12 => 64,
        7..=9 | 15 | 19 | 65 => 128,
        13 => 256,
        17 | 18 => 40,
        41 => 256,
        42 => 16,
        43 => 5,
        44 => 3,
        5 => {
            return Some(NameAttributeSyntax::Printable {
                min_characters: 1,
                max_characters: 64,
            });
        }
        6 => {
            return Some(NameAttributeSyntax::Printable {
                min_characters: 2,
                max_characters: 2,
            });
        }
        20 => {
            return Some(NameAttributeSyntax::Printable {
                min_characters: 1,
                max_characters: 32,
            });
        }
        46 => {
            return Some(NameAttributeSyntax::Printable {
                min_characters: 1,
                max_characters: 256,
            });
        }
        _ => return None,
    };
    Some(NameAttributeSyntax::Directory { max_characters })
}

fn valid_printable_string(value: &[u8], min_characters: usize, max_characters: usize) -> bool {
    (min_characters..=max_characters).contains(&value.len())
        && value.iter().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    *byte,
                    b' ' | b'\''
                        | b'('
                        | b')'
                        | b'+'
                        | b','
                        | b'-'
                        | b'.'
                        | b'/'
                        | b':'
                        | b'='
                        | b'?'
                )
        })
}

fn valid_utf8_string(value: &[u8], max_characters: usize) -> bool {
    core::str::from_utf8(value)
        .ok()
        .is_some_and(|text| (1..=max_characters).contains(&text.chars().count()))
}

fn valid_bmp_string(value: &[u8], max_characters: usize) -> bool {
    let character_count = value.len() / 2;
    !value.is_empty()
        && value.len().is_multiple_of(2)
        && character_count <= max_characters
        && value.chunks_exact(2).all(|bytes| {
            let code_point = u16::from_be_bytes([bytes[0], bytes[1]]);
            !(0xd800..=0xdfff).contains(&code_point)
        })
}

fn valid_universal_string(value: &[u8], max_characters: usize) -> bool {
    let character_count = value.len() / 4;
    !value.is_empty()
        && value.len().is_multiple_of(4)
        && character_count <= max_characters
        && value.chunks_exact(4).all(|bytes| {
            let code_point = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            char::from_u32(code_point).is_some()
        })
}

fn valid_directory_string(tag: u8, value: &[u8], max_characters: usize) -> bool {
    match tag {
        0x0c => valid_utf8_string(value, max_characters),
        0x13 => valid_printable_string(value, 1, max_characters),
        0x1c => valid_universal_string(value, max_characters),
        0x1e => valid_bmp_string(value, max_characters),
        _ => false,
    }
}

fn valid_name_attribute_value(oid: &[u8], tag: u8, value: &[u8]) -> bool {
    match name_attribute_syntax(oid) {
        Some(NameAttributeSyntax::Directory { max_characters }) => {
            valid_directory_string(tag, value, max_characters)
        }
        Some(NameAttributeSyntax::Printable {
            min_characters,
            max_characters,
        }) => tag == 0x13 && valid_printable_string(value, min_characters, max_characters),
        Some(NameAttributeSyntax::Ia5 { max_characters }) => {
            tag == 0x16
                && (1..=max_characters).contains(&value.len())
                && value.iter().all(|byte| matches!(*byte, 0x20..=0x7e))
        }
        None => false,
    }
}

fn valid_name(name: &[u8]) -> bool {
    if name.is_empty() {
        return false;
    }
    let mut rdns = name;
    while !rdns.is_empty() {
        let Some((rdn, _)) = take_der_value(&mut rdns, 0x31) else {
            return false;
        };
        if rdn.is_empty() {
            return false;
        }
        let mut attributes = rdn;
        let mut previous_attribute: Option<&[u8]> = None;
        while !attributes.is_empty() {
            let Some((attribute, encoded_attribute)) = take_der_value(&mut attributes, 0x30) else {
                return false;
            };
            if previous_attribute.is_some_and(|previous| previous > encoded_attribute) {
                return false;
            }
            previous_attribute = Some(encoded_attribute);
            let mut fields = attribute;
            let Some((oid, _)) = take_der_value(&mut fields, 0x06) else {
                return false;
            };
            let Some((tag, value, _)) = take_any_der_value(&mut fields) else {
                return false;
            };
            if !valid_der_oid(oid)
                || !valid_name_attribute_value(oid, tag, value)
                || !fields.is_empty()
            {
                return false;
            }
        }
    }
    true
}

fn two_decimal_digits(input: &[u8]) -> Option<u8> {
    if input.len() != 2 || !input.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some((input[0] - b'0') * 10 + input[1] - b'0')
}

fn valid_time(tag: u8, time: &[u8]) -> bool {
    let date_offset = match tag {
        0x17 if time.len() == 13 => 2,
        0x18 if time.len() == 15 => 4,
        _ => return false,
    };
    if time.last() != Some(&b'Z') || !time[..date_offset].iter().all(u8::is_ascii_digit) {
        return false;
    }
    let components = &time[date_offset..time.len() - 1];
    let Some(month) = two_decimal_digits(&components[0..2]) else {
        return false;
    };
    let Some(day) = two_decimal_digits(&components[2..4]) else {
        return false;
    };
    let Some(hour) = two_decimal_digits(&components[4..6]) else {
        return false;
    };
    let Some(minute) = two_decimal_digits(&components[6..8]) else {
        return false;
    };
    let Some(second) = two_decimal_digits(&components[8..10]) else {
        return false;
    };
    (1..=12).contains(&month)
        && (1..=31).contains(&day)
        && hour <= 23
        && minute <= 59
        && second <= 59
}

fn valid_validity(validity: &[u8]) -> bool {
    let mut times = validity;
    let Some((not_before_tag, not_before, _)) = take_any_der_value(&mut times) else {
        return false;
    };
    let Some((not_after_tag, not_after, _)) = take_any_der_value(&mut times) else {
        return false;
    };
    times.is_empty()
        && valid_time(not_before_tag, not_before)
        && valid_time(not_after_tag, not_after)
}

fn valid_subject_public_key_info(subject_public_key_info: &[u8]) -> bool {
    let mut fields = subject_public_key_info;
    let Some((algorithm, _)) = take_der_value(&mut fields, 0x30) else {
        return false;
    };
    let Some((subject_public_key, _)) = take_der_value(&mut fields, 0x03) else {
        return false;
    };
    fields.is_empty()
        && valid_algorithm_identifier(algorithm)
        && valid_der_bit_string(subject_public_key)
}

fn valid_extensions(explicit_extensions: &[u8]) -> bool {
    let mut explicit = explicit_extensions;
    let Some((extension_sequence, _)) = take_der_value(&mut explicit, 0x30) else {
        return false;
    };
    if !explicit.is_empty() || extension_sequence.is_empty() {
        return false;
    }
    let mut extensions = extension_sequence;
    while !extensions.is_empty() {
        let Some((extension, _)) = take_der_value(&mut extensions, 0x30) else {
            return false;
        };
        let mut fields = extension;
        let Some((oid, _)) = take_der_value(&mut fields, 0x06) else {
            return false;
        };
        if !valid_der_oid(oid) {
            return false;
        }
        if fields.first() == Some(&0x01) {
            let Some((critical, _)) = take_der_value(&mut fields, 0x01) else {
                return false;
            };
            if critical != [0xff] {
                return false;
            }
        }
        let Some((extension_value, _)) = take_der_value(&mut fields, 0x04) else {
            return false;
        };
        if extension_value.is_empty() || !fields.is_empty() {
            return false;
        }
    }
    true
}

fn tbs_signature_algorithm(tbs: &[u8]) -> Option<&[u8]> {
    let mut fields = tbs;
    let mut version = 0_u8;
    if fields.first() == Some(&0xa0) {
        let (explicit_version, _) = take_der_value(&mut fields, 0xa0)?;
        let mut version_fields = explicit_version;
        let (encoded_version, _) = take_der_value(&mut version_fields, 0x02)?;
        if !version_fields.is_empty() || encoded_version.len() != 1 || encoded_version[0] > 2 {
            return None;
        }
        version = encoded_version[0];
    }

    let (serial_number, _) = take_der_value(&mut fields, 0x02)?;
    if !valid_der_integer(serial_number, 20) {
        return None;
    }
    let (signature_algorithm, encoded_signature_algorithm) = take_der_value(&mut fields, 0x30)?;
    if !valid_algorithm_identifier(signature_algorithm) {
        return None;
    }
    let (issuer, _) = take_der_value(&mut fields, 0x30)?;
    if !valid_name(issuer) {
        return None;
    }
    let (validity, _) = take_der_value(&mut fields, 0x30)?;
    if !valid_validity(validity) {
        return None;
    }
    let (subject, _) = take_der_value(&mut fields, 0x30)?;
    if !valid_name(subject) {
        return None;
    }
    let (subject_public_key_info, _) = take_der_value(&mut fields, 0x30)?;
    if !valid_subject_public_key_info(subject_public_key_info) {
        return None;
    }

    for tag in [0x81, 0x82] {
        if fields.first() == Some(&tag) {
            if version == 0 {
                return None;
            }
            let (unique_id, _) = take_der_value(&mut fields, tag)?;
            if !valid_der_bit_string(unique_id) {
                return None;
            }
        }
    }
    if fields.first() == Some(&0xa3) {
        if version != 2 {
            return None;
        }
        let (extensions, _) = take_der_value(&mut fields, 0xa3)?;
        if !valid_extensions(extensions) {
            return None;
        }
    }
    if !fields.is_empty() {
        return None;
    }
    Some(encoded_signature_algorithm)
}

fn valid_der_certificate(anchor: &[u8]) -> bool {
    let Some((certificate, trailing)) = der_value(anchor, 0x30) else {
        return false;
    };
    if !trailing.is_empty() {
        return false;
    }
    let mut fields = certificate;
    let Some((tbs, _)) = take_der_value(&mut fields, 0x30) else {
        return false;
    };
    let Some((algorithm, encoded_algorithm)) = take_der_value(&mut fields, 0x30) else {
        return false;
    };
    let Some((signature, _)) = take_der_value(&mut fields, 0x03) else {
        return false;
    };
    fields.is_empty()
        && valid_algorithm_identifier(algorithm)
        && valid_der_bit_string(signature)
        && tbs_signature_algorithm(tbs) == Some(encoded_algorithm)
}
fn normalize_trust(trust: TlsTrust) -> Result<TlsTrust, TransportError> {
    match trust {
        TlsTrust::Platform => Ok(TlsTrust::Platform),
        TlsTrust::Custom(anchors) => {
            let bytes = anchors
                .iter()
                .try_fold(0_usize, |total, anchor| total.checked_add(anchor.len()))
                .ok_or(TransportError::InvalidTlsTrust)?;
            if anchors.is_empty()
                || anchors.len() > MAX_CUSTOM_TRUST_ANCHORS
                || bytes > MAX_CUSTOM_TRUST_BYTES
                || anchors.iter().any(|anchor| !valid_der_certificate(anchor))
            {
                return Err(TransportError::InvalidTlsTrust);
            }
            Ok(TlsTrust::Custom(
                anchors
                    .into_iter()
                    .map(|anchor| anchor.into_boxed_slice().into_vec())
                    .collect::<Vec<_>>()
                    .into_boxed_slice()
                    .into_vec(),
            ))
        }
    }
}

/// Provider capabilities that affect portable configuration admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderCapabilities {
    /// STUN over UDP is supported.
    pub stun_udp: bool,
    /// STUN over TCP is supported.
    pub stun_tcp: bool,
    /// TURN over UDP is supported.
    pub turn_udp: bool,
    /// TURN over TCP is supported.
    pub turn_tcp: bool,
    /// TURN over TLS is supported with platform trust.
    pub turn_tls: bool,
    /// Custom TURN TLS roots are supported.
    pub custom_tls_trust: bool,
    /// Explicit ICE restart is supported.
    pub ice_restart: bool,
    /// One reliable ordered binary data channel is supported.
    pub reliable_ordered_data_channel: bool,
    /// Stable bounded statistics are supported.
    pub stats: bool,
}

impl ProviderCapabilities {
    /// Capabilities implemented by the deterministic fake.
    pub const ALL: Self = Self {
        stun_udp: true,
        stun_tcp: true,
        turn_udp: true,
        turn_tcp: true,
        turn_tls: true,
        custom_tls_trust: true,
        ice_restart: true,
        reliable_ordered_data_channel: true,
        stats: true,
    };
}

/// Capabilities that must be native for peer construction to succeed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RequiredCapabilities {
    /// Require explicit ICE restart.
    pub ice_restart: bool,
    /// Require the reliable ordered binary data channel.
    pub reliable_ordered_data_channel: bool,
    /// Require stable statistics.
    pub stats: bool,
}

/// Identifies the seam's single reliable ordered data channel.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ChannelId(pub u16);

/// An owned, absolutely bounded binary data-channel message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BinaryPayload(Vec<u8>);

impl BinaryPayload {
    /// Constructs a binary payload and normalizes retained capacity to length.
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, TransportError> {
        let bytes = bytes.into();
        if bytes.len() > MAX_MESSAGE_BYTES {
            return Err(TransportError::MessageTooLarge);
        }
        Ok(Self(bytes.into_boxed_slice().into_vec()))
    }

    /// Returns the payload bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Returns the payload length.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.0.len()
    }

    /// Reports whether this is an empty binary message.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// A stable, bounded provider-neutral statistics snapshot.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StatsReport {
    /// Monotonic fake/provider sample sequence.
    pub sequence: u64,
    /// Binary messages accepted by the provider.
    pub messages_sent: u64,
    /// Binary bytes accepted by the provider.
    pub bytes_sent: u64,
    /// Binary messages surfaced to the caller.
    pub messages_received: u64,
    /// Binary bytes surfaced to the caller.
    pub bytes_received: u64,
    /// Bytes still buffered by the provider.
    pub buffered_send_bytes: u64,
    /// Messages still buffered by the provider.
    pub buffered_send_messages: u64,
}

/// Commands accepted by a [`PeerDriver`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Starts an offerer negotiation generation.
    CreateOffer {
        /// Correlation identifier.
        operation_id: OperationId,
        /// New negotiation generation.
        epoch: NegotiationEpoch,
    },
    /// Creates an answer for an installed remote offer.
    CreateAnswer {
        /// Correlation identifier.
        operation_id: OperationId,
        /// Active negotiation generation.
        epoch: NegotiationEpoch,
    },
    /// Installs an opaque local description.
    SetLocalDescription {
        /// Correlation identifier.
        operation_id: OperationId,
        /// Description to install.
        description: SessionDescription,
    },
    /// Installs an opaque remote description.
    SetRemoteDescription {
        /// Correlation identifier.
        operation_id: OperationId,
        /// Description to install.
        description: SessionDescription,
    },
    /// Adds one trickled remote candidate.
    AddRemoteCandidate {
        /// Correlation identifier.
        operation_id: OperationId,
        /// Candidate to install.
        candidate: IceCandidate,
    },
    /// Marks the remote trickle stream complete for one generation.
    EndRemoteCandidates {
        /// Correlation identifier.
        operation_id: OperationId,
        /// Bounded canonical empty-candidate marker.
        end: EndOfCandidates,
    },
    /// Explicitly begins a new ICE generation.
    RestartIce {
        /// Correlation identifier.
        operation_id: OperationId,
        /// New offerer generation, or the answerer's newly installed remote generation.
        epoch: NegotiationEpoch,
    },
    /// Opens the single reliable ordered binary data channel.
    OpenDataChannel {
        /// Correlation identifier.
        operation_id: OperationId,
        /// Stable channel identifier.
        channel_id: ChannelId,
    },
    /// Closes the data channel. Repeated closes complete idempotently.
    CloseDataChannel {
        /// Correlation identifier.
        operation_id: OperationId,
        /// Stable channel identifier.
        channel_id: ChannelId,
    },
    /// Atomically offers one binary message to the provider send buffer.
    Send {
        /// Correlation identifier.
        operation_id: OperationId,
        /// Open channel identifier.
        channel_id: ChannelId,
        /// Complete owned message; it is never partially admitted.
        payload: BinaryPayload,
    },
    /// Requests one stable bounded statistics snapshot.
    RequestStats {
        /// Correlation identifier.
        operation_id: OperationId,
    },
    /// Begins terminal peer shutdown.
    Shutdown {
        /// Correlation identifier.
        operation_id: OperationId,
    },
}

impl Command {
    /// Returns the command's correlation identifier.
    #[must_use]
    pub const fn operation_id(&self) -> OperationId {
        match self {
            Self::CreateOffer { operation_id, .. }
            | Self::CreateAnswer { operation_id, .. }
            | Self::SetLocalDescription { operation_id, .. }
            | Self::SetRemoteDescription { operation_id, .. }
            | Self::AddRemoteCandidate { operation_id, .. }
            | Self::EndRemoteCandidates { operation_id, .. }
            | Self::RestartIce { operation_id, .. }
            | Self::OpenDataChannel { operation_id, .. }
            | Self::CloseDataChannel { operation_id, .. }
            | Self::Send { operation_id, .. }
            | Self::RequestStats { operation_id }
            | Self::Shutdown { operation_id } => *operation_id,
        }
    }
}

/// Explicit portable peer lifecycle.
///
/// These states belong to the provider-neutral adapter seam. They do not add
/// fields or variants to the frozen V1 signalling wire format.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeerState {
    /// No negotiation has started.
    New,
    /// A description exchange is active.
    Negotiating,
    /// Connectivity checks or provider connection setup are in progress.
    Connecting,
    /// Matching local and remote descriptions are installed and transport is usable.
    Connected,
    /// A connected peer is creating and installing a fresh ICE generation.
    Restarting,
    /// Connectivity was lost, but the provider may recover without reconstruction.
    Disconnected,
    /// Shutdown was accepted and provider resources are being drained.
    Closing,
    /// Shutdown completed; no later event is legal.
    Closed,
    /// The provider failed terminally and cannot make further transport progress.
    ///
    /// Explicit shutdown is still required; teardown may subsequently report
    /// [`PeerState::Closing`] and [`PeerState::Closed`].
    Failed,
}

/// Stable provider-neutral failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportError {
    /// A queue capacity was zero or above [`MAX_QUEUE_CAPACITY`].
    InvalidCommandCapacity,
    /// Event capacity cannot hold one fake-provider operation batch.
    InvalidEventCapacity,
    /// The configured SDP cap was zero or above [`MAX_SDP_BYTES`].
    InvalidSdpCapacity,
    /// The configured candidate cap was zero or above [`MAX_CANDIDATE_BYTES`].
    InvalidCandidateCapacity,
    /// ICE server configuration is malformed or semantically inconsistent.
    InvalidIceServer,
    /// A TURN TLS server name is empty or invalid.
    InvalidTlsServerName,
    /// Custom trust is empty, malformed, or above [`MAX_CUSTOM_TRUST_BYTES`].
    InvalidTlsTrust,
    /// An ICE server or explicitly required feature is not supported.
    UnsupportedCapability,
    /// The configured message cap was zero or above [`MAX_MESSAGE_BYTES`].
    InvalidMessageCapacity,
    /// A send byte or message budget is zero or above its absolute bound.
    InvalidSendCapacity,
    /// An operation or shutdown timeout is zero or above [`MAX_TIMEOUT_MILLIS`].
    InvalidTimeout,
    /// The configured low-water mark is not below the byte budget.
    InvalidLowWaterMark,
    /// An SDP value exceeded its configured or absolute bound.
    SdpTooLarge,
    /// A candidate value exceeded its configured or absolute bound.
    CandidateTooLarge,
    /// A binary message exceeded its configured or absolute bound.
    MessageTooLarge,
    /// The bounded command queue has no capacity.
    QueueFull,
    /// Provider byte/message capacity cannot atomically hold the complete send.
    WouldBlock,
    /// A provider callback could not enter the bounded event queue.
    EventQueueOverflow,
    /// The ID was not greater than every previously accepted operation ID.
    DuplicateOperation,
    /// The reserved terminal operation ID was used for non-shutdown work.
    OperationIdExhausted,
    /// Input referred to an older or unknown negotiation generation.
    StaleEpoch,
    /// A description disagreed with the description already retained for its epoch.
    ConflictingDescription,
    /// The command is not valid for the role or current state.
    InvalidState,
    /// The native provider failed and cannot make further transport progress.
    ProviderFailure,
    /// An accepted operation exceeded its configured portable deadline.
    OperationTimeout,
    /// The provider did not finish teardown within its configured hard bound.
    ShutdownTimeout,
    /// Shutdown was already accepted or completed.
    Shutdown,
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidCommandCapacity => "invalid command queue capacity",
            Self::InvalidEventCapacity => "invalid event queue capacity",
            Self::InvalidSdpCapacity => "invalid SDP capacity",
            Self::InvalidCandidateCapacity => "invalid candidate capacity",
            Self::InvalidIceServer => "invalid ICE server configuration",
            Self::InvalidTlsServerName => "invalid TURN TLS server name",
            Self::InvalidTlsTrust => "invalid TURN TLS trust configuration",
            Self::UnsupportedCapability => "required transport capability is unsupported",
            Self::InvalidMessageCapacity => "invalid message capacity",
            Self::InvalidSendCapacity => "invalid send buffer capacity",
            Self::InvalidTimeout => "invalid operation or shutdown timeout",
            Self::InvalidLowWaterMark => "invalid send low-water mark",
            Self::SdpTooLarge => "SDP exceeds configured capacity",
            Self::CandidateTooLarge => "candidate exceeds configured capacity",
            Self::MessageTooLarge => "binary message exceeds configured capacity",
            Self::QueueFull => "command queue is full",
            Self::WouldBlock => "provider send buffer would block",
            Self::EventQueueOverflow => "provider event queue overflowed",
            Self::DuplicateOperation => "operation ID is duplicate or out of order",
            Self::OperationIdExhausted => "terminal operation ID is reserved for shutdown",
            Self::StaleEpoch => "negotiation epoch is stale or unknown",
            Self::ConflictingDescription => "description conflicts with the active epoch",
            Self::InvalidState => "command is invalid in the current state",
            Self::ProviderFailure => "native transport provider failed",
            Self::OperationTimeout => "native transport operation timed out",
            Self::ShutdownTimeout => "native transport shutdown timed out",
            Self::Shutdown => "peer shutdown was already accepted",
        })
    }
}

impl std::error::Error for TransportError {}

/// A command rejected before admission to a peer's bounded queue.
///
/// The exact submitted command is returned so the caller can retry or recover
/// its owned payload without cloning it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubmitError {
    error: TransportError,
    command: Command,
}

impl SubmitError {
    /// Rejects `command` before it is admitted to a peer queue.
    #[must_use]
    pub fn new(error: TransportError, command: Command) -> Self {
        Self { error, command }
    }

    /// Returns the stable rejection classification.
    #[must_use]
    pub const fn error(&self) -> TransportError {
        self.error
    }

    /// Returns the rejected command.
    #[must_use]
    pub const fn command(&self) -> &Command {
        &self.command
    }

    /// Splits this rejection into its classification and exact command.
    #[must_use]
    pub fn into_parts(self) -> (TransportError, Command) {
        (self.error, self.command)
    }
}

impl fmt::Display for SubmitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::error::Error for SubmitError {}

/// Events emitted by a [`PeerDriver`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    /// The accepted operation succeeded. Exactly one terminal operation event is emitted.
    OperationCompleted {
        /// Correlation identifier.
        operation_id: OperationId,
    },
    /// The accepted operation failed. Exactly one terminal operation event is emitted.
    OperationFailed {
        /// Correlation identifier.
        operation_id: OperationId,
        /// Stable failure classification.
        error: TransportError,
    },
    /// A provider-created local description is ready for signalling.
    LocalDescription {
        /// Opaque bounded description.
        description: SessionDescription,
    },
    /// A provider-created local candidate is ready for trickle signalling.
    LocalCandidate {
        /// Opaque bounded candidate.
        candidate: IceCandidate,
    },
    /// The provider completed local candidate gathering for a generation.
    LocalCandidatesEnded {
        /// Bounded canonical empty-candidate marker.
        end: EndOfCandidates,
    },
    /// The explicit portable lifecycle changed.
    StateChanged {
        /// New state.
        state: PeerState,
    },
    /// The reliable ordered data channel opened.
    DataChannelOpened {
        /// Stable channel identifier.
        channel_id: ChannelId,
    },
    /// The reliable ordered data channel closed.
    DataChannelClosed {
        /// Stable channel identifier.
        channel_id: ChannelId,
    },
    /// One complete bounded inbound binary message.
    Message {
        /// Open channel identifier.
        channel_id: ChannelId,
        /// Complete binary payload.
        payload: BinaryPayload,
    },
    /// Provider send space became retryable after crossing the low-water mark.
    SendCapacity {
        /// Open channel identifier.
        channel_id: ChannelId,
        /// Whole bytes currently available.
        available_bytes: usize,
        /// Whole message slots currently available.
        available_messages: usize,
    },
    /// Stable bounded statistics requested by an operation.
    Stats {
        /// Correlates this snapshot with [`Command::RequestStats`].
        operation_id: OperationId,
        /// Provider-neutral snapshot.
        report: StatsReport,
    },
    /// Reports an uncorrelated terminal provider failure with a stable classification.
    ///
    /// The provider must first report [`PeerState::Failed`] through
    /// [`Event::StateChanged`]. This event is terminal for transport progress:
    /// after it, only [`Command::Shutdown`] may be accepted. It is not the
    /// terminal event-stream marker. Explicit shutdown is still required, its
    /// teardown events may follow, and [`Event::ShutdownComplete`] remains the
    /// only event after which polling returns permanent `Ready(None)`.
    FatalError {
        /// Stable provider-neutral failure classification.
        error: TransportError,
    },
    /// Terminal marker. No event may follow this marker.
    ShutdownComplete,
}

/// Mandatory peer-certificate verification policy.
///
/// WebRTC peers authenticate the certificate fingerprint carried by the
/// installed signalling descriptions. The portable seam deliberately exposes
/// no disabled or trust-any-certificate alternative.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeerCertificatePolicy {
    /// Require the provider to verify the negotiated signalling fingerprint.
    VerifySignallingFingerprint,
}

/// Unvalidated portable construction parameters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeerConfig {
    /// Native peer role.
    pub role: Role,
    /// Maximum number of accepted commands waiting to be polled.
    pub command_capacity: usize,
    /// Maximum number of provider events buffered by an implementation.
    pub event_capacity: usize,
    /// Per-description SDP byte cap.
    pub max_sdp_bytes: usize,
    /// Per-candidate or end-marker aggregate text byte cap.
    pub max_candidate_bytes: usize,
    /// Per inbound or outbound binary-message byte cap.
    pub max_message_bytes: usize,
    /// Aggregate provider-owned outbound byte budget.
    pub send_buffer_bytes: usize,
    /// Aggregate provider-owned outbound message budget.
    pub send_buffer_messages: usize,
    /// A drain crossing from above this value to at-or-below it emits capacity.
    pub send_low_water_bytes: usize,
    /// Hard deadline for every accepted non-shutdown operation.
    pub operation_timeout_ms: u64,
    /// Hard deadline for provider teardown after shutdown is accepted.
    pub shutdown_timeout_ms: u64,
    /// Mandatory provider-neutral peer-certificate verification policy.
    pub peer_certificate_policy: PeerCertificatePolicy,
    /// Provider-neutral STUN and TURN endpoints.
    pub ice_servers: Vec<IceServer>,
    /// Features which the selected provider must implement natively.
    pub required_capabilities: RequiredCapabilities,
    /// Offer a sendonly Opus audio track instead of a data channel.
    pub sendonly_opus: bool,
}

impl PeerConfig {
    /// Conservative offerer defaults.
    #[must_use]
    pub fn offerer() -> Self {
        Self::for_role(Role::Offerer)
    }

    /// Conservative answerer defaults.
    #[must_use]
    pub fn answerer() -> Self {
        Self::for_role(Role::Answerer)
    }

    fn for_role(role: Role) -> Self {
        Self {
            role,
            command_capacity: 32,
            event_capacity: 32,
            max_sdp_bytes: 16 * 1_024,
            max_candidate_bytes: 2 * 1_024,
            max_message_bytes: 64 * 1_024,
            send_buffer_bytes: 256 * 1_024,
            send_buffer_messages: 64,
            send_low_water_bytes: 64 * 1_024,
            operation_timeout_ms: 30_000,
            shutdown_timeout_ms: 5_000,
            peer_certificate_policy: PeerCertificatePolicy::VerifySignallingFingerprint,
            ice_servers: Vec::new(),
            required_capabilities: RequiredCapabilities::default(),
            sendonly_opus: false,
        }
    }

    /// Validates every capacity and security invariant before construction.
    pub fn validate(&self) -> Result<ValidatedPeerConfig, TransportError> {
        self.validate_for(ProviderCapabilities::ALL)
    }

    /// Validates against one provider's stable capability report.
    pub fn validate_for(
        &self,
        capabilities: ProviderCapabilities,
    ) -> Result<ValidatedPeerConfig, TransportError> {
        if self.command_capacity == 0 || self.command_capacity > MAX_QUEUE_CAPACITY {
            return Err(TransportError::InvalidCommandCapacity);
        }
        if self.event_capacity < MIN_EVENT_CAPACITY || self.event_capacity > MAX_QUEUE_CAPACITY {
            return Err(TransportError::InvalidEventCapacity);
        }
        if self.max_sdp_bytes == 0 || self.max_sdp_bytes > MAX_SDP_BYTES {
            return Err(TransportError::InvalidSdpCapacity);
        }
        if self.max_candidate_bytes == 0 || self.max_candidate_bytes > MAX_CANDIDATE_BYTES {
            return Err(TransportError::InvalidCandidateCapacity);
        }
        if self.max_message_bytes == 0 || self.max_message_bytes > MAX_MESSAGE_BYTES {
            return Err(TransportError::InvalidMessageCapacity);
        }
        if self.send_buffer_bytes == 0
            || self.send_buffer_bytes > MAX_SEND_BUFFER_BYTES
            || self.send_buffer_messages == 0
            || self.send_buffer_messages > MAX_SEND_BUFFER_MESSAGES
            || self.max_message_bytes > self.send_buffer_bytes
        {
            return Err(TransportError::InvalidSendCapacity);
        }
        if self.send_low_water_bytes >= self.send_buffer_bytes
            || self.send_low_water_bytes
                > self
                    .send_buffer_bytes
                    .saturating_sub(self.max_message_bytes)
        {
            return Err(TransportError::InvalidLowWaterMark);
        }
        if self.operation_timeout_ms == 0
            || self.operation_timeout_ms > MAX_TIMEOUT_MILLIS
            || self.shutdown_timeout_ms == 0
            || self.shutdown_timeout_ms > MAX_TIMEOUT_MILLIS
        {
            return Err(TransportError::InvalidTimeout);
        }
        if self.ice_servers.len() > MAX_ICE_SERVERS {
            return Err(TransportError::InvalidIceServer);
        }
        if (self.required_capabilities.ice_restart && !capabilities.ice_restart)
            || (self.required_capabilities.reliable_ordered_data_channel
                && !capabilities.reliable_ordered_data_channel)
            || (self.required_capabilities.stats && !capabilities.stats)
        {
            return Err(TransportError::UnsupportedCapability);
        }

        let servers = self
            .ice_servers
            .iter()
            .map(|server| validate_ice_server(server, capabilities))
            .collect::<Result<Vec<_>, _>>()?;
        for (index, server) in servers.iter().enumerate() {
            if servers[..index]
                .iter()
                .any(|earlier| same_ice_endpoint(earlier, server))
            {
                return Err(TransportError::InvalidIceServer);
            }
        }
        let mut normalized = self.clone();
        normalized.ice_servers = servers.into_boxed_slice().into_vec();
        Ok(ValidatedPeerConfig(normalized))
    }
}

fn same_ice_endpoint(left: &IceServer, right: &IceServer) -> bool {
    match (left, right) {
        (
            IceServer::Stun {
                host: left_host,
                port: left_port,
                transport: left_transport,
            },
            IceServer::Stun {
                host: right_host,
                port: right_port,
                transport: right_transport,
            },
        )
        | (
            IceServer::Turn {
                host: left_host,
                port: left_port,
                transport: left_transport,
                ..
            },
            IceServer::Turn {
                host: right_host,
                port: right_port,
                transport: right_transport,
                ..
            },
        ) => {
            left_host.eq_ignore_ascii_case(right_host)
                && left_port == right_port
                && left_transport == right_transport
        }
        _ => false,
    }
}

fn validate_ice_server(
    server: &IceServer,
    capabilities: ProviderCapabilities,
) -> Result<IceServer, TransportError> {
    match server {
        IceServer::Stun {
            host,
            port,
            transport,
        } => {
            let supported = match transport {
                IceTransport::Udp => capabilities.stun_udp,
                IceTransport::Tcp => capabilities.stun_tcp,
                IceTransport::Tls => false,
            };
            if !supported {
                return Err(TransportError::UnsupportedCapability);
            }
            IceServer::stun(host.clone(), *port, *transport)
        }
        IceServer::Turn {
            host,
            port,
            transport,
            credentials,
            tls,
        } => {
            let supported = match transport {
                IceTransport::Udp => capabilities.turn_udp,
                IceTransport::Tcp => capabilities.turn_tcp,
                IceTransport::Tls => capabilities.turn_tls,
            };
            if !supported {
                return Err(TransportError::UnsupportedCapability);
            }
            if tls.as_ref().is_some_and(|tls| {
                matches!(tls.trust(), TlsTrust::Custom(_)) && !capabilities.custom_tls_trust
            }) {
                return Err(TransportError::UnsupportedCapability);
            }
            IceServer::turn(
                host.clone(),
                *port,
                *transport,
                credentials.clone(),
                tls.clone(),
            )
        }
    }
}

/// A configuration that is safe for provider allocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedPeerConfig(PeerConfig);

impl ValidatedPeerConfig {
    /// Returns a clone of the validated portable values.
    #[must_use]
    pub fn get(&self) -> PeerConfig {
        self.0.clone()
    }
}

/// Object-safe provider factory.
///
/// No runtime handle, provider value, or network object crosses this boundary.
pub trait NativeTransportProvider: Send + Sync + 'static {
    /// Returns stable provider capabilities before peer construction.
    fn capabilities(&self) -> ProviderCapabilities;

    /// Constructs a single-owner peer from validated portable configuration.
    fn create_peer(
        &self,
        config: ValidatedPeerConfig,
    ) -> Result<Box<dyn PeerDriver>, TransportError>;
}

/// Object-safe, runtime-neutral, single-owner command/event driver.
///
/// A caller must not invoke one driver concurrently. Implementations arrange
/// that a successful [`PeerDriver::submit`] eventually produces exactly one
/// [`Event::OperationCompleted`] or [`Event::OperationFailed`].
pub trait PeerDriver: Send + 'static {
    /// Transfers a complete command into the bounded queue, or returns its ownership.
    fn submit(&mut self, command: Command) -> Result<(), SubmitError>;

    /// Polls one ordered event.
    ///
    /// On `Poll::Pending`, the waker from the latest supplied [`Context`] is
    /// registered as the sole notification target, replacing any previously
    /// registered waker. The implementation must wake it whenever progress may
    /// exist (an event or the terminal result may be ready). A wake is only a
    /// prompt: the caller must re-poll to observe progress. Callers must wait
    /// for that notification rather than busy-polling.
    ///
    /// `Poll::Ready(None)` is permanent and occurs only after
    /// [`Event::ShutdownComplete`] has been returned.
    fn poll_event(&mut self, context: &mut Context<'_>) -> Poll<Option<Event>>;
}

/// Deterministic offline provider used to verify the public contract.
#[derive(Clone, Copy, Debug, Default)]
pub struct FakeNativeTransportProvider;

impl NativeTransportProvider for FakeNativeTransportProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::ALL
    }

    fn create_peer(
        &self,
        config: ValidatedPeerConfig,
    ) -> Result<Box<dyn PeerDriver>, TransportError> {
        Ok(Box::new(FakePeer::new(config)))
    }
}

impl FakeNativeTransportProvider {
    /// Constructs an inspectable fake peer for deterministic fault injection.
    pub fn create_fake_peer(
        &self,
        config: ValidatedPeerConfig,
    ) -> Result<FakePeer, TransportError> {
        Ok(FakePeer::new(config))
    }
}

/// Deterministic fake provider with caller-selected capability gaps.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapabilityLimitedFakeProvider {
    capabilities: ProviderCapabilities,
}

impl CapabilityLimitedFakeProvider {
    /// Constructs a fake provider that advertises exactly `capabilities`.
    #[must_use]
    pub const fn new(capabilities: ProviderCapabilities) -> Self {
        Self { capabilities }
    }
}

impl NativeTransportProvider for CapabilityLimitedFakeProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        self.capabilities
    }

    fn create_peer(
        &self,
        config: ValidatedPeerConfig,
    ) -> Result<Box<dyn PeerDriver>, TransportError> {
        let validated = config.get().validate_for(self.capabilities)?;
        Ok(Box::new(FakePeer::new(validated)))
    }
}

/// Short alias for [`FakeNativeTransportProvider`].
pub type FakeProvider = FakeNativeTransportProvider;

/// Read-only observation that a deterministic fake peer was dropped.
#[derive(Clone, Debug)]
pub struct FakeDropProbe(Arc<AtomicBool>);

impl FakeDropProbe {
    /// Reports whether the associated fake peer's destructor ran.
    #[must_use]
    pub fn was_dropped(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Debug)]
struct QueuedCommand {
    command: Command,
    deadline_ms: u64,
    stalled: bool,
}

#[derive(Clone, Copy, Debug)]
struct PendingFatal {
    error: TransportError,
    observed_at_ms: u64,
}

/// Read-only observation that fake provider-owned resources were torn down.
#[derive(Clone, Debug)]
pub struct FakeTeardownProbe(Arc<AtomicBool>);

impl FakeTeardownProbe {
    /// Reports whether provider-owned resources have been forcefully or orderly released.
    #[must_use]
    pub fn was_torn_down(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Inspectable deterministic peer used by black-box contract tests.
#[derive(Debug)]
pub struct FakePeer {
    config: PeerConfig,
    commands: VecDeque<QueuedCommand>,
    events: VecDeque<Event>,
    provider_sends: VecDeque<(ChannelId, BinaryPayload)>,
    highest_operation: Option<OperationId>,
    epoch: Option<NegotiationEpoch>,
    local_description: Option<SessionDescription>,
    remote_description: Option<SessionDescription>,
    channel: Option<ChannelId>,
    channel_closed: bool,
    reserved_send_bytes: usize,
    reserved_send_messages: usize,
    messages_sent: u64,
    bytes_sent: u64,
    messages_received: u64,
    bytes_received: u64,
    stats_sequence: u64,
    state: PeerState,
    shutdown_requested: bool,
    shutdown_complete: bool,
    shutdown_timeout: bool,
    stall_next_operation: bool,
    now_ms: u64,
    pending_fatal: Option<PendingFatal>,
    provider_failed: bool,
    provider_torn_down: Arc<AtomicBool>,
    drop_probe: Arc<AtomicBool>,
    waker: Option<std::task::Waker>,
}

impl FakePeer {
    fn new(config: ValidatedPeerConfig) -> Self {
        let config = config.get();
        Self {
            commands: VecDeque::with_capacity(config.command_capacity),
            events: VecDeque::with_capacity(config.event_capacity),
            provider_sends: VecDeque::with_capacity(config.send_buffer_messages),
            highest_operation: None,
            epoch: None,
            local_description: None,
            remote_description: None,
            channel: None,
            channel_closed: false,
            reserved_send_bytes: 0,
            reserved_send_messages: 0,
            messages_sent: 0,
            bytes_sent: 0,
            messages_received: 0,
            bytes_received: 0,
            stats_sequence: 0,
            state: PeerState::New,
            shutdown_requested: false,
            shutdown_complete: false,
            shutdown_timeout: false,
            stall_next_operation: false,
            now_ms: 0,
            pending_fatal: None,
            provider_failed: false,
            provider_torn_down: Arc::new(AtomicBool::new(false)),
            drop_probe: Arc::new(AtomicBool::new(false)),
            waker: None,
            config,
        }
    }

    /// Returns a probe which becomes true when this peer is dropped.
    #[must_use]
    pub fn drop_probe(&self) -> FakeDropProbe {
        FakeDropProbe(Arc::clone(&self.drop_probe))
    }

    /// Returns a probe for provider-owned resource teardown.
    #[must_use]
    pub fn teardown_probe(&self) -> FakeTeardownProbe {
        FakeTeardownProbe(Arc::clone(&self.provider_torn_down))
    }

    /// Advances the deterministic monotonic fake clock.
    pub fn advance_time(&mut self, millis: u64) {
        self.now_ms = self.now_ms.saturating_add(millis);
        self.wake();
    }

    /// Makes the next accepted non-shutdown operation wait for its hard deadline.
    pub fn inject_operation_stall(&mut self) -> Result<(), TransportError> {
        if self.shutdown_requested || self.shutdown_complete || self.provider_failed {
            return Err(TransportError::Shutdown);
        }
        self.stall_next_operation = true;
        Ok(())
    }

    /// Injects one provider callback event through the real bounded event path.
    ///
    /// Saturation returns a stable error and schedules a fatal overflow after
    /// already-buffered events and accepted operation terminals are drained.
    pub fn inject_provider_event(&mut self, event: Event) -> Result<(), TransportError> {
        if self.shutdown_requested || self.shutdown_complete {
            return Err(TransportError::Shutdown);
        }
        if self.pending_fatal.is_some() || self.provider_failed {
            return Err(TransportError::ProviderFailure);
        }
        if let Event::FatalError { error } = &event {
            return self.inject_fatal(*error);
        }
        if !matches!(&event, Event::Message { .. } | Event::SendCapacity { .. }) {
            return Err(TransportError::InvalidState);
        }
        if let Some(error) = self.injected_event_error(&event) {
            return Err(error);
        }
        if self.events.len() >= self.config.event_capacity {
            self.schedule_fatal(TransportError::EventQueueOverflow);
            return Err(TransportError::EventQueueOverflow);
        }
        let message_bytes = match &event {
            Event::Message { payload, .. } => Some(payload.len()),
            _ => None,
        };
        self.events.push_back(event);
        if let Some(bytes) = message_bytes {
            self.messages_received = self.messages_received.saturating_add(1);
            self.bytes_received = self
                .bytes_received
                .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
        }
        self.wake();
        Ok(())
    }

    fn injected_event_error(&self, event: &Event) -> Option<TransportError> {
        match event {
            Event::LocalDescription { description }
                if description.sdp().len() > self.config.max_sdp_bytes =>
            {
                Some(TransportError::SdpTooLarge)
            }
            Event::LocalCandidate { candidate }
                if candidate.text_bytes() > self.config.max_candidate_bytes =>
            {
                Some(TransportError::CandidateTooLarge)
            }
            Event::LocalCandidatesEnded { end }
                if end.text_bytes() > self.config.max_candidate_bytes =>
            {
                Some(TransportError::CandidateTooLarge)
            }
            Event::Message {
                channel_id: _,
                payload,
            } if payload.len() > self.config.max_message_bytes => {
                Some(TransportError::MessageTooLarge)
            }
            Event::Message { channel_id, .. }
                if self.channel != Some(*channel_id)
                    || self.channel_closed
                    || self.state != PeerState::Connected =>
            {
                Some(TransportError::InvalidState)
            }
            Event::SendCapacity {
                channel_id,
                available_bytes,
                available_messages,
            } if self.channel != Some(*channel_id)
                || self.channel_closed
                || self.state != PeerState::Connected
                || *available_bytes
                    != self
                        .config
                        .send_buffer_bytes
                        .saturating_sub(self.reserved_send_bytes)
                || *available_messages
                    != self
                        .config
                        .send_buffer_messages
                        .saturating_sub(self.reserved_send_messages) =>
            {
                Some(TransportError::InvalidSendCapacity)
            }
            Event::Stats { report, .. }
                if report.buffered_send_bytes
                    > u64::try_from(self.config.send_buffer_bytes).unwrap_or(u64::MAX)
                    || report.buffered_send_messages
                        > u64::try_from(self.config.send_buffer_messages).unwrap_or(u64::MAX) =>
            {
                Some(TransportError::InvalidSendCapacity)
            }
            _ => None,
        }
    }

    /// Injects an uncorrelated fatal provider error.
    pub fn inject_fatal(&mut self, error: TransportError) -> Result<(), TransportError> {
        if self.shutdown_requested || self.shutdown_complete {
            return Err(TransportError::Shutdown);
        }
        if self.pending_fatal.is_some() || self.provider_failed {
            return Err(TransportError::ProviderFailure);
        }
        self.schedule_fatal(error);
        Ok(())
    }

    /// Injects deterministic loss of the provider endpoint.
    pub fn inject_provider_drop(&mut self) -> Result<(), TransportError> {
        self.inject_fatal(TransportError::ProviderFailure)
    }

    /// Injects recoverable connectivity loss through the validated transition path.
    pub fn inject_disconnect(&mut self) -> Result<(), TransportError> {
        if self.pending_fatal.is_some() || self.provider_failed {
            return Err(TransportError::ProviderFailure);
        }
        if self.shutdown_requested || self.shutdown_complete {
            return Err(TransportError::Shutdown);
        }
        if self.state != PeerState::Connected {
            return Err(TransportError::InvalidState);
        }
        if self.events.len() >= self.config.event_capacity {
            self.schedule_fatal(TransportError::EventQueueOverflow);
            return Err(TransportError::EventQueueOverflow);
        }
        self.transition(PeerState::Disconnected);
        self.wake();
        Ok(())
    }

    /// Injects provider recovery after [`PeerState::Disconnected`].
    pub fn inject_recovery(&mut self) -> Result<(), TransportError> {
        if self.pending_fatal.is_some() || self.provider_failed {
            return Err(TransportError::ProviderFailure);
        }
        if self.shutdown_requested || self.shutdown_complete {
            return Err(TransportError::Shutdown);
        }
        if self.state != PeerState::Disconnected {
            return Err(TransportError::InvalidState);
        }
        if self.events.len().saturating_add(2) > self.config.event_capacity {
            self.schedule_fatal(TransportError::EventQueueOverflow);
            return Err(TransportError::EventQueueOverflow);
        }
        self.transition(PeerState::Connecting);
        self.transition(PeerState::Connected);
        self.wake();
        Ok(())
    }

    /// Makes the next orderly shutdown take the hard-timeout teardown path.
    pub fn inject_shutdown_timeout(&mut self) -> Result<(), TransportError> {
        if self.shutdown_requested || self.shutdown_complete {
            return Err(TransportError::Shutdown);
        }
        self.shutdown_timeout = true;
        Ok(())
    }

    /// Injects one complete inbound provider message.
    pub fn inject_message(
        &mut self,
        channel_id: ChannelId,
        payload: BinaryPayload,
    ) -> Result<(), TransportError> {
        if payload.len() > self.config.max_message_bytes {
            return Err(TransportError::MessageTooLarge);
        }
        if self.channel != Some(channel_id) || self.channel_closed {
            return Err(TransportError::InvalidState);
        }
        self.inject_provider_event(Event::Message {
            channel_id,
            payload,
        })
    }

    /// Drains the oldest complete provider-owned send and emits capacity on a
    /// deterministic low-water or message-slot-unblocked edge.
    pub fn drain_provider_send(&mut self) -> Option<(ChannelId, BinaryPayload)> {
        let before_bytes = self.reserved_send_bytes;
        let before_messages = self.reserved_send_messages;
        let send = self.provider_sends.pop_front()?;
        self.reserved_send_bytes = self.reserved_send_bytes.saturating_sub(send.1.len());
        self.reserved_send_messages = self.reserved_send_messages.saturating_sub(1);
        let crossed_low = before_bytes > self.config.send_low_water_bytes
            && self.reserved_send_bytes <= self.config.send_low_water_bytes;
        let opened_message_slot = before_messages == self.config.send_buffer_messages;
        if (crossed_low || opened_message_slot)
            && !self.shutdown_requested
            && !self.shutdown_complete
            && self.channel == Some(send.0)
            && !self.channel_closed
        {
            let event = Event::SendCapacity {
                channel_id: send.0,
                available_bytes: self
                    .config
                    .send_buffer_bytes
                    .saturating_sub(self.reserved_send_bytes),
                available_messages: self
                    .config
                    .send_buffer_messages
                    .saturating_sub(self.reserved_send_messages),
            };
            let _ = self.inject_provider_event(event);
        }
        Some(send)
    }

    /// Returns currently provider-buffered byte and message counts.
    #[must_use]
    pub const fn buffered_send(&self) -> (usize, usize) {
        (self.reserved_send_bytes, self.reserved_send_messages)
    }

    fn schedule_fatal(&mut self, error: TransportError) {
        if self.pending_fatal.is_none() && !self.provider_failed && !self.shutdown_complete {
            self.pending_fatal = Some(PendingFatal {
                error,
                observed_at_ms: self.now_ms,
            });
            self.wake();
        }
    }

    fn wake(&mut self) {
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }

    fn configured_size_error(&self, command: &Command) -> Option<TransportError> {
        match command {
            Command::SetLocalDescription { description, .. }
            | Command::SetRemoteDescription { description, .. }
                if description.sdp.len() > self.config.max_sdp_bytes =>
            {
                Some(TransportError::SdpTooLarge)
            }
            Command::AddRemoteCandidate { candidate, .. }
                if candidate.text_bytes() > self.config.max_candidate_bytes =>
            {
                Some(TransportError::CandidateTooLarge)
            }
            Command::EndRemoteCandidates { end, .. }
                if end.text_bytes() > self.config.max_candidate_bytes =>
            {
                Some(TransportError::CandidateTooLarge)
            }
            Command::Send { payload, .. } if payload.len() > self.config.max_message_bytes => {
                Some(TransportError::MessageTooLarge)
            }
            _ => None,
        }
    }

    fn emit(&mut self, event: Event) {
        debug_assert!(self.events.len() < self.config.event_capacity);
        self.events.push_back(event);
    }

    fn transition(&mut self, state: PeerState) {
        if self.state != state {
            self.state = state;
            self.emit(Event::StateChanged { state });
        }
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

    fn process(&mut self, command: Command) {
        match command {
            Command::CreateOffer {
                operation_id,
                epoch,
            } => self.create_offer(operation_id, epoch),
            Command::CreateAnswer {
                operation_id,
                epoch,
            } => self.create_answer(operation_id, epoch),
            Command::SetLocalDescription {
                operation_id,
                description,
            } => self.set_description(operation_id, description, true),
            Command::SetRemoteDescription {
                operation_id,
                description,
            } => self.set_description(operation_id, description, false),
            Command::AddRemoteCandidate {
                operation_id,
                candidate,
            } => self.add_remote_candidate(operation_id, &candidate),
            Command::EndRemoteCandidates { operation_id, end } => {
                self.end_remote_candidates(operation_id, &end);
            }
            Command::RestartIce {
                operation_id,
                epoch,
            } => self.restart_ice(operation_id, epoch),
            Command::OpenDataChannel {
                operation_id,
                channel_id,
            } => self.open_data_channel(operation_id, channel_id),
            Command::CloseDataChannel {
                operation_id,
                channel_id,
            } => self.close_data_channel(operation_id, channel_id),
            Command::Send {
                operation_id,
                channel_id,
                payload,
            } => self.send(operation_id, channel_id, payload),
            Command::RequestStats { operation_id } => self.request_stats(operation_id),
            Command::Shutdown { operation_id } => self.shutdown(operation_id),
        }
    }

    fn create_offer(&mut self, operation_id: OperationId, epoch: NegotiationEpoch) {
        if self.config.role != Role::Offerer {
            self.fail(operation_id, TransportError::InvalidState);
            return;
        }
        if self.epoch.is_some_and(|active| epoch <= active) {
            self.fail(operation_id, TransportError::StaleEpoch);
            return;
        }
        if self.epoch.is_some()
            && matches!(self.state, PeerState::Connected | PeerState::Disconnected)
        {
            self.transition(PeerState::Restarting);
        }
        self.create_local_description(operation_id, epoch, DescriptionKind::Offer);
    }

    fn create_answer(&mut self, operation_id: OperationId, epoch: NegotiationEpoch) {
        if self.config.role != Role::Answerer
            || !self.remote_description.as_ref().is_some_and(|description| {
                description.epoch == epoch && description.kind == DescriptionKind::Offer
            })
        {
            let error = if self.epoch.is_some_and(|active| epoch < active) {
                TransportError::StaleEpoch
            } else {
                TransportError::InvalidState
            };
            self.fail(operation_id, error);
            return;
        }
        self.create_local_description(operation_id, epoch, DescriptionKind::Answer);
    }

    fn create_local_description(
        &mut self,
        operation_id: OperationId,
        epoch: NegotiationEpoch,
        kind: DescriptionKind,
    ) {
        let restarting = self.state == PeerState::Restarting;
        let username_fragment = if restarting {
            format!("native-restart-v{}", epoch.0)
        } else {
            "native-base-v1".to_owned()
        };
        let sdp_text = if restarting {
            let role_text = match kind {
                DescriptionKind::Offer => "offer",
                DescriptionKind::Answer => "answer",
            };
            let setup = match kind {
                DescriptionKind::Offer => "actpass",
                DescriptionKind::Answer => "active",
            };
            format!(
                "v=0\r\no=- 30000 {} IN IP4 127.0.0.1\r\ns=RELAY native {role_text} restart\r\nt=0 0\r\na=ice-options:trickle\r\na=ice-ufrag:{username_fragment}\r\na=setup:{setup}\r\n",
                epoch.0,
            )
        } else {
            match kind {
                DescriptionKind::Offer => FAKE_OFFER_SDP.to_owned(),
                DescriptionKind::Answer => FAKE_ANSWER_SDP.to_owned(),
            }
        };
        if sdp_text.len() > self.config.max_sdp_bytes {
            self.fail(operation_id, TransportError::SdpTooLarge);
            return;
        }
        let Ok(description) = SessionDescription::new(epoch, kind, sdp_text) else {
            self.fail(operation_id, TransportError::SdpTooLarge);
            return;
        };
        let candidate_text = if restarting {
            format!(
                "candidate:1 1 UDP 2122260223 198.51.100.20 50002 typ host generation {} ufrag {username_fragment}",
                epoch.0,
            )
        } else {
            FAKE_CANDIDATE.to_owned()
        };
        let Ok(candidate) = IceCandidate::new(
            epoch,
            candidate_text,
            Some("data".to_owned()),
            Some(0),
            Some(username_fragment.clone()),
        ) else {
            self.fail(operation_id, TransportError::CandidateTooLarge);
            return;
        };
        if candidate.text_bytes() > self.config.max_candidate_bytes {
            self.fail(operation_id, TransportError::CandidateTooLarge);
            return;
        }

        if kind == DescriptionKind::Offer {
            self.epoch = Some(epoch);
            self.local_description = None;
            self.remote_description = None;
        }
        if !restarting {
            self.transition(PeerState::Negotiating);
        }
        self.emit(Event::LocalDescription { description });
        self.emit(Event::LocalCandidate { candidate });
        let Ok(end) = EndOfCandidates::new(
            epoch,
            Some("data".to_owned()),
            Some(0),
            Some(username_fragment),
        ) else {
            self.fail(operation_id, TransportError::CandidateTooLarge);
            return;
        };
        if end.text_bytes() > self.config.max_candidate_bytes {
            self.fail(operation_id, TransportError::CandidateTooLarge);
            return;
        }
        self.emit(Event::LocalCandidatesEnded { end });
        self.complete(operation_id);
    }

    fn set_description(
        &mut self,
        operation_id: OperationId,
        description: SessionDescription,
        local: bool,
    ) {
        if description.sdp.len() > self.config.max_sdp_bytes {
            self.fail(operation_id, TransportError::SdpTooLarge);
            return;
        }
        let expected_kind = match (self.config.role, local) {
            (Role::Offerer, true) | (Role::Answerer, false) => DescriptionKind::Offer,
            (Role::Offerer, false) | (Role::Answerer, true) => DescriptionKind::Answer,
        };
        if description.kind != expected_kind {
            self.fail(operation_id, TransportError::InvalidState);
            return;
        }

        let epoch = description.epoch;
        let starts_answerer_epoch = !local && self.config.role == Role::Answerer;
        if starts_answerer_epoch {
            if self.epoch.is_some_and(|active| epoch < active) {
                self.fail(operation_id, TransportError::StaleEpoch);
                return;
            }
            if self.epoch != Some(epoch) {
                let is_restart = matches!(
                    self.state,
                    PeerState::Connected | PeerState::Disconnected | PeerState::Restarting
                );
                self.epoch = Some(epoch);
                self.local_description = None;
                self.remote_description = None;
                self.transition(if is_restart {
                    PeerState::Restarting
                } else {
                    PeerState::Negotiating
                });
            }
        } else if self.epoch != Some(epoch) {
            self.fail(operation_id, TransportError::StaleEpoch);
            return;
        }

        let retained = if local {
            &mut self.local_description
        } else {
            &mut self.remote_description
        };
        if let Some(existing) = retained {
            if existing == &description {
                self.complete(operation_id);
            } else {
                self.fail(operation_id, TransportError::ConflictingDescription);
            }
            return;
        }
        *retained = Some(description);
        self.maybe_connect();
        self.complete(operation_id);
    }

    fn maybe_connect(&mut self) {
        let Some(epoch) = self.epoch else {
            return;
        };
        let has_description = |description: &Option<SessionDescription>, kind| {
            description
                .as_ref()
                .is_some_and(|value| value.epoch == epoch && value.kind == kind)
        };
        let complete = match self.config.role {
            Role::Offerer => {
                has_description(&self.local_description, DescriptionKind::Offer)
                    && has_description(&self.remote_description, DescriptionKind::Answer)
            }
            Role::Answerer => {
                has_description(&self.remote_description, DescriptionKind::Offer)
                    && has_description(&self.local_description, DescriptionKind::Answer)
            }
        };
        if complete {
            self.transition(PeerState::Connecting);
            self.transition(PeerState::Connected);
        }
    }

    fn add_remote_candidate(&mut self, operation_id: OperationId, candidate: &IceCandidate) {
        if candidate.text_bytes() > self.config.max_candidate_bytes {
            self.fail(operation_id, TransportError::CandidateTooLarge);
        } else if !self.has_active_remote_description(candidate.epoch) {
            self.fail(operation_id, TransportError::StaleEpoch);
        } else {
            self.complete(operation_id);
        }
    }

    fn end_remote_candidates(&mut self, operation_id: OperationId, end: &EndOfCandidates) {
        if end.text_bytes() > self.config.max_candidate_bytes {
            self.fail(operation_id, TransportError::CandidateTooLarge);
        } else if !self.has_active_remote_description(end.epoch) {
            self.fail(operation_id, TransportError::StaleEpoch);
        } else {
            self.complete(operation_id);
        }
    }

    fn has_active_remote_description(&self, epoch: NegotiationEpoch) -> bool {
        self.epoch == Some(epoch)
            && self
                .remote_description
                .as_ref()
                .is_some_and(|description| description.epoch == epoch)
    }

    fn restart_ice(&mut self, operation_id: OperationId, epoch: NegotiationEpoch) {
        match self.config.role {
            Role::Offerer => {
                if self.epoch.is_some_and(|active| epoch <= active) {
                    self.fail(operation_id, TransportError::StaleEpoch);
                    return;
                }
                if self.epoch.is_none()
                    || !matches!(self.state, PeerState::Connected | PeerState::Disconnected)
                {
                    self.fail(operation_id, TransportError::InvalidState);
                    return;
                }
                self.transition(PeerState::Restarting);
                self.create_local_description(operation_id, epoch, DescriptionKind::Offer);
            }
            Role::Answerer => {
                if self.epoch != Some(epoch) {
                    self.fail(operation_id, TransportError::StaleEpoch);
                } else if self.state != PeerState::Restarting {
                    self.fail(operation_id, TransportError::InvalidState);
                } else if self.remote_description.as_ref().is_some_and(|description| {
                    description.epoch == epoch && description.kind == DescriptionKind::Offer
                }) {
                    self.create_local_description(operation_id, epoch, DescriptionKind::Answer);
                } else {
                    self.fail(operation_id, TransportError::InvalidState);
                }
            }
        }
    }

    fn open_data_channel(&mut self, operation_id: OperationId, channel_id: ChannelId) {
        if self.state != PeerState::Connected {
            self.fail(operation_id, TransportError::InvalidState);
        } else if self.channel == Some(channel_id) && !self.channel_closed {
            self.complete(operation_id);
        } else if self.channel.is_some() {
            self.fail(operation_id, TransportError::InvalidState);
        } else {
            self.channel = Some(channel_id);
            self.channel_closed = false;
            self.emit(Event::DataChannelOpened { channel_id });
            self.complete(operation_id);
        }
    }

    fn close_data_channel(&mut self, operation_id: OperationId, channel_id: ChannelId) {
        if self.channel != Some(channel_id) {
            self.fail(operation_id, TransportError::InvalidState);
        } else if self.channel_closed {
            self.complete(operation_id);
        } else {
            self.channel_closed = true;
            self.emit(Event::DataChannelClosed { channel_id });
            self.complete(operation_id);
        }
    }

    fn send(&mut self, operation_id: OperationId, channel_id: ChannelId, payload: BinaryPayload) {
        if self.channel != Some(channel_id) || self.channel_closed {
            self.release_send_reservation(payload.len());
            self.fail(operation_id, TransportError::InvalidState);
            return;
        }
        self.messages_sent = self.messages_sent.saturating_add(1);
        self.bytes_sent = self
            .bytes_sent
            .saturating_add(u64::try_from(payload.len()).unwrap_or(u64::MAX));
        self.provider_sends.push_back((channel_id, payload));
        self.complete(operation_id);
    }

    fn request_stats(&mut self, operation_id: OperationId) {
        self.stats_sequence = self.stats_sequence.saturating_add(1);
        self.emit(Event::Stats {
            operation_id,
            report: StatsReport {
                sequence: self.stats_sequence,
                messages_sent: self.messages_sent,
                bytes_sent: self.bytes_sent,
                messages_received: self.messages_received,
                bytes_received: self.bytes_received,
                buffered_send_bytes: u64::try_from(self.reserved_send_bytes).unwrap_or(u64::MAX),
                buffered_send_messages: u64::try_from(self.reserved_send_messages)
                    .unwrap_or(u64::MAX),
            },
        });
        self.complete(operation_id);
    }

    fn release_send_reservation(&mut self, bytes: usize) {
        self.reserved_send_bytes = self.reserved_send_bytes.saturating_sub(bytes);
        self.reserved_send_messages = self.reserved_send_messages.saturating_sub(1);
    }

    fn shutdown(&mut self, operation_id: OperationId) {
        self.transition(PeerState::Closing);
        self.provider_sends.clear();
        self.reserved_send_bytes = 0;
        self.reserved_send_messages = 0;
        self.provider_torn_down.store(true, Ordering::Release);
        if self.shutdown_timeout {
            self.fail(operation_id, TransportError::ShutdownTimeout);
        } else {
            self.complete(operation_id);
        }
        self.transition(PeerState::Closed);
        self.emit(Event::ShutdownComplete);
        self.shutdown_complete = true;
        self.pending_fatal = None;
    }
}

impl PeerDriver for FakePeer {
    fn submit(&mut self, command: Command) -> Result<(), SubmitError> {
        if self.shutdown_requested || self.shutdown_complete {
            return Err(SubmitError::new(TransportError::Shutdown, command));
        }
        if (self.pending_fatal.is_some() || self.provider_failed)
            && !matches!(command, Command::Shutdown { .. })
        {
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
        if self.commands.len() >= self.config.command_capacity {
            return Err(SubmitError::new(TransportError::QueueFull, command));
        }
        if let Some(error) = self.configured_size_error(&command) {
            return Err(SubmitError::new(error, command));
        }
        if let Command::Send {
            channel_id,
            payload,
            ..
        } = &command
        {
            let closing = self.commands.iter().any(|queued| {
                matches!(
                    &queued.command,
                    Command::CloseDataChannel {
                        channel_id: queued_channel,
                        ..
                    } if queued_channel == channel_id
                )
            });
            if self.channel != Some(*channel_id) || self.channel_closed || closing {
                return Err(SubmitError::new(TransportError::InvalidState, command));
            }
            let Some(bytes) = self.reserved_send_bytes.checked_add(payload.len()) else {
                return Err(SubmitError::new(TransportError::WouldBlock, command));
            };
            if bytes > self.config.send_buffer_bytes
                || self.reserved_send_messages >= self.config.send_buffer_messages
            {
                return Err(SubmitError::new(TransportError::WouldBlock, command));
            }
            self.reserved_send_bytes = bytes;
            self.reserved_send_messages += 1;
        }

        let is_shutdown = matches!(command, Command::Shutdown { .. });
        if is_shutdown {
            self.shutdown_requested = true;
        }
        let timeout_ms = if is_shutdown {
            self.config.shutdown_timeout_ms
        } else {
            self.config.operation_timeout_ms
        };
        let stalled = !is_shutdown && self.stall_next_operation;
        if stalled {
            self.stall_next_operation = false;
        }
        self.highest_operation = Some(operation_id);
        self.commands.push_back(QueuedCommand {
            command,
            deadline_ms: self.now_ms.saturating_add(timeout_ms),
            stalled,
        });
        self.wake();
        Ok(())
    }

    fn poll_event(&mut self, context: &mut Context<'_>) -> Poll<Option<Event>> {
        if let Some(event) = self.events.pop_front() {
            return Poll::Ready(Some(event));
        }
        if self.shutdown_complete {
            return Poll::Ready(None);
        }
        let expired_before_pending_fatal = self.commands.front().is_some_and(|queued| {
            !matches!(queued.command, Command::Shutdown { .. })
                && self.now_ms >= queued.deadline_ms
                && self
                    .pending_fatal
                    .is_none_or(|fatal| queued.deadline_ms <= fatal.observed_at_ms)
        });
        if expired_before_pending_fatal && let Some(queued) = self.commands.pop_front() {
            if let Command::Send { payload, .. } = &queued.command {
                self.release_send_reservation(payload.len());
            }
            return Poll::Ready(Some(Event::OperationFailed {
                operation_id: queued.command.operation_id(),
                error: TransportError::OperationTimeout,
            }));
        }
        if let Some(fatal) = self.pending_fatal {
            let shutdown_is_next = self
                .commands
                .front()
                .is_some_and(|queued| matches!(queued.command, Command::Shutdown { .. }));
            if !shutdown_is_next && let Some(queued) = self.commands.pop_front() {
                if let Command::Send { payload, .. } = &queued.command {
                    self.release_send_reservation(payload.len());
                }
                return Poll::Ready(Some(Event::OperationFailed {
                    operation_id: queued.command.operation_id(),
                    error: fatal.error,
                }));
            }
            if self.state != PeerState::Failed {
                self.state = PeerState::Failed;
                return Poll::Ready(Some(Event::StateChanged {
                    state: PeerState::Failed,
                }));
            }
            self.pending_fatal = None;
            self.provider_failed = true;
            return Poll::Ready(Some(Event::FatalError { error: fatal.error }));
        }
        if let Some(queued) = self.commands.front() {
            let waiting_for_operation = queued.stalled && self.now_ms < queued.deadline_ms;
            let waiting_for_shutdown = self.shutdown_timeout
                && matches!(queued.command, Command::Shutdown { .. })
                && self.now_ms < queued.deadline_ms;
            if waiting_for_operation || waiting_for_shutdown {
                self.waker = Some(context.waker().clone());
                return Poll::Pending;
            }
        }
        if let Some(queued) = self.commands.pop_front() {
            if queued.stalled {
                if let Command::Send { payload, .. } = &queued.command {
                    self.release_send_reservation(payload.len());
                }
                return Poll::Ready(Some(Event::OperationFailed {
                    operation_id: queued.command.operation_id(),
                    error: TransportError::OperationTimeout,
                }));
            }
            self.process(queued.command);
            let event = self.events.pop_front();
            debug_assert!(event.is_some());
            return Poll::Ready(event);
        }
        self.waker = Some(context.waker().clone());
        Poll::Pending
    }
}

impl Drop for FakePeer {
    fn drop(&mut self) {
        self.provider_sends.clear();
        self.reserved_send_bytes = 0;
        self.reserved_send_messages = 0;
        self.provider_torn_down.store(true, Ordering::Release);
        self.drop_probe.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod allocation_tests {
    use super::*;

    #[test]
    fn constructors_normalize_retained_string_capacity_to_length() {
        let mut sdp = String::with_capacity(MAX_SDP_BYTES * 8);
        sdp.push_str("short SDP");
        let description = SessionDescription::new(NegotiationEpoch(1), DescriptionKind::Offer, sdp)
            .expect("short text is within the absolute cap");
        assert_eq!(description.sdp.capacity(), description.sdp.len());

        let mut candidate_text = String::with_capacity(MAX_CANDIDATE_BYTES * 8);
        candidate_text.push_str("candidate");
        let mut sdp_mid = String::with_capacity(MAX_CANDIDATE_BYTES * 8);
        sdp_mid.push_str("data");
        let mut username_fragment = String::with_capacity(MAX_CANDIDATE_BYTES * 8);
        username_fragment.push_str("ufrag");
        let candidate = IceCandidate::new(
            NegotiationEpoch(1),
            candidate_text,
            Some(sdp_mid),
            Some(0),
            Some(username_fragment),
        )
        .expect("short aggregate text is within the absolute cap");
        assert_eq!(candidate.candidate.capacity(), candidate.candidate.len());
        let retained_mid = candidate.sdp_mid.as_ref().expect("SDP mid is retained");
        assert_eq!(retained_mid.capacity(), retained_mid.len());
        let retained_ufrag = candidate
            .username_fragment
            .as_ref()
            .expect("username fragment is retained");
        assert_eq!(retained_ufrag.capacity(), retained_ufrag.len());

        let mut end_mid = String::with_capacity(MAX_CANDIDATE_BYTES * 8);
        end_mid.push_str("data");
        let mut end_ufrag = String::with_capacity(MAX_CANDIDATE_BYTES * 8);
        end_ufrag.push_str("ufrag");
        let end =
            EndOfCandidates::new(NegotiationEpoch(1), Some(end_mid), Some(0), Some(end_ufrag))
                .expect("short end marker is within the absolute cap");
        let retained_mid = end.sdp_mid.as_ref().expect("SDP mid is retained");
        assert_eq!(retained_mid.capacity(), retained_mid.len());
        let retained_ufrag = end
            .username_fragment
            .as_ref()
            .expect("username fragment is retained");
        assert_eq!(retained_ufrag.capacity(), retained_ufrag.len());

        let mut bytes = Vec::with_capacity(MAX_MESSAGE_BYTES * 2);
        bytes.extend_from_slice(&[1, 2, 3]);
        let payload = BinaryPayload::new(bytes).expect("short payload is bounded");
        assert_eq!(payload.0.capacity(), payload.0.len());

        let certificate = include_bytes!("../tests/fixtures/minimal-ed25519-cert.der");
        let mut anchor = Vec::with_capacity(MAX_CUSTOM_TRUST_BYTES * 2);
        anchor.extend_from_slice(certificate);
        let mut anchors = Vec::with_capacity(MAX_ICE_SERVERS * 8);
        anchors.push(anchor);
        let tls = TurnTlsConfig::new("turn.example", TlsTrust::Custom(anchors))
            .expect("bounded custom roots");
        let TlsTrust::Custom(anchors) = &tls.trust else {
            panic!("custom trust retained");
        };
        assert_eq!(anchors.capacity(), anchors.len());
        assert_eq!(anchors[0].capacity(), anchors[0].len());

        let mut username = String::with_capacity(MAX_ICE_TEXT_BYTES * 2);
        username.push_str("user");
        let mut credential = String::with_capacity(MAX_ICE_TEXT_BYTES * 2);
        credential.push_str("secret");
        let credentials = TurnCredentials::new(username, credential).expect("bounded credentials");
        assert_eq!(credentials.username.capacity(), credentials.username.len());
        assert_eq!(
            credentials.credential.capacity(),
            credentials.credential.len()
        );
    }
}

#[cfg(test)]
mod x509_name_tests {
    use super::*;

    const COMMON_NAME: &[u8] = &[0x55, 0x04, 0x03];
    const COUNTRY_NAME: &[u8] = &[0x55, 0x04, 0x06];

    #[test]
    fn directory_string_accepts_only_supported_well_formed_primitive_encodings() {
        assert!(valid_name_attribute_value(
            COMMON_NAME,
            0x0c,
            "relay.example".as_bytes()
        ));
        assert!(valid_name_attribute_value(COMMON_NAME, 0x13, b"Relay Test"));
        assert!(valid_name_attribute_value(
            COMMON_NAME,
            0x1e,
            &[0x00, b'R', 0x03, 0xa9],
        ));
        assert!(valid_name_attribute_value(
            COMMON_NAME,
            0x1c,
            &[0x00, 0x01, 0xf6, 0x80],
        ));

        for tag in [0x02, 0x04, 0x05, 0x14, 0x20, 0x2c] {
            assert!(!valid_name_attribute_value(COMMON_NAME, tag, b"relay"));
        }
        assert!(!valid_name_attribute_value(COMMON_NAME, 0x0c, b""));
        assert!(!valid_name_attribute_value(
            COMMON_NAME,
            0x0c,
            &[0xc0, 0x80]
        ));
        assert!(!valid_name_attribute_value(COMMON_NAME, 0x13, b"bad@value"));
        assert!(!valid_name_attribute_value(COMMON_NAME, 0x1e, &[0x00]));
        assert!(!valid_name_attribute_value(
            COMMON_NAME,
            0x1e,
            &[0xd8, 0x00],
        ));
        assert!(!valid_name_attribute_value(
            COMMON_NAME,
            0x1c,
            &[0x00, 0x01, 0x10],
        ));
        assert!(!valid_name_attribute_value(
            COMMON_NAME,
            0x1c,
            &[0x00, 0x11, 0x00, 0x00],
        ));
    }

    #[test]
    fn name_attribute_syntax_is_oid_specific_and_bounded() {
        assert!(valid_name_attribute_value(COUNTRY_NAME, 0x13, b"US"));
        assert!(!valid_name_attribute_value(COUNTRY_NAME, 0x0c, b"US"));
        assert!(!valid_name_attribute_value(COUNTRY_NAME, 0x13, b"USA"));

        assert!(valid_name_attribute_value(
            OID_DOMAIN_COMPONENT,
            0x16,
            b"example",
        ));
        assert!(valid_name_attribute_value(
            OID_EMAIL_ADDRESS,
            0x16,
            b"root@example.test",
        ));
        assert!(!valid_name_attribute_value(
            OID_DOMAIN_COMPONENT,
            0x0c,
            b"example",
        ));
        assert!(!valid_name_attribute_value(
            OID_DOMAIN_COMPONENT,
            0x16,
            &[0x80],
        ));
        assert!(!valid_name_attribute_value(
            OID_DOMAIN_COMPONENT,
            0x16,
            &[0x1f],
        ));
        assert!(!valid_name_attribute_value(
            &[0x2a, 0x03, 0x04],
            0x0c,
            b"unknown syntax",
        ));

        assert!(valid_name_attribute_value(COMMON_NAME, 0x0c, &[b'a'; 64]));
        assert!(!valid_name_attribute_value(COMMON_NAME, 0x0c, &[b'a'; 65]));
    }
}

#[cfg(test)]
mod ice_url_tests {
    use super::*;

    fn creds() -> TurnCredentials {
        TurnCredentials::new("user", "secret").expect("credentials")
    }

    #[test]
    fn parses_the_urls_cloudflare_hands_out() {
        let servers = [
            "stun:stun.cloudflare.com:3478",
            "turn:turn.cloudflare.com:3478?transport=udp",
            "turn:turn.cloudflare.com:80?transport=tcp",
            "turns:turn.cloudflare.com:443?transport=tcp",
        ];
        let parsed: Vec<_> = servers
            .iter()
            .map(|url| IceServer::parse_url(url, Some(creds())).expect(url))
            .collect();
        assert_eq!(
            parsed[0],
            IceServer::stun("stun.cloudflare.com", 3478, IceTransport::Udp).expect("stun")
        );
        let IceServer::Turn {
            port, transport, ..
        } = &parsed[2]
        else {
            panic!("expected turn, got {:?}", parsed[2]);
        };
        assert_eq!((*port, *transport), (80, IceTransport::Tcp));
        let IceServer::Turn {
            transport, tls, ..
        } = &parsed[3]
        else {
            panic!("expected turns");
        };
        assert_eq!(*transport, IceTransport::Tls);
        assert_eq!(
            tls.as_ref().map(TurnTlsConfig::server_name),
            Some("turn.cloudflare.com"),
            "turns must verify the hostname it dialed"
        );
    }

    #[test]
    fn schemes_default_their_port() {
        let stun = IceServer::parse_url("stun:stun.example.com", None).expect("stun");
        assert!(matches!(stun, IceServer::Stun { port: 3478, .. }));
        let turns = IceServer::parse_url("turns:t.example.com", Some(creds())).expect("turns");
        assert!(matches!(turns, IceServer::Turn { port: 5349, .. }));
    }

    #[test]
    fn junk_is_rejected_rather_than_guessed() {
        assert!(IceServer::parse_url("turn:t.example.com:3478", None).is_err());
        assert!(IceServer::parse_url("https://t.example.com", Some(creds())).is_err());
        assert!(IceServer::parse_url("turn:t.example.com:3478?transport=sctp", Some(creds())).is_err());
        assert!(IceServer::parse_url("stun:", None).is_err());
        assert!(IceServer::parse_url("stun:host:0", None).is_err());
        assert!(
            IceServer::parse_url("stun:stun.example.com:3478?transport=tcp", None).is_ok(),
            "stun over tcp is legal; only stuns is not"
        );
    }
}
