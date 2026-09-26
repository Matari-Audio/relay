//! Signaling client: one WebSocket to the room's Durable Object, on its own
//! thread so DNS, TLS and reconnects never stall the link thread. The link
//! talks to it through two channels.

use std::io::ErrorKind;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use tungstenite::{Error, Message};

const PING_EVERY: Duration = Duration::from_secs(30);

pub enum Msg {
    /// Connected (again). The object's `ice` message follows.
    Up,
    /// Unreachable or dropped; retrying with backoff.
    Down,
    Text(String),
}

/// Drop it to close the socket and end the thread.
pub struct Signal {
    pub out: Sender<String>,
    pub inbox: Receiver<Msg>,
}

impl Signal {
    /// `path` is `/<slug>/host?key=…` or `/<slug>/peer`.
    pub fn connect(site: &str, path: &str) -> Self {
        let (out, out_rx) = mpsc::channel();
        let (in_tx, inbox) = mpsc::channel();
        let (site, url) = (site.to_owned(), format!("wss://{site}{path}"));
        let _ = thread::Builder::new()
            .name("relay-signal".into())
            .spawn(move || run(&site, &url, &out_rx, &in_tx));
        Self { out, inbox }
    }

    pub fn send(&self, msg: &serde_json::Value) {
        let _ = self.out.send(msg.to_string());
    }
}

fn run(site: &str, url: &str, out: &Receiver<String>, inbox: &Sender<Msg>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        if let Some(mut ws) = open(site, url) {
            backoff = Duration::from_secs(1);
            if inbox.send(Msg::Up).is_err() {
                return;
            }
            let mut ping = Instant::now();
            loop {
                match out.try_recv() {
                    Ok(text) => {
                        if ws.send(Message::text(text)).is_err() {
                            break;
                        }
                        continue;
                    }
                    Err(TryRecvError::Disconnected) => {
                        let _ = ws.close(None);
                        let _ = ws.flush();
                        return;
                    }
                    Err(TryRecvError::Empty) => {}
                }
                if ping.elapsed() >= PING_EVERY {
                    ping = Instant::now();
                    if ws.send(Message::text("ping")).is_err() {
                        break;
                    }
                }
                match ws.read() {
                    Ok(Message::Text(t)) if t.as_str() != "pong" => {
                        if inbox.send(Msg::Text(t.as_str().to_owned())).is_err() {
                            return;
                        }
                    }
                    Ok(Message::Close(_)) => break,
                    Ok(_) => {}
                    Err(Error::Io(e))
                        if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                    Err(_) => break,
                }
            }
        }
        if inbox.send(Msg::Down).is_err() {
            return;
        }
        // Wait out the backoff, still noticing when the link hangs up.
        let end = Instant::now() + backoff;
        while Instant::now() < end {
            if let Err(RecvTimeoutError::Disconnected) =
                out.recv_timeout(Duration::from_millis(100))
            {
                return;
            }
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

type Ws = tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>;

fn open(site: &str, url: &str) -> Option<Ws> {
    let tcp = (site, 443)
        .to_socket_addrs()
        .ok()?
        .find_map(|a| TcpStream::connect_timeout(&a, Duration::from_secs(5)).ok())?;
    tcp.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let _ = tcp.set_nodelay(true);
    let (ws, _) = tungstenite::client_tls(url, tcp).ok()?;
    // Short reads from here on so outgoing messages go out promptly.
    let tcp = match ws.get_ref() {
        tungstenite::stream::MaybeTlsStream::Rustls(s) => &s.sock,
        tungstenite::stream::MaybeTlsStream::Plain(s) => s,
        _ => return None,
    };
    tcp.set_read_timeout(Some(Duration::from_millis(20))).ok()?;
    Some(ws)
}
