use std::path::Path;

use super::*;

/// A base image of `n` bytes where byte i is (i / 7) as u8: no chunk looks like another.
fn base(dir: &Path, n: u64) -> PathBuf {
    let p = dir.join("base.img");
    std::fs::write(&p, (0..n).map(|i| (i / 7) as u8).collect::<Vec<u8>>()).unwrap();
    p
}

fn read(d: &dyn Disk, off: u64, n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    d.read_at(&mut b, off).unwrap();
    b
}

fn expect_base(off: u64, n: usize) -> Vec<u8> {
    (off..off + n as u64).map(|i| (i / 7) as u8).collect()
}

#[test]
fn plain_reads_writes_and_bounds() {
    let dir = tempfile::tempdir().unwrap();
    let p = base(dir.path(), 10_000);
    let d = Plain::open(&p, false).unwrap();
    assert_eq!(d.size(), 10_000);
    assert_eq!(read(&d, 4090, 20), expect_base(4090, 20));
    d.write_at(b"hello", 9_995).unwrap();
    assert_eq!(read(&d, 9_995, 5), b"hello");
    assert!(d.write_at(b"x", 10_000).is_err());
    assert!(d.read_at(&mut [0u8; 2], 9_999).is_err());
    d.flush().unwrap();
    let ro = Plain::open(&p, true).unwrap();
    assert!(ro.read_only());
    assert!(ro.write_at(b"x", 0).is_err());
}

#[test]
fn cow_reads_the_base_until_written_then_the_overlay() {
    let dir = tempfile::tempdir().unwrap();
    let b = base(dir.path(), 3 * CHUNK + 100);
    let ov = dir.path().join("overlays");
    let d = Cow::open(&b, &ov, "pi-a", false).unwrap();
    assert_eq!(d.size(), 3 * CHUNK + 100);
    assert_eq!(read(&d, 0, (3 * CHUNK + 100) as usize), expect_base(0, (3 * CHUNK + 100) as usize));
    assert_eq!(d.written_chunks(), 0);

    // A write inside chunk 1: the rest of the chunk is copied up around it.
    d.write_at(b"ABCDE", CHUNK + 10).unwrap();
    assert_eq!(d.written_chunks(), 1);
    let mut want = expect_base(0, (3 * CHUNK + 100) as usize);
    want[(CHUNK + 10) as usize..(CHUNK + 15) as usize].copy_from_slice(b"ABCDE");
    assert_eq!(read(&d, 0, want.len()), want);
    // Reads that straddle base and overlay chunks.
    assert_eq!(read(&d, CHUNK - 5, 20), want[(CHUNK - 5) as usize..(CHUNK + 15) as usize]);

    // A write across chunks 1..3, ending inside the short last chunk.
    let big: Vec<u8> = (0..(2 * CHUNK + 30) as usize).map(|i| 200 + (i % 50) as u8).collect();
    d.write_at(&big, CHUNK + 60).unwrap();
    want[(CHUNK + 60) as usize..(CHUNK + 60) as usize + big.len()].copy_from_slice(&big);
    assert_eq!(read(&d, 0, want.len()), want);
    assert_eq!(d.written_chunks(), 3);
    assert!(d.write_at(b"x", 3 * CHUNK + 100).is_err(), "past the end");

    // The base is untouched; the overlay only holds what was written.
    assert_eq!(std::fs::read(&b).unwrap(), expect_base(0, (3 * CHUNK + 100) as usize));
    d.flush().unwrap();

    // Reopened: the same view. A read-only view can't write.
    drop(d);
    let d = Cow::open(&b, &ov, "pi-a", true).unwrap();
    assert_eq!(read(&d, 0, want.len()), want);
    assert_eq!(d.written_chunks(), 3);
    assert!(d.write_at(b"x", 0).is_err());

    // Another machine's overlay over the same base starts clean.
    let other = Cow::open(&b, &ov, "pi-b", false).unwrap();
    assert_eq!(read(&other, 0, want.len()), expect_base(0, want.len()));
    other.write_at(&[9u8; 4096], 0).unwrap();
    assert_eq!(read(&d, 0, 8)[..], expect_base(0, 8)[..], "pi-a doesn't see pi-b's write");

    // Reset: back on the base.
    assert!(Cow::exists(&ov, "pi-a"));
    Cow::remove(&ov, "pi-a").unwrap();
    assert!(!Cow::exists(&ov, "pi-a"));
    Cow::remove(&ov, "pi-a").unwrap();
    let d = Cow::open(&b, &ov, "pi-a", false).unwrap();
    assert_eq!(read(&d, 0, want.len()), expect_base(0, want.len()));
}

#[test]
fn a_replaced_base_is_refused_and_junk_maps_too() {
    let dir = tempfile::tempdir().unwrap();
    let b = base(dir.path(), 2 * CHUNK);
    let ov = dir.path().join("overlays");
    let d = Cow::open(&b, &ov, "pi", false).unwrap();
    d.write_at(b"zz", 0).unwrap();
    drop(d);
    base(dir.path(), 3 * CHUNK);
    let e = Cow::open(&b, &ov, "pi", false).unwrap_err();
    assert!(e.to_string().contains("reset the overlay"), "{e}");
    Cow::remove(&ov, "pi").unwrap();
    assert!(Cow::open(&b, &ov, "pi", false).is_ok());

    // Same size, rebuilt: refused too (the overlay would be read against the wrong bytes).
    let d = Cow::open(&b, &ov, "pi", false).unwrap();
    d.write_at(b"zz", 0).unwrap();
    drop(d);
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&b, vec![7u8; 3 * CHUNK as usize]).unwrap();
    let e = Cow::open(&b, &ov, "pi", false).unwrap_err();
    assert!(e.to_string().contains("has changed"), "{e}");
    Cow::remove(&ov, "pi").unwrap();

    std::fs::write(overlay_paths(&ov, "junk").1, b"not a map").unwrap();
    assert!(Cow::open(&b, &ov, "junk", false).unwrap_err().to_string().contains("not an overlay map"));
}

/// Whether this test can run. On Linux it must: skipping there would hide a broken probe.
fn can_discard(d: &dyn Disk) -> bool {
    assert!(d.can_discard() || !cfg!(target_os = "linux"), "punching holes works on every filesystem these tests run on");
    d.can_discard()
}

#[test]
fn discarding_a_plain_disk_reads_zeroes_and_gives_space_back() {
    let dir = tempfile::tempdir().unwrap();
    let p = base(dir.path(), 64 * CHUNK + 100);
    let d = Plain::open(&p, false).unwrap();
    if !can_discard(&d) {
        return;
    }
    let before = allocated(&p).unwrap();
    let modified = std::fs::metadata(&p).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let again = Plain::open(&p, false).unwrap();
    assert_eq!(std::fs::metadata(&p).unwrap().modified().unwrap(), modified, "opening a disk doesn't count as changing it");
    drop(again);

    let mut want = expect_base(0, (64 * CHUNK + 100) as usize);
    // Whole blocks and partial ones at both ends, and to the very end of a short last block.
    for (off, len) in [(CHUNK * 2 + 10, CHUNK * 40 + 20), (10, 5), (63 * CHUNK + 1, CHUNK + 99)] {
        d.discard(off, len).unwrap();
        want[off as usize..(off + len) as usize].fill(0);
    }
    assert_eq!(read(&d, 0, want.len()), want);
    d.flush().unwrap();
    assert_eq!(std::fs::metadata(&p).unwrap().len(), want.len() as u64, "the length doesn't change");
    let after = allocated(&p).unwrap();
    assert!(after <= before - 39 * CHUNK, "{before} -> {after}");

    assert!(d.discard(64 * CHUNK, 101).is_err(), "past the end");
    assert!(d.discard(u64::MAX, 2).is_err());
    d.discard(5, 0).unwrap();
    assert!(Plain::open(&p, true).unwrap().discard(0, 1).is_err(), "read-only");
    let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(left, ["base.img"], "the probe's scratch file is gone");
}

#[test]
fn discarding_an_overlay_reads_zeroes_not_the_base() {
    let dir = tempfile::tempdir().unwrap();
    let size = 64 * CHUNK + 100;
    let b = base(dir.path(), size);
    let ov = dir.path().join("overlays");
    let d = Cow::open(&b, &ov, "pi", false).unwrap();
    if !can_discard(&d) {
        return;
    }
    let mut want = expect_base(0, size as usize);
    d.write_at(&[0xee; 40 * CHUNK as usize], 8 * CHUNK).unwrap();
    want[(8 * CHUNK) as usize..(48 * CHUNK) as usize].fill(0xee);
    d.flush().unwrap();
    let before = Cow::allocated(&ov, "pi").unwrap();

    // Over the overlay's own chunks: the space comes back, and it isn't the base that shows.
    // Then over chunks only the base has. Partial chunks keep what's around them.
    for (off, len) in [(10 * CHUNK, 30 * CHUNK), (50 * CHUNK + 5, 4 * CHUNK), (CHUNK + 1, 7), (3 * CHUNK - 4, 8)] {
        d.discard(off, len).unwrap();
        want[off as usize..(off + len) as usize].fill(0);
    }
    assert_eq!(read(&d, 0, want.len()), want);
    d.flush().unwrap();
    let after = Cow::allocated(&ov, "pi").unwrap();
    // 30 chunks punched, 5 partly covered ones copied up.
    assert!(after <= before - 24 * CHUNK, "{before} -> {after}");
    assert_eq!(std::fs::read(&b).unwrap(), expect_base(0, size as usize), "the base is untouched");

    // A partial write into a discarded chunk keeps zeroes around it, not the base.
    d.write_at(b"new", 20 * CHUNK + 100).unwrap();
    want[(20 * CHUNK + 100) as usize..][..3].copy_from_slice(b"new");
    assert_eq!(read(&d, 0, want.len()), want);

    // The last chunk is short, and the end of the disk closes it.
    d.discard(64 * CHUNK, 100).unwrap();
    want[(64 * CHUNK) as usize..].fill(0);
    assert_eq!(read(&d, 0, want.len()), want);

    // It all survives a reopen, and a view that can't write reads the same.
    d.flush().unwrap();
    drop(d);
    let d = Cow::open(&b, &ov, "pi", true).unwrap();
    assert_eq!(read(&d, 0, want.len()), want);
    assert!(!d.can_discard());
    assert!(d.discard(0, CHUNK).is_err(), "read-only");

    // A reset puts it all back.
    drop(d);
    Cow::remove(&ov, "pi").unwrap();
    let d = Cow::open(&b, &ov, "pi", false).unwrap();
    assert_eq!(read(&d, 0, size as usize), expect_base(0, size as usize));
    let left: Vec<_> = std::fs::read_dir(&ov).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    assert!(!left.iter().any(|f| f.contains("punch")), "{left:?}");
}

/// An overlay as a build from before discarding wrote it, byte for byte. If the format ever
/// changes, this is the test that says what must keep working.
#[test]
fn an_overlay_from_before_discard_stays_readable_and_can_be_discarded() {
    let dir = tempfile::tempdir().unwrap();
    let b = base(dir.path(), 8 * CHUNK);
    let ov = dir.path().join("overlays");
    std::fs::create_dir_all(&ov).unwrap();
    let mtime = std::fs::metadata(&b).unwrap().modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64;
    let mut map = vec![0u8; HEADER as usize + 1];
    map[..8].copy_from_slice(b"BBCOW01\n");
    map[8..12].copy_from_slice(&4096u32.to_le_bytes());
    map[12..20].copy_from_slice(&(8 * CHUNK).to_le_bytes());
    map[20..28].copy_from_slice(&mtime.to_le_bytes());
    map[HEADER as usize] = 0b0000_0110; // chunks 1 and 2
    let (ovl_p, map_p) = overlay_paths(&ov, "old");
    std::fs::write(&map_p, map).unwrap();
    let f = File::create(&ovl_p).unwrap();
    f.set_len(8 * CHUNK).unwrap();
    pwrite(&f, &[0xaa; 2 * CHUNK as usize], CHUNK).unwrap();
    drop(f);

    let d = Cow::open(&b, &ov, "old", false).unwrap();
    let mut want = expect_base(0, (8 * CHUNK) as usize);
    want[CHUNK as usize..(3 * CHUNK) as usize].fill(0xaa);
    assert_eq!(read(&d, 0, want.len()), want);
    assert_eq!(d.written_chunks(), 2);
    if !can_discard(&d) {
        return;
    }
    d.discard(2 * CHUNK, 3 * CHUNK).unwrap();
    want[(2 * CHUNK) as usize..(5 * CHUNK) as usize].fill(0);
    assert_eq!(read(&d, 0, want.len()), want);

    // What it wrote is the same format: the map's bits, and holes in the overlay.
    d.flush().unwrap();
    drop(d);
    let map = std::fs::read(&map_p).unwrap();
    assert_eq!(&map[..8], b"BBCOW01\n");
    assert_eq!(map.len(), HEADER as usize + 1);
    assert_eq!(map[HEADER as usize], 0b0001_1110);
}

/// A crash between a discard's hole and its bit leaves a chunk reading as the base: as if
/// the discard never came. This is that state, made by hand.
#[test]
fn a_hole_with_no_bit_still_reads_the_base() {
    let dir = tempfile::tempdir().unwrap();
    let b = base(dir.path(), 4 * CHUNK);
    let ov = dir.path().join("overlays");
    let d = Cow::open(&b, &ov, "pi", false).unwrap();
    if !can_discard(&d) {
        return;
    }
    punch_hole(&d.overlay, CHUNK, CHUNK).unwrap();
    assert_eq!(read(&d, 0, (4 * CHUNK) as usize), expect_base(0, (4 * CHUNK) as usize));
}

/// Every kind of operation, unaligned and overlapping, against a plain array that says what
/// the disk must hold.
#[test]
fn a_disk_always_reads_what_was_done_to_it() {
    fn run(open: &dyn Fn() -> Box<dyn Disk>, start: Vec<u8>, seed: u64) {
        let mut d = open();
        if !can_discard(d.as_ref()) {
            return;
        }
        let mut want = start;
        let size = want.len() as u64;
        let mut x = seed;
        let mut next = move |n: u64| {
            // xorshift64: repeatable, and all this needs.
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n
        };
        for step in 0..4000u64 {
            // Near chunk boundaries as often as not: that's where the edge cases are.
            let mut off = next(size);
            if next(2) == 0 {
                off = (off / CHUNK * CHUNK + next(3)).min(size - 1);
            }
            let len = match next(3) {
                0 => next(CHUNK),
                1 => next(4 * CHUNK),
                _ => (next(4) + 1) * CHUNK,
            }
            .min(size - off);
            let (a, b) = (off as usize, (off + len) as usize);
            match next(5) {
                0 | 1 if len > 0 => {
                    let data = vec![(step % 251) as u8 + 1; len as usize];
                    d.write_at(&data, off).unwrap();
                    want[a..b].copy_from_slice(&data);
                }
                2 => {
                    d.discard(off, len).unwrap();
                    want[a..b].fill(0);
                }
                3 => {
                    d.write_zeroes(off, len).unwrap();
                    want[a..b].fill(0);
                }
                _ => assert_eq!(read(d.as_ref(), off, len as usize), want[a..b], "step {step}"),
            }
            if step % 500 == 0 {
                d.flush().unwrap();
                d = open();
            }
        }
        assert_eq!(read(d.as_ref(), 0, want.len()), want);
    }

    for seed in [1, 0x9e37_79b9_7f4a_7c15, 0xdead_beef] {
        let dir = tempfile::tempdir().unwrap();
        let size = 37 * CHUNK + 1234;
        let p = base(dir.path(), size);
        run(&|| Box::new(Plain::open(&p, false).unwrap()), expect_base(0, size as usize), seed);

        let dir = tempfile::tempdir().unwrap();
        let b = base(dir.path(), size);
        let ov = dir.path().join("overlays");
        run(&|| Box::new(Cow::open(&b, &ov, "pi", false).unwrap()), expect_base(0, size as usize), seed);
        assert_eq!(std::fs::read(&b).unwrap(), expect_base(0, size as usize), "the base is untouched");
    }
}

#[test]
fn allocated_space_follows_writes() {
    let dir = tempfile::tempdir().unwrap();
    let b = base(dir.path(), 64 * CHUNK);
    let ov = dir.path().join("overlays");
    let d = Cow::open(&b, &ov, "pi", false).unwrap();
    d.write_at(&[1u8; 8192], 8192).unwrap();
    d.flush().unwrap();
    let used = Cow::allocated(&ov, "pi").unwrap();
    // A sparse overlay: well under the base's size on unix; on Windows it's the length.
    assert!(used <= 64 * CHUNK, "{used}");
    #[cfg(unix)]
    assert!((8192..32 * CHUNK).contains(&used), "{used}");
}
