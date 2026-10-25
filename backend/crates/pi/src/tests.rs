use std::io::{Cursor, Seek, SeekFrom, Write};

use super::*;

/// A FAT filesystem holding a little Pi boot tree, as raw bytes.
fn fat_volume() -> Vec<u8> {
    let mut disk = Cursor::new(vec![0u8; 8 << 20]);
    fatfs::format_volume(&mut disk, fatfs::FormatVolumeOptions::new()).unwrap();
    {
        let fs = fatfs::FileSystem::new(&mut disk, fatfs::FsOptions::new()).unwrap();
        let root = fs.root_dir();
        root.create_file("start4.elf").unwrap().write_all(b"FIRMWARE").unwrap();
        root.create_file("config.txt").unwrap().write_all(b"arm_64bit=1\n").unwrap();
        root.create_file("Cmdline.txt").unwrap().write_all(b"console=tty1 root=/dev/mmcblk0p2\n").unwrap();
        let big: Vec<u8> = (0..300_000u32).map(|i| i as u8).collect();
        root.create_file("kernel8.img").unwrap().write_all(&big).unwrap();
        let ov = root.create_dir("overlays").unwrap();
        ov.create_file("vc4-kms-v3d.dtbo").unwrap().write_all(b"DTBO").unwrap();
    }
    disk.into_inner()
}

/// An MBR disk: a Linux partition entry first (to be skipped), then FAT32-LBA at 1 MiB.
fn mbr_image() -> Vec<u8> {
    let vol = fat_volume();
    let mut img = vec![0u8; (1 << 20) + vol.len()];
    let mut entry = |i: usize, kind: u8, start: u32, count: u32| {
        let e = 446 + i * 16;
        img[e + 4] = kind;
        img[e + 8..e + 12].copy_from_slice(&start.to_le_bytes());
        img[e + 12..e + 16].copy_from_slice(&count.to_le_bytes());
    };
    entry(0, 0x83, 99_999, 10);
    entry(1, 0x0c, 2048, (vol.len() / 512) as u32);
    img[510] = 0x55;
    img[511] = 0xaa;
    img[1 << 20..].copy_from_slice(&vol);
    img
}

/// A GPT disk: a Linux filesystem partition first, then basic data at LBA 2048.
fn gpt_image() -> Vec<u8> {
    let vol = fat_volume();
    let mut img = Cursor::new(vec![0u8; (1 << 20) + vol.len()]);
    let mut mbr = [0u8; 512];
    mbr[446 + 4] = 0xee;
    mbr[510] = 0x55;
    mbr[511] = 0xaa;
    img.write_all(&mbr).unwrap();
    let mut hdr = [0u8; 92];
    hdr[0..8].copy_from_slice(b"EFI PART");
    hdr[72..80].copy_from_slice(&2u64.to_le_bytes());
    hdr[80..84].copy_from_slice(&128u32.to_le_bytes());
    hdr[84..88].copy_from_slice(&128u32.to_le_bytes());
    img.write_all(&hdr).unwrap();
    let linux = [0xafu8, 0x3d, 0xc6, 0x0f, 0x83, 0x84, 0x72, 0x47, 0x8e, 0x79, 0x3d, 0x69, 0xd8, 0x47, 0x7d, 0xe4];
    let mut e = [0u8; 128];
    e[0..16].copy_from_slice(&linux);
    e[32..40].copy_from_slice(&100_000u64.to_le_bytes());
    e[40..48].copy_from_slice(&100_010u64.to_le_bytes());
    img.seek(SeekFrom::Start(2 * 512)).unwrap();
    img.write_all(&e).unwrap();
    e[0..16].copy_from_slice(&crate::disk::GPT_BASIC_DATA);
    e[32..40].copy_from_slice(&2048u64.to_le_bytes());
    e[40..48].copy_from_slice(&(2048 + vol.len() as u64 / 512 - 1).to_le_bytes());
    img.write_all(&e).unwrap();
    img.seek(SeekFrom::Start(1 << 20)).unwrap();
    img.write_all(&vol).unwrap();
    img.into_inner()
}

fn bytes(f: Option<Found>) -> Option<Vec<u8>> {
    match f? {
        Found::Bytes(b) => Some(b),
        Found::Path { path, .. } => Some(fs::read(path).unwrap()),
    }
}

fn check_tree(source: &Path) {
    assert_eq!(bytes(find(source, "start4.elf").unwrap()).unwrap(), b"FIRMWARE");
    // Names match whatever the case.
    assert_eq!(bytes(find(source, "cmdline.txt").unwrap()).unwrap(), b"console=tty1 root=/dev/mmcblk0p2\n");
    assert_eq!(bytes(find(source, "OVERLAYS/vc4-kms-v3d.dtbo").unwrap()).unwrap(), b"DTBO");
    let k = bytes(find(source, "kernel8.img").unwrap()).unwrap();
    assert_eq!(k.len(), 300_000);
    assert!(k.iter().enumerate().all(|(i, &b)| b == i as u8));
    assert!(find(source, "start.elf").unwrap().is_none());
    assert!(find(source, "overlays").unwrap().is_none(), "a directory isn't a file");
    assert!(find(source, "start4.elf/x").unwrap().is_none());
}

#[test]
fn reads_a_bare_fat_filesystem() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("boot.img");
    fs::write(&p, fat_volume()).unwrap();
    check_tree(&p);
}

#[test]
fn reads_the_fat_partition_of_an_mbr_disk() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("pios.img");
    fs::write(&p, mbr_image()).unwrap();
    check_tree(&p);
}

#[test]
fn reads_the_fat_partition_of_a_gpt_disk() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("gpt.img");
    fs::write(&p, gpt_image()).unwrap();
    check_tree(&p);
}

#[test]
fn reads_a_directory() {
    let dir = tempfile::tempdir().unwrap();
    let t = dir.path().join("tree");
    fs::create_dir_all(t.join("overlays")).unwrap();
    fs::write(t.join("start4.elf"), b"FIRMWARE").unwrap();
    fs::write(t.join("CMDLINE.TXT"), b"console=tty1 root=/dev/mmcblk0p2\n").unwrap();
    fs::write(t.join("overlays/vc4-kms-v3d.dtbo"), b"DTBO").unwrap();
    let big: Vec<u8> = (0..300_000u32).map(|i| i as u8).collect();
    fs::write(t.join("kernel8.img"), big).unwrap();
    check_tree(&t);
    assert!(matches!(find(&t, "start4.elf").unwrap(), Some(Found::Path { size: 8, .. })));
}

#[test]
fn junk_is_an_error_not_a_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("junk.img");
    fs::write(&p, vec![7u8; 4096]).unwrap();
    let e = find(&p, "start4.elf").unwrap_err();
    assert!(e.to_string().contains("not a disk image"), "{e}");

    let mut mbr = vec![0u8; 4096];
    mbr[446 + 4] = 0x83;
    mbr[446 + 12] = 1;
    mbr[510] = 0x55;
    mbr[511] = 0xaa;
    fs::write(&p, mbr).unwrap();
    assert!(find(&p, "start4.elf").unwrap_err().to_string().contains("no FAT partition"));
}

#[test]
fn prefixes() {
    assert_eq!(split_prefix("6a6f4d1d/start4.elf"), (Some(Prefix::Serial("6a6f4d1d".into())), "start4.elf"));
    assert_eq!(split_prefix("6A6F4D1D/overlays/x.dtbo"), (Some(Prefix::Serial("6a6f4d1d".into())), "overlays/x.dtbo"));
    assert_eq!(split_prefix("0123456789abcdef/config.txt").0, Some(Prefix::Serial("0123456789abcdef".into())));
    assert_eq!(split_prefix("dc-a6-32-01-02-03/start4.elf"), (Some(Prefix::Mac("dc:a6:32:01:02:03".parse().unwrap())), "start4.elf"));
    assert_eq!(split_prefix("start4.elf"), (None, "start4.elf"));
    assert_eq!(split_prefix("overlays/x.dtbo"), (None, "overlays/x.dtbo"));
    assert_eq!(split_prefix("ipxe/bootbob-bios.kpxe"), (None, "ipxe/bootbob-bios.kpxe"));
    // A serial-looking name with no file after it is just a file.
    assert_eq!(split_prefix("6a6f4d1d"), (None, "6a6f4d1d"));
}

#[test]
fn sources_stay_inside_the_images_directory() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("pios")).unwrap();
    assert!(resolve_source(dir.path(), "pios").is_ok());
    assert!(resolve_source(dir.path(), "missing.img").unwrap_err().contains("isn't in the images directory"));
    for bad in ["", "../etc", "pios/../../x", "/etc/passwd", "./pios"] {
        assert!(resolve_source(dir.path(), bad).is_err(), "{bad:?}");
    }
}

#[test]
fn scan_lists_trees_and_images() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("pios.img"), mbr_image()).unwrap();
    fs::write(dir.path().join("notes.txt"), b"not an image").unwrap();
    fs::write(dir.path().join("junk.img"), vec![1u8; 1024]).unwrap();
    fs::create_dir_all(dir.path().join("tree")).unwrap();
    fs::write(dir.path().join("tree/config.txt"), b"").unwrap();
    let found = scan(dir.path());
    let names: Vec<&str> = found.iter().map(|s| s.path.as_str()).collect();
    assert_eq!(names, vec!["junk.img", "pios.img", "tree"]);
    assert!(found[0].error.is_some());
    assert_eq!(found[1].kind, SourceKind::DiskImage);
    assert_eq!(found[1].boot_files, vec!["start4.elf", "config.txt", "cmdline.txt", "kernel8.img"]);
    assert_eq!(found[2].kind, SourceKind::Directory);
    assert_eq!(found[2].boot_files, vec!["config.txt"]);
}
