//! Host/join status: a lone Link host is "hosting", bind collisions stay Failed.

use std::net::SocketAddr;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use relay_audio::FrameDuration;
use relay_domain::{ConnectionState, SessionMode};
use relay_session::{
    EngineCommand, MonitorMode, SessionConfig, SessionControl, SessionEngine, SessionPill,
    SessionRole, SessionRuntime,
};

fn config(ssrc: u32) -> SessionConfig {
    SessionConfig {
        mode: SessionMode::Connect,
        device_rate_hz: 48_000,
        frame_duration: FrameDuration::Ms20,
        ssrc,
        monitor: MonitorMode::Remote,
        lan: true,
    }
}

fn pill(
    linked: bool,
    state: ConnectionState,
    peers: usize,
    web_ok: bool,
    web_silent: bool,
    bound: bool,
    lan_browsers: u32,
) -> SessionPill {
    relay_session::classify_session(relay_session::SessionView {
        linked,
        role: SessionRole::ConnectListen,
        state,
        peers,
        lan_browsers,
        web_listeners: 0,
        web_ok,
        web_silent,
        web_wanted: web_ok,
        bound,
    })
}

/// Symptom: a lone Link host that *is* bound still looks like it never started.
#[test]
fn repro_lone_listen_host_looks_ready() {
    let mut host = SessionEngine::prepare(config(0x5245_0001)).expect("host");
    host.apply(EngineCommand::Listen(SocketAddr::from(([127, 0, 0, 1], 0))))
        .expect("listen bind");
    for _ in 0..8 {
        host.drive().expect("drive");
    }
    let snap = host.snapshot();
    assert!(
        snap.local_port.is_some(),
        "host is bound — Cali thought it was not even hosting"
    );
    assert_eq!(snap.state, ConnectionState::Connecting);
    assert_eq!(snap.peers, 0);
    assert!(snap.bound);
    assert_eq!(
        pill(true, snap.state, snap.peers, false, false, snap.bound, 0),
        SessionPill::Hosting,
        "lone bound host must classify as hosting, not joining"
    );
}

/// Symptom: two default Link instances both Listen on 17492. Second bind fails.
#[test]
fn repro_second_listen_same_port_fails() {
    let mut a = SessionEngine::prepare(config(0x5245_0002)).expect("a");
    a.apply(EngineCommand::Listen(SocketAddr::from(([127, 0, 0, 1], 0))))
        .expect("first listen");
    let port = a.snapshot().local_port.expect("bound");
    let mut b = SessionEngine::prepare(config(0x5245_0003)).expect("b");
    let second = b.apply(EngineCommand::Listen(SocketAddr::from((
        [127, 0, 0, 1],
        port,
    ))));
    assert!(
        second.is_err(),
        "two Link hosts on one UDP port must collide (Cali's second VST)"
    );
    assert_eq!(b.snapshot().state, ConnectionState::Idle);
    assert_eq!(
        pill(true, ConnectionState::Failed, 0, false, false, false, 0),
        SessionPill::Failed,
        "a bind collision must paint Failed"
    );
}

/// Symptom: worker writes Failed on bind error, then `publish()` overwrites it
/// with the engine snapshot (still Idle). UI never sees Failed.
#[test]
fn repro_failed_listen_is_published_as_idle() {
    let mut holder = SessionEngine::prepare(config(0x5245_0004)).expect("holder");
    holder
        .apply(EngineCommand::Listen(SocketAddr::from(([127, 0, 0, 1], 0))))
        .expect("occupy port");
    let port = holder.snapshot().local_port.expect("bound");

    let control = Arc::new(SessionControl::default());
    control.set_linked(true);
    control.set_role(SessionRole::ConnectListen);
    control.set_bind_port(port);

    let _runtime = SessionRuntime::start_with(config(0x5245_0005), Arc::clone(&control))
        .expect("second runtime starts even if Listen will fail");

    let mut seen_failed = false;
    for _ in 0..40 {
        thread::sleep(Duration::from_millis(5));
        if control.snapshot().state == ConnectionState::Failed {
            seen_failed = true;
        }
    }
    let snap = control.snapshot();
    assert!(seen_failed, "bind failure must publish Failed");
    assert_eq!(snap.state, ConnectionState::Failed);
    assert_eq!(
        pill(true, snap.state, snap.peers, false, false, snap.bound, 0),
        SessionPill::Failed
    );
    assert!(
        !control.last_error().expect("error").is_empty(),
        "Failed must keep a readable bind error"
    );
}

/// Symptom: two Listen hosts never Hello/Who each other, so they never connect
/// even on different ports.
#[test]
fn repro_two_listen_hosts_do_not_pair() {
    let mut a = SessionEngine::prepare(config(0x5245_0006)).expect("a");
    let mut b = SessionEngine::prepare(config(0x5245_0007)).expect("b");
    a.apply(EngineCommand::Listen(SocketAddr::from(([127, 0, 0, 1], 0))))
        .expect("a");
    b.apply(EngineCommand::Listen(SocketAddr::from(([127, 0, 0, 1], 0))))
        .expect("b");
    for _ in 0..20 {
        a.drive().expect("a");
        b.drive().expect("b");
    }
    assert_eq!(a.snapshot().state, ConnectionState::Connecting);
    assert_eq!(b.snapshot().state, ConnectionState::Connecting);
    assert_eq!(a.snapshot().peers, 0);
    assert_eq!(b.snapshot().peers, 0);
}
