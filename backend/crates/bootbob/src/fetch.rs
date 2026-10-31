//! Downloading one file from upstream to a part file: resumable, and checked.
//!
//! The file comes in ranges when the server allows it. That's what makes a dropped
//! connection or a restart cost only what was in flight, and it's also the stall detector:
//! `ureq` has no idle timeout, only a limit on a whole body, so a range has a time limit
//! and a stalled one is simply asked for again from where it got to.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

use sha2::{Digest, Sha256, Sha512};

/// How much of the file is written at a time, and how often the caller hears about it.
const BUF: usize = 1 << 20;

/// `sha256:<hex>` or `sha512:<hex>`: Debian's cloud images publish only SHA-512.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checksum {
    pub sha512: bool,
    pub hex: String,
}

impl Checksum {
    pub fn parse(s: &str) -> Result<Checksum, String> {
        let bad = || format!("a checksum is sha256:<64 hex digits> or sha512:<128 hex digits>, not {s:?}");
        let (algo, hex) = s.trim().split_once(':').ok_or_else(bad)?;
        let sha512 = match algo {
            "sha256" => false,
            "sha512" => true,
            _ => return Err(bad()),
        };
        if hex.len() != if sha512 { 128 } else { 64 } || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(bad());
        }
        Ok(Checksum { sha512, hex: hex.to_ascii_lowercase() })
    }

    pub fn hasher(&self) -> Hasher {
        if self.sha512 { Hasher::Sha512(Box::default()) } else { Hasher::Sha256(Box::default()) }
    }
}

pub enum Hasher {
    Sha256(Box<Sha256>),
    Sha512(Box<Sha512>),
}

impl Hasher {
    pub fn update(&mut self, data: &[u8]) {
        match self {
            Hasher::Sha256(h) => h.update(data),
            Hasher::Sha512(h) => h.update(data),
        }
    }

    pub fn finish(self) -> String {
        fn hex(b: &[u8]) -> String {
            b.iter().map(|x| format!("{x:02x}")).collect()
        }
        match self {
            Hasher::Sha256(h) => hex(&h.finalize()),
            Hasher::Sha512(h) => hex(&h.finalize()),
        }
    }
}

/// The hex SHA-256 of a file.
pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut h = Checksum::parse(&format!("sha256:{}", "0".repeat(64))).expect("a valid checksum").hasher();
    hash_into(path, u64::MAX, &mut h, &mut |_| Ok(()))?;
    Ok(h.finish())
}

fn hash_into(path: &Path, limit: u64, h: &mut Hasher, tick: &mut dyn FnMut(u64) -> Result<(), String>) -> io::Result<u64> {
    let mut f = File::open(path)?.take(limit);
    let mut buf = vec![0u8; BUF];
    let mut done = 0;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(done);
        }
        h.update(&buf[..n]);
        done += n as u64;
        tick(done).map_err(io::Error::other)?;
    }
}

/// Whether a file matches a checksum. Returns what it really is on a mismatch.
pub fn check_file(path: &Path, want: &Checksum, tick: &mut dyn FnMut(u64) -> Result<(), String>) -> io::Result<Result<(), String>> {
    let mut h = want.hasher();
    hash_into(path, u64::MAX, &mut h, tick)?;
    let got = h.finish();
    Ok(if got == want.hex { Ok(()) } else { Err(got) })
}

#[derive(Debug, Clone)]
pub struct Tuning {
    /// One range. Bigger than most boot files, so those take a single request.
    pub range: u64,
    /// A range slower than this is dropped and asked for again: a stall.
    pub range_timeout: Duration,
    /// The limit on a whole file from a server that won't do ranges.
    pub whole_timeout: Duration,
    /// Failures in a row, with no bytes gained, before giving up for now.
    pub attempts: u32,
    /// The first wait between attempts; doubles each time.
    pub backoff: Duration,
    /// Space left on the filesystem that fetching must not touch.
    pub min_free: u64,
    /// The first wait before a job that failed is tried again; five times longer each
    /// failure, up to an hour.
    pub retry_after: Duration,
    /// How often a running fetch checks that it still has room, in bytes fetched.
    pub room_check: u64,
    /// The cap in bytes, if not the config's `cache_gib`.
    pub cap: Option<u64>,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning {
            range: 256 << 20,
            range_timeout: Duration::from_secs(600),
            whole_timeout: Duration::from_secs(12 * 3600),
            attempts: 5,
            backoff: Duration::from_secs(2),
            min_free: 5 << 30,
            retry_after: Duration::from_secs(60),
            room_check: 64 << 20,
            cap: None,
        }
    }
}

/// How a download went wrong.
#[derive(Debug)]
pub enum Failure {
    /// Worth trying again later; what's on disk is kept.
    Network(String),
    /// Upstream said no, or the file isn't usable. Retrying won't help until someone acts.
    Refused(String),
    /// The whole file arrived and it's the wrong one. Nothing is kept. `resumed`: it was
    /// put together from an earlier attempt, so the two halves may not belong together.
    Checksum { resumed: bool, want: String, got: String },
    /// The caller stopped it (no room left).
    Stopped(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Network(m) | Failure::Refused(m) | Failure::Stopped(m) => f.write_str(m),
            Failure::Checksum { want, got, .. } => {
                let s = |x: &str| x.chars().take(12).collect::<String>();
                write!(f, "the checksum doesn't match: expected {}…, got {}…", s(want), s(got))
            }
        }
    }
}

/// What a download is doing, for progress.
#[derive(Debug, Clone, Copy)]
pub enum Step {
    /// Rehashing what an earlier attempt left.
    Resuming {
        done: u64,
    },
    Downloading {
        done: u64,
        total: Option<u64>,
    },
}

fn agent(t: &Tuning, recv_body: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        .timeout_recv_body(Some(recv_body.min(t.whole_timeout)))
        .user_agent(concat!("BootBob/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// `bytes 5-9/20`: (5, Some(20)). `bytes 5-9/*`: (5, None).
fn content_range(s: &str) -> Option<(u64, Option<u64>)> {
    let (range, total) = s.strip_prefix("bytes ")?.split_once('/')?;
    let (first, _) = range.split_once('-')?;
    Some((first.trim().parse().ok()?, total.trim().parse().ok()))
}

/// The bytes so far: the part file, and the hash of what's in it.
struct Part {
    f: File,
    have: u64,
    hash: Option<Hasher>,
    expect: Option<Checksum>,
}

impl Part {
    /// Starts over, for a server that sent something other than what was asked for.
    fn reset(&mut self) -> io::Result<()> {
        self.f.set_len(0)?;
        self.f.seek(SeekFrom::Start(0))?;
        self.have = 0;
        self.hash = self.expect.as_ref().map(Checksum::hasher);
        Ok(())
    }

    /// Appends a response body. Bytes that arrived before an error are kept.
    fn take(&mut self, body: &mut dyn Read, total: Option<u64>, on: &mut dyn FnMut(Step) -> Result<(), String>) -> Result<(), Failure> {
        let mut buf = vec![0u8; BUF];
        loop {
            let mut n = 0;
            let mut end = None;
            while n < buf.len() {
                match body.read(&mut buf[n..]) {
                    Ok(0) => {
                        end = Some(Ok(()));
                        break;
                    }
                    Ok(k) => n += k,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => {
                        end = Some(Err(e));
                        break;
                    }
                }
            }
            if let Some(h) = &mut self.hash {
                h.update(&buf[..n]);
            }
            let io = |e: io::Error| Failure::Stopped(format!("can't write the part file: {e}"));
            crate::sparse::write_with_holes(&mut self.f, &buf[..n]).map_err(io)?;
            self.have += n as u64;
            // A hole at the end has nothing after it to give the file its length.
            self.f.set_len(self.have).map_err(io)?;
            on(Step::Downloading { done: self.have, total }).map_err(Failure::Stopped)?;
            match end {
                None => {}
                Some(Ok(())) => return Ok(()),
                Some(Err(e)) => return Err(Failure::Network(format!("the download broke off: {e}"))),
            }
        }
    }
}

enum Got {
    /// Some or all of what was asked for arrived; ask for what's left.
    More,
    /// The file is complete.
    Done,
    /// This server doesn't do ranges. Ask for the whole file, with a longer limit.
    NoRanges,
}

/// One request for the rest of the file (or the next range of it).
fn request(
    url: &str,
    part: &mut Part,
    t: &Tuning,
    total: &mut Option<u64>,
    ranges: bool,
    on: &mut dyn FnMut(Step) -> Result<(), String>,
) -> Result<Got, Failure> {
    let net = |e: &dyn std::fmt::Display| Failure::Network(format!("{url}: {e}"));
    let timeout = if ranges { t.range_timeout } else { t.whole_timeout };
    let mut req = agent(t, timeout).get(url);
    if ranges {
        let end = part.have + t.range - 1;
        let end = total.map_or(end, |n| end.min(n.saturating_sub(1)));
        req = req.header("Range", format!("bytes={}-{end}", part.have));
    } else if part.have > 0 {
        part.reset().map_err(|e| net(&e))?;
    }
    let mut res = req.call().map_err(|e| net(&e))?;
    let code = res.status().as_u16();
    let header = |res: &ureq::http::Response<ureq::Body>, h: &str| res.headers().get(h).and_then(|v| v.to_str().ok()).map(String::from);
    match code {
        206 if ranges => {
            let Some((first, size)) = header(&res, "content-range").as_deref().and_then(content_range) else {
                return Ok(Got::NoRanges);
            };
            if first != part.have || (total.is_some() && size.is_some() && *total != size) {
                // Not what was asked for, or the file changed size: start clean.
                *total = None;
                part.reset().map_err(|e| net(&e))?;
                return Err(Failure::Network(format!("{url}: the file changed during the download")));
            }
            *total = size.or(*total);
            let before = part.have;
            part.take(&mut res.body_mut().as_reader(), *total, on)?;
            // A range to the end of a file of unknown size: a short one means that's all.
            let short = part.have - before < t.range;
            Ok(if total.map_or(short, |n| part.have >= n) { Got::Done } else { Got::More })
        }
        200 if ranges => Ok(Got::NoRanges),
        200 => {
            *total = header(&res, "content-length").and_then(|s| s.parse().ok());
            part.take(&mut res.body_mut().as_reader(), *total, on)?;
            match *total {
                Some(n) if part.have != n => Err(Failure::Network(format!("{url}: got {} of {n} bytes", part.have))),
                _ => Ok(Got::Done),
            }
        }
        // Past the end: right if that's exactly where the file ends.
        416 if ranges => {
            let size = header(&res, "content-range").and_then(|s| s.rsplit_once('/').and_then(|(_, n)| n.trim().parse::<u64>().ok()));
            if size == Some(part.have) && part.have > 0 {
                Ok(Got::Done)
            } else {
                part.reset().map_err(|e| net(&e))?;
                Ok(Got::More)
            }
        }
        401 | 403 | 404 | 410 => Err(Failure::Refused(format!("{url}: HTTP {code}"))),
        _ => Err(Failure::Network(format!("{url}: HTTP {code}"))),
    }
}

/// Fetches `url` into `part`, carrying on from whatever `part` already holds. When this
/// returns `Ok`, `part` is the whole file and matches `expect`, if there is one. Blocking.
///
/// A checksum failure removes `part`, since nothing in it can be trusted. Anything else
/// leaves it, so the next attempt resumes.
pub fn download(
    url: &str,
    part_path: &Path,
    expect: Option<&Checksum>,
    t: &Tuning,
    on: &mut dyn FnMut(Step) -> Result<(), String>,
) -> Result<u64, Failure> {
    let io = |e: io::Error| Failure::Stopped(format!("can't use the part file: {e}"));
    let f = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(part_path).map_err(io)?;
    let have = f.metadata().map_err(io)?.len();
    let resumed = have > 0;
    let mut hash = expect.map(Checksum::hasher);
    if let Some(h) = &mut hash {
        hash_into(part_path, have, h, &mut |done| on(Step::Resuming { done })).map_err(io)?;
    }
    let mut part = Part { f, have, hash, expect: expect.cloned() };
    part.f.seek(SeekFrom::End(0)).map_err(io)?;

    let mut total = None;
    let mut ranges = true;
    let mut failures = 0;
    loop {
        let before = part.have;
        match request(url, &mut part, t, &mut total, ranges, on) {
            Ok(Got::Done) => break,
            Ok(Got::More) => failures = 0,
            Ok(Got::NoRanges) => {
                tracing::info!(url, "no ranges from this server: fetching it whole");
                ranges = false;
            }
            Err(Failure::Network(why)) => {
                failures = if part.have > before && ranges { 1 } else { failures + 1 };
                if failures >= t.attempts {
                    return Err(Failure::Network(why));
                }
                tracing::info!(url, have = part.have, "download: {why}; trying again");
                std::thread::sleep(t.backoff * (1 << (failures - 1).min(6)));
            }
            Err(other) => return Err(other),
        }
    }
    if part.have == 0 {
        return Err(Failure::Refused(format!("{url}: the file is empty")));
    }
    part.f.sync_all().map_err(io)?;
    let len = part.have;
    let got = part.hash.take().map(Hasher::finish);
    drop(part);
    if let (Some(want), Some(got)) = (expect, got)
        && want.hex != got
    {
        let _ = std::fs::remove_file(part_path);
        return Err(Failure::Checksum { resumed, want: want.hex.clone(), got });
    }
    Ok(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums() {
        let c = Checksum::parse(&format!("sha256:{}", "AB".repeat(32))).unwrap();
        assert!(!c.sha512);
        assert_eq!(c.hex, "ab".repeat(32));
        assert!(Checksum::parse(&format!("sha512:{}", "0".repeat(128))).unwrap().sha512);
        for bad in ["", "abc", "md5:00", "sha256:00", &format!("sha256:{}", "g".repeat(64)), &format!("sha512:{}", "0".repeat(64))] {
            assert!(Checksum::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn hashes_are_the_real_ones() {
        let empty256 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let empty512 = "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e";
        assert_eq!(Checksum::parse(&format!("sha256:{empty256}")).unwrap().hasher().finish(), empty256);
        assert_eq!(Checksum::parse(&format!("sha512:{empty512}")).unwrap().hasher().finish(), empty512);
    }

    #[test]
    fn reads_content_range() {
        assert_eq!(content_range("bytes 5-9/20"), Some((5, Some(20))));
        assert_eq!(content_range("bytes 5-9/*"), Some((5, None)));
        assert_eq!(content_range("nonsense"), None);
    }
}
