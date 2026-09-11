//! One-line session status for the editor, derived from `SessionControl`.

use relay_session::{ConnectionState, SessionControl};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    /// Live switch is off.
    Off,
    /// Engine reported a failure.
    Failed,
    /// Listen page is up but the host stopped sending (DTX hold).
    Asleep,
    /// Sockets are up, nobody connected yet.
    Ready,
    /// Still binding / dialing.
    Pending,
    /// Audio is flowing to someone.
    Live,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub health: Health,
    pub line: String,
}

/// Everything the status line depends on, in one plain value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    pub share: bool,
    pub live: bool,
    pub failed: bool,
    pub error: String,
    pub connected: bool,
    pub bound: bool,
    pub web_ok: bool,
    pub web_silent: bool,
    pub listeners: u32,
    pub dropouts: u32,
    /// A cloud join is carrying the host's audio instead of the direct route.
    pub cloud: bool,
}

impl Facts {
    pub fn read(control: &SessionControl, share: bool) -> Self {
        let snap = control.snapshot();
        let peers = u32::try_from(snap.peers).unwrap_or(u32::MAX);
        Self {
            share,
            live: control.linked(),
            failed: snap.state == ConnectionState::Failed,
            error: control.last_error().unwrap_or_default(),
            connected: snap.state == ConnectionState::Connected,
            bound: snap.bound,
            web_ok: control.web_ok(),
            web_silent: control.web_silent(),
            listeners: peers
                .saturating_add(control.web_listeners())
                .saturating_add(control.lan_listeners()),
            dropouts: snap.dropouts,
            cloud: control.cloud_rx(),
        }
    }
}

pub fn describe(facts: &Facts) -> Status {
    if !facts.live {
        return Status {
            health: Health::Off,
            line: "Off".into(),
        };
    }
    if facts.failed {
        let line = if facts.error.is_empty() {
            "Failed".into()
        } else {
            format!("Failed · {}", facts.error)
        };
        return Status {
            health: Health::Failed,
            line,
        };
    }

    let (health, mut bits): (Health, Vec<String>) = if facts.share {
        share_bits(facts)
    } else {
        join_bits(facts)
    };
    if facts.dropouts > 0 {
        bits.push(format!("{} dropouts", facts.dropouts));
    }
    Status {
        health,
        line: bits.join(" · "),
    }
}

fn share_bits(facts: &Facts) -> (Health, Vec<String>) {
    if facts.listeners > 0 {
        return (Health::Live, vec![format!("{} listening", facts.listeners)]);
    }
    if facts.web_ok && facts.web_silent {
        return (Health::Asleep, vec!["Asleep".into()]);
    }
    if facts.web_ok {
        return (Health::Ready, vec!["Ready".into()]);
    }
    if facts.bound {
        return (
            Health::Ready,
            vec!["Ready".into(), "listen page offline".into()],
        );
    }
    (Health::Pending, vec!["Starting".into()])
}

fn join_bits(facts: &Facts) -> (Health, Vec<String>) {
    if facts.connected || facts.listeners > 0 {
        return (Health::Live, vec!["Connected".into()]);
    }
    if facts.cloud {
        return (Health::Live, vec!["Connected".into(), "via cloud".into()]);
    }
    if facts.bound {
        return (Health::Pending, vec!["Joining".into()]);
    }
    (Health::Pending, vec!["Starting".into()])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn share() -> Facts {
        Facts {
            share: true,
            live: true,
            bound: true,
            ..Facts::default()
        }
    }

    #[test]
    fn off_wins_over_everything() {
        let facts = Facts {
            live: false,
            failed: true,
            listeners: 3,
            ..share()
        };
        assert_eq!(describe(&facts).health, Health::Off);
        assert_eq!(describe(&facts).line, "Off");
    }

    #[test]
    fn failed_carries_the_error() {
        let facts = Facts {
            failed: true,
            error: "port in use".into(),
            ..share()
        };
        assert_eq!(describe(&facts).line, "Failed · port in use");
        let bare = Facts {
            failed: true,
            ..share()
        };
        assert_eq!(describe(&bare).line, "Failed");
    }

    #[test]
    fn share_never_says_joining() {
        let starting = Facts {
            bound: false,
            ..share()
        };
        assert_eq!(describe(&starting).line, "Starting");
        let offline = share();
        assert_eq!(describe(&offline).line, "Ready · listen page offline");
        let ready = Facts {
            web_ok: true,
            ..share()
        };
        assert_eq!(describe(&ready).line, "Ready");
        let asleep = Facts {
            web_ok: true,
            web_silent: true,
            ..share()
        };
        assert_eq!(describe(&asleep).health, Health::Asleep);
    }

    #[test]
    fn listeners_make_share_live() {
        let facts = Facts {
            web_ok: true,
            web_silent: true,
            listeners: 2,
            dropouts: 1,
            ..share()
        };
        let status = describe(&facts);
        assert_eq!(status.health, Health::Live);
        assert_eq!(status.line, "2 listening · 1 dropouts");
    }

    #[test]
    fn join_reports_connection_progress() {
        let joining = Facts {
            share: false,
            ..share()
        };
        assert_eq!(describe(&joining).line, "Joining");
        assert_eq!(describe(&joining).health, Health::Pending);
        let connected = Facts {
            share: false,
            connected: true,
            ..share()
        };
        assert_eq!(describe(&connected).line, "Connected");
        assert_eq!(describe(&connected).health, Health::Live);
    }
}
