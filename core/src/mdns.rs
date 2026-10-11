//! LAN discovery: Share advertises `<slug>._relay._udp.local.` with its tag,
//! Join browses for it. mdns-sd runs its own thread; we only poll.

use std::net::{IpAddr, SocketAddr};

use mdns_sd::{Receiver, ServiceDaemon, ServiceEvent, ServiceInfo};

const TYPE: &str = "_relay._udp.local.";

pub fn hex(tag: [u8; 8]) -> String {
    tag.iter().map(|b| format!("{b:02x}")).collect()
}

pub struct Mdns {
    daemon: ServiceDaemon,
    browse: Option<Receiver<ServiceEvent>>,
    name: String,
    tag: String,
}

impl Mdns {
    pub fn advertise(slug: &str, tag: [u8; 8], port: u16) -> Option<Self> {
        let daemon = ServiceDaemon::new().ok()?;
        let host = format!("relay-{}.local.", &crate::random_key()[..8]);
        let props = [("tag", hex(tag))];
        let info = ServiceInfo::new(TYPE, slug, &host, "", port, &props[..]).ok()?;
        daemon.register(info.enable_addr_auto()).ok()?;
        Some(Self {
            daemon,
            browse: None,
            name: String::new(),
            tag: String::new(),
        })
    }

    pub fn browse(slug: &str, tag: [u8; 8]) -> Option<Self> {
        let daemon = ServiceDaemon::new().ok()?;
        let browse = Some(daemon.browse(TYPE).ok()?);
        Some(Self {
            daemon,
            browse,
            name: format!("{slug}.{TYPE}"),
            tag: hex(tag),
        })
    }

    /// Our room's host, once resolved. ponytail: first IPv4 address only.
    pub fn found(&self) -> Option<SocketAddr> {
        let mut hit = None;
        for event in self.browse.as_ref()?.try_iter() {
            if let ServiceEvent::ServiceResolved(s) = event
                && s.get_fullname() == self.name
                && s.get_property_val_str("tag") == Some(&self.tag)
            {
                let ip = s.get_addresses_v4().into_iter().next();
                hit = ip
                    .map(|ip| SocketAddr::new(IpAddr::V4(ip), s.get_port()))
                    .or(hit);
            }
        }
        hit
    }
}

impl Drop for Mdns {
    fn drop(&mut self) {
        // Unregisters (goodbye packets) and stops the daemon thread.
        let _ = self.daemon.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Needs multicast on some interface; CI sandboxes often have none.
    #[test]
    #[ignore = "needs multicast"]
    fn advertise_then_find() {
        let tag = crate::tag("mdns-test-room", "pw");
        let _ad = Mdns::advertise("mdns-test-room", tag, 17_499).unwrap();
        let join = Mdns::browse("mdns-test-room", tag).unwrap();
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end {
            if let Some(addr) = join.found() {
                assert_eq!(addr.port(), 17_499);
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("not found");
    }
}
