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

    std::fs::write(overlay_paths(&ov, "junk").1, b"not a map").unwrap();
    assert!(Cow::open(&b, &ov, "junk", false).unwrap_err().to_string().contains("not an overlay map"));
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
