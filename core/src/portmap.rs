//! Best-effort port mapping for the WebRTC socket: PCP (RFC 6887), then
//! NAT-PMP (RFC 6886) to the default gateway, then UPnP IGD. Blocking; the
//! caller runs it on a background thread. Dropping a [`Mapping`] deletes it.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::Duration;

const LIFETIME: u32 = 7_200;
const PCP_PORT: u16 = 5_351;

pub struct Mapping {
    pub external: SocketAddr,
    kind: Kind,
}

enum Kind {
    Pcp(SocketAddrV4, [u8; 12], SocketAddrV4),
    Pmp(SocketAddrV4, SocketAddrV4),
    Upnp(igd_next::Gateway, u16),
}

/// Map UDP `internal` (a socket bound to this machine's LAN address).
pub fn map(internal: SocketAddrV4) -> Option<Mapping> {
    let gw = netdev::get_default_gateway().ok()?.ipv4.first().copied();
    if let Some(gw) = gw.map(|ip| SocketAddrV4::new(ip, PCP_PORT)) {
        let nonce: [u8; 12] = crate::random_key().as_bytes()[..12].try_into().ok()?;
        let reply = ask(internal, gw, &pcp_map(internal, nonce, LIFETIME));
        if let Some(external) = reply.as_deref().and_then(|r| pcp_parse(r, nonce)) {
            let kind = Kind::Pcp(gw, nonce, internal);
            return Some(Mapping { external, kind });
        }
        let ip = ask(internal, gw, &[0, 0]).and_then(|r| pmp_parse_ip(&r));
        let port =
            ask(internal, gw, &pmp_map(internal.port(), LIFETIME)).and_then(|r| pmp_parse_map(&r));
        if let (Some(ip), Some(port)) = (ip, port) {
            let kind = Kind::Pmp(gw, internal);
            return Some(Mapping {
                external: SocketAddr::from((ip, port)),
                kind,
            });
        }
    }
    let opts = igd_next::SearchOptions {
        bind_addr: SocketAddr::from((*internal.ip(), 0)),
        timeout: Some(Duration::from_secs(3)),
        ..Default::default()
    };
    let gw = igd_next::search_gateway(opts).ok()?;
    let ip = gw.get_external_ip().ok()?;
    let (proto, local) = (igd_next::PortMappingProtocol::UDP, SocketAddr::V4(internal));
    gw.add_port(proto, internal.port(), local, LIFETIME, "RELAY")
        .ok()?;
    let external = SocketAddr::new(ip, internal.port());
    Some(Mapping {
        external,
        kind: Kind::Upnp(gw, internal.port()),
    })
}

impl Drop for Mapping {
    fn drop(&mut self) {
        match &self.kind {
            Kind::Pcp(gw, nonce, internal) => {
                send(*internal, *gw, &pcp_map(*internal, *nonce, 0));
            }
            Kind::Pmp(gw, internal) => {
                send(*internal, *gw, &pmp_map(internal.port(), 0));
            }
            Kind::Upnp(gw, port) => {
                let (gw, port) = (gw.clone(), *port);
                // SOAP over HTTP can take a while; never on the link thread.
                std::thread::spawn(move || {
                    gw.remove_port(igd_next::PortMappingProtocol::UDP, port)
                });
            }
        }
    }
}

fn send(from: SocketAddrV4, to: SocketAddrV4, req: &[u8]) -> Option<UdpSocket> {
    let sock = UdpSocket::bind((*from.ip(), 0)).ok()?;
    sock.send_to(req, to).ok()?;
    Some(sock)
}

/// Two tries, 250 then 500 ms. ponytail: RFC 6887 wants 3 s initial RTO;
/// routers that slow get UPnP instead.
fn ask(from: SocketAddrV4, to: SocketAddrV4, req: &[u8]) -> Option<Vec<u8>> {
    for wait in [250, 500] {
        let sock = send(from, to, req)?;
        sock.set_read_timeout(Some(Duration::from_millis(wait)))
            .ok()?;
        let mut buf = [0; 1100];
        if let Ok((n, src)) = sock.recv_from(&mut buf)
            && src == SocketAddr::V4(to)
        {
            return Some(buf[..n].to_vec());
        }
    }
    None
}

fn mapped_v4(ip: Ipv4Addr) -> [u8; 16] {
    ip.to_ipv6_mapped().octets()
}

fn pcp_map(internal: SocketAddrV4, nonce: [u8; 12], lifetime: u32) -> Vec<u8> {
    let mut p = vec![2, 1, 0, 0];
    p.extend(lifetime.to_be_bytes());
    p.extend(mapped_v4(*internal.ip()));
    p.extend(nonce);
    p.extend([17, 0, 0, 0]);
    p.extend(internal.port().to_be_bytes());
    p.extend(internal.port().to_be_bytes());
    p.extend(mapped_v4(Ipv4Addr::UNSPECIFIED));
    p
}

fn pcp_parse(r: &[u8], nonce: [u8; 12]) -> Option<SocketAddr> {
    if r.len() < 60 || r[0] != 2 || r[1] != 0x81 || r[3] != 0 || r[24..36] != nonce {
        return None;
    }
    let port = u16::from_be_bytes([r[42], r[43]]);
    let ip = std::net::Ipv6Addr::from(<[u8; 16]>::try_from(&r[44..60]).ok()?);
    let ip = ip.to_ipv4_mapped().map_or(IpAddr::V6(ip), IpAddr::V4);
    Some(SocketAddr::new(ip, port))
}

fn pmp_map(port: u16, lifetime: u32) -> Vec<u8> {
    let mut p = vec![0, 1, 0, 0];
    p.extend(port.to_be_bytes());
    p.extend(if lifetime == 0 { 0 } else { port }.to_be_bytes());
    p.extend(lifetime.to_be_bytes());
    p
}

fn pmp_parse_ip(r: &[u8]) -> Option<Ipv4Addr> {
    (r.len() >= 12 && r[..4] == [0, 128, 0, 0]).then(|| Ipv4Addr::new(r[8], r[9], r[10], r[11]))
}

fn pmp_parse_map(r: &[u8]) -> Option<u16> {
    (r.len() >= 16 && r[..4] == [0, 129, 0, 0]).then(|| u16::from_be_bytes([r[10], r[11]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nat_pmp_and_pcp_packets() {
        assert_eq!(
            pmp_map(17_492, 7_200),
            [0, 1, 0, 0, 0x44, 0x54, 0x44, 0x54, 0, 0, 0x1c, 0x20]
        );
        assert_eq!(pmp_map(17_492, 0)[6..], [0, 0, 0, 0, 0, 0]);
        let ip = [0, 128, 0, 0, 0, 0, 0, 9, 203, 0, 113, 7];
        assert_eq!(pmp_parse_ip(&ip), Some(Ipv4Addr::new(203, 0, 113, 7)));
        let map = [
            0, 129, 0, 0, 0, 0, 0, 9, 0x44, 0x54, 0x9c, 0x40, 0, 0, 0x1c, 0x20,
        ];
        assert_eq!(pmp_parse_map(&map), Some(40_000));
        let mut bad = map;
        bad[3] = 3; // result code: network failure
        assert_eq!(pmp_parse_map(&bad), None);

        let internal = SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 20), 50_000);
        let req = pcp_map(internal, [7; 12], 7_200);
        assert_eq!(
            (req.len(), &req[..2], &req[20..24]),
            (60, &[2, 1][..], &[192, 168, 1, 20][..])
        );
        // The router's answer: same layout, R bit set, assigned address.
        let mut resp = req.clone();
        resp[1] = 0x81;
        resp[42..44].copy_from_slice(&61_000u16.to_be_bytes());
        resp[44..60].copy_from_slice(&mapped_v4(Ipv4Addr::new(203, 0, 113, 7)));
        let want = SocketAddr::from(([203, 0, 113, 7], 61_000));
        assert_eq!(pcp_parse(&resp, [7; 12]), Some(want));
        assert_eq!(pcp_parse(&resp, [8; 12]), None, "someone else's nonce");
    }
}
