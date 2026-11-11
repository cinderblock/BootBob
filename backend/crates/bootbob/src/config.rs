//! The config file. Paths have defaults; a server needs at least one `[[boot]]` interface
//! to boot anything.
//!
//! A server has two separate sets of settings: the **control plane** (`[control]`, the web
//! UI and the management API, which boot nothing) and its **boot interfaces** (`[[boot]]`,
//! where machines netboot, with no UI or API). The two never share settings. When they share
//! an address and port, one socket serves both.
//!
//! ```toml
//! data_dir = "/var/lib/bootbob"
//!
//! [control]
//! http = ["10.255.0.5:8067", "127.0.0.1:8067"]
//! network = "eno1"              # the LAN: network check, time zone, mDNS
//!
//! [[boot]]
//! interface = "eno1"
//! label = "TSL LAN"
//!
//! [[boot]]
//! interface = "eno1.5"          # a VLAN subinterface the host made
//! label = "Hardware CI"
//! services = ["tftp", "http", "nbd"]
//!
//! [[stores]]                    # image stores on other hosts (see stores.rs)
//! name = "uberfall"
//! url = "http://10.255.0.3:8068"
//! ```
//!
//! An image store runs the same binary with `mode = "store"` and `main_url` pointing at
//! the main server; it serves disk images (NBD and boot files) and nothing else, on `http`
//! and `nbd`.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const HTTP_PORT: u16 = 8067;
pub const TFTP_PORT: u16 = 69;
pub const NBD_PORT: u16 = 10809;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// The database and anything else BootBob writes.
    pub data_dir: PathBuf,
    /// The web UI and the management API (server mode).
    pub control: Control,
    /// Where machines netboot (server mode). Each is one interface the host has made.
    pub boot: Vec<BootIface>,
    /// The single boot file name the network's DHCP server hands out. A TFTP request for
    /// it gets the iPXE build that matches the requester's firmware.
    pub boot_filename: String,
    /// iPXE builds, named `bootbob-<firmware>.<ext>` (see `ipxe/README.md`).
    pub ipxe_dir: PathBuf,
    /// The built web UI. Unset or missing: the API still works, the page doesn't.
    pub web_dir: Option<PathBuf>,
    /// Image files: Raspberry Pi boot trees (directories) and disk images. Unset:
    /// `<data_dir>/images`.
    pub images_dir: Option<PathBuf>,
    /// Files machines fetch to set themselves up, served at `/files/tools/`: the NBD
    /// client for diskless machines and its initramfs hook. Unset: `tools` next to
    /// `ipxe_dir` (in the image, `/usr/share/bootbob/tools`).
    pub tools_dir: Option<PathBuf>,
    /// The firmware assumed when a machine's can't be worked out.
    pub default_firmware: String,
    /// `server` (the default): the whole of BootBob. `store`: an image store on another
    /// host, serving disk images (NBD, boot files, uploads) for the main server at
    /// `main_url`, which decides which machine may open which disk.
    pub mode: Mode,
    /// A store's HTTP API (store mode only). Unset: `0.0.0.0:8067`.
    pub http: Option<SocketAddr>,
    /// A store's NBD address (store mode only, where it's required).
    pub nbd: Option<SocketAddr>,
    /// A store's main server, e.g. `http://10.255.0.5:8067`. Required in store mode.
    pub main_url: Option<String>,
    /// A store's name, as the main server's `stores` call it. Required in store mode.
    pub store_name: Option<String>,
    /// The main server's image stores. An image's source `<store>:<disk>.img` lives there.
    pub stores: Vec<StoreConfig>,
    /// The most disk space, in GiB, files fetched from the catalogue may take on this host.
    /// Past it the least recently used ones go. Disks uploaded or made by hand don't count
    /// and are never touched.
    pub cache_gib: u64,

    // Keys from before boot interfaces and the control plane were separate. Read only so
    // `validate` can say what replaces them.
    #[serde(rename = "interface")]
    pub old_interface: Option<toml::Value>,
    #[serde(rename = "tftp")]
    pub old_tftp: Option<toml::Value>,
    #[serde(rename = "public_url")]
    pub old_public_url: Option<toml::Value>,
    #[serde(rename = "mdns")]
    pub old_mdns: Option<toml::Value>,
}

/// The web UI and the management API. They boot nothing; a machine never needs them.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Control {
    /// Where the UI and API listen. A listener on a specific address only takes
    /// connections that arrive on the interface holding it (a loopback address: only from
    /// this host, such as a reverse proxy here). `0.0.0.0` takes them from anywhere,
    /// boot VLANs included, so don't use it on a host with untrusted boot interfaces.
    pub http: Vec<SocketAddr>,
    /// The interface BootBob's own LAN is on: the network check probes it, the network's
    /// time zone is learnt only there, and mDNS advertises there. Unset: none of those.
    pub network: Option<String>,
    /// Advertise BootBob over mDNS (`_bootbob._tcp`) on `network`, so BenchBob and other
    /// tools find it.
    pub mdns: bool,
}

impl Default for Control {
    fn default() -> Self {
        Control { http: vec![SocketAddr::from(([0, 0, 0, 0], HTTP_PORT))], network: None, mdns: true }
    }
}

/// What a boot interface does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Service {
    /// The receive-only DHCP listener: which firmware is asking, and machines that try to
    /// netboot but never reach BootBob.
    Listen,
    /// TFTP: iPXE for firmware PXE, and Raspberry Pi boot files.
    Tftp,
    /// HTTP for booting machines: iPXE scripts, kernels, the rollout check-in. Never the UI
    /// or the API.
    Http,
    /// NBD: network root filesystems.
    Nbd,
}

pub const ALL_SERVICES: [Service; 4] = [Service::Listen, Service::Tftp, Service::Http, Service::Nbd];

/// One interface machines netboot on.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootIface {
    /// The kernel's name for it: `eno1`, or a VLAN subinterface such as `eno1.5`. Machines
    /// are admitted to it by this name.
    pub interface: String,
    /// What the UI calls it, e.g. `Hardware CI`. Unset: the interface name.
    #[serde(default)]
    pub label: Option<String>,
    /// The address to serve on. Unset: the interface's first IPv4 address.
    #[serde(default)]
    pub address: Option<Ipv4Addr>,
    #[serde(default = "all_services")]
    pub services: Vec<Service>,
    #[serde(default = "http_port")]
    pub http_port: u16,
    #[serde(default = "tftp_port")]
    pub tftp_port: u16,
    #[serde(default = "nbd_port")]
    pub nbd_port: u16,
    /// The URL machines here are told to fetch from (`{bootbob}`). Unset:
    /// `http://<address>:<http_port>`.
    #[serde(default)]
    pub url: Option<String>,
}

fn all_services() -> Vec<Service> {
    ALL_SERVICES.to_vec()
}
fn http_port() -> u16 {
    HTTP_PORT
}
fn tftp_port() -> u16 {
    TFTP_PORT
}
fn nbd_port() -> u16 {
    NBD_PORT
}

impl BootIface {
    /// A boot interface with everything on, at `address` (for tests).
    #[cfg(test)]
    pub fn new(interface: &str, address: Option<Ipv4Addr>) -> BootIface {
        BootIface {
            interface: interface.into(),
            label: None,
            address,
            services: all_services(),
            http_port: HTTP_PORT,
            tftp_port: TFTP_PORT,
            nbd_port: NBD_PORT,
            url: None,
        }
    }

    pub fn serves(&self, s: Service) -> bool {
        self.services.contains(&s)
    }

    pub fn label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.interface)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Server,
    Store,
}

/// An image store, as the main server knows it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreConfig {
    /// What image sources call it: `uberfall` in `uberfall:golden.img`.
    pub name: String,
    /// Its HTTP API, e.g. `http://10.255.0.3:8068`.
    pub url: String,
    /// Where machines reach its NBD server. Unset: the host in `url`.
    #[serde(default)]
    pub nbd_host: Option<String>,
}

impl StoreConfig {
    /// The host machines connect to for NBD.
    pub fn nbd_host(&self) -> String {
        self.nbd_host.clone().unwrap_or_else(|| {
            let h = self.url.split("://").nth(1).unwrap_or(&self.url);
            let h = h.split('/').next().unwrap_or(h);
            h.rsplit_once(':').map(|(h, _)| h).unwrap_or(h).to_string()
        })
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            data_dir: PathBuf::from("data"),
            control: Control::default(),
            boot: Vec::new(),
            boot_filename: "bootbob".into(),
            ipxe_dir: PathBuf::from("ipxe"),
            web_dir: None,
            images_dir: None,
            tools_dir: None,
            default_firmware: "efi-x64".into(),
            mode: Mode::Server,
            http: None,
            nbd: None,
            main_url: None,
            store_name: None,
            stores: Vec::new(),
            cache_gib: 150,
            old_interface: None,
            old_tftp: None,
            old_public_url: None,
            old_mdns: None,
        }
    }
}

impl Config {
    pub fn images_dir(&self) -> PathBuf {
        self.images_dir.clone().unwrap_or_else(|| self.data_dir.join("images"))
    }

    pub fn tools_dir(&self) -> PathBuf {
        self.tools_dir.clone().unwrap_or_else(|| match self.ipxe_dir.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.join("tools"),
            _ => PathBuf::from("tools"),
        })
    }

    /// Machines' copy-on-write overlays, one directory per MAC.
    pub fn overlays_dir(&self) -> PathBuf {
        self.data_dir.join("overlays")
    }

    /// A store's HTTP address.
    pub fn store_http(&self) -> SocketAddr {
        self.http.unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], HTTP_PORT)))
    }

    /// A boot interface by name.
    pub fn boot_iface(&self, name: &str) -> Option<&BootIface> {
        self.boot.iter().find(|b| b.interface == name)
    }

    pub fn load(path: Option<&Path>) -> anyhow::Result<Config> {
        let Some(path) = path else { return Ok(Config::default()) };
        let text = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("can't read {}: {e}", path.display()))?;
        let c: Config = toml::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        c.validate().map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        Ok(c)
    }

    /// Settings that parse but don't make sense together, and keys that have moved.
    pub fn validate(&self) -> Result<(), String> {
        let moved = [
            (self.old_interface.is_some(), "interface", "name it in a [[boot]] section (interface = \"eno1\"), and as [control] network"),
            (self.old_tftp.is_some(), "tftp", "TFTP runs on each [[boot]] interface (services, tftp_port)"),
            (self.old_public_url.is_some(), "public_url", "set url on the [[boot]] interface"),
            (self.old_mdns.is_some(), "mdns", "it's [control] mdns now"),
        ];
        if let Some((_, key, instead)) = moved.iter().find(|(set, _, _)| *set) {
            return Err(format!("`{key}` has gone: {instead} (see the README's \"Running it\")"));
        }
        match self.mode {
            Mode::Store => {
                if !self.boot.is_empty() {
                    return Err("a store boots nothing, so it has no [[boot]] interfaces".into());
                }
            }
            Mode::Server => {
                if self.http.is_some() || self.nbd.is_some() {
                    return Err("`http` and `nbd` are for stores: a server listens on [control] http and on each [[boot]] \
                                interface (http_port, nbd_port)"
                        .into());
                }
                for (i, b) in self.boot.iter().enumerate() {
                    if b.interface.is_empty() {
                        return Err("a [[boot]] section needs interface".into());
                    }
                    if self.boot[..i].iter().any(|o| o.interface == b.interface) {
                        return Err(format!("[[boot]] interface {} is listed twice", b.interface));
                    }
                    if b.services.is_empty() {
                        return Err(format!("[[boot]] {} serves nothing", b.interface));
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_control_and_boot_interfaces() {
        let c: Config = toml::from_str(
            "[control]\nhttp = [\"10.255.0.5:8067\", \"127.0.0.1:8067\"]\nnetwork = \"eno1\"\n\
             [[boot]]\ninterface = \"eno1\"\n\
             [[boot]]\ninterface = \"eno1.5\"\nlabel = \"Hardware CI\"\nservices = [\"tftp\", \"nbd\"]\nurl = \"http://10.5.0.5:8067\"\n",
        )
        .unwrap();
        c.validate().unwrap();
        assert_eq!(c.control.http.len(), 2);
        assert_eq!(c.control.network.as_deref(), Some("eno1"));
        assert_eq!(c.boot.len(), 2);
        assert_eq!(c.boot[0].services, ALL_SERVICES.to_vec());
        assert_eq!(c.boot[0].http_port, 8067);
        assert_eq!(c.boot[0].label(), "eno1");
        let ci = c.boot_iface("eno1.5").unwrap();
        assert_eq!(ci.label(), "Hardware CI");
        assert!(ci.serves(Service::Nbd) && !ci.serves(Service::Http) && !ci.serves(Service::Listen));
        assert_eq!(c.boot_filename, "bootbob");
        assert!(toml::from_str::<Config>("nope = 1").is_err());
    }

    #[test]
    fn old_keys_say_what_replaces_them() {
        for (text, key) in [
            ("interface = \"eno1\"", "interface"),
            ("tftp = \"10.255.0.5:69\"", "tftp"),
            ("public_url = \"http://x\"", "public_url"),
            ("http = \"10.255.0.5:8067\"", "`http` and `nbd`"),
        ] {
            let c: Config = toml::from_str(text).unwrap();
            let e = c.validate().unwrap_err();
            assert!(e.contains(key), "{text}: {e}");
        }
        let c: Config = toml::from_str("[[boot]]\ninterface = \"eno1\"\n[[boot]]\ninterface = \"eno1\"\n").unwrap();
        assert!(c.validate().unwrap_err().contains("twice"));
    }

    #[test]
    fn parses_stores_and_store_mode() {
        let main: Config = toml::from_str(include_str!("../../../../packaging/test-configs/main-with-store.toml")).unwrap();
        main.validate().unwrap();
        assert_eq!(main.stores.len(), 1);
        assert_eq!(main.stores[0].name, "uberfall");
        assert_eq!(main.stores[0].nbd_host(), "10.255.0.3");
        let store: Config = toml::from_str(include_str!("../../../../packaging/test-configs/store.toml")).unwrap();
        store.validate().unwrap();
        assert_eq!(store.mode, Mode::Store);
        assert_eq!(store.store_name.as_deref(), Some("uberfall"));
        assert_eq!(store.nbd.unwrap().to_string(), "10.255.0.3:10809");
    }
}
