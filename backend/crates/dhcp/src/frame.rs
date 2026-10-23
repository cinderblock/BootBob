//! Ethernet II / IPv4 / UDP frame parsing, for the passive listener.

use std::net::Ipv4Addr;

use crate::Mac;

/// A UDP datagram seen on the wire, with its link-layer addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Udp<'a> {
    pub src_mac: Mac,
    pub dst_mac: Mac,
    pub src_ip: Ipv4Addr,
    pub dst_ip: Ipv4Addr,
    pub src_port: u16,
    pub dst_port: u16,
    pub payload: &'a [u8],
}

/// Parses an untagged Ethernet II frame carrying IPv4/UDP. Fragments (other than a first
/// fragment) and anything else return `None`.
pub fn parse(frame: &[u8]) -> Option<Udp<'_>> {
    let eth = frame.get(..14)?;
    if eth[12..14] != [0x08, 0x00] {
        return None;
    }
    let ip = &frame[14..];
    let ihl = (*ip.first()? & 0x0f) as usize * 4;
    if ip[0] >> 4 != 4 || ihl < 20 || ip.len() < ihl + 8 || ip[9] != 17 {
        return None;
    }
    let frag_offset = u16::from_be_bytes([ip[6], ip[7]]) & 0x1fff;
    if frag_offset != 0 {
        return None;
    }
    let total = (u16::from_be_bytes([ip[2], ip[3]]) as usize).min(ip.len());
    let udp = &ip[ihl..total];
    let udp_len = (u16::from_be_bytes([udp[4], udp[5]]) as usize).clamp(8, udp.len());
    Some(Udp {
        dst_mac: Mac(eth[0..6].try_into().unwrap()),
        src_mac: Mac(eth[6..12].try_into().unwrap()),
        src_ip: Ipv4Addr::new(ip[12], ip[13], ip[14], ip[15]),
        dst_ip: Ipv4Addr::new(ip[16], ip[17], ip[18], ip[19]),
        src_port: u16::from_be_bytes([udp[0], udp[1]]),
        dst_port: u16::from_be_bytes([udp[2], udp[3]]),
        payload: &udp[8..udp_len],
    })
}

/// Builds a frame, for tests.
pub fn build(src_mac: Mac, dst_mac: Mac, src: (Ipv4Addr, u16), dst: (Ipv4Addr, u16), payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(42 + payload.len());
    f.extend_from_slice(&dst_mac.0);
    f.extend_from_slice(&src_mac.0);
    f.extend_from_slice(&[0x08, 0x00]);
    let total = (20 + 8 + payload.len()) as u16;
    f.extend_from_slice(&[0x45, 0, (total >> 8) as u8, total as u8, 0, 0, 0, 0, 64, 17, 0, 0]);
    f.extend_from_slice(&src.0.octets());
    f.extend_from_slice(&dst.0.octets());
    f.extend_from_slice(&src.1.to_be_bytes());
    f.extend_from_slice(&dst.1.to_be_bytes());
    f.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
    f.extend_from_slice(&[0, 0]);
    f.extend_from_slice(payload);
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let a: Mac = "52:54:00:12:34:56".parse().unwrap();
        let b = Mac([0xff; 6]);
        let f = build(a, b, (Ipv4Addr::UNSPECIFIED, 68), (Ipv4Addr::BROADCAST, 67), b"hello");
        let u = parse(&f).unwrap();
        assert_eq!(u.src_mac, a);
        assert_eq!(u.dst_mac, b);
        assert_eq!((u.src_port, u.dst_port), (68, 67));
        assert_eq!(u.dst_ip, Ipv4Addr::BROADCAST);
        assert_eq!(u.payload, b"hello");
        // Ethernet padding past the IP total length is ignored.
        let mut padded = f.clone();
        padded.extend_from_slice(&[0; 20]);
        assert_eq!(parse(&padded).unwrap().payload, b"hello");
    }

    #[test]
    fn ignores_non_udp_and_fragments() {
        let a = Mac([2; 6]);
        let mut f = build(a, a, (Ipv4Addr::LOCALHOST, 1), (Ipv4Addr::LOCALHOST, 2), b"x");
        f[14 + 9] = 6;
        assert!(parse(&f).is_none());
        let mut f = build(a, a, (Ipv4Addr::LOCALHOST, 1), (Ipv4Addr::LOCALHOST, 2), b"x");
        f[14 + 7] = 8;
        assert!(parse(&f).is_none());
        assert!(parse(&f[..20]).is_none());
    }
}
