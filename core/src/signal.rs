//! Signaling client: one WebSocket to the room's Durable Object, on its own
//! thread so DNS, TLS and reconnects never stall the link thread. The link
//! talks to it through two channels.

use std::io::ErrorKind;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use tungstenite::{Error, Message};

const PING_EVERY: Duration = Duration::from_secs(30);
const CONNECT_STAGGER: Duration = Duration::from_millis(250);

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

// Preserve the resolver's preferred family, but alternate IPv4/IPv6 before
// racing addresses. A blackholed route must not hold a working route hostage.
fn connect<T: Send + 'static>(
    mut addresses: Vec<SocketAddr>,
    dial: impl Fn(SocketAddr) -> Option<T> + Send + Sync + 'static,
) -> Option<T> {
    for i in 1..addresses.len() {
        if let Some(offset) = addresses[i..]
            .iter()
            .position(|a| a.is_ipv6() != addresses[i - 1].is_ipv6())
        {
            let address = addresses.remove(i + offset);
            addresses.insert(i, address);
        }
    }
    let dial = std::sync::Arc::new(dial);
    let (tx, rx) = mpsc::channel();
    // Bound temporary threads even if DNS returns an unusually large set.
    for address in addresses.into_iter().take(8) {
        let (dial, tx) = (std::sync::Arc::clone(&dial), tx.clone());
        if thread::Builder::new()
            .name("relay-connect".into())
            .spawn(move || {
                let _ = tx.send(dial(address));
            })
            .is_err()
        {
            continue;
        }
        if let Ok(Some(stream)) = rx.recv_timeout(CONNECT_STAGGER) {
            return Some(stream);
        }
    }
    drop(tx);
    rx.into_iter().flatten().next()
}

fn open(site: &str, url: &str) -> Option<Ws> {
    let addresses = (site, 443).to_socket_addrs().ok()?.collect();
    let tcp = connect(addresses, |a| {
        TcpStream::connect_timeout(&a, Duration::from_secs(5)).ok()
    })?;
    tcp.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    tcp.set_write_timeout(Some(Duration::from_secs(5))).ok()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn working_family_does_not_wait_for_stalled_addresses() {
        let slow: SocketAddr = "[::1]:443".parse().unwrap();
        let slow2: SocketAddr = "[::1]:444".parse().unwrap();
        let good: SocketAddr = "127.0.0.1:443".parse().unwrap();
        let release = std::sync::Arc::new(std::sync::Barrier::new(2));
        let unblock = std::sync::Arc::clone(&release);
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = connect(vec![slow, slow2, good], move |address| {
                if address == slow {
                    unblock.wait();
                    return None;
                }
                assert_eq!(
                    address, good,
                    "alternate families before another slow route"
                );
                Some(address)
            });
            tx.send(result).unwrap();
        });
        let result = rx.recv_timeout(Duration::from_secs(2));
        release.wait();
        worker.join().unwrap();
        assert_eq!(result.unwrap(), Some(good));
    }

    #[test]
    fn refused_addresses_fall_back_and_all_failures_finish() {
        let a = "127.0.0.1:443".parse().unwrap();
        let b = "127.0.0.1:444".parse().unwrap();
        assert_eq!(connect(vec![a, b], move |x| (x == b).then_some(x)), Some(b));
        assert_eq!(connect(vec![a, b], |_| None::<SocketAddr>), None);
        assert_eq!(connect(Vec::new(), Some), None::<SocketAddr>);
    }
}
