//! Which machine gets which NBD export.
//!
//! An export is a disk image (`.img`) in the images directory, named by its file name.
//! A machine may use it only if it's the source of the Pi image the machine is assigned
//! (or would get as the Pi default), or of the image it's running now. So a one-shot
//! assignment, used up as the boot starts, still covers the root filesystem for the rest
//! of that boot, and nobody else on the network can open someone's root disk.
//!
//! A Pi image marked copy-on-write hands each machine its own overlay of the disk
//! (`<data_dir>/overlays/<mac>/<disk>.{ovl,map}`, see [`bootbob_cow`]); the image itself
//! is then never written, so one golden image serves any number of Pis. Resetting a
//! machine's overlay puts it back on the pristine image at its next boot.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use bootbob_cow::Cow;
use bootbob_dhcp::Mac;
use bootbob_nbd::{Export, Exports, Stats};
use serde::{Deserialize, Serialize};

use crate::state::State;

pub struct Nbd {
    pub st: Arc<State>,
}

/// Where a machine's overlays live.
pub fn overlay_dir(st: &State, mac: Mac) -> PathBuf {
    let m = mac.0;
    st.config.overlays_dir().join(format!("{:02x}-{:02x}-{:02x}-{:02x}-{:02x}-{:02x}", m[0], m[1], m[2], m[3], m[4], m[5]))
}

/// An overlay's name: the disk image's, without `.img` (`desky.img` → `desky.ovl`).
pub fn overlay_name(source: &str) -> &str {
    source.strip_suffix(".img").unwrap_or(source)
}

/// A disk a peer may use, and whether through an overlay.
struct Allowed {
    source: String,
    cow: bool,
}

impl Nbd {
    fn mac(&self, peer: SocketAddr) -> Option<Mac> {
        match peer.ip() {
            IpAddr::V4(a) => self.st.mac_for(a),
            IpAddr::V6(a) => a.to_ipv4_mapped().and_then(|a| self.st.mac_for(a)),
        }
    }

    /// The disk images this peer may use.
    fn allowed(&self, peer: SocketAddr) -> Result<(Mac, Vec<Allowed>), String> {
        let ip = match peer.ip() {
            IpAddr::V4(a) => a,
            IpAddr::V6(a) => a.to_ipv4_mapped().ok_or("IPv6 clients aren't supported")?,
        };
        let mac = self.st.mac_for(ip).ok_or_else(|| format!("BootBob doesn't know which machine {ip} is"))?;
        let dev = self.st.device(mac).ok_or_else(|| format!("{mac} has never netbooted from BootBob"))?;
        // What it would boot now as a Pi or a PC, and what it is running.
        let mut images: Vec<String> = self.st.resolve_pi(&dev).map(|r| r.image).into_iter().collect();
        images.push(self.st.resolve(&dev).image);
        images.extend(dev.last_boot.as_ref().map(|b| b.image.clone()));
        images.sort();
        images.dedup();
        let mut disks: Vec<Allowed> = images
            .iter()
            .filter_map(|id| self.st.image(id))
            .flat_map(|i| {
                let pi = i.pi.map(|p| Allowed { source: p.source, cow: p.cow });
                let pc = i.disk.map(|d| Allowed { source: d.source, cow: d.cow });
                pi.into_iter().chain(pc)
            })
            .filter(|a| a.source.ends_with(".img"))
            .collect();
        disks.sort_by(|a, b| a.source.cmp(&b.source));
        disks.dedup_by(|a, b| a.source == b.source);
        Ok((mac, disks))
    }
}

impl Exports for Nbd {
    fn open(&self, name: &str, peer: SocketAddr) -> Result<Export, String> {
        let path = bootbob_pi::resolve_source(&self.st.config.images_dir(), name).map_err(|_| format!("no export {name:?}"))?;
        if !path.is_file() {
            return Err(format!("no export {name:?}"));
        }
        let (mac, disks) = self.allowed(peer)?;
        let Some(disk) = disks.iter().find(|d| d.source == name) else {
            return Err(format!("{name} isn't the disk of an image assigned to this machine"));
        };
        let overlay = disk.cow.then(|| (overlay_dir(&self.st, mac), overlay_name(name).to_string()));
        Ok(Export { name: name.to_string(), path, read_only: false, overlay })
    }

    fn list(&self, peer: SocketAddr) -> Vec<String> {
        self.allowed(peer).map(|(_, d)| d.into_iter().map(|d| d.source).collect()).unwrap_or_default()
    }

    fn connected(&self, export: &Export, peer: SocketAddr) {
        let how = if export.overlay.is_some() { ", through its own overlay" } else { "" };
        self.st.event(self.mac(peer), "nbd", format!("Connected to disk {} (NBD{how})", export.name));
    }

    fn disconnected(&self, export: &Export, peer: SocketAddr, stats: Stats, why: &str) {
        let mb = |b: u64| b.div_ceil(1 << 20);
        self.st.event(
            self.mac(peer),
            "nbd",
            format!("Disk {} (NBD) {why}: read {} MB, wrote {} MB", export.name, mb(stats.read), mb(stats.written)),
        );
    }
}

/// A machine's copy-on-write overlay, for the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlayInfo {
    /// The disk image it is over, e.g. `pios.img`.
    pub source: String,
    /// Bytes the overlay occupies on disk.
    pub allocated: u64,
    /// A machine is connected to it now (so it can't be reset).
    pub in_use: bool,
}

/// The overlays a machine has, here and (on the main server) on every store. Blocking.
pub fn overlays(st: &State, mac: Mac) -> Vec<OverlayInfo> {
    let mut out = local_overlays(st, mac);
    if st.config.mode == crate::config::Mode::Server {
        for s in &st.config.stores {
            out.extend(crate::stores::overlays(s, mac));
        }
    }
    out
}

/// The overlays a machine has on this host.
pub fn local_overlays(st: &State, mac: Mac) -> Vec<OverlayInfo> {
    let dir = overlay_dir(st, mac);
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut out: Vec<OverlayInfo> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let stem = name.strip_suffix(".map")?.to_string();
            Some(OverlayInfo {
                source: format!("{stem}.img"),
                allocated: Cow::allocated(&dir, &stem).unwrap_or(0),
                in_use: st.nbd_live.in_use(&bootbob_cow::overlay_paths(&dir, &stem).1),
            })
        })
        .collect();
    out.sort_by(|a, b| a.source.cmp(&b.source));
    out
}

/// Deletes a machine's overlay of `source`: back on the pristine image at its next boot.
/// Refused while the machine is connected to it. `Ok(true)` when something was removed.
pub fn reset_overlay(st: &State, mac: Mac, source: &str) -> Result<bool, (u16, String)> {
    if let Some((store, disk)) = source.split_once(':') {
        let s = st.config.stores.iter().find(|s| s.name == store).ok_or((404, format!("no store {store:?}")))?;
        let r = crate::stores::reset_overlay(s, mac, disk)?;
        if r {
            st.event(Some(mac), "disk", format!("Overlay of {source} reset: back on the image at the next boot"));
        }
        return Ok(r);
    }
    let dir = overlay_dir(st, mac);
    let name = overlay_name(source);
    if !Cow::exists(&dir, name) {
        return Ok(false);
    }
    if st.nbd_live.in_use(&bootbob_cow::overlay_paths(&dir, name).1) {
        return Err((409, format!("{} is running from its overlay of {source}; power it off first", mac.short())));
    }
    Cow::remove(&dir, name).map_err(|e| (500, format!("can't remove the overlay: {e}")))?;
    st.event(Some(mac), "disk", format!("Overlay of {source} reset: back on the image at the next boot"));
    Ok(true)
}

/// A disk image's file name: letters, digits, `-`, `_` and `.`, ending in `.img`.
pub fn check_disk_name(name: &str) -> Result<(), String> {
    let ok = name.ends_with(".img")
        && name.len() > 4
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        && !name.starts_with('.');
    if ok { Ok(()) } else { Err("a disk name is letters, digits, '-', '_' and '.', ending in .img".into()) }
}

/// Deletes a disk image and every machine's overlay of it. Refused while an image uses
/// it or a machine is connected to it.
pub fn delete_disk(st: &State, name: &str) -> Result<(), (u16, String)> {
    check_disk_name(name).map_err(|e| (400, e))?;
    let path = st.config.images_dir().join(name);
    if !path.is_file() {
        return Err((404, format!("no disk {name}")));
    }
    let users: Vec<String> = st
        .images()
        .into_iter()
        .filter(|i| i.pi.as_ref().is_some_and(|p| p.source == name) || i.disk.as_ref().is_some_and(|d| d.source == name))
        .map(|i| i.id)
        .collect();
    if !users.is_empty() {
        return Err((409, format!("{name} is used by image(s) {}", users.join(", "))));
    }
    if st.nbd_live.in_use(&path) {
        return Err((409, format!("{name} is in use over NBD")));
    }
    let stem = overlay_name(name);
    if let Ok(macs) = std::fs::read_dir(st.config.overlays_dir()) {
        for m in macs.filter_map(Result::ok) {
            if st.nbd_live.in_use(&bootbob_cow::overlay_paths(&m.path(), stem).1) {
                return Err((409, format!("a machine is running from its overlay of {name}")));
            }
            let _ = Cow::remove(&m.path(), stem);
        }
    }
    std::fs::remove_file(&path).map_err(|e| (500, format!("can't delete {name}: {e}")))?;
    st.event(None, "disk", format!("Deleted disk {name} and its overlays"));
    Ok(())
}

/// The main server's answer to store `store` asking whether `ip` may open its disk
/// `export`: allowed if `<store>:<export>` is the disk of an image the machine is
/// assigned or running.
pub fn allow_for_store(st: &Arc<State>, store: &str, ip: std::net::Ipv4Addr, export: &str) -> Result<crate::stores::Allow, String> {
    let nbd = Nbd { st: st.clone() };
    let (mac, disks) = nbd.allowed(SocketAddr::from((ip, 0)))?;
    let want = format!("{store}:{export}");
    let d = disks.iter().find(|d| d.source == want).ok_or_else(|| format!("{want} isn't the disk of an image assigned to this machine"))?;
    Ok(crate::stores::Allow { mac: crate::stores::dashed(mac), cow: d.cow })
}

/// An image store's exports: its own disk images, each opened only when the main server
/// says this machine may (and through its overlay when the image is copy-on-write).
pub struct StoreExports {
    pub st: Arc<State>,
}

impl Exports for StoreExports {
    fn open(&self, name: &str, peer: SocketAddr) -> Result<Export, String> {
        let path = bootbob_pi::resolve_source(&self.st.config.images_dir(), name).map_err(|_| format!("no export {name:?}"))?;
        if !path.is_file() {
            return Err(format!("no export {name:?}"));
        }
        let ip = match peer.ip() {
            IpAddr::V4(a) => a,
            IpAddr::V6(a) => a.to_ipv4_mapped().ok_or("IPv6 clients aren't supported")?,
        };
        // The main server answers over HTTP; don't stall the runtime's other tasks.
        let config = self.st.config.clone();
        let allow = tokio::task::block_in_place(|| crate::stores::ask_main(&config, ip, name))?;
        let mac: Mac = allow.mac.parse().map_err(|_| format!("the main server named machine {:?}", allow.mac))?;
        let overlay = allow.cow.then(|| (overlay_dir(&self.st, mac), overlay_name(name).to_string()));
        Ok(Export { name: name.to_string(), path, read_only: false, overlay })
    }

    fn list(&self, _peer: SocketAddr) -> Vec<String> {
        // Listing would need the main server's answer for every disk; nothing needs it.
        Vec::new()
    }

    fn connected(&self, export: &Export, peer: SocketAddr) {
        self.st.event(None, "nbd", format!("{} connected to disk {} (NBD)", peer.ip(), export.name));
    }

    fn disconnected(&self, export: &Export, peer: SocketAddr, stats: Stats, why: &str) {
        let mb = |b: u64| b.div_ceil(1 << 20);
        self.st.event(
            None,
            "nbd",
            format!("{} on disk {} (NBD) {why}: read {} MB, wrote {} MB", peer.ip(), export.name, mb(stats.read), mb(stats.written)),
        );
    }
}

/// Creates an empty (sparse) disk image in the images directory, for a machine to
/// partition and fill over NBD.
pub fn create_disk(st: &State, name: &str, size: u64) -> Result<(), String> {
    check_disk_name(name)?;
    if !(1 << 30..=4 << 40).contains(&size) || !size.is_multiple_of(512) {
        return Err("a disk is 1 GiB to 4 TiB, in whole 512-byte sectors".into());
    }
    let path = st.config.images_dir().join(name);
    let f = std::fs::OpenOptions::new().write(true).create_new(true).open(&path).map_err(|e| format!("can't create {name}: {e}"))?;
    f.set_len(size).map_err(|e| format!("can't size {name}: {e}"))?;
    st.event(None, "disk", format!("Created empty disk {name} ({} GiB)", size >> 30));
    Ok(())
}
