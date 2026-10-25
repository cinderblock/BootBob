//! Which machine gets which NBD export.
//!
//! An export is a disk image (`.img`) in the images directory, named by its file name.
//! A machine may use it only if it's the source of the Pi image the machine is assigned
//! (or would get as the Pi default), or of the image it's running now. So a one-shot
//! assignment, used up as the boot starts, still covers the root filesystem for the rest
//! of that boot, and nobody else on the network can open someone's root disk.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use bootbob_nbd::{Export, Exports, Stats};

use crate::state::State;

pub struct Nbd {
    pub st: Arc<State>,
}

impl Nbd {
    /// The disk images this peer may use.
    fn allowed(&self, peer: SocketAddr) -> Result<Vec<String>, String> {
        let ip = match peer.ip() {
            IpAddr::V4(a) => a,
            IpAddr::V6(a) => a.to_ipv4_mapped().ok_or("IPv6 clients aren't supported")?,
        };
        let mac = self.st.mac_for(ip).ok_or_else(|| format!("BootBob doesn't know which machine {ip} is"))?;
        let dev = self.st.device(mac).ok_or_else(|| format!("{mac} has never netbooted from BootBob"))?;
        let mut images: Vec<String> = self.st.resolve_pi(&dev).map(|r| r.image).into_iter().collect();
        images.extend(dev.last_boot.as_ref().map(|b| b.image.clone()));
        Ok(images.iter().filter_map(|id| self.st.image(id)?.pi).map(|p| p.source).filter(|s| s.ends_with(".img")).collect())
    }
}

impl Exports for Nbd {
    fn open(&self, name: &str, peer: SocketAddr) -> Result<Export, String> {
        let path = bootbob_pi::resolve_source(&self.st.config.images_dir(), name).map_err(|_| format!("no export {name:?}"))?;
        if !path.is_file() {
            return Err(format!("no export {name:?}"));
        }
        if !self.allowed(peer)?.iter().any(|s| s == name) {
            return Err(format!("{name} isn't the disk of an image assigned to this machine"));
        }
        Ok(Export { name: name.to_string(), path, read_only: false })
    }

    fn list(&self, peer: SocketAddr) -> Vec<String> {
        self.allowed(peer).unwrap_or_default()
    }

    fn connected(&self, export: &Export, peer: SocketAddr) {
        let mac = self.mac(peer);
        self.st.event(mac, "nbd", format!("Connected to disk {} (NBD)", export.name));
    }

    fn disconnected(&self, export: &Export, peer: SocketAddr, stats: Stats, why: &str) {
        let mac = self.mac(peer);
        let mb = |b: u64| b.div_ceil(1 << 20);
        self.st.event(mac, "nbd", format!("Disk {} (NBD) {why}: read {} MB, wrote {} MB", export.name, mb(stats.read), mb(stats.written)));
    }
}

impl Nbd {
    fn mac(&self, peer: SocketAddr) -> Option<bootbob_dhcp::Mac> {
        match peer.ip() {
            IpAddr::V4(a) => self.st.mac_for(a),
            IpAddr::V6(a) => a.to_ipv4_mapped().and_then(|a| self.st.mac_for(a)),
        }
    }
}

/// Creates an empty (sparse) disk image in the images directory, for a machine to
/// partition and fill over NBD.
pub fn create_disk(st: &State, name: &str, size: u64) -> Result<(), String> {
    let ok_name = name.ends_with(".img")
        && name.len() > 4
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        && !name.starts_with('.');
    if !ok_name {
        return Err("a disk name is letters, digits, '-', '_' and '.', ending in .img".into());
    }
    if !(1 << 30..=4 << 40).contains(&size) || !size.is_multiple_of(512) {
        return Err("a disk is 1 GiB to 4 TiB, in whole 512-byte sectors".into());
    }
    let path = st.config.images_dir().join(name);
    let f = std::fs::OpenOptions::new().write(true).create_new(true).open(&path).map_err(|e| format!("can't create {name}: {e}"))?;
    f.set_len(size).map_err(|e| format!("can't size {name}: {e}"))?;
    st.event(None, "disk", format!("Created empty disk {name} ({} GiB)", size >> 30));
    Ok(())
}
