//! Image stores: where an image's disk lives.
//!
//! A source is either local (`golden.img`, a file or directory in this BootBob's images
//! directory) or on a store (`uberfall:golden.img`): another host running BootBob with
//! `mode = "store"`, for the big things (backups, installers, many golden images) on big
//! disks. Machines reach a store directly for NBD, so their root-filesystem traffic never
//! goes through the main server; the main server reads boot files (kernels, initrds, Pi
//! firmware) from the store over HTTP and serves them on as usual.
//!
//! The main server stays the one that decides: a store asks it, for every NBD
//! connection, whether that machine may open that disk and through whose overlay
//! (`GET /api/store/allow`).

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use bootbob_dhcp::Mac;
use bootbob_pi::Found;
use serde::Deserialize;

use crate::config::{Config, StoreConfig};
use crate::state::State;

/// Where a source lives.
pub enum Location<'a> {
    Local(PathBuf),
    Remote { store: &'a StoreConfig, disk: String },
}

/// Finds a source: `name:disk.img` on store `name`, anything else in the images directory.
pub fn locate<'a>(config: &'a Config, source: &str) -> Result<Location<'a>, String> {
    if let Some((store, disk)) = source.split_once(':') {
        let s = config.stores.iter().find(|s| s.name == store).ok_or_else(|| format!("no store {store:?} (the config's stores)"))?;
        crate::nbd::check_disk_name(disk).map_err(|_| format!("a store source is <store>:<disk>.img, not {source:?}"))?;
        return Ok(Location::Remote { store: s, disk: disk.to_string() });
    }
    bootbob_pi::resolve_source(&config.images_dir(), source).map(Location::Local)
}

/// The host machines use for NBD to this source's disk: the store's for a store source,
/// `local` (this server's NBD address) otherwise.
pub fn nbd_host(config: &Config, source: &str, local: String) -> String {
    match locate(config, source) {
        Ok(Location::Remote { store, .. }) => store.nbd_host(),
        _ => local,
    }
}

/// The name machines ask the NBD server for: the store knows its disks without the
/// `name:` that says which store.
pub fn export_name(source: &str) -> &str {
    source.split_once(':').map_or(source, |(_, disk)| disk)
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(60))).http_status_as_error(false).build().into()
}

/// The machine's MAC as the stores' URLs carry it: `dc-a6-32-01-02-03`.
pub fn dashed(mac: Mac) -> String {
    let m = mac.0;
    format!("{:02x}-{:02x}-{:02x}-{:02x}-{:02x}-{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
}

/// GETs `url` from a store: the body on 200, `None` on 404. Blocking.
fn get(url: &str) -> io::Result<Option<Vec<u8>>> {
    let mut res = agent().get(url).call().map_err(io::Error::other)?;
    match res.status().as_u16() {
        200 => res.body_mut().with_config().limit(1 << 30).read_to_vec().map(Some).map_err(io::Error::other),
        404 => Ok(None),
        code => {
            let msg = res.body_mut().read_to_string().unwrap_or_default();
            Err(io::Error::other(format!("{url}: HTTP {code} {msg}")))
        }
    }
}

/// A boot file (kernel, initrd, Pi firmware…) from a source's boot partition: through
/// `mac`'s copy-on-write overlay when `cow` and it has one, else from the image itself.
/// `None`: no such file. Blocking.
pub fn boot_file(st: &State, source: &str, cow: bool, mac: Option<Mac>, path: &str) -> io::Result<Option<Found>> {
    match locate(&st.config, source).map_err(io::Error::other)? {
        Location::Local(src) => local_boot_file(st, &src, source, cow, mac, path),
        Location::Remote { store, disk } => {
            let mut url = format!("{}/store/boot/{disk}/{}", store.url.trim_end_matches('/'), path.trim_start_matches('/'));
            if let Some(m) = mac.filter(|_| cow) {
                url.push_str(&format!("?cow=1&mac={}", dashed(m)));
            }
            Ok(get(&url)?.map(Found::Bytes))
        }
    }
}

/// [`boot_file`] for a file here (also what a store serves at `/store/boot/`).
pub fn local_boot_file(
    st: &State,
    src: &std::path::Path,
    source: &str,
    cow: bool,
    mac: Option<Mac>,
    path: &str,
) -> io::Result<Option<Found>> {
    let overlay = mac.filter(|_| cow).map(|m| (crate::nbd::overlay_dir(st, m), crate::nbd::overlay_name(source).to_string()));
    match overlay {
        Some((dir, name)) if bootbob_cow::Cow::exists(&dir, &name) => {
            bootbob_pi::find_on(&bootbob_cow::Cow::open(src, &dir, &name, true)?, path)
        }
        _ => bootbob_pi::find(src, path),
    }
}

/// What a store's sources list says, for the main server's.
pub fn sources(store: &StoreConfig) -> Vec<bootbob_pi::SourceInfo> {
    #[derive(Deserialize)]
    struct S {
        path: String,
        kind: bootbob_pi::SourceKind,
        size: Option<u64>,
        boot_files: Vec<String>,
        error: Option<String>,
    }
    let url = format!("{}/api/pi/sources", store.url.trim_end_matches('/'));
    let list: Vec<S> = match get(&url).ok().flatten().map(|b| serde_json::from_slice(&b)) {
        Some(Ok(l)) => l,
        _ => {
            return vec![bootbob_pi::SourceInfo {
                path: format!("{}:", store.name),
                kind: bootbob_pi::SourceKind::DiskImage,
                size: None,
                boot_files: vec![],
                error: Some(format!("store {} ({}) isn't answering", store.name, store.url)),
            }];
        }
    };
    list.into_iter()
        .filter(|s| s.kind == bootbob_pi::SourceKind::DiskImage)
        .map(|s| bootbob_pi::SourceInfo {
            path: format!("{}:{}", store.name, s.path),
            kind: s.kind,
            size: s.size,
            // The well-known names, as the store reported them.
            boot_files: s.boot_files.iter().filter_map(|f| bootbob_pi::BOOT_FILES.iter().find(|k| **k == f.as_str()).copied()).collect(),
            error: s.error,
        })
        .collect()
}

/// A machine's overlays on a store, named with the store prefix.
pub fn overlays(store: &StoreConfig, mac: Mac) -> Vec<crate::nbd::OverlayInfo> {
    let url = format!("{}/api/overlays/{}", store.url.trim_end_matches('/'), dashed(mac));
    let list: Vec<crate::nbd::OverlayInfo> = get(&url).ok().flatten().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    list.into_iter().map(|o| crate::nbd::OverlayInfo { source: format!("{}:{}", store.name, o.source), ..o }).collect()
}

/// Resets a machine's overlay on a store. Same results as the local reset.
pub fn reset_overlay(store: &StoreConfig, mac: Mac, disk: &str) -> Result<bool, (u16, String)> {
    let url = format!("{}/api/overlays/{}/{disk}", store.url.trim_end_matches('/'), dashed(mac));
    let mut res = agent().delete(&url).call().map_err(|e| (502, format!("store {}: {e}", store.name)))?;
    match res.status().as_u16() {
        204 => Ok(true),
        404 => Ok(false),
        code => Err((code, res.body_mut().read_to_string().unwrap_or_default())),
    }
}

/// The main server's answer to a store asking about an NBD connection.
#[derive(Debug, Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct Allow {
    /// The machine, as `dc-a6-32-01-02-03`: its overlay directory on the store.
    pub mac: String,
    pub cow: bool,
}

/// Asks the main server whether `ip` may open `disk` on this store. Blocking.
pub fn ask_main(config: &Config, ip: std::net::Ipv4Addr, disk: &str) -> Result<Allow, String> {
    let main = config.main_url.as_deref().ok_or("this store has no main_url")?;
    let name = config.store_name.as_deref().ok_or("this store has no store_name")?;
    let url = format!("{}/api/store/allow?store={name}&ip={ip}&export={disk}", main.trim_end_matches('/'));
    let mut res = agent().get(&url).call().map_err(|e| format!("can't ask the main server: {e}"))?;
    let body = res.body_mut().read_to_string().unwrap_or_default();
    if res.status().as_u16() != 200 {
        let why = serde_json::from_str::<serde_json::Value>(&body).ok().and_then(|v| v["error"].as_str().map(String::from)).unwrap_or(body);
        return Err(why);
    }
    serde_json::from_str(&body).map_err(|e| format!("the main server's answer: {e}"))
}
