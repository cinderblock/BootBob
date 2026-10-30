//! Receiving a disk image. Disk images are mostly zeros: blocks of zeros become holes in
//! the file, so they take no space, and an upload can be zstd compressed, so they take
//! next to no time on the network either.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

use ruzstd::decoding::StreamingDecoder;

/// Filesystems allocate in blocks of about this size, so shorter runs of zeros save
/// nothing.
const BLOCK: usize = 4096;
/// How much of the image is looked at at a time. A multiple of `BLOCK`.
const READ: usize = 1 << 20;

/// Writes `src` to a new file at `path`, decompressing it first if `zstd`. Returns the
/// image's length.
pub fn receive(src: impl Read, zstd: bool, path: &Path) -> io::Result<u64> {
    let mut f = File::create(path)?;
    let mut src = BufReader::with_capacity(READ, src);
    let len = if zstd { copy_frames(&mut src, &mut f)? } else { copy(&mut src, &mut f)? };
    // A trailing hole has nothing written after it to give the file its length.
    f.set_len(len)?;
    f.sync_all()?;
    Ok(len)
}

/// A zstd stream is one or more frames (`pzstd` writes several), and the decoder reads one.
fn copy_frames<R: BufRead>(src: &mut R, f: &mut File) -> io::Result<u64> {
    let mut len = 0;
    loop {
        let mut frame = StreamingDecoder::new(&mut *src).map_err(io::Error::other)?;
        len += copy(&mut frame, f)?;
        // Frames needn't have one (`zstd --no-check`).
        let sent = frame.decoder.get_checksum_from_data();
        if sent.is_some() && sent != frame.decoder.get_calculated_checksum() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "the zstd checksum doesn't match"));
        }
        if src.fill_buf()?.is_empty() {
            return Ok(len);
        }
    }
}

fn copy(src: &mut impl Read, f: &mut File) -> io::Result<u64> {
    let mut buf = vec![0u8; READ];
    let mut len = 0;
    loop {
        let n = fill(src, &mut buf)?;
        write_with_holes(f, &buf[..n])?;
        len += n as u64;
        if n < buf.len() {
            return Ok(len);
        }
    }
}

/// Reads until `buf` is full or `src` ends: a block has to be whole to be seen as zeros,
/// however the network cut it up.
fn fill(src: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match src.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(got) => n += got,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// Writes `data` at the file's position, seeking over whole blocks of zeros.
fn write_with_holes(f: &mut File, data: &[u8]) -> io::Result<()> {
    let zeros = |block: &[u8]| block.iter().all(|&b| b == 0);
    let mut blocks = data.chunks(BLOCK).peekable();
    let mut start = 0;
    while let Some(first) = blocks.next() {
        let hole = zeros(first);
        let mut end = start + first.len();
        while let Some(next) = blocks.next_if(|b| zeros(b) == hole) {
            end += next.len();
        }
        if hole {
            f.seek(SeekFrom::Current((end - start) as i64))?;
        } else {
            f.write_all(&data[start..end])?;
        }
        start = end;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruzstd::encoding::{CompressionLevel, compress_to_vec};

    /// Data, 8 MiB of zeros, data, 4 MiB of zeros: something like a disk image.
    fn image() -> Vec<u8> {
        let mut img: Vec<u8> = (0..3 * BLOCK + 100).map(|i| (i % 251) as u8 + 1).collect();
        img.resize(img.len() + (8 << 20), 0);
        img.extend_from_slice(b"the end of the data");
        img.resize(img.len() + (4 << 20), 0);
        img
    }

    fn compress(data: &[u8]) -> Vec<u8> {
        compress_to_vec(data, CompressionLevel::Fastest)
    }

    /// Hands out a few bytes at a time, as a network does.
    struct Dribble<'a>(&'a [u8]);

    impl Read for Dribble<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = buf.len().min(self.0.len()).min(1021);
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }

    #[test]
    fn keeps_every_byte_and_makes_holes_of_the_zeros() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out");
        let img = image();
        assert_eq!(receive(Dribble(&img), false, &path).unwrap(), img.len() as u64);
        assert!(std::fs::read(&path).unwrap() == img, "the image changed");
        // Only a sparse file is smaller than its length; on Windows it's the length.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let allocated = std::fs::metadata(&path).unwrap().blocks() * 512;
            assert!(allocated < 1 << 20, "{allocated} bytes on disk");
        }
    }

    #[test]
    fn an_image_of_only_zeros_keeps_its_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out");
        receive(&vec![0u8; 3 << 20][..], false, &path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 3 << 20);
    }

    #[test]
    fn decompresses_one_frame_or_several() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out");
        let img = image();
        receive(Dribble(&compress(&img)), true, &path).unwrap();
        assert!(std::fs::read(&path).unwrap() == img, "the image changed");

        // Split off-block, as pzstd would split it: nothing may be dropped or moved.
        let (a, b) = img.split_at(5 * BLOCK + 17);
        let frames = [compress(a), compress(b)].concat();
        assert_eq!(receive(Dribble(&frames), true, &path).unwrap(), img.len() as u64);
        assert!(std::fs::read(&path).unwrap() == img, "the image changed");
    }

    #[test]
    fn refuses_a_cut_off_or_corrupt_stream() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out");
        let z = compress(&image());
        assert!(receive(&z[..z.len() / 2], true, &path).is_err(), "cut off");
        assert!(receive(&b"this isn't zstd"[..], true, &path).is_err(), "not zstd");
    }

    /// `zstd --check` and `zstd --no-check` of "a real zstd frame".
    const WITH_CHECKSUM: [u8; 30] =
        [40, 181, 47, 253, 36, 17, 137, 0, 0, 97, 32, 114, 101, 97, 108, 32, 122, 115, 116, 100, 32, 102, 114, 97, 109, 101, 49, 55, 192, 78];
    const NO_CHECKSUM: [u8; 26] = [40, 181, 47, 253, 32, 17, 137, 0, 0, 97, 32, 114, 101, 97, 108, 32, 122, 115, 116, 100, 32, 102, 114, 97, 109, 101];

    #[test]
    fn checks_the_checksum_of_a_frame_that_has_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out");
        for z in [&WITH_CHECKSUM[..], &NO_CHECKSUM[..]] {
            receive(z, true, &path).unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), b"a real zstd frame");
        }
        let mut bad = WITH_CHECKSUM;
        bad[12] ^= 1; // the frame's data
        let e = receive(&bad[..], true, &path).unwrap_err();
        assert!(e.to_string().contains("checksum"), "{e}");
    }
}
