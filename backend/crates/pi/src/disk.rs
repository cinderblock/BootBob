//! Boot files out of a disk image, without unpacking it: find the FAT boot partition
//! (MBR, GPT, or the whole file being one FAT filesystem) and read through `fatfs`.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// Disk images use 512-byte sectors (Pi OS, and anything `dd`'d from an SD card).
const SECTOR: u64 = 512;

/// MBR partition types that hold FAT: FAT12, FAT16 (<32 MB, >32 MB, LBA), FAT32 (CHS,
/// LBA), and the EFI system partition.
const MBR_FAT: &[u8] = &[0x01, 0x04, 0x06, 0x0e, 0x0b, 0x0c, 0xef];

/// GPT type GUIDs as stored on disk (mixed-endian): EFI system, Microsoft basic data.
const GPT_ESP: [u8; 16] = [0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e, 0xc9, 0x3b];
pub(crate) const GPT_BASIC_DATA: [u8; 16] =
    [0xa2, 0xa0, 0xd0, 0xeb, 0xe5, 0xb9, 0x33, 0x44, 0x87, 0xc0, 0x68, 0xb6, 0xb7, 0x26, 0x99, 0xc7];

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn u32le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn u64le(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// A FAT boot sector: a jump, a sane sector size, and "FAT" where FAT12/16 or FAT32 put
/// their type string. (An MBR can start with a jump too, so the jump alone isn't enough.)
fn is_fat_boot_sector(s: &[u8]) -> bool {
    let bps = u16::from_le_bytes([s[11], s[12]]);
    matches!(s[0], 0xeb | 0xe9) && matches!(bps, 512 | 1024 | 2048 | 4096) && (&s[54..57] == b"FAT" || &s[82..85] == b"FAT")
}

/// Byte offset and length of the FAT boot partition.
fn fat_partition(file: &mut File) -> io::Result<(u64, u64)> {
    let len = file.metadata()?.len();
    let mut s0 = [0u8; 512];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut s0).map_err(|_| invalid("too small to be a disk image"))?;
    if is_fat_boot_sector(&s0) {
        return Ok((0, len));
    }
    if s0[510..512] != [0x55, 0xaa] {
        return Err(invalid("not a disk image: no partition table and no FAT filesystem"));
    }
    let entries: Vec<&[u8]> = (0..4).map(|i| &s0[446 + i * 16..446 + (i + 1) * 16]).collect();
    if entries.iter().any(|e| e[4] == 0xee) {
        return gpt_partition(file);
    }
    for e in entries {
        let (start, count) = (u32le(e, 8) as u64, u32le(e, 12) as u64);
        if MBR_FAT.contains(&e[4]) && count > 0 {
            return Ok((start * SECTOR, count * SECTOR));
        }
    }
    Err(invalid("the partition table has no FAT partition"))
}

fn gpt_partition(file: &mut File) -> io::Result<(u64, u64)> {
    let mut hdr = [0u8; 92];
    file.seek(SeekFrom::Start(SECTOR))?;
    file.read_exact(&mut hdr)?;
    if &hdr[0..8] != b"EFI PART" {
        return Err(invalid("a protective MBR but no GPT header"));
    }
    let (table, count, size) = (u64le(&hdr, 72), u32le(&hdr, 80) as u64, u32le(&hdr, 84) as u64);
    if !(128..=4096).contains(&size) || count > 1024 {
        return Err(invalid("a GPT header with an implausible partition table"));
    }
    let mut buf = vec![0u8; (count * size) as usize];
    file.seek(SeekFrom::Start(table * SECTOR))?;
    file.read_exact(&mut buf)?;
    for e in buf.chunks(size as usize) {
        if e[0..16] == GPT_ESP || e[0..16] == GPT_BASIC_DATA {
            let (first, last) = (u64le(e, 32), u64le(e, 40));
            if last >= first {
                return Ok((first * SECTOR, (last - first + 1) * SECTOR));
            }
        }
    }
    Err(invalid("the GPT has no FAT (EFI system or basic data) partition"))
}

/// A read-only window onto part of a file, which is what `fatfs` mounts.
struct Window {
    file: File,
    start: u64,
    len: u64,
    pos: u64,
}

impl Read for Window {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.len.saturating_sub(self.pos);
        let n = (buf.len() as u64).min(left) as usize;
        if n == 0 {
            return Ok(0);
        }
        self.file.seek(SeekFrom::Start(self.start + self.pos))?;
        let got = self.file.read(&mut buf[..n])?;
        self.pos += got as u64;
        Ok(got)
    }
}

impl Seek for Window {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let new = match to {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
            SeekFrom::End(d) => self.len.checked_add_signed(d),
        };
        self.pos = new.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before the start"))?;
        Ok(self.pos)
    }
}

/// `fatfs` wants a writable device; BootBob never writes to an image.
impl Write for Window {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "BootBob opens disk images read-only"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Reads one file out of the image's FAT boot partition.
pub fn read(image: &Path, path: &str) -> io::Result<Option<Vec<u8>>> {
    let mut file = File::open(image)?;
    let (start, len) = fat_partition(&mut file)?;
    let fs = fatfs::FileSystem::new(Window { file, start, len, pos: 0 }, fatfs::FsOptions::new())
        .map_err(|e| invalid(format!("the boot partition isn't a readable FAT filesystem: {e}")))?;
    let mut dir = fs.root_dir();
    let mut parts = path.split('/').peekable();
    while let Some(part) = parts.next() {
        let mut found = None;
        for entry in dir.iter() {
            let entry = entry?;
            if entry.file_name().eq_ignore_ascii_case(part) {
                found = Some(entry);
                break;
            }
        }
        let Some(entry) = found else { return Ok(None) };
        if parts.peek().is_none() {
            if !entry.is_file() {
                return Ok(None);
            }
            let mut out = Vec::with_capacity(entry.len() as usize);
            entry.to_file().read_to_end(&mut out)?;
            return Ok(Some(out));
        }
        if !entry.is_dir() {
            return Ok(None);
        }
        dir = entry.to_dir();
    }
    Ok(None)
}
