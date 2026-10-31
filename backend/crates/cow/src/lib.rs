//! Disks as files.
//!
//! [`Disk`] is what the NBD server serves and the Pi boot-file reader reads: random
//! access to a fixed number of bytes. Two kinds:
//!
//! - [`Plain`]: one disk image file, read-write or read-only.
//! - [`Cow`]: a read-only **base** image shared by many machines, plus one machine's
//!   **overlay**: a sparse file the same size as the base, holding only the chunks that
//!   machine has written, and a **map** of which chunks those are. Reads come from the
//!   overlay where the map says so and from the base elsewhere; a write goes to the
//!   overlay (copying the rest of a partly written chunk up from the base first) and sets
//!   the chunk's bit. The base is never written. Deleting the overlay and map resets the
//!   machine to the base image.
//!
//! The map is a file too (`<name>.map`): a 4 KiB header (which also records the base's
//! size and modification time, so an overlay is never paired with a base it wasn't made
//! over), then one bit per chunk. A write
//! lands in the overlay before its bit is set, so a crash in between loses that write but
//! never shows a half-written chunk. [`Disk::flush`] makes both durable.
//!
//! Space goes back with [`Disk::discard`]. On a [`Plain`] disk that's a hole in the file,
//! which reads as zeroes. In an overlay a hole means nothing by itself: the map decides.
//! With the chunk's bit clear, a hole is just a chunk that was never written, and reads
//! show the base. So a discarded chunk is a hole **with its bit set**: reads go to the
//! overlay, find a hole, and get zeroes, while the chunk takes no space. That needs no
//! change to either file's format, and every build reads such an overlay correctly. The
//! hole comes before the bit, for the same reason a write does: a crash in between
//! leaves the chunk as it was.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// A copy-on-write chunk: the unit a bit in the map covers. The Pi's filesystems use
/// 4 KiB blocks, so a typical write needs no copy-up at all.
pub const CHUNK: u64 = 4096;
const HEADER: u64 = 4096;
const MAGIC: &[u8; 8] = b"BBCOW01\n";
/// [`Disk::write_zeroes`] writes this much at a time.
const ZERO_RUN: u64 = 1 << 20;

/// Random access to a fixed-size block of bytes.
pub trait Disk: Send + Sync {
    fn size(&self) -> u64;
    fn read_only(&self) -> bool;
    /// Fills `buf` from `off`. `off + buf.len()` must be within the disk.
    fn read_at(&self, buf: &mut [u8], off: u64) -> io::Result<()>;
    /// Writes `buf` at `off`, which must be within the disk.
    fn write_at(&self, buf: &[u8], off: u64) -> io::Result<()>;
    /// Makes every write so far durable.
    fn flush(&self) -> io::Result<()>;
    /// Whether [`Disk::discard`] works here: it depends on the filesystem and the platform.
    fn can_discard(&self) -> bool;
    /// Makes `len` bytes at `off` read as zeroes, giving the space back. Partial blocks at
    /// either end read as zeroes too; they just keep their space. As with a write,
    /// [`Disk::flush`] makes it durable. The range must be within the disk.
    fn discard(&self, off: u64, len: u64) -> io::Result<()>;
    /// Makes `len` bytes at `off` read as zeroes and keeps them allocated, so a later write
    /// there can't run out of space. The range must be within the disk.
    fn write_zeroes(&self, off: u64, len: u64) -> io::Result<()> {
        let zeroes = vec![0u8; len.min(ZERO_RUN) as usize];
        let mut at = off;
        while at < off + len {
            let n = (off + len - at).min(ZERO_RUN);
            self.write_at(&zeroes[..n as usize], at)?;
            at += n;
        }
        Ok(())
    }
}

#[cfg(unix)]
pub fn pread(f: &File, buf: &mut [u8], off: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.read_exact_at(buf, off)
}

#[cfg(unix)]
pub fn pwrite(f: &File, buf: &[u8], off: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.write_all_at(buf, off)
}

#[cfg(windows)]
pub fn pread(f: &File, mut buf: &mut [u8], mut off: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match f.seek_read(buf, off)? {
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            n => {
                buf = &mut buf[n..];
                off += n as u64;
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
pub fn pwrite(f: &File, mut buf: &[u8], mut off: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = f.seek_write(buf, off)?;
        buf = &buf[n..];
        off += n as u64;
    }
    Ok(())
}

/// Gives a range of a file back to the filesystem. It reads as zeroes afterwards, and the
/// file stays the same length. Fails on filesystems that can't, and off Linux.
#[cfg(target_os = "linux")]
pub fn punch_hole(f: &File, off: u64, len: u64) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let mode = libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE;
    loop {
        // SAFETY: no pointers, and the descriptor is open for as long as `f` is borrowed.
        if unsafe { libc::fallocate(f.as_raw_fd(), mode, off as libc::off_t, len as libc::off_t) } == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Gives a range of a file back to the filesystem: only on Linux so far.
#[cfg(not(target_os = "linux"))]
pub fn punch_hole(_: &File, _: u64, _: u64) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

/// Whether files in `dir` can have holes punched in them. This tries a scratch file and not
/// the disk, because even a punch that changes nothing can count as modifying a file, and
/// an image's modification time is what ties overlays to their base.
fn can_punch(dir: &Path) -> bool {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    if !cfg!(target_os = "linux") {
        return false;
    }
    let scratch = dir.join(format!(".punch-test-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let Ok(f) = OpenOptions::new().write(true).create_new(true).open(&scratch) else { return false };
    let ok = f.set_len(2 * CHUNK).is_ok() && punch_hole(&f, 0, CHUNK).is_ok();
    let _ = std::fs::remove_file(&scratch);
    ok
}

/// Bytes a file occupies on disk (a sparse file's holes don't count). On Windows, its
/// length.
pub fn allocated(path: &Path) -> io::Result<u64> {
    let m = std::fs::metadata(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(m.blocks() * 512)
    }
    #[cfg(not(unix))]
    Ok(m.len())
}

fn check(ok: bool, what: &str) -> io::Result<()> {
    if ok { Ok(()) } else { Err(io::Error::new(io::ErrorKind::InvalidInput, what.to_string())) }
}

// ── Plain ─────────────────────────────────────────────────────────────

/// One disk image file.
pub struct Plain {
    file: File,
    size: u64,
    read_only: bool,
    can_discard: bool,
}

impl Plain {
    pub fn open(path: &Path, read_only: bool) -> io::Result<Plain> {
        let file = OpenOptions::new().read(true).write(!read_only).open(path)?;
        let size = file.metadata()?.len();
        let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        Ok(Plain { file, size, read_only, can_discard: !read_only && can_punch(dir) })
    }
}

impl Disk for Plain {
    fn size(&self) -> u64 {
        self.size
    }
    fn read_only(&self) -> bool {
        self.read_only
    }
    fn read_at(&self, buf: &mut [u8], off: u64) -> io::Result<()> {
        check(off + buf.len() as u64 <= self.size, "read past the end of the disk")?;
        pread(&self.file, buf, off)
    }
    fn write_at(&self, buf: &[u8], off: u64) -> io::Result<()> {
        check(!self.read_only, "the disk is read-only")?;
        check(off + buf.len() as u64 <= self.size, "write past the end of the disk")?;
        pwrite(&self.file, buf, off)
    }
    fn flush(&self) -> io::Result<()> {
        self.file.sync_data()
    }
    fn can_discard(&self) -> bool {
        self.can_discard
    }
    fn discard(&self, off: u64, len: u64) -> io::Result<()> {
        check(!self.read_only, "the disk is read-only")?;
        check(off.checked_add(len).is_some_and(|end| end <= self.size), "discard past the end of the disk")?;
        if len == 0 {
            return Ok(());
        }
        punch_hole(&self.file, off, len)
    }
}

// ── Cow ───────────────────────────────────────────────────────────────

/// A machine's copy-on-write view of a base image. See the crate docs.
#[derive(Debug)]
pub struct Cow {
    base: File,
    size: u64,
    overlay: File,
    map: File,
    /// One bit per chunk, LSB first: set when the overlay decides what the chunk reads
    /// as, whether that's data or (a discarded chunk) a hole.
    bits: Mutex<Vec<u8>>,
    read_only: bool,
    can_discard: bool,
}

/// The two files of an overlay named `name` in `dir`.
pub fn overlay_paths(dir: &Path, name: &str) -> (PathBuf, PathBuf) {
    (dir.join(format!("{name}.ovl")), dir.join(format!("{name}.map")))
}

impl Cow {
    /// Opens the overlay `name` in `dir` over `base`, creating it empty if it isn't
    /// there. An existing overlay made over a different base (another size, or the same
    /// size but modified since: a rebuilt image uploaded under the same name) is refused:
    /// its chunks no longer mean anything over the new base, and reading them would
    /// corrupt the machine's filesystem. Reset the overlay to use the new base.
    pub fn open(base: &Path, dir: &Path, name: &str, read_only: bool) -> io::Result<Cow> {
        let base_f = File::open(base)?;
        let meta = base_f.metadata()?;
        let size = meta.len();
        // Nanoseconds since the epoch; 0 when the platform can't say.
        let mtime =
            meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos() as u64).unwrap_or(0);
        let chunks = size.div_ceil(CHUNK);
        let map_len = HEADER + chunks.div_ceil(8);
        let (ovl_p, map_p) = overlay_paths(dir, name);
        std::fs::create_dir_all(dir)?;
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create(true).truncate(false);
        let overlay = opts.open(&ovl_p)?;
        let map = opts.open(&map_p)?;

        let mut bits = vec![0u8; chunks.div_ceil(8) as usize];
        if map.metadata()?.len() == 0 {
            // New: a header, then zero bits; the overlay is one big hole.
            let mut hdr = vec![0u8; HEADER as usize];
            hdr[..8].copy_from_slice(MAGIC);
            hdr[8..12].copy_from_slice(&(CHUNK as u32).to_le_bytes());
            hdr[12..20].copy_from_slice(&size.to_le_bytes());
            hdr[20..28].copy_from_slice(&mtime.to_le_bytes());
            pwrite(&map, &hdr, 0)?;
            map.set_len(map_len)?;
            overlay.set_len(size)?;
            map.sync_all()?;
        } else {
            let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {what}", map_p.display()));
            let mut hdr = [0u8; 28];
            if map.metadata()?.len() < HEADER || pread(&map, &mut hdr, 0).is_err() || &hdr[..8] != MAGIC {
                return Err(bad("not an overlay map"));
            }
            if u32::from_le_bytes(hdr[8..12].try_into().unwrap()) as u64 != CHUNK {
                return Err(bad("made with a different chunk size"));
            }
            let made_for = u64::from_le_bytes(hdr[12..20].try_into().unwrap());
            if made_for != size {
                return Err(bad(&format!(
                    "the overlay was made over a {made_for}-byte base, but the base image is now {size} bytes; \
                     reset the overlay to use the new base"
                )));
            }
            let made_at = u64::from_le_bytes(hdr[20..28].try_into().unwrap());
            if made_at != 0 && mtime != 0 && made_at != mtime {
                return Err(bad("the base image has changed since the overlay was made over it; \
                     reset the overlay to use the new base"));
            }
            if map.metadata()?.len() < map_len {
                return Err(bad("truncated"));
            }
            pread(&map, &mut bits, HEADER)?;
            if overlay.metadata()?.len() != size {
                overlay.set_len(size)?;
            }
        }
        Ok(Cow { base: base_f, size, overlay, map, bits: Mutex::new(bits), read_only, can_discard: !read_only && can_punch(dir) })
    }

    /// Whether an overlay `name` exists in `dir`.
    pub fn exists(dir: &Path, name: &str) -> bool {
        overlay_paths(dir, name).1.is_file()
    }

    /// Deletes the overlay `name` in `dir`: the machine is back on the base image at its
    /// next boot. Nothing happens if there is none.
    pub fn remove(dir: &Path, name: &str) -> io::Result<()> {
        let (o, m) = overlay_paths(dir, name);
        // The map first: an overlay without a map is ignored, a map without an overlay
        // would read as zeros.
        for p in [m, o] {
            match std::fs::remove_file(&p) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Bytes the overlay occupies on disk.
    pub fn allocated(dir: &Path, name: &str) -> io::Result<u64> {
        allocated(&overlay_paths(dir, name).0)
    }

    /// Chunks the machine has written.
    pub fn written_chunks(&self) -> u64 {
        self.bits.lock().unwrap().iter().map(|b| b.count_ones() as u64).sum()
    }

    fn in_overlay(bits: &[u8], chunk: u64) -> bool {
        bits[(chunk / 8) as usize] & (1 << (chunk % 8)) != 0
    }

    /// Reads one run of chunks that are all in the overlay or all in the base.
    fn read_run(&self, bits: &[u8], buf: &mut [u8], off: u64) -> io::Result<()> {
        let from = if Self::in_overlay(bits, off / CHUNK) { &self.overlay } else { &self.base };
        pread(from, buf, off)
    }

    /// Writes `buf`, which isn't empty, at `off`, and sets its chunks' bits.
    fn write_locked(&self, bits: &mut [u8], buf: &[u8], off: u64) -> io::Result<()> {
        let end = off + buf.len() as u64;
        let (first, last) = (off / CHUNK, (end - 1) / CHUNK);

        // A partly covered chunk not yet in the overlay: bring the whole chunk up from
        // the base first, so the bytes around the write stay what they were.
        for c in [first, last] {
            let (cs, ce) = (c * CHUNK, ((c + 1) * CHUNK).min(self.size));
            if !Self::in_overlay(bits, c) && (off > cs || end < ce) {
                let mut chunk = vec![0u8; (ce - cs) as usize];
                pread(&self.base, &mut chunk, cs)?;
                pwrite(&self.overlay, &chunk, cs)?;
            }
        }
        pwrite(&self.overlay, buf, off)?;

        // Then the bits: a crash before this point loses the write, not the chunk.
        self.set_bits(bits, first, last)
    }

    /// Sets the bits of chunks `first..=last`, in memory and in the map.
    fn set_bits(&self, bits: &mut [u8], first: u64, last: u64) -> io::Result<()> {
        for c in first..=last {
            bits[(c / 8) as usize] |= 1 << (c % 8);
        }
        let (b0, b1) = ((first / 8) as usize, (last / 8) as usize);
        pwrite(&self.map, &bits[b0..=b1], HEADER + b0 as u64)
    }
}

impl Disk for Cow {
    fn size(&self) -> u64 {
        self.size
    }
    fn read_only(&self) -> bool {
        self.read_only
    }

    fn read_at(&self, buf: &mut [u8], off: u64) -> io::Result<()> {
        check(off + buf.len() as u64 <= self.size, "read past the end of the disk")?;
        let bits = self.bits.lock().unwrap().clone();
        let mut pos = 0usize;
        while pos < buf.len() {
            let at = off + pos as u64;
            let here = Self::in_overlay(&bits, at / CHUNK);
            // Extend the run while the next chunks come from the same file.
            let mut end = (at / CHUNK + 1) * CHUNK;
            let limit = off + buf.len() as u64;
            while end < limit && Self::in_overlay(&bits, end / CHUNK) == here {
                end += CHUNK;
            }
            let n = (end.min(limit) - at) as usize;
            self.read_run(&bits, &mut buf[pos..pos + n], at)?;
            pos += n;
        }
        Ok(())
    }

    fn write_at(&self, buf: &[u8], off: u64) -> io::Result<()> {
        check(!self.read_only, "the disk is read-only")?;
        check(off + buf.len() as u64 <= self.size, "write past the end of the disk")?;
        if buf.is_empty() {
            return Ok(());
        }
        self.write_locked(&mut self.bits.lock().unwrap(), buf, off)
    }

    fn flush(&self) -> io::Result<()> {
        self.overlay.sync_data()?;
        self.map.sync_data()
    }

    fn can_discard(&self) -> bool {
        self.can_discard
    }

    fn discard(&self, off: u64, len: u64) -> io::Result<()> {
        check(!self.read_only, "the disk is read-only")?;
        check(off.checked_add(len).is_some_and(|end| end <= self.size), "discard past the end of the disk")?;
        if len == 0 {
            return Ok(());
        }
        let end = off + len;
        // The chunks wholly inside the range. The end of the disk closes a short last chunk.
        let first = off.div_ceil(CHUNK);
        let stop = if end == self.size { self.size.div_ceil(CHUNK) } else { end / CHUNK };
        let mut bits = self.bits.lock().unwrap();
        // What's left at either end is part of a chunk: zeroes written like any other write,
        // copying the rest of the chunk up. A range inside one chunk, or across the boundary
        // of two, is all left over.
        let parts = if first < stop {
            let (from, to) = (first * CHUNK, (stop * CHUNK).min(self.size));
            // The hole first: until the bit is set, a chunk that wasn't in the overlay still
            // reads from the base, so a crash in between leaves it untouched.
            punch_hole(&self.overlay, from, to - from)?;
            self.set_bits(&mut bits, first, stop - 1)?;
            [(off, from), (to, end)]
        } else {
            [(off, end), (end, end)]
        };
        for (from, to) in parts {
            if from < to {
                self.write_locked(&mut bits, &vec![0u8; (to - from) as usize], from)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
