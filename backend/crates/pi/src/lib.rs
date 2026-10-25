//! Raspberry Pi network boot.
//!
//! A Pi 4 or 5 with network boot in its EEPROM `BOOT_ORDER` fetches its boot files over
//! TFTP, normally under a per-device prefix (`TFTP_PREFIX`): its serial number
//! (`6a6f4d1d/start4.elf`, the default) or its MAC (`dc-a6-32-01-02-03/start4.elf`). When
//! nothing is found under the prefix it drops it and asks again without.
//!
//! - [`split_prefix`] tells the prefix from the file.
//! - [`find`] reads a boot file from an image's source: a directory, or a disk image whose
//!   FAT boot partition is read in place (MBR, GPT, or a bare FAT filesystem). So a Pi OS
//!   `.img` works as downloaded, once decompressed, with nothing unpacked.
//! - [`scan`] lists the candidate sources in BootBob's images directory, for the web UI.

mod disk;

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use bootbob_dhcp::Mac;
use serde::Serialize;

/// The per-device part of a Pi's request path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prefix {
    /// The board serial number, lower-case hex (`TFTP_PREFIX=0`, the default).
    Serial(String),
    /// The Ethernet MAC (`TFTP_PREFIX=1`).
    Mac(Mac),
}

/// Splits a cleaned TFTP path into its per-device prefix, if it has one, and the file.
pub fn split_prefix(path: &str) -> (Option<Prefix>, &str) {
    if let Some((first, rest)) = path.split_once('/')
        && let Some(p) = parse_prefix(first)
    {
        return (Some(p), rest);
    }
    (None, path)
}

fn parse_prefix(s: &str) -> Option<Prefix> {
    // A Pi 4 sends 8 hex digits; a Pi 5's serial is 16.
    if matches!(s.len(), 8 | 16) && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Some(Prefix::Serial(s.to_ascii_lowercase()));
    }
    if s.len() == 17 && s.as_bytes()[2] == b'-' {
        return s.parse().ok().map(Prefix::Mac);
    }
    None
}

/// A boot file, found.
#[derive(Debug)]
pub enum Found {
    /// A file in a directory source: serve it from disk.
    Path { path: PathBuf, size: u64 },
    /// A file read out of a disk image.
    Bytes(Vec<u8>),
}

/// Reads `path` (a cleaned TFTP path with the prefix removed) from a source. Names match
/// case-insensitively, as they do on the FAT card a Pi normally boots from. `None`: no
/// such file (normal: the bootloader asks for plenty of optional files).
///
/// Blocking: call it from `spawn_blocking`.
pub fn find(source: &Path, path: &str) -> io::Result<Option<Found>> {
    if source.is_dir() {
        let mut here = source.to_path_buf();
        for part in path.split('/') {
            if !here.is_dir() {
                return Ok(None);
            }
            let Some(entry) =
                fs::read_dir(&here)?.filter_map(Result::ok).find(|e| e.file_name().to_str().is_some_and(|n| n.eq_ignore_ascii_case(part)))
            else {
                return Ok(None);
            };
            here = entry.path();
        }
        let meta = fs::metadata(&here)?;
        return Ok(meta.is_file().then_some(Found::Path { path: here, size: meta.len() }));
    }
    let disk = bootbob_cow::Plain::open(source, true)?;
    find_on(&disk, path)
}

/// [`find`] on an open disk (a plain image, or a machine's copy-on-write view of one).
/// Blocking.
pub fn find_on(disk: &dyn bootbob_cow::Disk, path: &str) -> io::Result<Option<Found>> {
    disk::read(disk, path).map(|o| o.map(Found::Bytes))
}

/// What a source looks like, for the web UI.
#[derive(Debug, Clone, Serialize)]
pub struct SourceInfo {
    /// Relative to the images directory: what an image's `source` names.
    pub path: String,
    pub kind: SourceKind,
    /// Bytes on disk (for a directory, not counted).
    pub size: Option<u64>,
    /// Which of the well-known Pi boot files it has. Empty: probably not a Pi boot tree.
    pub boot_files: Vec<&'static str>,
    /// Why it couldn't be read, if it couldn't.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Directory,
    DiskImage,
}

/// Files that mark a Pi boot tree: firmware for the Pi 4 and earlier, the configs, and
/// the kernels for the Pi 4 (64- and 32-bit) and Pi 5.
const BOOT_FILES: &[&str] = &["start4.elf", "start.elf", "config.txt", "cmdline.txt", "kernel8.img", "kernel7l.img", "kernel_2712.img"];

/// Describes one source.
pub fn describe(images_dir: &Path, rel: &str) -> SourceInfo {
    let full = images_dir.join(rel);
    let kind = if full.is_dir() { SourceKind::Directory } else { SourceKind::DiskImage };
    let size = (kind == SourceKind::DiskImage).then(|| fs::metadata(&full).map(|m| m.len()).ok()).flatten();
    let mut boot_files = Vec::new();
    let mut error = None;
    for f in BOOT_FILES {
        match find(&full, f) {
            Ok(Some(_)) => boot_files.push(*f),
            Ok(None) => {}
            Err(e) => {
                error = Some(e.to_string());
                break;
            }
        }
    }
    SourceInfo { path: rel.to_string(), kind, size, boot_files, error }
}

/// The directories and disk images (`.img`) directly in the images directory.
/// Blocking.
pub fn scan(images_dir: &Path) -> Vec<SourceInfo> {
    let Ok(entries) = fs::read_dir(images_dir) else { return Vec::new() };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|e| {
            let p = e.path();
            p.is_dir() || p.extension().is_some_and(|x| x.eq_ignore_ascii_case("img"))
        })
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| !n.starts_with('.'))
        .collect();
    names.sort();
    names.iter().map(|n| describe(images_dir, n)).collect()
}

/// Where an image's `source` (relative to the images directory) really is. Refuses
/// absolute paths and anything that climbs out with `..`.
pub fn resolve_source(images_dir: &Path, rel: &str) -> Result<PathBuf, String> {
    let p = Path::new(rel);
    if rel.is_empty() || !p.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err(format!("a Pi source is a path inside the images directory, not {rel:?}"));
    }
    let full = images_dir.join(p);
    if !full.exists() {
        return Err(format!("{rel:?} isn't in the images directory ({})", images_dir.display()));
    }
    Ok(full)
}

#[cfg(test)]
mod tests;
