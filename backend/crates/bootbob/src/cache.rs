//! The files this host fetched on demand: a size-capped, least-recently-used cache.
//!
//! The main server and each image store run one of these for themselves. A file arrives
//! whole and verified or not at all: it's built under a hidden directory, and only then
//! renamed to where NBD, TFTP and HTTP can see it, so nothing unverified ever reaches a
//! machine. A disk image (`*.img`) lands in the images directory, where the rest of
//! BootBob finds it like any other, and is unpacked on the way. Anything else (a kernel, an
//! initrd, an ISO) lands in `<data_dir>/cache/files`, byte for byte: `initrd.gz` must reach
//! the machine as it is.
//!
//! The cache keeps one row per file it fetched. Eviction only ever looks at rows, so disks
//! somebody uploaded or built by hand can't be evicted.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use serde::{Deserialize, Serialize};

use crate::fetch::{self, Checksum, Failure, Step, Tuning};
use crate::model::{Millis, now_ms};
use crate::sparse::{self, Packing};
use crate::state::State;

/// A file used this recently can't be evicted: a machine that has its boot files and
/// hasn't connected to the disk yet looks exactly like one that left.
const LEASE_MS: Millis = 15 * 60 * 1000;

/// What to fetch, as the main server tells a host.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Want {
    /// An upstream URL, or `store:<store>/<disk>.img` for a disk on one of this server's
    /// image stores.
    pub url: String,
    /// The download's checksum.
    #[serde(default)]
    pub checksum: Option<String>,
    /// The unpacked disk image's checksum: all Raspberry Pi's list publishes.
    #[serde(default)]
    pub image_checksum: Option<String>,
    #[serde(default)]
    pub pinned: bool,
    /// Try again now, even if it failed recently.
    #[serde(default)]
    pub retry: bool,
}

impl Want {
    fn same_file(&self, other: &Want) -> bool {
        self.url == other.url && self.checksum == other.checksum && self.image_checksum == other.image_checksum
    }
}

/// A file in the cache.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Row {
    pub name: String,
    pub url: String,
    pub checksum: Option<String>,
    pub image_checksum: Option<String>,
    pub size: u64,
    /// Bytes it takes on disk: less than `size` for a disk image with holes.
    pub allocated: u64,
    pub fetched_at: Millis,
    pub last_used: Millis,
    pub pinned: bool,
}

/// Where a file stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Status {
    Absent,
    Ready {
        size: u64,
    },
    Fetching {
        /// `resuming`, `downloading`, `verifying` or `unpacking`.
        phase: String,
        done: u64,
        total: Option<u64>,
    },
    Failed {
        why: String,
        /// `false`: it will be tried again by itself, the next time something asks.
        permanent: bool,
    },
}

#[derive(Default)]
pub struct Progress {
    phase: Mutex<&'static str>,
    done: AtomicU64,
    /// 0: not known.
    total: AtomicU64,
}

impl Progress {
    fn set(&self, phase: &'static str, done: u64, total: Option<u64>) {
        *self.phase.lock().unwrap() = phase;
        self.done.store(done, Ordering::Relaxed);
        self.total.store(total.unwrap_or(0), Ordering::Relaxed);
    }

    fn status(&self) -> Status {
        let total = self.total.load(Ordering::Relaxed);
        let phase = (*self.phase.lock().unwrap()).to_string();
        Status::Fetching { phase, done: self.done.load(Ordering::Relaxed), total: (total > 0).then_some(total) }
    }
}

enum Job {
    Running(Arc<Progress>),
    /// `want`: what failed. Asking for something else (a corrected entry) starts afresh.
    Failed {
        why: String,
        at: Millis,
        permanent: bool,
        failures: u32,
        want: Want,
    },
}

/// One host's jobs in memory. A restart forgets failures, so anything failed is tried
/// again when a machine next asks.
pub struct Cache {
    jobs: Mutex<HashMap<String, Job>>,
    /// Taken while files are being chosen for eviction.
    room: Mutex<()>,
    pub tuning: RwLock<Tuning>,
}

impl Default for Cache {
    fn default() -> Self {
        Cache { jobs: Mutex::default(), room: Mutex::default(), tuning: RwLock::new(Tuning::default()) }
    }
}

/// A cached file's name: what a disk or a URL path can safely hold.
pub fn check_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty() && !name.starts_with('.') && name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    if ok { Ok(()) } else { Err("a name is letters, digits, '-', '_' and '.'".into()) }
}

pub fn is_disk(name: &str) -> bool {
    name.ends_with(".img")
}

/// Where non-disk files are, and what `/files/cache/` serves.
pub fn files_dir(st: &State) -> PathBuf {
    st.config.data_dir.join("cache").join("files")
}

/// Hidden from every listing, and on the same filesystem as where the file ends up.
fn work_dir(st: &State, name: &str) -> PathBuf {
    if is_disk(name) { st.config.images_dir().join(".fetch") } else { st.config.data_dir.join("cache").join(".fetch") }
}

fn part_path(st: &State, name: &str) -> PathBuf {
    work_dir(st, name).join(format!("{name}.part"))
}

fn unpack_path(st: &State, name: &str) -> PathBuf {
    work_dir(st, name).join(format!("{name}.unpack"))
}

/// Where a cached file lives once it's whole.
pub fn file_path(st: &State, name: &str) -> PathBuf {
    if is_disk(name) { st.config.images_dir().join(name) } else { files_dir(st).join(name) }
}

fn allocated(path: &Path) -> u64 {
    let Ok(m) = std::fs::metadata(path) else { return 0 };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        m.blocks() * 512
    }
    #[cfg(not(unix))]
    m.len()
}

/// Free bytes on the filesystem holding `path`. `None`: can't tell.
fn free_space(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: `c` is a valid C string, and `s` is only read after statvfs filled it in.
        unsafe {
            let mut s: libc::statvfs = std::mem::zeroed();
            (libc::statvfs(c.as_ptr(), &mut s) == 0).then(|| s.f_bavail as u64 * s.f_frsize as u64)
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// What a machine's being on `name` keeps it in the cache for. `None`: it can go.
fn kept_by(st: &State, r: &Row) -> Option<&'static str> {
    if r.pinned {
        return Some("pinned");
    }
    if st.nbd_live.in_use(&file_path(st, &r.name)) {
        return Some("a machine is using it");
    }
    if has_overlays(st, &r.name) {
        return Some("machines have overlays on it");
    }
    if now_ms().saturating_sub(r.last_used) < LEASE_MS {
        return Some("used in the last 15 minutes");
    }
    None
}

/// Whether any machine has a copy-on-write overlay of this disk. Those are the machine's
/// own data, and mean nothing over any base but this very file.
fn has_overlays(st: &State, name: &str) -> bool {
    if !is_disk(name) {
        return false;
    }
    let stem = crate::nbd::overlay_name(name);
    let Ok(macs) = std::fs::read_dir(st.config.overlays_dir()) else { return false };
    macs.filter_map(Result::ok).any(|m| bootbob_cow::Cow::exists(&m.path(), stem))
}

fn cap_of(st: &State) -> u64 {
    st.cache.tuning.read().unwrap().cap.unwrap_or(st.config.cache_gib << 30)
}

/// Everything the cache holds on disk, finished or not.
fn used(st: &State) -> u64 {
    let rows: u64 = st.cache_rows().iter().map(|r| allocated(&file_path(st, &r.name))).sum();
    let work: u64 = [work_dir(st, "x.img"), work_dir(st, "x")]
        .iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flatten()
        .filter_map(Result::ok)
        .map(|e| allocated(&e.path()))
        .sum();
    rows + work
}

fn remove_file(st: &State, r: &Row) -> Result<(), String> {
    match std::fs::remove_file(file_path(st, &r.name)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(format!("can't remove {}: {e}", r.name)),
        _ => {}
    }
    st.delete_cache_row(&r.name);
    Ok(())
}

/// Makes room for `need` more bytes while fetching `fetching`, evicting the least recently
/// used files that nothing depends on. Evicts nothing if that still wouldn't be enough.
fn make_room(st: &State, fetching: &str, need: u64) -> Result<(), String> {
    let _guard = st.cache.room.lock().unwrap();
    let cap = cap_of(st);
    let min_free = st.cache.tuning.read().unwrap().min_free;
    let dir = work_dir(st, fetching);
    if let Some(free) = free_space(&dir)
        && free < min_free.saturating_add(need)
    {
        return Err(format!("only {} MB free on the disk holding the cache", free >> 20));
    }
    let mut now = used(st);
    if now + need <= cap {
        return Ok(());
    }
    let mut rows: Vec<Row> = st.cache_rows().into_iter().filter(|r| r.name != fetching && kept_by(st, r).is_none()).collect();
    rows.sort_by_key(|r| r.last_used);
    let free_able: u64 = rows.iter().map(|r| allocated(&file_path(st, &r.name))).sum();
    if now + need - free_able.min(now) > cap {
        return Err(format!(
            "the cache is full: {} of {} GiB is pinned, in use, under overlays or just used, and the rest isn't enough",
            now >> 30,
            cap >> 30
        ));
    }
    for r in rows {
        if now + need <= cap {
            break;
        }
        let size = allocated(&file_path(st, &r.name));
        remove_file(st, &r)?;
        now = now.saturating_sub(size);
        st.event(None, "cache", format!("Evicted {} ({} MB, least recently used) to make room for {fetching}", r.name, size >> 20));
    }
    Ok(())
}

/// Where `name` stands. Doesn't start anything.
pub fn status(st: &State, name: &str) -> Status {
    match st.cache.jobs.lock().unwrap().get(name) {
        Some(Job::Running(p)) => return p.status(),
        Some(Job::Failed { why, permanent, .. }) => return Status::Failed { why: why.clone(), permanent: *permanent },
        None => {}
    }
    match st.cache_row(name) {
        Some(r) if file_path(st, name).is_file() => Status::Ready { size: r.size },
        _ => Status::Absent,
    }
}

/// Makes sure `name` is here or on its way, and says where it stands. Whoever asks for a
/// file already being fetched joins that download, so two machines cost one.
pub fn ensure(st: &Arc<State>, name: &str, want: &Want) -> Status {
    if let Err(why) = check_name(name) {
        return Status::Failed { why, permanent: true };
    }
    // Held to the end, so two askers can't both start a job.
    let mut jobs = st.cache.jobs.lock().unwrap();
    // First: a finishing job writes its row a moment before the file appears, and that
    // must not look like a file that went missing.
    if let Some(Job::Running(p)) = jobs.get(name) {
        return p.status();
    }
    let path = file_path(st, name);
    match (st.cache_row(name), path.is_file()) {
        (Some(mut r), true) => {
            let wanted =
                Want { url: r.url.clone(), checksum: r.checksum.clone(), image_checksum: r.image_checksum.clone(), ..Want::default() };
            if !wanted.same_file(want) {
                let why = format!("the cached {name} came from {}; delete it to fetch the new one", r.url);
                return Status::Failed { why, permanent: true };
            }
            r.last_used = now_ms();
            r.pinned = want.pinned;
            st.put_cache_row(&r);
            jobs.remove(name);
            return Status::Ready { size: r.size };
        }
        (None, true) => {
            let why = format!("{name} is already here, and didn't come from the catalogue; it won't be overwritten");
            return Status::Failed { why, permanent: true };
        }
        // Its file is gone: treat it as never fetched.
        (Some(_), false) => st.delete_cache_row(name),
        (None, false) => {}
    }
    let failures = match jobs.get(name) {
        Some(Job::Running(_)) => unreachable!("handled above, under the same lock"),
        Some(Job::Failed { want: failed, .. }) if !failed.same_file(want) => 0,
        Some(Job::Failed { why, at, permanent, failures, .. }) => {
            let base = st.cache.tuning.read().unwrap().retry_after.as_millis() as u64;
            let wait = base.saturating_mul(5u64.saturating_pow(failures.saturating_sub(1))).min(3_600_000);
            if !want.retry && (*permanent || now_ms().saturating_sub(*at) < wait) {
                return Status::Failed { why: why.clone(), permanent: *permanent };
            }
            *failures
        }
        None => 0,
    };
    let progress = Arc::new(Progress::default());
    progress.set("downloading", 0, None);
    jobs.insert(name.to_string(), Job::Running(progress.clone()));
    drop(jobs);
    st.event(None, "cache", format!("Fetching {name} from {}", want.url));
    st.cache_changed();
    let (st2, name2, want2) = (st.clone(), name.to_string(), want.clone());
    std::thread::spawn(move || job(&st2, &name2, &want2, &progress, failures));
    status(st, name)
}

fn job(st: &Arc<State>, name: &str, want: &Want, p: &Arc<Progress>, failures: u32) {
    let started = std::time::Instant::now();
    let mut result = run(st, name, want, p);
    // A part file left by an earlier attempt may not match what came after it.
    if let Err(Failure::Checksum { resumed: true, .. }) = result {
        st.event(None, "cache", format!("{name} didn't match its checksum after resuming; fetching it again from the start"));
        result = run(st, name, want, p);
    }
    let mut jobs = st.cache.jobs.lock().unwrap();
    match result {
        Ok(r) => {
            jobs.remove(name);
            drop(jobs);
            let secs = started.elapsed().as_secs();
            st.event(None, "cache", format!("Fetched {name}: {} MB, checksum good, in {secs} s", r.size.div_ceil(1 << 20)));
        }
        Err(e) => {
            let permanent = matches!(e, Failure::Refused(_) | Failure::Checksum { .. });
            let why = e.to_string();
            let failed = Job::Failed { why: why.clone(), at: now_ms(), permanent, failures: failures + 1, want: want.clone() };
            jobs.insert(name.to_string(), failed);
            drop(jobs);
            st.event(None, "error", format!("Fetching {name} failed: {why}"));
        }
    }
    st.cache_changed();
}

/// `(url, checksum)` to download: a store's disk comes with the digest the store computes.
fn upstream(st: &State, want: &Want) -> Result<(String, Option<Checksum>), Failure> {
    let parse = |c: &Option<String>| c.as_deref().map(Checksum::parse).transpose().map_err(Failure::Refused);
    if let Some(rest) = want.url.strip_prefix("store:") {
        let (store, disk) = rest.split_once('/').ok_or_else(|| Failure::Refused(format!("{} isn't store:<store>/<disk>.img", want.url)))?;
        let s = st.config.stores.iter().find(|s| s.name == store).ok_or_else(|| Failure::Refused(format!("no store {store:?}")))?;
        let sha256 = crate::stores::disk_digest(s, disk).map_err(Failure::Network)?;
        let sum = Checksum::parse(&format!("sha256:{sha256}")).map_err(Failure::Network)?;
        return Ok((format!("{}/store/disk/{disk}", s.url.trim_end_matches('/')), Some(sum)));
    }
    Ok((want.url.clone(), parse(&want.checksum)?))
}

fn run(st: &State, name: &str, want: &Want, p: &Progress) -> Result<Row, Failure> {
    let tuning = st.cache.tuning.read().unwrap().clone();
    let io = |what: &str, e: std::io::Error| Failure::Stopped(format!("can't {what}: {e}"));
    let image_sum = want.image_checksum.as_deref().map(Checksum::parse).transpose().map_err(Failure::Refused)?;
    let (part, unpacked, target) = (part_path(st, name), unpack_path(st, name), file_path(st, name));
    let dir = work_dir(st, name);
    std::fs::create_dir_all(&dir).map_err(|e| io("create the fetch directory", e))?;
    std::fs::create_dir_all(target.parent().expect("a file in a directory")).map_err(|e| io("create the cache directory", e))?;
    let (url, sum) = upstream(st, want)?;
    if let Ok(m) = std::fs::metadata(&part)
        && m.len() > 0
    {
        st.event(None, "cache", format!("Resuming {name}: {} MB already here", m.len() >> 20));
    }

    let mut checked = 0;
    let mut room = |done: u64| -> Result<(), String> {
        if done >= checked {
            checked = done + tuning.room_check;
            make_room(st, name, tuning.room_check)?;
        }
        Ok(())
    };
    fetch::download(&url, &part, sum.as_ref(), &tuning, &mut |step| match step {
        Step::Resuming { done } => {
            p.set("resuming", done, None);
            Ok(())
        }
        Step::Downloading { done, total } => {
            p.set("downloading", done, total);
            room(done)
        }
    })?;

    // Past this point nothing resumes: a failure here is in the file, not the network.
    let mut ready = part.clone();
    if is_disk(name) {
        let refuse = |why: String| {
            let _ = std::fs::remove_file(&part);
            Failure::Refused(why)
        };
        let mut head = vec![0u8; 4096];
        let n =
            std::fs::File::open(&part).and_then(|mut f| std::io::Read::read(&mut f, &mut head)).map_err(|e| io("read the part file", e))?;
        let packing = Packing::detect(&head[..n]).map_err(refuse)?;
        if packing == Packing::Raw {
            if let Some(want) = &image_sum {
                p.set("verifying", 0, None);
                let verdict = fetch::check_file(&part, want, &mut |done| {
                    p.set("verifying", done, None);
                    Ok(())
                });
                if let Err(got) = verdict.map_err(|e| io("read the part file", e))? {
                    let _ = std::fs::remove_file(&part);
                    return Err(Failure::Checksum { resumed: false, want: want.hex.clone(), got });
                }
            }
        } else {
            ready = unpacked.clone();
            unpack(st, name, &part, &unpacked, packing, image_sum.as_ref(), p)?;
        }
    }

    let now = now_ms();
    let size = std::fs::metadata(&ready).map_err(|e| io("read the file", e))?.len();
    let row = Row {
        name: name.to_string(),
        url: want.url.clone(),
        checksum: want.checksum.clone(),
        image_checksum: want.image_checksum.clone(),
        size,
        allocated: allocated(&ready),
        fetched_at: now,
        last_used: now,
        pinned: want.pinned,
    };
    // The row first: a crash in between leaves a row with no file, which is the same as
    // never fetched. The other way round would leave a file nobody admits to.
    st.put_cache_row(&row);
    if let Err(e) = std::fs::rename(&ready, &target) {
        st.delete_cache_row(name);
        return Err(io("move the file into place", e));
    }
    let _ = std::fs::remove_file(&part);
    Ok(row)
}

fn unpack(st: &State, name: &str, part: &Path, to: &Path, packing: Packing, sum: Option<&Checksum>, p: &Progress) -> Result<(), Failure> {
    let clean = || {
        let _ = std::fs::remove_file(part);
        let _ = std::fs::remove_file(to);
    };
    let src = std::fs::File::open(part).map_err(|e| Failure::Stopped(format!("can't read the part file: {e}")))?;
    let mut hash = sum.map(Checksum::hasher);
    let mut done = 0u64;
    let mut checked = 0u64;
    let room_check = st.cache.tuning.read().unwrap().room_check;
    let mut stopped = None;
    let mut first = true;
    let res = sparse::unpack(src, packing, to, &mut |data| {
        if first {
            first = false;
            sparse::not_a_disk(data).map_err(std::io::Error::other)?;
        }
        if let Some(h) = &mut hash {
            h.update(data);
        }
        done += data.len() as u64;
        p.set("unpacking", done, None);
        if done >= checked {
            checked = done + room_check;
            if let Err(e) = make_room(st, name, room_check) {
                stopped = Some(e.clone());
                return Err(std::io::Error::other(e));
            }
        }
        Ok(())
    });
    if let Some(why) = stopped {
        let _ = std::fs::remove_file(to);
        return Err(Failure::Stopped(why));
    }
    if let Err(e) = res {
        clean();
        return Err(Failure::Refused(format!("can't unpack it: {e}")));
    }
    if let (Some(want), Some(hash)) = (sum, hash) {
        let got = hash.finish();
        if want.hex != got {
            clean();
            return Err(Failure::Checksum { resumed: false, want: want.hex.clone(), got });
        }
    }
    Ok(())
}

/// A machine used `name`: it goes to the back of the eviction queue.
pub fn touch(st: &State, name: &str) {
    if let Some(mut r) = st.cache_row(name) {
        r.last_used = now_ms();
        st.put_cache_row(&r);
    }
}

/// Pins or unpins a cached file. `false`: it isn't cached.
pub fn pin(st: &State, name: &str, pinned: bool) -> bool {
    let Some(mut r) = st.cache_row(name) else { return false };
    r.pinned = pinned;
    st.put_cache_row(&r);
    st.cache_changed();
    true
}

/// Removes a file from the cache on request, with any partial download. Refused while
/// something depends on it, except the pin and the lease: this is someone choosing to.
/// `Ok(false)`: nothing to remove.
pub fn remove(st: &State, name: &str) -> Result<bool, (u16, String)> {
    let mut jobs = st.cache.jobs.lock().unwrap();
    if matches!(jobs.get(name), Some(Job::Running(_))) {
        return Err((409, format!("{name} is being fetched")));
    }
    let failed = jobs.remove(name).is_some();
    drop(jobs);
    let part = [part_path(st, name), unpack_path(st, name)].iter().any(|p| std::fs::remove_file(p).is_ok());
    let Some(r) = st.cache_row(name) else {
        st.cache_changed();
        return Ok(part || failed);
    };
    if st.nbd_live.in_use(&file_path(st, name)) {
        return Err((409, format!("a machine is using {name}")));
    }
    if has_overlays(st, name) {
        return Err((409, format!("machines have overlays on {name}; reset them first")));
    }
    remove_file(st, &r).map_err(|e| (500, e))?;
    st.event(None, "cache", format!("Removed {name} from the cache"));
    st.cache_changed();
    Ok(true)
}

/// A cached file, for the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub name: String,
    pub status: Status,
    pub pinned: bool,
    pub allocated: u64,
    pub last_used: Option<Millis>,
    /// Why it can't be evicted right now.
    pub kept: Option<String>,
}

/// What a host's cache holds.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Listing {
    pub cap: u64,
    pub used: u64,
    pub free: Option<u64>,
    pub items: Vec<Item>,
}

pub fn list(st: &State) -> Listing {
    let mut items: Vec<Item> = st
        .cache_rows()
        .into_iter()
        .map(|r| Item {
            status: status(st, &r.name),
            pinned: r.pinned,
            allocated: allocated(&file_path(st, &r.name)),
            last_used: Some(r.last_used),
            kept: kept_by(st, &r).map(String::from),
            name: r.name,
        })
        .collect();
    let jobs: Vec<String> = st.cache.jobs.lock().unwrap().keys().cloned().collect();
    for name in jobs {
        if !items.iter().any(|i| i.name == name) {
            let status = status(st, &name);
            items.push(Item { name, status, pinned: false, allocated: 0, last_used: None, kept: None });
        }
    }
    items.sort_by(|a, b| a.name.cmp(&b.name));
    Listing { cap: cap_of(st), used: used(st), free: free_space(&st.config.data_dir), items }
}

/// Tidies up after a restart: half-unpacked files go (the part file they came from stays,
/// to resume), and rows whose file disappeared are dropped.
pub fn startup(st: &State) {
    for d in [work_dir(st, "x.img"), work_dir(st, "x")] {
        for e in std::fs::read_dir(d).into_iter().flatten().filter_map(Result::ok) {
            if e.path().extension().is_some_and(|x| x == "unpack") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    for r in st.cache_rows() {
        if !file_path(st, &r.name).is_file() {
            st.delete_cache_row(&r.name);
        }
    }
}
