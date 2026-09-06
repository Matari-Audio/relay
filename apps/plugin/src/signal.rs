//! JSON wire messages between the plugin, the cloud room, and listeners.

use relay_session::{CodecSettings, WIRE_BITS};
use serde::{Deserialize, Serialize};

/// Listener → plugin WebRTC signaling, relayed by the cloud room.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Signal {
    Want {
        id: String,
    },
    Answer {
        id: String,
        sdp: String,
    },
    Ice {
        id: String,
        cand: String,
        #[serde(default)]
        mid: Option<String>,
    },
    Bye {
        id: String,
    },
}

impl Signal {
    pub fn id(&self) -> &str {
        match self {
            Self::Want { id }
            | Self::Answer { id, .. }
            | Self::Ice { id, .. }
            | Self::Bye { id } => id,
        }
    }

    pub fn is_bye(&self) -> bool {
        matches!(self, Self::Bye { .. })
    }
}

impl Signal {
    /// Parse one inbound room frame. Room chatter that is not signaling
    /// (claim info, listener counts) yields `None`.
    pub fn parse(text: &str) -> Option<Self> {
        serde_json::from_str::<Self>(text)
            .ok()
            .filter(|signal| !signal.id().is_empty())
    }
}

/// Plugin → room / listener messages.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Outbound<'a> {
    #[serde(rename_all = "camelCase")]
    Cfg {
        codec: &'static str,
        bitrate: u32,
        bits: u8,
        compression: u8,
        rate: u32,
        port: u16,
        lan_http: u16,
    },
    Stat {
        dropouts: u32,
        peers: usize,
        lan: u32,
        web: u32,
        ready: u32,
        sent: u64,
        peak: f32,
        port: u16,
    },
    Room {
        host: bool,
        live: bool,
        silent: bool,
        listeners: u32,
        peers: usize,
        dropouts: u32,
        port: u16,
        asleep: bool,
    },
    Offer {
        id: &'a str,
        sdp: &'a str,
    },
    Ice {
        id: &'a str,
        cand: &'a str,
        mid: Option<&'a str>,
    },
    Bye {
        id: &'a str,
    },
    Go,
    Dtx,
}

impl Outbound<'_> {
    pub fn cfg(settings: CodecSettings, port: u16, lan_http: u16) -> Self {
        Self::Cfg {
            codec: settings.codec().as_str(),
            bitrate: settings.bitrate_kbps().unwrap_or(0),
            bits: settings.bits().max(WIRE_BITS),
            compression: settings.flac_level().unwrap_or(0),
            rate: 48_000,
            port,
            lan_http,
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// `POST /api/claim` body.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimBody<'a> {
    pub name: &'a str,
    pub port: u16,
    pub lan: Vec<String>,
    pub lan_http: u16,
    pub mode: &'static str,
    pub codec: &'static str,
    pub rate: u32,
    pub device_rate: u32,
    pub block: u32,
    pub bitrate: u32,
    pub bits: u8,
    pub compression: u8,
    pub pass: &'a str,
}

impl<'a> ClaimBody<'a> {
    pub fn new(
        name: &'a str,
        port: u16,
        settings: CodecSettings,
        pass: &'a str,
        device_rate: u32,
        block: u32,
        lan_http: u16,
    ) -> Self {
        let codec = settings.codec().as_str();
        Self {
            name,
            port,
            lan: relay_session::local_ipv4_addrs(),
            lan_http,
            mode: codec,
            codec,
            rate: 48_000,
            device_rate,
            block,
            bitrate: settings.bitrate_kbps().unwrap_or(0),
            bits: settings.bits().max(WIRE_BITS),
            compression: settings.flac_level().unwrap_or(0),
            pass,
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_parses_only_rtc_messages() {
        assert!(Signal::parse(r#"{"claim":{"name":"mix"},"listeners":4}"#).is_none());
        assert!(Signal::parse("not json").is_none());
        assert!(
            Signal::parse(r#"{"t":"want","id":""}"#).is_none(),
            "empty ids are dropped"
        );
        assert_eq!(
            Signal::parse(r#"{"t":"ice","id":"ab","cand":"candidate:1"}"#),
            Some(Signal::Ice {
                id: "ab".into(),
                cand: "candidate:1".into(),
                mid: None
            })
        );
        assert_eq!(
            Signal::parse(r#"{"t":"bye","id":"ab","extra":1}"#),
            Some(Signal::Bye { id: "ab".into() })
        );
    }

    #[test]
    fn cfg_json_names_codec() {
        let json = Outbound::cfg(CodecSettings::live(), 17_492, 8_787).to_json();
        assert!(json.contains("\"t\":\"cfg\""));
        assert!(json.contains("\"codec\":\"opus\""));
        assert!(json.contains("\"port\":17492"));
        assert!(json.contains("\"lanHttp\":8787"));
        assert!(!json.contains("deviceRate"));
    }

    #[test]
    fn offer_and_bye_are_tagged_objects() {
        let offer = Outbound::Offer {
            id: "ab",
            sdp: "v=0",
        }
        .to_json();
        assert!(offer.contains("\"t\":\"offer\""));
        assert!(offer.contains("\"id\":\"ab\""));
        assert_eq!(Outbound::Go.to_json(), r#"{"t":"go"}"#);
        assert_eq!(
            Outbound::Bye { id: "x" }.to_json(),
            r#"{"t":"bye","id":"x"}"#
        );
    }

    #[test]
    fn claim_body_includes_daw_rate_and_block() {
        let body =
            ClaimBody::new("mix", 17_492, CodecSettings::live(), "", 44_100, 128, 8_787).to_json();
        assert!(body.contains("\"name\":\"mix\""));
        assert!(body.contains("\"rate\":48000"));
        assert!(body.contains("\"deviceRate\":44100"));
        assert!(body.contains("\"block\":128"));
        assert!(body.contains("\"lanHttp\":8787"));
    }
}
