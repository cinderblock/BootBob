//! The config file. Everything has a default, so BootBob runs with no file at all.
//!
//! ```toml
//! data_dir = "/var/lib/bootbob"
//! http = "10.255.0.5:8067"
//! tftp = "10.255.0.5:69"
//! interface = "eno1"            # the DHCP listener; leave out to turn it off
//! public_url = "http://10.255.0.5:8067"
//!
//! [[stores]]                    # image stores on other hosts (see stores.rs)
//! name = "uberfall"
//! url = "http://10.255.0.3:8068"
//! ```
//!
//! An image store runs the same binary with `mode = "store"` and `main_url` pointing at
//! the main server; it serves disk images (NBD and boot files) and nothing else.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// The database and anything else BootBob writes.
    pub data_dir: PathBuf,
    /// HTTP: the web UI, the API and iPXE scripts. Machines and people both use it.
    pub http: SocketAddr,
    /// TFTP, for firmware PXE and Raspberry Pis. `None` turns it off.
    pub tftp: Option<SocketAddr>,
    /// NBD, for network root filesystems (port 10809 is NBD's own). `None` (the
    /// default) turns it off.
    pub nbd: Option<SocketAddr>,
    /// The interface the DHCP listener watches. `None` turns it off.
    pub interface: Option<String>,
    /// Advertise BootBob over mDNS (`_bootbob._tcp`) on `interface`, so BenchBob and
    /// other tools on the LAN find it. Needs `interface`.
    pub mdns: bool,
    /// The URL machines use to reach BootBob. Unset: taken from each request's `Host`.
    pub public_url: Option<String>,
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
    /// A store's main server, e.g. `http://10.255.0.5:8067`. Required in store mode.
    pub main_url: Option<String>,
    /// A store's name, as the main server's `stores` call it. Required in store mode.
    pub store_name: Option<String>,
    /// The main server's image stores. An image's source `<store>:<disk>.img` lives there.
    pub stores: Vec<StoreConfig>,
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
            http: "0.0.0.0:8067".parse().unwrap(),
            tftp: Some("0.0.0.0:69".parse().unwrap()),
            nbd: None,
            interface: None,
            mdns: true,
            public_url: None,
            boot_filename: "bootbob".into(),
            ipxe_dir: PathBuf::from("ipxe"),
            web_dir: None,
            images_dir: None,
            tools_dir: None,
            default_firmware: "efi-x64".into(),
            mode: Mode::Server,
            main_url: None,
            store_name: None,
            stores: Vec::new(),
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

    pub fn load(path: Option<&Path>) -> anyhow::Result<Config> {
        let Some(path) = path else { return Ok(Config::default()) };
        let text = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("can't read {}: {e}", path.display()))?;
        toml::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_defaults() {
        let c: Config = toml::from_str("http = \"10.255.0.5:8067\"\ninterface = \"eno1\"\n").unwrap();
        assert_eq!(c.http.to_string(), "10.255.0.5:8067");
        assert_eq!(c.interface.as_deref(), Some("eno1"));
        assert_eq!(c.boot_filename, "bootbob");
        assert!(toml::from_str::<Config>("nope = 1").is_err());
    }

    #[test]
    fn parses_stores_and_store_mode() {
        let main: Config = toml::from_str(include_str!("../../../../packaging/test-configs/main-with-store.toml")).unwrap();
        assert_eq!(main.stores.len(), 1);
        assert_eq!(main.stores[0].name, "uberfall");
        assert_eq!(main.stores[0].nbd_host(), "10.255.0.3");
        let store: Config = toml::from_str(include_str!("../../../../packaging/test-configs/store.toml")).unwrap();
        assert_eq!(store.mode, Mode::Store);
        assert_eq!(store.store_name.as_deref(), Some("uberfall"));
        assert_eq!(store.nbd.unwrap().to_string(), "10.255.0.3:10809");
    }
}
