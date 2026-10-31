//! The catalogue: images BootBob can fetch when something needs them.
//!
//! An entry is an upstream URL and a checksum, and says which host keeps the file: this
//! server, or an image store. An image's disk source or iPXE action can then name a file
//! that isn't here yet. The first boot that needs it is held until it has arrived and
//! been verified (see [`hold`]); after that it's an ordinary file on an ordinary store,
//! until the cache there needs the room.

use std::sync::Arc;

use bootbob_ipxe::Action;
use serde::{Deserialize, Serialize};

use crate::cache::{self, Status, Want};
use crate::fetch::Checksum;
use crate::model::{Image, Millis};
use crate::state::State;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// From the URL in `PUT /api/catalogue/{id}`, so optional in the body. The file's name
    /// on its store: `.img` for a disk image, which gets unpacked; anything else is kept
    /// byte for byte.
    #[serde(default)]
    pub id: String,
    pub title: String,
    /// The store that keeps it. `None`: this server.
    #[serde(default)]
    pub store: Option<String>,
    /// Where it comes from: an upstream URL, or for a copy of a store's disk,
    /// `store:<store>/<disk>.img`.
    pub url: String,
    /// The download's checksum, `sha256:<hex>` or `sha512:<hex>`.
    #[serde(default)]
    pub checksum: Option<String>,
    /// The unpacked disk image's checksum, for upstreams that publish only that.
    #[serde(default)]
    pub image_checksum: Option<String>,
    /// Never evicted.
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub added_at: Millis,
}

impl Entry {
    /// What an image's disk source calls it.
    pub fn source(&self) -> String {
        match &self.store {
            Some(s) => format!("{s}:{}", self.id),
            None => self.id.clone(),
        }
    }

    fn want(&self, retry: bool) -> Want {
        Want {
            url: self.url.clone(),
            checksum: self.checksum.clone(),
            image_checksum: self.image_checksum.clone(),
            pinned: self.pinned,
            retry,
        }
    }
}

pub fn validate(st: &State, e: &Entry) -> Result<(), String> {
    cache::check_name(&e.id).map_err(|m| format!("a catalogue id: {m}"))?;
    if e.title.trim().is_empty() {
        return Err("a catalogue entry needs a title".into());
    }
    if let Some(s) = &e.store
        && !st.config.stores.iter().any(|c| &c.name == s)
    {
        return Err(format!("no store {s:?}"));
    }
    if let Some(rest) = e.url.strip_prefix("store:") {
        if e.store.is_some() {
            return Err("only this server can copy from a store, so leave the entry's store empty".into());
        }
        let (store, disk) = rest.split_once('/').ok_or("a store URL is store:<store>/<disk>.img")?;
        if !st.config.stores.iter().any(|c| c.name == store) {
            return Err(format!("no store {store:?}"));
        }
        crate::nbd::check_disk_name(disk)?;
    } else {
        if !(e.url.starts_with("http://") || e.url.starts_with("https://")) {
            return Err("the URL must be http:// or https:// (or store:<store>/<disk>.img)".into());
        }
        if e.checksum.is_none() && e.image_checksum.is_none() {
            return Err("a catalogue entry needs a checksum: nothing unverified reaches a machine".into());
        }
    }
    if let Some(c) = &e.checksum {
        Checksum::parse(c)?;
    }
    if let Some(c) = &e.image_checksum {
        Checksum::parse(c)?;
        if !cache::is_disk(&e.id) {
            return Err("image_checksum is for disk images (ids ending in .img), which get unpacked".into());
        }
    }
    Ok(())
}

/// Makes sure an entry's file is on its host or on its way. Blocking.
pub fn ensure(st: &Arc<State>, e: &Entry, retry: bool) -> Status {
    match &e.store {
        None => cache::ensure(st, &e.id, &e.want(retry)),
        Some(name) => match st.config.stores.iter().find(|s| &s.name == name) {
            Some(s) => crate::stores::cache_ensure(s, &e.id, &e.want(retry)),
            None => Status::Failed { why: format!("no store {name:?}"), permanent: true },
        },
    }
}

/// An entry and where its file stands.
#[derive(Debug, Clone, Serialize)]
pub struct View {
    #[serde(flatten)]
    pub entry: Entry,
    pub status: Status,
    pub allocated: u64,
    pub kept: Option<String>,
    pub last_used: Option<Millis>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Host {
    /// `None`: this server.
    pub store: Option<String>,
    pub cap: u64,
    pub used: u64,
    pub free: Option<u64>,
    /// Why its numbers aren't here.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Overview {
    pub entries: Vec<View>,
    pub hosts: Vec<Host>,
}

/// The whole catalogue with every host's state. Blocking: it asks the stores.
pub fn overview(st: &State) -> Overview {
    let mut lists = vec![(None, cache::list(st), None)];
    for s in &st.config.stores {
        match crate::stores::cache_list(s) {
            Ok(l) => lists.push((Some(s.name.clone()), l, None)),
            Err(e) => lists.push((Some(s.name.clone()), Default::default(), Some(e))),
        }
    }
    let entries = st
        .catalogue()
        .into_iter()
        .map(|entry| {
            let item = lists.iter().find(|(s, ..)| *s == entry.store).and_then(|(_, l, _)| l.items.iter().find(|i| i.name == entry.id));
            View {
                status: item.map_or(Status::Absent, |i| i.status.clone()),
                allocated: item.map_or(0, |i| i.allocated),
                kept: item.and_then(|i| i.kept.clone()),
                last_used: item.and_then(|i| i.last_used),
                entry,
            }
        })
        .collect();
    let hosts = lists.into_iter().map(|(store, l, error)| Host { store, cap: l.cap, used: l.used, free: l.free, error }).collect();
    Overview { entries, hosts }
}

/// What a boot has to wait for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hold {
    Go,
    /// Still arriving; the text is for a person.
    Wait(String),
    Failed(String),
}

fn mb(n: u64) -> u64 {
    n.div_ceil(1 << 20)
}

fn progress(phase: &str, done: u64, total: Option<u64>) -> String {
    match (phase, total) {
        ("downloading", Some(t)) if t > 0 => format!("downloading, {}% of {} MB", done.saturating_mul(100) / t, mb(t)),
        ("downloading", _) => format!("downloading, {} MB so far", mb(done)),
        ("unpacking", _) => format!("unpacking, {} MB so far", mb(done)),
        ("verifying", _) => "verifying".to_string(),
        (other, _) => format!("{other}, {} MB", mb(done)),
    }
}

/// Starts whatever `entries` still need, and says whether a boot can go ahead. Blocking.
pub fn hold(st: &Arc<State>, entries: &[Entry]) -> Hold {
    let mut waiting = Vec::new();
    for e in entries {
        match ensure(st, e, false) {
            Status::Ready { .. } => {}
            Status::Fetching { phase, done, total } => waiting.push(format!("{}: {}", e.title, progress(&phase, done, total))),
            Status::Absent => waiting.push(e.title.clone()),
            Status::Failed { why, .. } => return Hold::Failed(format!("{}: {why}", e.title)),
        }
    }
    if waiting.is_empty() { Hold::Go } else { Hold::Wait(waiting.join("\n")) }
}

/// The `{cached:<id>}` names in a string.
fn refs(s: &str) -> impl Iterator<Item = &str> {
    s.split("{cached:").skip(1).filter_map(|t| t.split_once('}').map(|(id, _)| id))
}

fn action_strings(a: &Action) -> Vec<&str> {
    match a {
        Action::Chain { url } => vec![url],
        Action::Kernel { kernel, initrds, cmdline } => {
            let mut v = vec![kernel.as_str(), cmdline.as_str()];
            v.extend(initrds.iter().map(String::as_str));
            v
        }
        Action::Script { script } => vec![script],
        _ => vec![],
    }
}

/// The catalogue files an iPXE action names.
pub fn action_refs(a: &Action) -> Vec<String> {
    let mut v: Vec<String> = action_strings(a).into_iter().flat_map(refs).map(String::from).collect();
    v.sort();
    v.dedup();
    v
}

/// What a PC needs before it can boot `img`.
pub fn for_pc(st: &State, img: &Image) -> Vec<Entry> {
    let mut v: Vec<Entry> = img.action.iter().flat_map(action_refs).filter_map(|id| st.catalogue_entry(&id)).collect();
    v.extend(img.disk.as_ref().and_then(|d| st.catalogue_for_source(&d.source)));
    v
}

/// What a Pi needs before it can boot `img`.
pub fn for_pi(st: &State, img: &Image) -> Vec<Entry> {
    img.pi.as_ref().and_then(|p| st.catalogue_for_source(&p.source)).into_iter().collect()
}

/// `action` with every `{cached:<id>}` replaced by where machines fetch that file: from
/// its store if it has one, so that traffic doesn't pass through this server either.
pub fn fill(st: &State, action: &Action, base: &str) -> Action {
    let one = |s: &str| {
        let mut out = s.to_string();
        for id in refs(s) {
            let host = st
                .catalogue_entry(id)
                .and_then(|e| e.store)
                .and_then(|name| st.config.stores.iter().find(|s| s.name == name).map(|s| s.url.trim_end_matches('/').to_string()))
                .unwrap_or_else(|| base.to_string());
            out = out.replace(&format!("{{cached:{id}}}"), &format!("{host}/files/cache/{id}"));
        }
        out
    };
    match action {
        Action::Chain { url } => Action::Chain { url: one(url) },
        Action::Kernel { kernel, initrds, cmdline } => {
            Action::Kernel { kernel: one(kernel), initrds: initrds.iter().map(|i| one(i)).collect(), cmdline: one(cmdline) }
        }
        Action::Script { script } => Action::Script { script: one(script) },
        other => other.clone(),
    }
}

/// An image in Raspberry Pi's own list, ready to become an entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PiOsImage {
    pub title: String,
    pub description: String,
    pub url: String,
    pub checksum: Option<String>,
    pub image_checksum: Option<String>,
    pub download_size: u64,
    pub image_size: u64,
    pub release_date: String,
}

/// The same list BenchBob's flash picker shows.
const PI_OS_LIST: &str = "https://downloads.raspberrypi.com/os_list_imagingutility_v4.json";

/// The images in Raspberry Pi's list that BootBob can store: compressed the ways it
/// unpacks, with a checksum published. Third-party entries are often zip files, which it
/// can't. Blocking.
pub fn pi_os_images() -> Result<Vec<PiOsImage>, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(std::time::Duration::from_secs(30))).build().into();
    let body = agent
        .get(PI_OS_LIST)
        .call()
        .map_err(|e| format!("can't fetch Raspberry Pi's list: {e}"))?
        .body_mut()
        .with_config()
        .limit(16 << 20)
        .read_to_string()
        .map_err(|e| format!("can't read Raspberry Pi's list: {e}"))?;
    parse_pi_os(&body)
}

pub fn parse_pi_os(body: &str) -> Result<Vec<PiOsImage>, String> {
    fn walk(items: &[serde_json::Value], out: &mut Vec<PiOsImage>) {
        let text = |i: &serde_json::Value, k: &str| i[k].as_str().unwrap_or("").to_string();
        let sum = |i: &serde_json::Value, k: &str| i[k].as_str().filter(|s| !s.is_empty()).map(|s| format!("sha256:{s}"));
        for i in items {
            if let Some(sub) = i["subitems"].as_array() {
                walk(sub, out);
                continue;
            }
            let url = text(i, "url");
            let packed = [".img.xz", ".img.gz", ".img.zst", ".img"].iter().any(|x| url.ends_with(x));
            let (checksum, image_checksum) = (sum(i, "image_download_sha256"), sum(i, "extract_sha256"));
            if packed && (checksum.is_some() || image_checksum.is_some()) && !out.iter().any(|o| o.url == url) {
                out.push(PiOsImage {
                    title: text(i, "name"),
                    description: text(i, "description"),
                    url,
                    checksum,
                    image_checksum,
                    download_size: i["image_download_size"].as_u64().unwrap_or(0),
                    image_size: i["extract_size"].as_u64().unwrap_or(0),
                    release_date: text(i, "release_date"),
                });
            }
        }
    }
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| format!("Raspberry Pi's list: {e}"))?;
    let mut out = Vec::new();
    walk(v["os_list"].as_array().ok_or("Raspberry Pi's list has no os_list")?, &mut out);
    Ok(out)
}
