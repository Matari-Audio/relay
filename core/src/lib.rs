//! RELAY core: the state the plugin, editor and network thread share, the
//! LAN wire format, the network thread ([`Link`]) and the playout buffer.
//!
//! Threads: the host audio thread only touches the two `rtrb` rings and
//! atomics. Sockets, strings and allocation live on the network thread.

mod net;
mod playout;

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

pub use net::Link;
pub use playout::Playout;

/// Share listens here; Join dials it when no port is given.
pub const PORT: u16 = 17_492;
/// The wire is always stereo; mono tracks are duplicated.
pub const CHANNELS: usize = 2;
/// Ring capacity in samples: a quarter second of stereo at 192 kHz.
pub const RING: usize = 48_000 * CHANNELS;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Role {
    Off = 0,
    Share = 1,
    Join = 2,
}

impl Role {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Share,
            2 => Self::Join,
            _ => Self::Off,
        }
    }
}

/// Everything the plugin, the editor and the network thread share.
#[derive(Default)]
pub struct Shared {
    role: AtomicU8,
    /// Local host sample rate.
    pub rate: AtomicU32,
    /// Room name. With the password it makes the tag every packet carries.
    pub room: Mutex<String>,
    pub password: Mutex<String>,
    /// Join target: `host`, or `host:port`.
    pub peer: Mutex<String>,
    /// Bumped whenever room, password or peer change, so the link rebuilds.
    pub config: AtomicU32,
    /// Share: listeners. Join: 1 while audio arrives.
    pub peers: AtomicU32,
    /// Share: the bound port. 0 when not bound.
    pub port: AtomicU32,
    /// Join: the sender's rate differs from ours (v1 plays silence then).
    pub rate_mismatch: AtomicBool,
    pub underruns: AtomicU64,
    stop: AtomicBool,
}

impl Shared {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    pub fn role(&self) -> Role {
        Role::from_u8(self.role.load(Relaxed))
    }

    pub fn set_role(&self, role: Role) {
        self.role.store(role as u8, Relaxed);
    }

    /// Replace one of the text settings and tell the link.
    pub fn set_text(&self, field: &Mutex<String>, value: &str) {
        let mut guard = field.lock().unwrap_or_else(|e| e.into_inner());
        if *guard != value {
            value.clone_into(&mut guard);
            self.config.fetch_add(1, Relaxed);
        }
    }

    pub fn text(field: &Mutex<String>) -> String {
        field.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// The 8-byte room tag: packets without it are ignored. Not encryption --
/// it keeps strangers and other rooms out, and a password makes it unguessable.
pub fn tag(room: &str, password: &str) -> [u8; 8] {
    use sha2::{Digest, Sha256};
    let digest = Sha256::new()
        .chain_update(room.trim().to_lowercase())
        .chain_update([0])
        .chain_update(password)
        .finalize();
    let mut tag = [0; 8];
    tag.copy_from_slice(&digest[..8]);
    tag
}

/// The LAN wire. One UDP datagram:
/// `"RL" kind:u8 channels:u8 tag:[u8;8] rate:u32 frame:u64 samples:[f32]`,
/// little-endian, samples interleaved.
pub mod wire {
    pub const HELLO: u8 = 0;
    pub const AUDIO: u8 = 1;
    pub const BYE: u8 = 2;
    pub const HEADER: usize = 24;
    /// Frames per datagram: 180 stereo f32 frames + header = 1464 bytes, under
    /// a 1500 MTU so nothing fragments.
    pub const MAX_FRAMES: usize = 180;

    pub struct Packet<'a> {
        pub kind: u8,
        pub tag: [u8; 8],
        pub rate: u32,
        pub frame: u64,
        pub samples: &'a [u8],
    }

    pub fn encode(out: &mut Vec<u8>, kind: u8, tag: [u8; 8], rate: u32, frame: u64, samples: &[f32]) {
        out.clear();
        out.extend_from_slice(b"RL");
        out.extend_from_slice(&[kind, super::CHANNELS as u8]);
        out.extend_from_slice(&tag);
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&frame.to_le_bytes());
        for s in samples {
            out.extend_from_slice(&s.to_le_bytes());
        }
    }

    pub fn decode(buf: &[u8]) -> Option<Packet<'_>> {
        if buf.len() < HEADER || &buf[..2] != b"RL" || usize::from(buf[3]) != super::CHANNELS {
            return None;
        }
        let samples = &buf[HEADER..];
        if samples.len() % (4 * super::CHANNELS) != 0 {
            return None;
        }
        Some(Packet {
            kind: buf[2],
            tag: buf[4..12].try_into().ok()?,
            rate: u32::from_le_bytes(buf[12..16].try_into().ok()?),
            frame: u64::from_le_bytes(buf[16..24].try_into().ok()?),
            samples,
        })
    }

    pub fn samples(bytes: &[u8]) -> impl Iterator<Item = f32> + '_ {
        bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
}

/// The two rings between the audio thread and the link, sized [`RING`].
pub fn rings() -> (
    (rtrb::Producer<f32>, rtrb::Consumer<f32>),
    (rtrb::Producer<f32>, rtrb::Consumer<f32>),
) {
    (rtrb::RingBuffer::new(RING), rtrb::RingBuffer::new(RING))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_round_trip_and_rejects_garbage() {
        let mut buf = Vec::new();
        let t = tag("big-filthy-papaya", "");
        wire::encode(&mut buf, wire::AUDIO, t, 48_000, 7, &[0.5, -0.25]);
        let p = wire::decode(&buf).unwrap();
        assert_eq!((p.kind, p.tag, p.rate, p.frame), (wire::AUDIO, t, 48_000, 7));
        assert_eq!(wire::samples(p.samples).collect::<Vec<_>>(), [0.5, -0.25]);
        assert!(wire::decode(&buf[..10]).is_none());
        assert!(wire::decode(&buf[..buf.len() - 1]).is_none());
        assert!(wire::decode(b"XXXXXXXXXXXXXXXXXXXXXXXXXXXX").is_none());
        assert_ne!(tag("room", ""), tag("room", "pw"));
        assert_eq!(tag(" Room ", "pw"), tag("room", "pw"));
    }
}
