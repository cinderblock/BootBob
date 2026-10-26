//! mDNS: BootBob says it's here, as `_bootbob._tcp` on the LAN interface, so tools on
//! the same network (BenchBob) find it without being told an address. TXT records: the
//! URL machines and tools use, the version, and what it can do.
//!
//! Only on the interface the DHCP listener watches (`interface` in the config): a server
//! with public addresses or container bridges shouldn't announce itself there.

use mdns_sd::{IfKind, ServiceDaemon, ServiceInfo};

use crate::config::Config;

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
pub fn txt(config: &Config) -> Vec<(String, String)> {
    let ip = config.http.ip();
    let url = config.public_url.clone().unwrap_or_else(|| format!("http://{}:{}", ip, config.http.port()));
    let mut caps = vec!["ipxe", "pi", "usb"];
    if config.nbd.is_some() {
        caps.push("nbd");
    }
    vec![("url".into(), url), ("version".into(), env!("CARGO_PKG_VERSION").into()), ("caps".into(), caps.join(","))]
}

/// Starts advertising. `None` when it's off (`mdns = false`, or no `interface`). Keep the
/// daemon alive for as long as BootBob should be advertised.
pub fn advertise(config: &Config) -> anyhow::Result<Option<ServiceDaemon>> {
    let Some(iface) = config.interface.clone().filter(|_| config.mdns) else { return Ok(None) };
    let ip = config.http.ip();
    let host = hostname();
    let d = ServiceDaemon::new()?;
    d.disable_interface(IfKind::All)?;
    d.enable_interface(IfKind::Name(iface.clone()))?;
    let props: Vec<(String, String)> = txt(config);
    let props: Vec<(&str, &str)> = props.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let info = if ip.is_unspecified() {
        ServiceInfo::new(SERVICE, &format!("BootBob on {host}"), &format!("{host}.local."), "", config.http.port(), &props[..])?
            .enable_addr_auto()
    } else {
        ServiceInfo::new(SERVICE, &format!("BootBob on {host}"), &format!("{host}.local."), ip, config.http.port(), &props[..])?
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
        let mut c = Config { http: "10.255.0.5:8067".parse().unwrap(), ..Config::default() };
        let t = txt(&c);
        assert_eq!(t[0], ("url".into(), "http://10.255.0.5:8067".into()));
        assert_eq!(t[2].1, "ipxe,pi,usb");
        c.nbd = Some("10.255.0.5:10809".parse().unwrap());
        c.public_url = Some("http://pxe.example:8067".into());
        let t = txt(&c);
        assert_eq!(t[0].1, "http://pxe.example:8067");
        assert_eq!(t[2].1, "ipxe,pi,usb,nbd");
        assert!(!hostname().is_empty());
    }

    #[test]
    fn off_without_an_interface_or_when_disabled() {
        assert!(advertise(&Config::default()).unwrap().is_none());
        let c = Config { interface: Some("eth0".into()), mdns: false, ..Config::default() };
        assert!(advertise(&c).unwrap().is_none());
    }
}
