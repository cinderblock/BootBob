//! mDNS: BootBob says it's here, as `_bootbob._tcp` on the LAN interface, so tools on
//! the same network (BenchBob) find it without being told an address. TXT records: the
//! URL machines and tools use, the version, and what it can do.
//!
//! Only on the control network (`[control] network` in the config): a server with public
//! addresses, container bridges or untrusted boot VLANs shouldn't announce itself there.

use std::net::Ipv4Addr;

use mdns_sd::{IfKind, ServiceDaemon, ServiceInfo};

use crate::state::State;

pub const SERVICE: &str = "_bootbob._tcp.local.";

/// This machine's hostname (`steamboat`), for the instance and host names. In a
/// host-network container that's the host's.
fn hostname() -> String {
    let name = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .unwrap_or_default();
    let name = name.split('.').next().unwrap_or("").trim().to_string();
    if name.is_empty() { "bootbob".into() } else { name }
}

/// What the TXT records say, for the tests and the advertisement.
pub fn txt(url: &str, nbd: bool) -> Vec<(String, String)> {
    let mut caps = vec!["ipxe", "pi", "usb"];
    if nbd {
        caps.push("nbd");
    }
    vec![("url".into(), url.into()), ("version".into(), env!("CARGO_PKG_VERSION").into()), ("caps".into(), caps.join(","))]
}

/// What to advertise: the boot interface on the control network (else the first one), as
/// its URL, its address and whether it serves NBD. `None` when it's off (`mdns = false`, or
/// no `network`) or there's nothing to point at.
pub fn what(st: &State) -> Option<(String, String, Option<Ipv4Addr>, bool)> {
    let c = &st.config.control;
    let iface = c.network.clone().filter(|_| c.mdns)?;
    let b = st.config.boot_iface(&iface).or(st.config.boot.first())?;
    let url = st.boot_url(&b.interface)?;
    Some((iface, url, st.boot_address(&b.interface), b.serves(crate::config::Service::Nbd)))
}

/// Starts advertising. `None` when it's off. Keep the daemon alive for as long as BootBob
/// should be advertised.
pub fn advertise(st: &State) -> anyhow::Result<Option<ServiceDaemon>> {
    let Some((iface, url, ip, nbd)) = what(st) else { return Ok(None) };
    let port = url.rsplit_once(':').and_then(|(_, p)| p.trim_end_matches('/').parse().ok()).unwrap_or(crate::config::HTTP_PORT);
    let host = hostname();
    let d = ServiceDaemon::new()?;
    d.disable_interface(IfKind::All)?;
    d.enable_interface(IfKind::Name(iface.clone()))?;
    let props: Vec<(String, String)> = txt(&url, nbd);
    let props: Vec<(&str, &str)> = props.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let info = match ip {
        Some(ip) => {
            ServiceInfo::new(SERVICE, &format!("BootBob on {host}"), &format!("{host}.local."), std::net::IpAddr::V4(ip), port, &props[..])?
        }
        None => {
            ServiceInfo::new(SERVICE, &format!("BootBob on {host}"), &format!("{host}.local."), "", port, &props[..])?.enable_addr_auto()
        }
    };
    d.register(info)?;
    tracing::info!(iface, host, "mDNS: advertising {SERVICE}");
    Ok(Some(d))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn txt_records() {
        let t = txt("http://10.255.0.5:8067", false);
        assert_eq!(t[0], ("url".into(), "http://10.255.0.5:8067".into()));
        assert_eq!(t[2].1, "ipxe,pi,usb");
        assert_eq!(txt("http://pxe.example:8067", true)[2].1, "ipxe,pi,usb,nbd");
        assert!(!hostname().is_empty());
    }

    #[test]
    fn off_without_a_network_or_when_disabled() {
        use crate::config::{BootIface, Config, Control};
        let dir = tempfile::tempdir().unwrap();
        let open = |control: Control| {
            let boot = vec![BootIface::new("eth0", Some(Ipv4Addr::new(10, 255, 0, 5)))];
            State::open(Config { data_dir: dir.path().to_path_buf(), control, boot, ..Config::default() }).unwrap()
        };
        assert!(what(&open(Control::default())).is_none());
        assert!(what(&open(Control { network: Some("eth0".into()), mdns: false, ..Control::default() })).is_none());
        let on = what(&open(Control { network: Some("eth0".into()), ..Control::default() })).unwrap();
        assert_eq!(on.1, "http://10.255.0.5:8067");
        assert!(on.3);
    }
}
