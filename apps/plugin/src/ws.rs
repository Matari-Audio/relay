//! Non-blocking WebSocket helpers shared by the cloud `/in` socket and the
//! LAN listen server.

use std::io;
use std::net::TcpStream;

use tungstenite::{Message, WebSocket};

pub fn tune_tcp(tcp: &TcpStream) {
    let _ = tcp.set_nodelay(true);
    let _ = tcp.set_nonblocking(true);
}

pub fn is_would_block(err: &tungstenite::Error) -> bool {
    if let tungstenite::Error::Io(io) = err {
        return matches!(
            io.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
        );
    }
    let text = err.to_string().to_ascii_lowercase();
    text.contains("would block") || text.contains("not ready")
}

/// Send without treating back-pressure as a failure. `false` means the
/// socket is gone and should be dropped.
pub fn send_keep<S>(ws: &mut WebSocket<S>, msg: Message) -> bool
where
    S: io::Read + io::Write,
{
    match ws.send(msg) {
        Ok(()) | Err(tungstenite::Error::WriteBufferFull(_)) => true,
        Err(err) => is_would_block(&err),
    }
}

/// Drain every pending frame. Pings are answered, text is handed to
/// `on_text`. Returns `false` once the peer has closed or errored.
pub fn drain<S>(ws: &mut WebSocket<S>, mut on_text: impl FnMut(&str)) -> bool
where
    S: io::Read + io::Write,
{
    loop {
        match ws.read() {
            Ok(Message::Ping(payload)) => {
                if !send_keep(ws, Message::Pong(payload)) {
                    return false;
                }
            }
            Ok(Message::Text(text)) => on_text(text.as_str()),
            Ok(Message::Close(_)) => return false,
            Ok(_) => {}
            Err(err) => return is_would_block(&err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn would_block_covers_timeout_and_interrupt() {
        for kind in [
            io::ErrorKind::WouldBlock,
            io::ErrorKind::TimedOut,
            io::ErrorKind::Interrupted,
        ] {
            let err = tungstenite::Error::Io(io::Error::new(kind, "soft"));
            assert!(is_would_block(&err), "{kind:?}");
        }
        let reset = tungstenite::Error::Io(io::Error::new(io::ErrorKind::ConnectionReset, "x"));
        assert!(!is_would_block(&reset));
        assert!(!is_would_block(&tungstenite::Error::AlreadyClosed));
    }
}
