//! Same-LAN browser listen: a tiny HTTP server plus WebSocket PCM on :8787.
//!
//! Never touches the cloud. The public listen page redirects here when a
//! browser's host candidates share a /24 with the plugin.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use relay_session::{DEFAULT_LINK_HTTP_PORT, SessionControl, normalize_slug};
use tungstenite::{Message, WebSocket};

use crate::ws;

type LocalSocket = WebSocket<TcpStream>;

const INDEX_HTML: &str = include_str!("../assets/lan/index.html");
const PLAYER_HTML: &str = include_str!("../assets/lan/player.html");

pub struct LocalHub {
    clients: Mutex<Vec<LocalSocket>>,
    port: u16,
    accept_stop: Arc<AtomicBool>,
}

impl Drop for LocalHub {
    fn drop(&mut self) {
        self.accept_stop.store(true, Ordering::Release);
    }
}

impl LocalHub {
    pub fn start(control: Arc<SessionControl>, stop: Arc<AtomicBool>) -> Arc<Self> {
        let (listener, port) = bind_listen();
        let accept_stop = Arc::new(AtomicBool::new(false));
        let hub = Arc::new(Self {
            clients: Mutex::new(Vec::new()),
            port,
            accept_stop: Arc::clone(&accept_stop),
        });
        control.set_lan_http_port(port);
        if let Some(listener) = listener {
            let accept_hub = Arc::clone(&hub);
            let _ = thread::Builder::new()
                .name("relay-lan-http".into())
                .spawn(move || accept_loop(&listener, &accept_hub, &control, &stop, &accept_stop));
        }
        hub
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Drop closed sockets and answer pings; returns the live count.
    pub fn prune_and_count(&self) -> u32 {
        let Ok(mut clients) = self.clients.lock() else {
            return 0;
        };
        clients.retain_mut(|socket| ws::drain(socket, |_| {}));
        u32::try_from(clients.len()).unwrap_or(u32::MAX)
    }

    pub fn broadcast_bin(&self, bytes: &[u8]) {
        self.send_all(&Message::Binary(bytes.to_vec().into()));
    }

    pub fn broadcast_text(&self, text: &str) {
        self.send_all(&Message::Text(text.to_owned().into()));
    }

    fn send_all(&self, msg: &Message) {
        if let Ok(mut clients) = self.clients.lock() {
            clients.retain_mut(|socket| ws::send_keep(socket, msg.clone()));
        }
    }

    fn adopt(&self, socket: LocalSocket) {
        ws::tune_tcp(socket.get_ref());
        if let Ok(mut clients) = self.clients.lock() {
            clients.push(socket);
        }
    }
}

/// Prefer the well-known port (retrying briefly across a plugin reload),
/// then walk up a dozen ports.
fn bind_listen() -> (Option<TcpListener>, u16) {
    let bind = |port: u16| {
        TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port)))
            .ok()
            .inspect(|listener| {
                let _ = listener.set_nonblocking(true);
            })
    };
    for _ in 0..20 {
        if let Some(listener) = bind(DEFAULT_LINK_HTTP_PORT) {
            return (Some(listener), DEFAULT_LINK_HTTP_PORT);
        }
        thread::sleep(Duration::from_millis(15));
    }
    (DEFAULT_LINK_HTTP_PORT + 1..=DEFAULT_LINK_HTTP_PORT + 12)
        .find_map(|port| bind(port).map(|listener| (Some(listener), port)))
        .unwrap_or((None, 0))
}

fn accept_loop(
    listener: &TcpListener,
    hub: &Arc<LocalHub>,
    control: &Arc<SessionControl>,
    stop: &AtomicBool,
    accept_stop: &AtomicBool,
) {
    while !stop.load(Ordering::Acquire) && !accept_stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                let hub = Arc::clone(hub);
                let control = Arc::clone(control);
                let _ = thread::Builder::new()
                    .name("relay-lan-client".into())
                    .spawn(move || handle_client(stream, &hub, &control));
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(_) => thread::sleep(Duration::from_millis(80)),
        }
    }
}

fn handle_client(mut stream: TcpStream, hub: &LocalHub, control: &SessionControl) {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut head = [0_u8; 2048];
    let n = match stream.peek(&mut head) {
        Ok(0) | Err(_) => return,
        Ok(n) => n,
    };
    let head = String::from_utf8_lossy(&head[..n]);
    if head.to_ascii_lowercase().contains("upgrade: websocket") {
        if let Ok(socket) = tungstenite::accept(stream) {
            hub.adopt(socket);
        }
        return;
    }
    let mut request = head.lines().next().unwrap_or_default().split_whitespace();
    let method = request.next().unwrap_or_default().to_owned();
    let path = request.next().unwrap_or("/").to_owned();
    let _ = stream.read(&mut [0_u8; 2048]);

    let name = control
        .session_name()
        .map(|name| normalize_slug(&name))
        .unwrap_or_default();
    let (status, content_type, body): (u16, &str, String) = match (method.as_str(), path.as_str()) {
        ("GET", "/health" | "/probe") => (200, "text/plain; charset=utf-8", "ok".into()),
        ("GET", "/status") => (
            200,
            "application/json; charset=utf-8",
            serde_json::json!({ "ok": true, "name": name, "port": hub.port() }).to_string(),
        ),
        ("GET", "/") => (
            200,
            "text/html; charset=utf-8",
            index_html(&name, hub.port()),
        ),
        ("GET", route) => {
            let slug = normalize_slug(route.trim_start_matches('/').trim_end_matches("/out"));
            if slug.is_empty() {
                (404, "text/plain", "missing name".into())
            } else {
                (200, "text/html; charset=utf-8", player_html(&slug))
            }
        }
        _ => (405, "text/plain", "method".into()),
    };
    write_http(&mut stream, status, content_type, body.as_bytes());
}

fn write_http(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body);
}

fn index_html(name: &str, port: u16) -> String {
    let listing = if name.is_empty() {
        String::new()
    } else {
        format!("<li><a href=\"/{name}\">{name}</a> · LAN {port}</li>")
    };
    INDEX_HTML.replace("{{LISTING}}", &listing)
}

fn player_html(name: &str) -> String {
    let name_json = serde_json::Value::String(name.to_owned()).to_string();
    PLAYER_HTML
        .replace("{{NAME}}", name)
        .replace("{{NAME_JSON}}", &name_json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn player_page_embeds_the_slug_as_json() {
        let html = player_html("late-mix");
        assert!(html.contains("<title>RELAY · late-mix</title>"));
        assert!(html.contains("const name = \"late-mix\";"));
        assert!(!html.contains("{{"));
    }

    #[test]
    fn index_lists_the_room_when_named() {
        assert!(index_html("", 8787).contains("<ul class=\"home-list\"></ul>"));
        let html = index_html("late-mix", 8790);
        assert!(html.contains("href=\"/late-mix\""));
        assert!(html.contains("LAN 8790"));
    }
}
