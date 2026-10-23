//! The passive listener: a receive-only packet socket on one interface.
//!
//! It reports DHCP messages (from clients and from servers) and TFTP read requests, with
//! the sender's MAC. It is only ever read from; nothing is transmitted on it.

use std::net::Ipv4Addr;

use crate::message::Message;
use crate::{Mac, frame};

/// Something the listener saw.
#[derive(Debug, Clone)]
pub enum Seen {
    /// A DHCP message. `from_server` is true for server-to-client traffic (source port 67).
    Dhcp { src_mac: Mac, src_ip: Ipv4Addr, dst_ip: Ipv4Addr, from_server: bool, msg: Message },
    /// A TFTP read request arriving at port 69.
    TftpRequest { src_mac: Mac, src_ip: Ipv4Addr, path: String },
}

/// Classifies one Ethernet frame.
pub fn classify(raw: &[u8]) -> Option<Seen> {
    let u = frame::parse(raw)?;
    match (u.src_port, u.dst_port) {
        (68, 67) | (67, 68) | (67, 67) => Some(Seen::Dhcp {
            src_mac: u.src_mac,
            src_ip: u.src_ip,
            dst_ip: u.dst_ip,
            from_server: u.src_port == 67 && u.dst_port == 68,
            msg: Message::parse(u.payload)?,
        }),
        (_, 69) if u.payload.get(..2) == Some(&[0, 1]) => {
            let rest = &u.payload[2..];
            let end = rest.iter().position(|&b| b == 0)?;
            Some(Seen::TftpRequest { src_mac: u.src_mac, src_ip: u.src_ip, path: String::from_utf8_lossy(&rest[..end]).into_owned() })
        }
        _ => None,
    }
}

/// Listens on `interface` until an error, calling `seen` for each match. Linux only; on
/// other systems it returns `Unsupported` at once.
pub async fn listen(interface: &str, seen: impl FnMut(Seen) + Send) -> std::io::Result<()> {
    imp::listen(interface, seen).await
}

#[cfg(target_os = "linux")]
mod imp {
    use std::ffi::CString;
    use std::io;
    use std::mem::{size_of, zeroed};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    use tokio::io::unix::AsyncFd;

    use super::{Seen, classify};

    const ETH_P_IP: u16 = 0x0800;
    const PACKET_OUTGOING: u8 = 4;

    /// `tcpdump -dd 'ip and udp and (port 67 or port 68 or dst port 69)'`: drop everything
    /// else in the kernel, so the busy server's other traffic never reaches us.
    const FILTER: [(u16, u8, u8, u32); 16] = [
        (0x28, 0, 0, 0x0000000c),
        (0x15, 0, 13, 0x00000800),
        (0x30, 0, 0, 0x00000017),
        (0x15, 0, 11, 0x00000011),
        (0x28, 0, 0, 0x00000014),
        (0x45, 9, 0, 0x00001fff),
        (0xb1, 0, 0, 0x0000000e),
        (0x48, 0, 0, 0x0000000e),
        (0x15, 5, 0, 0x00000043),
        (0x15, 4, 0, 0x00000044),
        (0x48, 0, 0, 0x00000010),
        (0x15, 2, 0, 0x00000043),
        (0x15, 1, 0, 0x00000044),
        (0x15, 0, 1, 0x00000045),
        (0x6, 0, 0, 0x00040000),
        (0x6, 0, 0, 0x00000000),
    ];

    fn check(r: libc::c_int) -> io::Result<libc::c_int> {
        if r < 0 { Err(io::Error::last_os_error()) } else { Ok(r) }
    }

    fn open(interface: &str) -> io::Result<OwnedFd> {
        let name = CString::new(interface).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad interface name"))?;
        // SAFETY: plain libc calls on a descriptor we own; every pointer passed is to a live
        // local of the declared size.
        unsafe {
            let ifindex = libc::if_nametoindex(name.as_ptr());
            if ifindex == 0 {
                return Err(io::Error::new(io::ErrorKind::NotFound, format!("no interface {interface}")));
            }
            let fd = check(libc::socket(
                libc::AF_PACKET,
                libc::SOCK_RAW | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                ETH_P_IP.to_be() as libc::c_int,
            ))?;
            let fd = OwnedFd::from_raw_fd(fd);
            let mut prog: Vec<libc::sock_filter> = FILTER.iter().map(|&(code, jt, jf, k)| libc::sock_filter { code, jt, jf, k }).collect();
            let fprog = libc::sock_fprog { len: prog.len() as u16, filter: prog.as_mut_ptr() };
            check(libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ATTACH_FILTER,
                &fprog as *const _ as *const libc::c_void,
                size_of::<libc::sock_fprog>() as libc::socklen_t,
            ))?;
            let mut sll: libc::sockaddr_ll = zeroed();
            sll.sll_family = libc::AF_PACKET as u16;
            sll.sll_protocol = ETH_P_IP.to_be();
            sll.sll_ifindex = ifindex as i32;
            check(libc::bind(
                fd.as_raw_fd(),
                &sll as *const _ as *const libc::sockaddr,
                size_of::<libc::sockaddr_ll>() as libc::socklen_t,
            ))?;
            // Frames from other interfaces may have queued before bind(); drop them.
            let mut buf = [0u8; 64];
            while libc::recv(fd.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) > 0 {}
            Ok(fd)
        }
    }

    pub async fn listen(interface: &str, mut seen: impl FnMut(Seen) + Send) -> io::Result<()> {
        let fd = open(interface)?;
        // SAFETY: an OwnedFd always returns the descriptor it owns, which stays open until
        // the AsyncFd (its owner) is dropped.
        let fd = unsafe { AsyncFd::register_with_interest(fd, tokio::io::Interest::READABLE) }?;
        tracing::info!(interface, "DHCP listener: watching (receive-only)");
        let mut buf = vec![0u8; 65536];
        loop {
            let mut guard = fd.readable().await?;
            let res = guard.try_io(|inner| {
                // SAFETY: recvfrom into a buffer and a sockaddr_ll we own, sizes passed.
                unsafe {
                    let mut sll: libc::sockaddr_ll = zeroed();
                    let mut len = size_of::<libc::sockaddr_ll>() as libc::socklen_t;
                    let n = libc::recvfrom(
                        inner.as_raw_fd(),
                        buf.as_mut_ptr() as *mut libc::c_void,
                        buf.len(),
                        0,
                        &mut sll as *mut _ as *mut libc::sockaddr,
                        &mut len,
                    );
                    if n < 0 { Err(io::Error::last_os_error()) } else { Ok((n as usize, sll.sll_pkttype)) }
                }
            });
            match res {
                Ok(Ok((n, pkttype))) => {
                    // Our own transmissions (TFTP replies, probes) are not observations.
                    if pkttype != PACKET_OUTGOING
                        && let Some(s) = classify(&buf[..n])
                    {
                        seen(s);
                    }
                }
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => continue,
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::Seen;

    pub async fn listen(_interface: &str, _seen: impl FnMut(Seen) + Send) -> std::io::Result<()> {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "the DHCP listener needs Linux"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{OPT_ARCH, OPT_MESSAGE_TYPE};

    #[test]
    fn classifies_dhcp_and_tftp() {
        let client: Mac = "52:54:00:12:34:56".parse().unwrap();
        let mut m = Message::request(7, client);
        m.set_opt(OPT_MESSAGE_TYPE, [1]);
        m.set_opt(OPT_ARCH, [0, 7]);
        let f = frame::build(client, Mac([0xff; 6]), (Ipv4Addr::UNSPECIFIED, 68), (Ipv4Addr::BROADCAST, 67), &m.encode());
        match classify(&f) {
            Some(Seen::Dhcp { src_mac, from_server: false, msg, .. }) => {
                assert_eq!(src_mac, client);
                assert_eq!(msg.arches(), vec![7]);
            }
            other => panic!("{other:?}"),
        }

        let rrq = bootbob_tftp_rrq("bootbob");
        let f = frame::build(client, Mac([2; 6]), (Ipv4Addr::new(10, 255, 7, 9), 1234), (Ipv4Addr::new(10, 255, 0, 5), 69), &rrq);
        match classify(&f) {
            Some(Seen::TftpRequest { src_ip, path, .. }) => {
                assert_eq!(src_ip, Ipv4Addr::new(10, 255, 7, 9));
                assert_eq!(path, "bootbob");
            }
            other => panic!("{other:?}"),
        }

        let f = frame::build(client, Mac([2; 6]), (Ipv4Addr::LOCALHOST, 5000), (Ipv4Addr::LOCALHOST, 53), b"dns");
        assert!(classify(&f).is_none());
    }

    fn bootbob_tftp_rrq(path: &str) -> Vec<u8> {
        let mut v = vec![0, 1];
        v.extend_from_slice(path.as_bytes());
        v.extend_from_slice(b"\0octet\0");
        v
    }
}
