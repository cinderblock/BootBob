//! A USB boot stick for this server: a small disk image with an EFI system partition
//! holding the USB iPXE builds (`bootbob-usb-efi-*.efi`, see `ipxe/usb.ipxe`), with this
//! server's URL written over the placeholder in their embedded script. A PC that boots
//! the stick reaches BootBob even on a network whose DHCP gives no boot options.
//! They're the full `ipxe.efi`, with iPXE's own NIC drivers, so the stick works even
//! where the PC's firmware network stack is switched off.
//!
//! Made on request (it's tens of milliseconds) so it always carries the server's
//! current URL and iPXE builds.

use std::io::{Cursor, Write};
use std::path::Path;

/// The placeholder in `ipxe/usb.ipxe`, read from the script itself so the two can't
/// drift apart.
fn placeholder() -> &'static str {
    let script = include_str!("../../../../ipxe/usb.ipxe");
    script
        .lines()
        .find_map(|l| l.strip_prefix("set bootbob-url "))
        .map(str::trim)
        .filter(|p| p.starts_with("BOOTBOB_URL_PLACEHOLDER_"))
        .expect("ipxe/usb.ipxe sets bootbob-url to the placeholder")
}

/// Writes `url` over the placeholder in an iPXE binary, padded with spaces.
pub fn patch(efi: &[u8], url: &str) -> Result<Vec<u8>, String> {
    let ph = placeholder().as_bytes();
    if url.len() > ph.len() || url.bytes().any(|b| !b.is_ascii_graphic()) {
        return Err(format!("the server URL {url:?} can't go on a stick (at most {} plain characters)", ph.len()));
    }
    let mut at = efi.windows(ph.len()).enumerate().filter(|(_, w)| *w == ph).map(|(i, _)| i);
    let (Some(i), None) = (at.next(), at.next()) else {
        return Err("this iPXE build doesn't have exactly one URL placeholder (not a USB build?)".into());
    };
    let mut out = efi.to_vec();
    out[i..i + ph.len()].fill(b' ');
    out[i..i + url.len()].copy_from_slice(url.as_bytes());
    Ok(out)
}

const SECTOR: usize = 512;
const PART_START: usize = 2048; // sectors: 1 MiB
const PART_SECTORS: usize = 30 * 2048; // 30 MiB

/// The stick's image: MBR, one bootable EFI system partition (FAT) with
/// `EFI/BOOT/BOOTX64.EFI` (and `BOOTAA64.EFI` when there's an arm64 build).
pub fn stick(ipxe_dir: &Path, url: &str) -> Result<Vec<u8>, String> {
    let read = |f: &str| std::fs::read(ipxe_dir.join(f));
    let x64 = read("bootbob-usb-efi-x64.efi").map_err(|e| format!("no USB iPXE build bootbob-usb-efi-x64.efi: {e}"))?;
    let arm64 = read("bootbob-usb-efi-arm64.efi").ok();

    let mut fat = Cursor::new(vec![0u8; PART_SECTORS * SECTOR]);
    fatfs::format_volume(&mut fat, fatfs::FormatVolumeOptions::new().volume_label(*b"BOOTBOB    ")).map_err(|e| e.to_string())?;
    {
        let fs = fatfs::FileSystem::new(&mut fat, fatfs::FsOptions::new()).map_err(|e| e.to_string())?;
        let root = fs.root_dir();
        let boot = root.create_dir("EFI").and_then(|d| d.create_dir("BOOT")).map_err(|e| e.to_string())?;
        let put = |name: &str, bytes: &[u8]| -> Result<(), String> {
            boot.create_file(name).and_then(|mut f| f.write_all(bytes)).map_err(|e| format!("{name}: {e}"))
        };
        put("BOOTX64.EFI", &patch(&x64, url)?)?;
        if let Some(a) = &arm64 {
            put("BOOTAA64.EFI", &patch(a, url)?)?;
        }
        let note = format!(
            "BootBob boot stick\r\n\r\nBoot a UEFI PC from this stick and it netboots from BootBob at\r\n{url}\r\n\
             even if the network's DHCP gives no boot options. Secure Boot must be off.\r\n"
        );
        root.create_file("BOOTBOB.TXT").and_then(|mut f| f.write_all(note.as_bytes())).map_err(|e| e.to_string())?;
    }
    let fat = fat.into_inner();

    let mut img = vec![0u8; (PART_START + PART_SECTORS) * SECTOR];
    let e = 446;
    img[e] = 0x80; // bootable
    img[e + 4] = 0xef; // EFI system partition
    img[e + 8..e + 12].copy_from_slice(&(PART_START as u32).to_le_bytes());
    img[e + 12..e + 16].copy_from_slice(&(PART_SECTORS as u32).to_le_bytes());
    // CHS fields: "use LBA" values, which every firmware accepts.
    img[e + 1..e + 4].copy_from_slice(&[0xfe, 0xff, 0xff]);
    img[e + 5..e + 8].copy_from_slice(&[0xfe, 0xff, 0xff]);
    img[510] = 0x55;
    img[511] = 0xaa;
    img[PART_START * SECTOR..].copy_from_slice(&fat);
    Ok(img)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patches_exactly_one_placeholder() {
        let ph = placeholder();
        let efi = [b"MZ..".as_slice(), ph.as_bytes(), b"..end"].concat();
        let out = patch(&efi, "http://10.255.0.5:8067").unwrap();
        assert_eq!(out.len(), efi.len());
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("MZ..http://10.255.0.5:8067   "));
        assert!(!s.contains("PLACEHOLDER"));
        assert!(patch(b"no placeholder here", "http://x").is_err());
        let twice = [ph.as_bytes(), ph.as_bytes()].concat();
        assert!(patch(&twice, "http://x").is_err());
        assert!(patch(&efi, "http://has space").is_err());
        assert!(patch(&efi, &"x".repeat(ph.len() + 1)).is_err());
    }
}
