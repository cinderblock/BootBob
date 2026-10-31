//! On-demand download and caching, against a fake upstream on a real socket.

use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::extract::State as Ax;
use axum::http::{HeaderMap, StatusCode as Code, header};
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256, Sha512};

use super::*;
use crate::cache::{self, Status, Want};
use crate::fetch::Tuning;

/// An upstream web server that misbehaves on request.
#[derive(Default)]
struct Upstream {
    data: Vec<u8>,
    /// Ignore `Range` and answer the whole file, as some servers do.
    no_ranges: bool,
    /// Break the connection after this many bytes of the next response. 0: don't.
    cut_after: AtomicU64,
    /// Stop sending, keeping the connection open, until `release`.
    hold: AtomicBool,
    /// Serve this instead of `data` from now on: a file replaced on the server.
    replaced: Mutex<Option<Vec<u8>>>,
    /// Each request's `Range` header ("" for none).
    requests: Mutex<Vec<String>>,
    status: AtomicU64,
    served: AtomicUsize,
}

impl Upstream {
    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

async fn upstream_file(Ax(u): Ax<Arc<Upstream>>, headers: HeaderMap) -> Response {
    let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    u.requests.lock().unwrap().push(range.clone());
    let status = u.status.load(Ordering::Relaxed);
    if status != 0 {
        return Code::from_u16(status as u16).unwrap().into_response();
    }
    let data = u.replaced.lock().unwrap().clone().unwrap_or_else(|| u.data.clone());
    let total = data.len() as u64;
    let span = range.strip_prefix("bytes=").and_then(|r| r.split_once('-')).filter(|_| !u.no_ranges).map(|(a, b)| {
        let a: u64 = a.parse().unwrap();
        (a, b.parse::<u64>().map_or(total - 1, |b| b.min(total.saturating_sub(1))))
    });
    let mut res = axum::http::Response::builder();
    let body = match span {
        Some((a, _)) if a >= total => {
            return (Code::RANGE_NOT_SATISFIABLE, [(header::CONTENT_RANGE, format!("bytes */{total}"))]).into_response();
        }
        Some((a, b)) => {
            res = res.status(206).header(header::CONTENT_RANGE, format!("bytes {a}-{b}/{total}"));
            data[a as usize..=b as usize].to_vec()
        }
        None => data,
    };
    res = res.header(header::CONTENT_LENGTH, body.len());
    let cut = u.cut_after.swap(0, Ordering::Relaxed) as usize;
    let u2 = u.clone();
    let chunks: Vec<Vec<u8>> = body.chunks(16 << 10).map(<[u8]>::to_vec).collect();
    let stream = futures_util::stream::unfold((chunks.into_iter(), 0usize), move |(mut it, sent)| {
        let u = u2.clone();
        async move {
            while u.hold.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let c = it.next()?;
            if cut > 0 {
                // hyper drops what it's holding when a body fails, so let it go out first.
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            if cut > 0 && sent >= cut {
                return Some((Err(std::io::Error::other("cut")), (it, sent)));
            }
            u.served.fetch_add(c.len(), Ordering::Relaxed);
            let sent = sent + c.len();
            Some((Ok::<_, std::io::Error>(bytes::Bytes::from(c)), (it, sent)))
        }
    });
    res.body(Body::from_stream(stream)).unwrap().into_response()
}

async fn serve_upstream(u: Upstream) -> (Arc<Upstream>, String) {
    let u = Arc::new(u);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/file", l.local_addr().unwrap());
    let app = axum::Router::new().route("/file", axum::routing::get(upstream_file)).with_state(u.clone());
    tokio::spawn(axum::serve(l, app).into_future());
    (u, url)
}

/// Waits for the download to reach the upstream, then a moment for any second one to.
async fn only_one_request(u: &Upstream) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while u.requests().is_empty() {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(u.requests().len(), 1, "{:?}", u.requests());
}

fn fast(t: &T) {
    let mut tuning = t.st.cache.tuning.write().unwrap();
    *tuning = Tuning {
        range: 256 << 10,
        range_timeout: Duration::from_secs(5),
        attempts: 3,
        backoff: Duration::from_millis(5),
        min_free: 0,
        retry_after: Duration::from_secs(3600),
        room_check: 1 << 10,
        ..Tuning::default()
    };
}

fn host() -> T {
    let t = setup();
    fast(&t);
    t
}

/// Bytes that don't compress and contain no runs of zeros.
fn noise(n: usize) -> Vec<u8> {
    let mut x = 0x2545f491u32;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8 | 1
        })
        .collect()
}

fn sha256(d: &[u8]) -> String {
    format!("sha256:{}", Sha256::digest(d).iter().map(|b| format!("{b:02x}")).collect::<String>())
}

fn want(url: &str, data: &[u8]) -> Want {
    Want { url: url.to_string(), checksum: Some(sha256(data)), ..Want::default() }
}

/// Asks until the file is no longer on its way.
async fn settled(st: &Arc<State>, name: &str, w: &Want) -> Status {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut s = cache::ensure(st, name, w);
    while matches!(s, Status::Fetching { .. }) {
        assert!(Instant::now() < deadline, "still {s:?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
        s = cache::status(st, name);
    }
    s
}

/// A visible file never exists until it's whole.
fn visible(st: &State, name: &str) -> bool {
    cache::file_path(st, name).exists()
}

#[tokio::test(flavor = "multi_thread")]
async fn fetches_in_ranges_and_keeps_the_file() {
    let t = host();
    let data = noise(700 << 10);
    let (u, url) = serve_upstream(Upstream { data: data.clone(), ..Default::default() }).await;
    let w = want(&url, &data);
    assert_eq!(settled(&t.st, "kernel", &w).await, Status::Ready { size: data.len() as u64 });
    assert!(std::fs::read(cache::file_path(&t.st, "kernel")).unwrap() == data);
    assert_eq!(u.requests().len(), 3, "three 256 KiB ranges: {:?}", u.requests());

    // The second boot costs upstream nothing.
    assert_eq!(cache::ensure(&t.st, "kernel", &w), Status::Ready { size: data.len() as u64 });
    assert_eq!(u.requests().len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_without_ranges_still_works() {
    let t = host();
    let data = noise(700 << 10);
    let (u, url) = serve_upstream(Upstream { data: data.clone(), no_ranges: true, ..Default::default() }).await;
    assert_eq!(settled(&t.st, "kernel", &want(&url, &data)).await, Status::Ready { size: data.len() as u64 });
    assert!(std::fs::read(cache::file_path(&t.st, "kernel")).unwrap() == data);
    assert_eq!(u.requests().len(), 2, "a ranged try, then the whole file");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_download_carries_on_where_it_stopped() {
    let t = host();
    let data = noise(700 << 10);
    let (u, url) = serve_upstream(Upstream { data: data.clone(), cut_after: AtomicU64::new(100 << 10), ..Default::default() }).await;
    assert_eq!(settled(&t.st, "kernel", &want(&url, &data)).await, Status::Ready { size: data.len() as u64 });
    assert!(std::fs::read(cache::file_path(&t.st, "kernel")).unwrap() == data);
    let second = u.requests()[1].clone();
    let from: u64 = second.strip_prefix("bytes=").unwrap().split('-').next().unwrap().parse().unwrap();
    assert!(from > 0, "it kept what arrived: {second}");
    assert!(u.served.load(Ordering::Relaxed) < data.len() + (64 << 10), "only the missing part came again");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_keeps_failing_is_a_failure_that_clears_itself() {
    let t = host();
    t.st.cache.tuning.write().unwrap().retry_after = Duration::from_millis(30);
    let data = noise(100 << 10);
    let (u, url) = serve_upstream(Upstream { data: data.clone(), status: AtomicU64::new(503), ..Default::default() }).await;
    let w = want(&url, &data);
    let s = settled(&t.st, "kernel", &w).await;
    assert!(matches!(&s, Status::Failed { permanent: false, why } if why.contains("503")), "{s:?}");
    assert!(!visible(&t.st, "kernel"));

    // Backing off: this ask doesn't reach upstream...
    let n = u.requests().len();
    assert!(matches!(cache::ensure(&t.st, "kernel", &w), Status::Failed { .. }));
    assert_eq!(u.requests().len(), n);
    // ...and once the wait is over, the next one does.
    u.status.store(0, Ordering::Relaxed);
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(settled(&t.st, "kernel", &w).await, Status::Ready { size: data.len() as u64 });
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restart_mid_download_resumes_from_the_part_file() {
    let t = host();
    let data = noise(700 << 10);
    let (u, url) = serve_upstream(Upstream { data: data.clone(), ..Default::default() }).await;
    // What a killed BootBob leaves behind.
    let dir = t.st.config.data_dir.join("cache").join(".fetch");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("kernel.part"), &data[..300 << 10]).unwrap();
    std::fs::write(dir.join("old.unpack"), b"half unpacked").unwrap();
    cache::startup(&t.st);
    assert!(dir.join("kernel.part").exists(), "kept to resume");
    assert!(!dir.join("old.unpack").exists(), "useless");

    assert_eq!(settled(&t.st, "kernel", &want(&url, &data)).await, Status::Ready { size: data.len() as u64 });
    assert!(std::fs::read(cache::file_path(&t.st, "kernel")).unwrap() == data);
    assert!(u.requests()[0].starts_with(&format!("bytes={}-", 300 << 10)), "{:?}", u.requests());
    assert!(!dir.join("kernel.part").exists(), "cleaned up");
    assert!(t.st.events(None, 20).iter().any(|e| e.message.starts_with("Resuming kernel")));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resume_that_doesnt_match_starts_again_cleanly() {
    let t = host();
    let data = noise(700 << 10);
    let (u, url) = serve_upstream(Upstream { data: data.clone(), ..Default::default() }).await;
    // Left by an earlier attempt, of a file that has since changed upstream.
    let dir = t.st.config.data_dir.join("cache").join(".fetch");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("kernel.part"), noise(300 << 10).iter().map(|b| b ^ 0x55).collect::<Vec<u8>>()).unwrap();

    assert_eq!(settled(&t.st, "kernel", &want(&url, &data)).await, Status::Ready { size: data.len() as u64 });
    assert!(std::fs::read(cache::file_path(&t.st, "kernel")).unwrap() == data);
    assert!(u.requests().iter().any(|r| r.starts_with("bytes=0-")), "fetched again from the start: {:?}", u.requests());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_checksum_keeps_nothing_and_isnt_retried_in_a_loop() {
    let t = host();
    let data = noise(100 << 10);
    let (u, url) = serve_upstream(Upstream { data: data.clone(), ..Default::default() }).await;
    let w = want(&url, b"something else entirely");
    let s = settled(&t.st, "kernel", &w).await;
    assert!(matches!(&s, Status::Failed { permanent: true, why } if why.contains("checksum")), "{s:?}");
    assert!(!visible(&t.st, "kernel"), "a bad file must never become visible");
    assert!(std::fs::read_dir(t.st.config.data_dir.join("cache").join(".fetch")).unwrap().next().is_none(), "no bytes kept");

    let n = u.requests().len();
    for _ in 0..3 {
        assert!(matches!(cache::ensure(&t.st, "kernel", &w), Status::Failed { permanent: true, .. }));
    }
    assert_eq!(u.requests().len(), n, "polling machines don't re-download it");

    // Someone asks for it again anyway: `retry` goes straight back to upstream.
    let again = Want { retry: true, ..w };
    assert!(matches!(settled(&t.st, "kernel", &again).await, Status::Failed { .. }));
    assert!(u.requests().len() > n);

    // Someone corrects the entry: that's a different file, and needs no retry.
    assert_eq!(settled(&t.st, "kernel", &want(&url, &data)).await, Status::Ready { size: data.len() as u64 });
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_replaced_upstream_during_a_download_is_caught() {
    let t = host();
    let data = noise(700 << 10);
    let (u, url) = serve_upstream(Upstream { data: data.clone(), hold: AtomicBool::new(true), ..Default::default() }).await;
    let w = want(&url, &data);
    assert!(matches!(cache::ensure(&t.st, "kernel", &w), Status::Fetching { .. }));
    only_one_request(&u).await;
    *u.replaced.lock().unwrap() = Some(noise(900 << 10));
    u.hold.store(false, Ordering::Relaxed);
    let s = settled(&t.st, "kernel", &w).await;
    assert!(matches!(s, Status::Failed { .. }), "{s:?}");
    assert!(!visible(&t.st, "kernel"));
}

#[tokio::test(flavor = "multi_thread")]
async fn two_machines_share_one_download() {
    let t = host();
    let data = noise(100 << 10);
    let (u, url) = serve_upstream(Upstream { data: data.clone(), hold: AtomicBool::new(true), ..Default::default() }).await;
    let w = want(&url, &data);
    let tasks: Vec<_> = (0..8)
        .map(|_| {
            let (st, w) = (t.st.clone(), w.clone());
            tokio::task::spawn_blocking(move || cache::ensure(&st, "kernel", &w))
        })
        .collect();
    for task in tasks {
        assert!(matches!(task.await.unwrap(), Status::Fetching { .. }));
    }
    only_one_request(&u).await;
    u.hold.store(false, Ordering::Relaxed);
    assert_eq!(settled(&t.st, "kernel", &w).await, Status::Ready { size: data.len() as u64 });
    assert_eq!(u.requests().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stall_is_noticed_and_retried() {
    let t = host();
    {
        let mut tuning = t.st.cache.tuning.write().unwrap();
        tuning.range_timeout = Duration::from_millis(100);
        tuning.attempts = 100;
    }
    let data = noise(100 << 10);
    let (u, url) = serve_upstream(Upstream { data: data.clone(), hold: AtomicBool::new(true), ..Default::default() }).await;
    let w = want(&url, &data);
    cache::ensure(&t.st, "kernel", &w);
    let deadline = Instant::now() + Duration::from_secs(30);
    while u.requests().len() < 2 {
        assert!(Instant::now() < deadline, "a stalled request gets replaced: {:?}", u.requests());
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    u.hold.store(false, Ordering::Relaxed);
    assert_eq!(settled(&t.st, "kernel", &w).await, Status::Ready { size: data.len() as u64 });
}

// ---- Disk images ----

fn xz(d: &[u8]) -> Vec<u8> {
    let mut w = lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::with_preset(1)).unwrap();
    w.write_all(d).unwrap();
    w.finish().unwrap()
}

fn gz(d: &[u8]) -> Vec<u8> {
    let mut w = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    w.write_all(d).unwrap();
    w.finish().unwrap()
}

/// Something shaped like a disk: data, and a lot of zeros.
fn disk_bytes() -> Vec<u8> {
    let mut d = noise(32 << 10);
    d.resize(4 << 20, 0);
    d.extend_from_slice(&noise(4 << 10));
    d
}

#[tokio::test(flavor = "multi_thread")]
async fn a_disk_is_unpacked_and_checked_against_the_image_checksum() {
    let t = host();
    let img = disk_bytes();
    // Raspberry Pi publishes only the unpacked image's checksum; Alpine publishes both.
    for (name, packed, checksum) in [
        ("xz.img", xz(&img), None),
        ("gz.img", gz(&img), None),
        ("zst.img", ruzstd::encoding::compress_to_vec(&img[..], ruzstd::encoding::CompressionLevel::Fastest), None),
        ("both.img", gz(&img), Some(sha256(&gz(&img)))),
        ("raw.img", img.clone(), None),
    ] {
        let (_u, url) = serve_upstream(Upstream { data: packed, ..Default::default() }).await;
        let w = Want { url, checksum, image_checksum: Some(sha256(&img)), ..Want::default() };
        assert_eq!(settled(&t.st, name, &w).await, Status::Ready { size: img.len() as u64 }, "{name}");
        assert!(std::fs::read(t.st.config.images_dir().join(name)).unwrap() == img, "{name} changed");
    }
    // A wrong image checksum: all of it arrived and unpacked, none of it is kept.
    let (_u, url) = serve_upstream(Upstream { data: gz(&img), ..Default::default() }).await;
    let w = Want { url, image_checksum: Some(sha256(b"not that")), ..Want::default() };
    let s = settled(&t.st, "bad.img", &w).await;
    assert!(matches!(&s, Status::Failed { permanent: true, why } if why.contains("checksum")), "{s:?}");
    assert!(!visible(&t.st, "bad.img"));
    assert!(std::fs::read_dir(t.st.config.images_dir().join(".fetch")).unwrap().next().is_none(), "no bytes kept");
}

#[tokio::test(flavor = "multi_thread")]
async fn sha512_works_for_debian() {
    let t = host();
    let img = disk_bytes();
    let (_u, url) = serve_upstream(Upstream { data: img.clone(), ..Default::default() }).await;
    let hex: String = Sha512::digest(&img).iter().map(|b| format!("{b:02x}")).collect();
    let w = Want { url, checksum: Some(format!("sha512:{hex}")), ..Want::default() };
    assert_eq!(settled(&t.st, "debian.img", &w).await, Status::Ready { size: img.len() as u64 });
}

#[tokio::test(flavor = "multi_thread")]
async fn what_isnt_a_raw_disk_is_refused() {
    let t = host();
    for (name, data, says) in [
        ("q.img", [b"QFI\xfb".as_slice(), &noise(8 << 10)].concat(), "qcow2"),
        ("z.img", [b"PK\x03\x04".as_slice(), &noise(8 << 10)].concat(), "zip"),
        // Inside the compression: only unpacking shows it.
        ("gq.img", gz(&[b"QFI\xfb".as_slice(), &noise(8 << 10)].concat()), "qcow2"),
    ] {
        let (_u, url) = serve_upstream(Upstream { data: data.clone(), ..Default::default() }).await;
        let s = settled(&t.st, name, &want(&url, &data)).await;
        assert!(matches!(&s, Status::Failed { permanent: true, why } if why.contains(says)), "{name}: {s:?}");
        assert!(!visible(&t.st, name));
    }
    // A file that isn't a disk is kept as it is, compressed or not: this is an initrd.
    let data = gz(&noise(8 << 10));
    let (_u, url) = serve_upstream(Upstream { data: data.clone(), ..Default::default() }).await;
    assert_eq!(settled(&t.st, "initrd.gz", &want(&url, &data)).await, Status::Ready { size: data.len() as u64 });
    assert!(std::fs::read(cache::file_path(&t.st, "initrd.gz")).unwrap() == data);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_nobody_fetched_is_never_overwritten() {
    let t = host();
    std::fs::write(t.st.config.images_dir().join("desky.img"), b"someone's root").unwrap();
    let data = noise(8 << 10);
    let (u, url) = serve_upstream(Upstream { data: data.clone(), ..Default::default() }).await;
    let s = cache::ensure(&t.st, "desky.img", &want(&url, &data));
    assert!(matches!(&s, Status::Failed { permanent: true, why } if why.contains("won't be overwritten")), "{s:?}");
    assert!(u.requests().is_empty());
    assert_eq!(std::fs::read(t.st.config.images_dir().join("desky.img")).unwrap(), b"someone's root");
}

// ---- Eviction ----

/// Fetches `name`, then makes it look used `age` ago.
async fn cached(t: &T, name: &str, size: usize, age: u64) -> Want {
    let data = noise(size);
    let (_u, url) = serve_upstream(Upstream { data: data.clone(), ..Default::default() }).await;
    let w = want(&url, &data);
    assert!(matches!(settled(&t.st, name, &w).await, Status::Ready { .. }), "{name}");
    let mut r = t.st.cache_row(name).unwrap();
    r.last_used = now_ms() - age;
    t.st.put_cache_row(&r);
    w
}

const HOUR: u64 = 3_600_000;

fn cap(t: &T, bytes: u64) {
    t.st.cache.tuning.write().unwrap().cap = Some(bytes);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_least_recently_used_goes_first() {
    let t = host();
    cap(&t, 1 << 20);
    cached(&t, "oldest", 300 << 10, 3 * HOUR).await;
    cached(&t, "older", 300 << 10, 2 * HOUR).await;
    cached(&t, "newest", 300 << 10, HOUR).await;
    cached(&t, "more", 300 << 10, 0).await;
    assert!(!visible(&t.st, "oldest"), "evicted");
    assert!(t.st.cache_row("oldest").is_none());
    for keep in ["older", "newest", "more"] {
        assert!(visible(&t.st, keep), "{keep}");
    }
    assert!(t.st.events(None, 50).iter().any(|e| e.message.contains("Evicted oldest")));
}

#[tokio::test(flavor = "multi_thread")]
async fn pinned_in_use_recent_overlaid_and_uploaded_files_stay() {
    let t = host();
    cap(&t, 1400 << 10);
    let mut w = cached(&t, "pinned", 300 << 10, 9 * HOUR).await;
    w.pinned = true;
    cache::ensure(&t.st, "pinned", &w);
    let mut r = t.st.cache_row("pinned").unwrap();
    r.last_used = now_ms() - 9 * HOUR;
    t.st.put_cache_row(&r);

    cached(&t, "live.img", 300 << 10, 8 * HOUR).await;
    let (_id, _rx) = t.st.nbd_live.claim_for_test(t.st.config.images_dir().join("live.img"));

    // Older than all of them, and a machine has built on it.
    cached(&t, "base.img", 300 << 10, 10 * HOUR).await;
    let mac: Mac = MAC.parse().unwrap();
    let dir = crate::nbd::overlay_dir(&t.st, mac);
    bootbob_cow::Cow::open(&t.st.config.images_dir().join("base.img"), &dir, "base", false).unwrap();

    cached(&t, "recent", 300 << 10, 5 * 60 * 1000).await;

    // Not the cache's: it doesn't count, and it can't be touched.
    std::fs::write(t.st.config.images_dir().join("desky.img"), noise(4 << 20)).unwrap();

    // Nothing can go, so this one fails, having evicted nothing.
    let data = noise(500 << 10);
    let (_u, url) = serve_upstream(Upstream { data: data.clone(), ..Default::default() }).await;
    let s = settled(&t.st, "big", &want(&url, &data)).await;
    assert!(matches!(&s, Status::Failed { permanent: false, why } if why.contains("cache is full")), "{s:?}");
    for keep in ["pinned", "live.img", "base.img", "recent", "desky.img"] {
        assert!(visible(&t.st, keep), "{keep}");
    }
    assert!(!visible(&t.st, "big"));
}

#[tokio::test(flavor = "multi_thread")]
async fn when_the_room_cant_be_found_nothing_is_evicted_for_nothing() {
    let t = host();
    cap(&t, 1 << 20);
    cached(&t, "a", 300 << 10, 3 * HOUR).await;
    let mut w = cached(&t, "b", 500 << 10, 3 * HOUR).await;
    w.pinned = true;
    cache::ensure(&t.st, "b", &w);
    // 500 KiB pinned and 1 MiB wanted: evicting `a` wouldn't make it fit.
    let data = noise(1 << 20);
    let (_u, url) = serve_upstream(Upstream { data: data.clone(), ..Default::default() }).await;
    let s = settled(&t.st, "big", &want(&url, &data)).await;
    assert!(matches!(s, Status::Failed { .. }), "{s:?}");
    assert!(!visible(&t.st, "big"));
}

#[tokio::test(flavor = "multi_thread")]
async fn removing_by_hand_respects_machines() {
    let t = host();
    cached(&t, "base.img", 8 << 10, 0).await;
    let mac: Mac = MAC.parse().unwrap();
    let dir = crate::nbd::overlay_dir(&t.st, mac);
    bootbob_cow::Cow::open(&t.st.config.images_dir().join("base.img"), &dir, "base", false).unwrap();
    let (code, _) = cache::remove(&t.st, "base.img").unwrap_err();
    assert_eq!(code, 409);
    bootbob_cow::Cow::remove(&dir, "base").unwrap();
    assert_eq!(cache::remove(&t.st, "base.img"), Ok(true));
    assert!(!visible(&t.st, "base.img"));
    assert_eq!(cache::remove(&t.st, "base.img"), Ok(false));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_cache_survives_a_restart() {
    let t = host();
    let w = cached(&t, "kernel", 8 << 10, 0).await;
    let config = t.st.config.clone();
    let T { st, _dir } = t;
    drop(st);
    let st = Arc::new(State::open(config).unwrap());
    cache::startup(&st);
    assert!(matches!(cache::ensure(&st, "kernel", &w), Status::Ready { .. }));
    // A row whose file went missing is the same as never fetched.
    std::fs::remove_file(cache::file_path(&st, "kernel")).unwrap();
    cache::startup(&st);
    assert!(st.cache_row("kernel").is_none());
}

// ---- Boots ----

async fn entry(t: &T, id: &str, url: &str, data: &[u8], image_checksum: bool) {
    let mut e = json!({"title": format!("The {id}"), "url": url});
    e[if image_checksum { "image_checksum" } else { "checksum" }] = json!(sha256(data));
    let (s, body) = call(&t.st, Method::PUT, &format!("/api/catalogue/{id}"), Some(e)).await;
    assert_eq!(s, StatusCode::OK, "{body}");
}

async fn assign_once(t: &T, image: &str) {
    let (s, body) = call(&t.st, Method::PUT, &format!("/api/devices/{MAC}/assignment"), Some(json!({"image": image, "once": true}))).await;
    assert_eq!(s, StatusCode::OK, "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pc_waits_at_the_menu_then_boots_what_arrived() {
    let t = host();
    let (kernel, initrd) = (noise(300 << 10), gz(&noise(8 << 10)));
    let (ku, kurl) = serve_upstream(Upstream { data: kernel.clone(), hold: AtomicBool::new(true), ..Default::default() }).await;
    let (_iu, iurl) = serve_upstream(Upstream { data: initrd.clone(), ..Default::default() }).await;
    entry(&t, "linux", &kurl, &kernel, false).await;
    entry(&t, "initrd.gz", &iurl, &initrd, false).await;
    let img = json!({"title": "Debian installer", "action": {"kind": "kernel", "kernel": "{cached:linux}",
        "initrds": ["{cached:initrd.gz}"], "cmdline": "console=ttyS0 url={cached:linux}"}});
    let (s, body) = call(&t.st, Method::PUT, "/api/images/debian", Some(img)).await;
    assert_eq!(s, StatusCode::OK, "{body}");

    get(&t.st, &ident("")).await;
    assign_once(&t, "debian").await;

    // Not there yet: the polling menu, with the normal ways out, and quick checks.
    let script = get(&t.st, &ident("")).await;
    assert!(script.contains("Waiting to boot Debian installer"), "{script}");
    assert!(script.contains("The linux: "), "{script}");
    assert!(script.contains("choose --timeout 5000"), "{script}");
    assert!(script.contains("item reboot"), "{script}");
    let d = json(&t.st, &format!("/api/devices/{MAC}")).await;
    assert_eq!(d["state"], "fetching", "{d}");
    assert_eq!(d["assignment"]["once"], true, "the one-shot isn't used up by waiting");
    assert!(d["last_boot"].is_null(), "and it hasn't booted");

    // Another poll doesn't start another download.
    get(&t.st, &ident("&poll=1")).await;
    only_one_request(&ku).await;

    ku.hold.store(false, Ordering::Relaxed);
    let deadline = Instant::now() + Duration::from_secs(30);
    let script = loop {
        let s = get(&t.st, &ident("&poll=1")).await;
        if !s.contains("Waiting to boot") {
            break s;
        }
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert!(
        script.contains("kernel http://10.255.0.5:8067/files/cache/linux console=ttyS0 url=http://10.255.0.5:8067/files/cache/linux"),
        "{script}"
    );
    assert!(script.contains("initrd http://10.255.0.5:8067/files/cache/initrd.gz"), "{script}");
    let d = json(&t.st, &format!("/api/devices/{MAC}")).await;
    assert_eq!(d["last_boot"]["image"], "debian");
    assert!(d["assignment"].is_null(), "used up by the real boot");
    assert!(d["waiting"].is_null());

    // What iPXE fetches, with a Range an ISO boot would use.
    let req = Request::builder().uri("/files/cache/linux").header("range", "bytes=10-19").body(Body::empty()).unwrap();
    let res = crate::http::router(t.st.clone()).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(res.into_body().collect().await.unwrap().to_bytes(), kernel[10..20]);

    // The second boot: straight through, and upstream hears nothing.
    let n = ku.requests().len();
    assign_once(&t, "debian").await;
    let script = get(&t.st, &ident("")).await;
    assert!(script.contains("kernel http://10.255.0.5:8067/files/cache/linux"), "{script}");
    assert_eq!(ku.requests().len(), n);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_download_sends_the_pc_to_the_menu_with_the_reason() {
    let t = host();
    let data = noise(8 << 10);
    let (_u, url) = serve_upstream(Upstream { data, ..Default::default() }).await;
    entry(&t, "linux", &url, b"the wrong thing", false).await;
    let (s, body) =
        call(&t.st, Method::PUT, "/api/images/x", Some(json!({"title": "X", "action": {"kind": "kernel", "kernel": "{cached:linux}"}})))
            .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    get(&t.st, &ident("")).await;
    assign_once(&t, "x").await;
    let deadline = Instant::now() + Duration::from_secs(30);
    let script = loop {
        let s = get(&t.st, &ident("&poll=1")).await;
        if !s.contains("Waiting to boot") {
            break s;
        }
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert!(script.contains("Can't boot X."), "{script}");
    assert!(script.contains("checksum"), "{script}");
    assert!(!script.contains("kernel http"), "{script}");
    let d = json(&t.st, &format!("/api/devices/{MAC}")).await;
    assert_eq!(d["assignment"]["once"], true, "still its next boot, once the entry is fixed");
    assert!(d["last_boot"].is_null());
}

#[tokio::test(flavor = "multi_thread")]
async fn something_chosen_at_the_menu_waits_for_that_choice() {
    let t = host();
    let data = noise(8 << 10);
    let (u, url) = serve_upstream(Upstream { data: data.clone(), hold: AtomicBool::new(true), ..Default::default() }).await;
    entry(&t, "memtest", &url, &data, false).await;
    let img = json!({"title": "Memtest", "group": "Tools", "action": {"kind": "chain", "url": "{cached:memtest}"}});
    call(&t.st, Method::PUT, "/api/images/memtest", Some(img)).await;
    let chose = ident("&item=memtest").replace("/boot/ipxe?", "/boot/ipxe/chose?");
    get(&t.st, &ident("")).await;

    let script = get(&t.st, &chose).await;
    assert!(
        script.contains(":refresh\nchain --replace --autofree ${bootbob}/boot/ipxe/chose?${bootbob-id}&item=memtest || goto unreachable"),
        "{script}"
    );
    u.hold.store(false, Ordering::Relaxed);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let s = get(&t.st, &chose).await;
        if s.contains("chain --autofree http://10.255.0.5:8067/files/cache/memtest") {
            break;
        }
        assert!(Instant::now() < deadline, "{s}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A bare FAT disk with a Pi's first boot file.
fn pi_disk() -> Vec<u8> {
    let mut disk = std::io::Cursor::new(vec![0u8; 4 << 20]);
    fatfs::format_volume(&mut disk, fatfs::FormatVolumeOptions::new()).unwrap();
    {
        let fs = fatfs::FileSystem::new(&mut disk, fatfs::FsOptions::new()).unwrap();
        fs.root_dir().create_file("start4.elf").unwrap().write_all(b"PI-FIRMWARE").unwrap();
    }
    disk.into_inner()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pi_is_told_no_until_its_image_is_here() {
    let t = host();
    let img = pi_disk();
    let (u, url) = serve_upstream(Upstream { data: xz(&img), hold: AtomicBool::new(true), ..Default::default() }).await;
    entry(&t, "pios.img", &url, &img, true).await;
    let (s, body) = put_pi_image(&t, "pios", json!({"source": "pios.img", "cow": true})).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let (s, _) = put_pi_image(&t, "bad", json!({"source": "pios.img", "cow": false})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "a catalogue disk must be copy-on-write");

    let mac = "dc:a6:32:01:02:03";
    let (s, body) = call(&t.st, Method::PUT, &format!("/api/devices/{mac}/assignment"), Some(json!({"image": "pios", "once": true}))).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let tftp = crate::tftp::Tftp { st: t.st.clone() };
    let ip = [10, 255, 7, 77];
    t.st.learn_address(Ipv4Addr::from(ip), mac.parse().unwrap());

    for _ in 0..3 {
        assert!(pi_file(&tftp, ip, "6a6f4d1d/start4.elf").await.is_none(), "told no");
    }
    let d = json(&t.st, &format!("/api/devices/{mac}")).await;
    assert_eq!(d["state"], "fetching", "{d}");
    assert!(d["last_boot"].is_null(), "no boot yet: {d}");
    assert_eq!(d["assignment"]["once"], true, "the one-shot isn't used up");
    assert!(d["pi_session"].is_null(), "and no boot session started: {d}");
    only_one_request(&u).await;

    u.hold.store(false, Ordering::Relaxed);
    let deadline = Instant::now() + Duration::from_secs(30);
    let got = loop {
        if let Some(f) = pi_file(&tftp, ip, "6a6f4d1d/start4.elf").await {
            break f;
        }
        assert!(Instant::now() < deadline, "{:?}, events {:?}", cache::status(&t.st, "pios.img"), t.st.events(None, 10));
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(got, b"PI-FIRMWARE");
    let d = json(&t.st, &format!("/api/devices/{mac}")).await;
    assert_eq!(d["last_boot"]["image"], "pios", "{d}");
    assert!(d["assignment"].is_null());
    assert!(d["waiting"].is_null());
    assert!(!d["pi_session"].is_null());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_catalogue_checks_its_entries() {
    let t = host();
    let sum = sha256(b"x");
    let put = |id: &'static str, e: Value| {
        let st = t.st.clone();
        async move { call(&st, Method::PUT, &format!("/api/catalogue/{id}"), Some(e)).await.0 }
    };
    assert_eq!(put("a", json!({"title": "A", "url": "http://x/a"})).await, StatusCode::BAD_REQUEST, "no checksum");
    assert_eq!(put("a", json!({"title": "A", "url": "http://x/a", "checksum": "md5:1"})).await, StatusCode::BAD_REQUEST);
    assert_eq!(put("a", json!({"title": "A", "url": "ftp://x/a", "checksum": sum})).await, StatusCode::BAD_REQUEST);
    assert_eq!(put("..a", json!({"title": "A", "url": "http://x/a", "checksum": sum})).await, StatusCode::BAD_REQUEST);
    assert_eq!(put("a", json!({"title": "A", "url": "http://x/a", "checksum": sum, "store": "nope"})).await, StatusCode::BAD_REQUEST);
    assert_eq!(
        put("a", json!({"title": "A", "url": "http://x/a", "image_checksum": sum})).await,
        StatusCode::BAD_REQUEST,
        "a file isn't unpacked"
    );
    assert_eq!(put("a", json!({"title": "A", "url": "store:nope/a.img"})).await, StatusCode::BAD_REQUEST);
    assert_eq!(put("a", json!({"title": "A", "url": "http://x/a", "checksum": sum})).await, StatusCode::OK);
    assert_eq!(put("d.img", json!({"title": "D", "url": "http://x/d", "image_checksum": sum})).await, StatusCode::OK);

    let v = json(&t.st, "/api/catalogue").await;
    assert_eq!(v["entries"].as_array().unwrap().len(), 2, "{v}");
    assert_eq!(v["entries"][0]["status"]["state"], "absent", "{v}");
    assert_eq!(v["hosts"][0]["cap"], 150u64 << 30, "{v}");

    // Images may name what the catalogue can fetch, and only that.
    let img = |u: &str| json!({"title": "I", "action": {"kind": "chain", "url": u}});
    assert_eq!(call(&t.st, Method::PUT, "/api/images/i", Some(img("{cached:nope}"))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(call(&t.st, Method::PUT, "/api/images/i", Some(img("{cached:d.img}"))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(call(&t.st, Method::PUT, "/api/images/i", Some(img("{cached:a}"))).await.0, StatusCode::OK);
    let (s, body) = call(&t.st, Method::DELETE, "/api/catalogue/a", None).await;
    assert_eq!(s, StatusCode::CONFLICT, "an image boots it: {body}");
    call(&t.st, Method::DELETE, "/api/images/i", None).await;
    assert_eq!(call(&t.st, Method::DELETE, "/api/catalogue/a", None).await.0, StatusCode::NO_CONTENT);

    // Entries survive a restart.
    let config = t.st.config.clone();
    let T { st, _dir } = t;
    drop(st);
    let st = Arc::new(State::open(config).unwrap());
    assert_eq!(st.catalogue().len(), 1);
}

#[test]
fn reads_raspberry_pis_list() {
    // The shape of os_list_imagingutility_v4.json, on 2026-10-31.
    let list = r#"{"os_list": [
        {"name": "Raspberry Pi OS", "subitems": [
            {"name": "Lite", "url": "https://downloads.raspberrypi.com/lite.img.xz", "extract_sha256": "ab", "image_download_sha256": null,
             "image_download_size": 5, "extract_size": 30, "release_date": "2026-10-06", "description": "d"}
        ]},
        {"name": "Zipped", "url": "https://x/y.zip", "extract_sha256": "ab", "image_download_sha256": "cd"},
        {"name": "No checksum", "url": "https://x/y.img.xz"},
        {"name": "Copy", "url": "https://downloads.raspberrypi.com/lite.img.xz", "extract_sha256": "ab"},
        {"name": "A link", "url": "https://www.raspberrypi.com/software/"}
    ]}"#;
    let v = crate::catalogue::parse_pi_os(list).unwrap();
    assert_eq!(v.len(), 1, "{v:?}");
    assert_eq!(v[0].title, "Lite");
    assert_eq!(v[0].checksum, None);
    assert_eq!(v[0].image_checksum.as_deref(), Some("sha256:ab"));
    assert_eq!(v[0].image_size, 30);
}

// ---- Stores ----

struct Pair {
    main: T,
    store: Arc<State>,
    _dir: tempfile::TempDir,
}

/// A main server and a store, on real sockets with real peer addresses.
async fn pair() -> Pair {
    let main_l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let store_l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (main_url, store_url) = (format!("http://{}", main_l.local_addr().unwrap()), format!("http://{}", store_l.local_addr().unwrap()));
    let dir = tempfile::tempdir().unwrap();
    let t = setup();
    let config = Config {
        stores: vec![crate::config::StoreConfig { name: "s1".into(), url: store_url, nbd_host: Some("10.255.0.3".into()) }],
        ..t.st.config.clone()
    };
    let T { st, _dir: main_dir } = t;
    drop(st);
    let main = T { st: Arc::new(State::open(config).unwrap()), _dir: main_dir };
    fast(&main);
    let store = Arc::new(
        State::open(Config {
            data_dir: dir.path().join("data"),
            mode: crate::config::Mode::Store,
            main_url: Some(main_url),
            store_name: Some("s1".into()),
            ..Config::default()
        })
        .unwrap(),
    );
    fast(&T { st: store.clone(), _dir: tempfile::tempdir().unwrap() });
    tokio::spawn(axum::serve(main_l, router(main.st.clone())).into_future());
    let app = crate::http::store_router(store.clone()).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(axum::serve(store_l, app).into_future());
    Pair { main, store, _dir: dir }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_store_fetches_for_itself_and_machines_go_to_it() {
    let p = pair().await;
    let t = &p.main;
    let (img, data) = (fat_disk_bytes(), noise(32 << 10));
    let (_u, url) = serve_upstream(Upstream { data: gz(&img), ..Default::default() }).await;
    let (_u2, kurl) = serve_upstream(Upstream { data: data.clone(), ..Default::default() }).await;
    let e = json!({"title": "Golden", "store": "s1", "url": url, "image_checksum": sha256(&img)});
    let (s, body) = call(&t.st, Method::PUT, "/api/catalogue/golden.img", Some(e)).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let e = json!({"title": "Kernel", "store": "s1", "url": kurl, "checksum": sha256(&data)});
    call(&t.st, Method::PUT, "/api/catalogue/kernel", Some(e)).await;

    let img_json = json!({"title": "G", "disk": {"source": "s1:golden.img", "initrd": "initrd.img",
        "cmdline": "nbdroot={server},{disk},nbd0", "cow": true}});
    let (s, body) = call(&t.st, Method::PUT, "/api/images/g", Some(img_json)).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let k = json!({"title": "K", "action": {"kind": "kernel", "kernel": "{cached:kernel}"}});
    call(&t.st, Method::PUT, "/api/images/k", Some(k)).await;

    get(&t.st, &ident("")).await;
    call(&t.st, Method::PUT, &format!("/api/devices/{MAC}/assignment"), Some(json!({"image": "g"}))).await;
    let deadline = Instant::now() + Duration::from_secs(30);
    let script = loop {
        let s = get(&t.st, &ident("&poll=1")).await;
        if !s.contains("Waiting to boot") {
            break s;
        }
        assert!(Instant::now() < deadline, "{s}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert!(script.contains("nbdroot=10.255.0.3,golden.img,nbd0"), "{script}");
    assert!(!t.st.config.images_dir().join("golden.img").exists(), "on the store, not here");
    assert!(p.store.config.images_dir().join("golden.img").is_file());
    assert_eq!(get(&t.st, "/boot/disk/g/vmlinuz").await, "STORE-KERNEL");

    // A file on a store is fetched straight from the store.
    call(&t.st, Method::PUT, &format!("/api/devices/{MAC}/assignment"), Some(json!({"image": "k"}))).await;
    let deadline = Instant::now() + Duration::from_secs(30);
    let script = loop {
        let s = get(&t.st, &ident("&poll=1")).await;
        if !s.contains("Waiting to boot") {
            break s;
        }
        assert!(Instant::now() < deadline, "{s}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let url = script.lines().find_map(|l| l.strip_prefix("kernel ")).unwrap().trim().to_string();
    assert!(url.contains("/files/cache/kernel") && url.starts_with(t.st.config.stores[0].url.as_str()), "{script}");

    // The main server sees the store's numbers, and pins through to it.
    let v = json(&t.st, "/api/catalogue").await;
    let golden = v["entries"].as_array().unwrap().iter().find(|e| e["id"] == "golden.img").unwrap().clone();
    assert_eq!(golden["status"]["state"], "ready", "{v}");
    assert_eq!(v["hosts"][1]["store"], "s1", "{v}");
    let e = json!({"title": "Golden", "store": "s1", "url": golden["url"], "image_checksum": golden["image_checksum"], "pinned": true});
    let (s, body) = call(&t.st, Method::PUT, "/api/catalogue/golden.img", Some(e)).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert!(p.store.cache_row("golden.img").unwrap().pinned);
}

fn fat_disk_bytes() -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    fat_disk_at(dir.path(), "x.img");
    std::fs::read(dir.path().join("x.img")).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stores_disk_can_be_cached_here_but_only_read_by_the_main_server() {
    let p = pair().await;
    let t = &p.main;
    let img = fat_disk_bytes();
    std::fs::write(p.store.config.images_dir().join("golden.img"), &img).unwrap();

    let e = json!({"title": "Local copy", "url": "store:s1/golden.img"});
    let (s, body) = call(&t.st, Method::PUT, "/api/catalogue/golden.img", Some(e)).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let status = {
        let (st, entry) = (t.st.clone(), t.st.catalogue_entry("golden.img").unwrap());
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let (st2, e2) = (st.clone(), entry.clone());
            let s = tokio::task::spawn_blocking(move || crate::catalogue::ensure(&st2, &e2, false)).await.unwrap();
            if !matches!(s, Status::Fetching { .. }) {
                break s;
            }
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    assert_eq!(status, Status::Ready { size: img.len() as u64 });
    assert!(std::fs::read(t.st.config.images_dir().join("golden.img")).unwrap() == img);

    // The store's disk route answers the main server's address only.
    let not_main = Arc::new(
        State::open(Config {
            data_dir: p._dir.path().join("other"),
            mode: crate::config::Mode::Store,
            main_url: Some("http://10.255.0.5:8067".into()),
            ..Config::default()
        })
        .unwrap(),
    );
    fat_disk_at(&not_main.config.images_dir(), "root.img");
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let app = crate::http::store_router(not_main.clone()).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(axum::serve(l, app).into_future());
    for path in ["/store/disk/root.img", "/store/digest/root.img"] {
        let url = format!("{url}{path}");
        let code = tokio::task::spawn_blocking(move || {
            let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
            agent.get(&url).call().unwrap().status().as_u16()
        })
        .await
        .unwrap();
        assert_eq!(code, 403, "{path}");
    }
}
