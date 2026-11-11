//! The host's network interfaces, as BootBob finds them: each one's IPv4 addresses and, for
//! a tagged subinterface such as `eno1.5`, its VLAN and parent.
//!
//! BootBob never changes the host's networking. The host makes its interfaces (netplan,
//! systemd-networkd, ops); BootBob only reads what's there.
//!
//! Sockets that serve one interface are bound to it with `SO_BINDTODEVICE` ([`bind_device`]),
//! not only to its address. Linux is a weak-host system: a packet for one interface's
//! address is accepted on any interface, so an address alone doesn't say where a request
//! came in. A device-bound socket only ever sees what arrived on that device.

use std::net::Ipv4Addr;

use serde::Serialize;

/// One network interface on this host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NetIf {
    pub name: String,
    /// IPv4 addresses with their prefix lengths, in the order the kernel lists them.
    pub addrs: Vec<(Ipv4Addr, u8)>,
    /// The 802.1Q VLAN id, for a VLAN subinterface.
    pub vlan: Option<u16>,
    /// The interface a VLAN subinterface is tagged on.
    pub parent: Option<String>,
}

impl NetIf {
    /// Its first IPv4 address.
    pub fn address(&self) -> Option<Ipv4Addr> {
        self.addrs.first().map(|(a, _)| *a)
    }
}

/// Parses `/proc/net/vlan/config` into (subinterface, VLAN id, parent).
///
/// ```text
/// VLAN Dev name    | VLAN ID
/// Name-Type: VLAN_NAME_TYPE_RAW_PLUS_VID_NO_PAD
/// eno1.5         | 5  | eno1
/// ```
pub fn parse_proc_vlan(text: &str) -> Vec<(String, u16, String)> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split('|').map(str::trim).collect();
            let [name, id, parent] = f[..] else { return None };
            Some((name.to_string(), id.parse().ok()?, parent.to_string()))
        })
        .collect()
}

/// Every interface with its addresses and VLAN. Empty off Linux.
pub fn discover() -> Vec<NetIf> {
    let vlans = std::fs::read_to_string("/proc/net/vlan/config").map(|t| parse_proc_vlan(&t)).unwrap_or_default();
    let mut out: Vec<NetIf> = imp::addresses().into_iter().fold(Vec::<NetIf>::new(), |mut acc, (name, addr)| {
        match acc.iter_mut().find(|i| i.name == name) {
            Some(i) => i.addrs.extend(addr),
            None => acc.push(NetIf { name, addrs: addr.into_iter().collect(), vlan: None, parent: None }),
        }
        acc
    });
    for i in &mut out {
        if let Some((_, id, parent)) = vlans.iter().find(|(n, _, _)| *n == i.name) {
            i.vlan = Some(*id);
            i.parent = Some(parent.clone());
        }
    }
    out
}

/// The interface that holds `ip`, if one does.
pub fn owner_of(ip: Ipv4Addr) -> Option<NetIf> {
    discover().into_iter().find(|i| i.addrs.iter().any(|(a, _)| *a == ip))
}

/// Binds a socket to one device (`SO_BINDTODEVICE`), so it only sees what arrives there.
/// Must happen before `bind`. Linux only; elsewhere it fails with `Unsupported`.
pub fn bind_device(sock: &socket2::Socket, name: &str) -> std::io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        sock.bind_device(Some(name.as_bytes()))
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let _ = (sock, name);
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "binding a socket to an interface needs Linux"))
    }
}

/// The device to bind a socket to, where that's possible (Linux). Elsewhere, for development,
/// nothing: the address alone has to do.
pub fn device_for(name: &str) -> Option<&str> {
    cfg!(target_os = "linux").then_some(name)
}

/// A TCP listener on `addr`, bound to `device` when one is given (see [`bind_device`]).
/// Needs a Tokio runtime.
pub fn tcp_listener(addr: std::net::SocketAddr, device: Option<&str>) -> std::io::Result<tokio::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let sock = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    #[cfg(unix)]
    sock.set_reuse_address(true)?;
    if let Some(d) = device {
        bind_device(&sock, d)?;
    }
    sock.set_nonblocking(true)?;
    sock.bind(&addr.into())?;
    sock.listen(1024)?;
    tokio::net::TcpListener::from_std(sock.into())
}

/// The device a control listener on `ip` should be bound to: the interface holding that
/// address. `None` for loopback (only this host reaches it) and for an unspecified address
/// (every interface, on purpose).
pub fn control_device(ip: std::net::IpAddr) -> Option<String> {
    match ip {
        std::net::IpAddr::V4(a) if !a.is_loopback() && !a.is_unspecified() => owner_of(a).map(|i| i.name),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::ffi::CStr;
    use std::net::Ipv4Addr;

    /// (interface, IPv4 address and prefix), and interfaces with no IPv4 address once with
    /// `None`, from `getifaddrs`.
    pub fn addresses() -> Vec<(String, Option<(Ipv4Addr, u8)>)> {
        let mut out = Vec::new();
        // SAFETY: getifaddrs hands us a list we walk read-only and free once; every pointer
        // is checked for null before it's read, and sockaddrs are read as the family says.
        unsafe {
            let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
            if libc::getifaddrs(&mut head) != 0 {
                return out;
            }
            let mut p = head;
            while !p.is_null() {
                let ifa = &*p;
                p = ifa.ifa_next;
                if ifa.ifa_name.is_null() {
                    continue;
                }
                let name = CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned();
                let addr = ifa.ifa_addr;
                if addr.is_null() || (*addr).sa_family as i32 != libc::AF_INET {
                    out.push((name, None));
                    continue;
                }
                let sin = &*(addr as *const libc::sockaddr_in);
                let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                let prefix = if ifa.ifa_netmask.is_null() {
                    32
                } else {
                    u32::from_be((*(ifa.ifa_netmask as *const libc::sockaddr_in)).sin_addr.s_addr).count_ones() as u8
                };
                out.push((name, Some((ip, prefix))));
            }
            libc::freeifaddrs(head);
        }
        out
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use std::net::Ipv4Addr;

    pub fn addresses() -> Vec<(String, Option<(Ipv4Addr, u8)>)> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc_vlan() {
        let text = "VLAN Dev name\t | VLAN ID\nName-Type: VLAN_NAME_TYPE_RAW_PLUS_VID_NO_PAD\neno1.5         | 5  | eno1\nci             | 40 | enp2s0\n";
        assert_eq!(
            parse_proc_vlan(text),
            vec![("eno1.5".to_string(), 5, "eno1".to_string()), ("ci".to_string(), 40, "enp2s0".to_string())]
        );
        assert!(parse_proc_vlan("").is_empty());
    }

    #[test]
    fn address() {
        let i = NetIf { name: "eno1.5".into(), addrs: vec![(Ipv4Addr::new(10, 5, 0, 5), 24)], vlan: Some(5), parent: Some("eno1".into()) };
        assert_eq!(i.address(), Some(Ipv4Addr::new(10, 5, 0, 5)));
    }
}
