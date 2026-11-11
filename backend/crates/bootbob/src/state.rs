//! BootBob's working state: devices, images and settings in memory, written through to the
//! store, with a change feed for the web UI.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex, RwLock};

use bootbob_dhcp::listener::Seen;
use bootbob_dhcp::{Firmware, Mac, MessageType};
use serde::Serialize;
use tokio::sync::broadcast;

use crate::config::Config;
use crate::model::*;
use crate::store::{Store, Table};

/// Told to the web UI over `/api/stream`, so it refetches what changed.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "what", rename_all = "snake_case")]
pub enum Change {
    Device {
        mac: String,
    },
    DeviceRemoved {
        mac: String,
    },
    Images,
    Settings,
    Event,
    /// The catalogue, or something in a cache, changed.
    Catalogue,
    Tracks,
}

/// Why a machine gets what it gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub image: String,
    pub reason: String,
    /// The image came from a one-shot assignment, which booting it uses up.
    pub once: bool,
}

/// One DHCP listener's state, for the UI and the network check.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ListenerStatus {
    pub interface: String,
    pub running: bool,
    pub error: Option<String>,
    pub dhcp_seen: u64,
    pub pxe_seen: u64,
    pub last_seen: Option<Millis>,
}

const EVENTS_IN_MEMORY: usize = 500;

pub struct State {
    pub config: Config,
    store: Store,
    devices: RwLock<BTreeMap<Mac, Device>>,
    images: RwLock<BTreeMap<String, Image>>,
    settings: RwLock<Settings>,
    /// The time zone the network last gave over DHCP. Kept across restarts, and through a
    /// quiet DHCP server, so the rollout window doesn't jump to UTC.
    network_zone: RwLock<Option<String>>,
    events: Mutex<VecDeque<Event>>,
    /// (boot interface, IP) → MAC, learnt from DHCP replies and TFTP request frames on that
    /// interface. Kept apart per interface, so a claim made on one never identifies a machine
    /// on another.
    addresses: Mutex<HashMap<(String, Ipv4Addr), (Mac, Millis)>>,
    /// The DHCP listeners, by boot interface.
    pub listeners: RwLock<BTreeMap<String, ListenerStatus>>,
    /// The host's interfaces as last read (addresses, VLANs).
    netifs: RwLock<Vec<crate::netif::NetIf>>,
    /// The doctor's probes in flight, by DHCP transaction id, with the offers they got.
    probes: Mutex<HashMap<u32, Vec<crate::doctor::Offer>>>,
    report: Mutex<Option<crate::doctor::Report>>,
    changes: broadcast::Sender<Change>,
    /// Which NBD exports are connected right now (shared with the NBD server).
    pub nbd_live: Arc<bootbob_nbd::Live>,
    catalogue: RwLock<BTreeMap<String, crate::catalogue::Entry>>,
    tracks: RwLock<BTreeMap<String, Track>>,
    /// This host's downloads in progress.
    pub cache: crate::cache::Cache,
}

impl State {
    pub fn open(config: Config) -> anyhow::Result<State> {
        std::fs::create_dir_all(&config.data_dir)?;
        // So it's obvious where Pi boot trees and disk images go. May be a read-only mount.
        if let Err(e) = std::fs::create_dir_all(config.images_dir()) {
            tracing::warn!("can't create the images directory {}: {e}", config.images_dir().display());
        }
        let store = Store::open(&config.data_dir.join("bootbob.redb"))?;
        let mut devices: BTreeMap<Mac, Device> =
            store.all::<Device>(Table::Devices)?.into_iter().filter_map(|d| Some((d.mac.parse::<Mac>().ok()?, d))).collect();
        // Machines known from before admission existed keep booting where they always did:
        // they're admitted to the first boot interface (the LAN), and nowhere new.
        if store.get::<bool>(Table::Meta, "admission")?.is_none()
            && let Some(first) = config.boot.first()
        {
            for d in devices.values_mut().filter(|d| d.admitted.is_empty()) {
                d.admitted.push(first.interface.clone());
                store.put(Table::Devices, &d.mac, d)?;
            }
            store.put(Table::Meta, "admission", &true)?;
        }
        let mut images: BTreeMap<String, Image> = store.all::<Image>(Table::Images)?.into_iter().map(|i| (i.id.clone(), i)).collect();
        for b in builtin_images() {
            images.insert(b.id.clone(), b);
        }
        let settings = store.get::<Settings>(Table::Meta, "settings")?.unwrap_or_default();
        let network_zone = store.get::<String>(Table::Meta, "network_time_zone")?;
        let catalogue = store.all::<crate::catalogue::Entry>(Table::Catalogue)?.into_iter().map(|e| (e.id.clone(), e)).collect();
        let tracks = store.all::<Track>(Table::Tracks)?.into_iter().map(|t| (t.id.clone(), t)).collect();
        let events = store.recent_events::<Event>(EVENTS_IN_MEMORY)?.into();
        let (changes, _) = broadcast::channel(256);
        Ok(State {
            listeners: RwLock::new(
                config
                    .boot
                    .iter()
                    .filter(|b| b.serves(crate::config::Service::Listen))
                    .map(|b| (b.interface.clone(), ListenerStatus { interface: b.interface.clone(), ..Default::default() }))
                    .collect(),
            ),
            netifs: RwLock::new(crate::netif::discover()),
            config,
            store,
            devices: RwLock::new(devices),
            images: RwLock::new(images),
            settings: RwLock::new(settings),
            network_zone: RwLock::new(network_zone),
            events: Mutex::new(events),
            addresses: Mutex::new(HashMap::new()),
            probes: Mutex::new(HashMap::new()),
            report: Mutex::new(None),
            changes,
            nbd_live: Arc::new(bootbob_nbd::Live::default()),
            catalogue: RwLock::new(catalogue),
            tracks: RwLock::new(tracks),
            cache: Default::default(),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Change> {
        self.changes.subscribe()
    }

    fn notify(&self, c: Change) {
        let _ = self.changes.send(c);
    }

    // ---- Devices ----

    pub fn devices(&self) -> Vec<Device> {
        self.devices.read().unwrap().values().cloned().collect()
    }

    pub fn device(&self, mac: Mac) -> Option<Device> {
        self.devices.read().unwrap().get(&mac).cloned()
    }

    /// Creates or updates a device, saves it and tells the UI. Returns the result and
    /// whether it was new.
    pub fn update_device(&self, mac: Mac, f: impl FnOnce(&mut Device)) -> (Device, bool) {
        let now = now_ms();
        let (dev, new) = {
            let mut devices = self.devices.write().unwrap();
            let new = !devices.contains_key(&mac);
            let d = devices.entry(mac).or_insert_with(|| Device { mac: mac.to_string(), first_seen: now, ..Default::default() });
            f(d);
            (d.clone(), new)
        };
        if let Err(e) = self.store.put(Table::Devices, &dev.mac, &dev) {
            tracing::error!(mac = %dev.mac, "can't save a device: {e}");
        }
        if new {
            self.event(Some(mac), "discovered", format!("New machine {}", describe(&dev)));
        }
        self.notify(Change::Device { mac: dev.mac.clone() });
        (dev, new)
    }

    /// Edits an existing device. `None` if there's no such device.
    pub fn edit_device(&self, mac: Mac, f: impl FnOnce(&mut Device)) -> Option<Device> {
        self.device(mac)?;
        Some(self.update_device(mac, f).0)
    }

    pub fn remove_device(&self, mac: Mac) -> bool {
        let removed = self.devices.write().unwrap().remove(&mac).is_some();
        if removed {
            if let Err(e) = self.store.delete(Table::Devices, &mac.to_string()) {
                tracing::error!(%mac, "can't delete a device: {e}");
            }
            self.notify(Change::DeviceRemoved { mac: mac.to_string() });
        }
        removed
    }

    /// Sets or clears a device's assignment, creating the device if needed (so a machine
    /// can be assigned before it ever boots).
    pub fn assign(&self, mac: Mac, assignment: Option<Assignment>) -> Result<Device, String> {
        let assignment = assignment.map(|a| self.follow(a)).transpose()?;
        if let Some(a) = &assignment
            && self.image(&a.image).is_none()
        {
            return Err(format!("no image {:?}", a.image));
        }
        let msg = match &assignment {
            Some(Assignment { track: Some(t), image, .. }) => format!("Following {t}, now at {}", self.image_title(image)),
            Some(a) => format!("{} {}", if a.once { "Next boot only:" } else { "Assigned" }, self.image_title(&a.image)),
            None => "Assignment cleared".to_string(),
        };
        // Saying what a machine boots admits it, where it was seen last (or, never seen, on
        // the first boot interface): a person has just vouched for it.
        let admit = assignment.as_ref().and_then(|_| {
            let seen = self.device(mac).and_then(|d| d.last_seen_on().map(String::from));
            seen.or_else(|| self.config.boot.first().map(|b| b.interface.clone()))
        });
        if let Some(i) = &admit
            && !self.device(mac).is_some_and(|d| d.is_admitted(i))
        {
            self.event(Some(mac), "admitted", format!("Admitted on {}", self.iface_label(i)));
        }
        let (dev, _) = self.update_device(mac, |d| {
            if let Some(i) = admit
                && !d.is_admitted(&i)
            {
                d.admitted.push(i);
                d.admitted.sort();
            }
            d.assignment = assignment.map(|a| Assignment { set_at: now_ms(), ..a });
            // A person just said what this machine boots, so a move in flight is over.
            d.moving = None;
            d.not_before = None;
        });
        self.event(Some(mac), "assigned", msg);
        Ok(dev)
    }

    /// Fills in an assignment that follows a track: its current version, unless it names one
    /// of the track's versions.
    fn follow(&self, a: Assignment) -> Result<Assignment, String> {
        let Some(id) = &a.track else { return Ok(a) };
        let t = self.track(id).ok_or_else(|| format!("no track {id:?}"))?;
        if a.once {
            return Err("following a track isn't a one-shot".into());
        }
        if !a.image.is_empty() && !t.versions.iter().any(|v| v.image == a.image) {
            return Err(format!("{:?} isn't a version of {id}", a.image));
        }
        Ok(Assignment { image: if a.image.is_empty() { t.current } else { a.image }, ..a })
    }

    // ---- Tracks ----

    pub fn tracks(&self) -> Vec<Track> {
        self.tracks.read().unwrap().values().cloned().collect()
    }

    pub fn track(&self, id: &str) -> Option<Track> {
        self.tracks.read().unwrap().get(id).cloned()
    }

    /// Only an image a rollout can move a machine onto and back from: copy-on-write, so a
    /// machine's changes stay out of the shared disk (this is what keeps plain disks, which are
    /// pets, off a track), with a command line that follows the disk and tells the machine how
    /// to reach BootBob. A hand-written disk name would boot the wrong root.
    pub fn check_version(&self, image: &Image) -> Result<(), String> {
        let Some((source, cow, cmdline)) = image.one_disk() else {
            return Err(format!("{} has to boot exactly one disk image, for Pis or for PCs", image.id));
        };
        if !cow {
            return Err(format!("{}: a rollout replaces the root, so a version must be copy-on-write", image.id));
        }
        if !source.ends_with(".img") {
            return Err(format!("{}: a version is a disk image (.img)", image.id));
        }
        for needed in ["{disk}", "{bootbob}"] {
            if !cmdline.contains(needed) {
                return Err(format!("{}: its command line must use {needed}, or a new version can't work", image.id));
            }
        }
        Ok(())
    }

    /// Creates a track on an image, or retitles one.
    pub fn put_track(&self, id: &str, title: &str, image: Option<&str>) -> Result<Track, String> {
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) {
            return Err("a track id is letters, digits, '-', '_' and '.'".into());
        }
        if title.trim().is_empty() {
            return Err("a track needs a title".into());
        }
        if self.track(id).is_some() {
            return self.edit_track(id, |t| t.title = title.trim().into()).ok_or_else(|| format!("no track {id:?}"));
        }
        let image = image.ok_or("a new track starts from an image")?;
        self.check_version(&self.image(image).ok_or_else(|| format!("no image {image:?}"))?)?;
        let t = Track {
            id: id.into(),
            title: title.trim().into(),
            current: image.into(),
            versions: vec![Version { image: image.into(), added_at: now_ms() }],
            halted: None,
            blocked: None,
            open_until: None,
        };
        self.store.put(Table::Tracks, id, &t).map_err(|e| e.to_string())?;
        self.tracks.write().unwrap().insert(id.into(), t.clone());
        self.notify(Change::Tracks);
        Ok(t)
    }

    /// Edits a track. `None` if there's no such track.
    pub fn edit_track(&self, id: &str, f: impl FnOnce(&mut Track)) -> Option<Track> {
        let t = {
            let mut tracks = self.tracks.write().unwrap();
            let t = tracks.get_mut(id)?;
            f(t);
            t.clone()
        };
        if let Err(e) = self.store.put(Table::Tracks, id, &t) {
            tracing::error!(id, "can't save a track: {e}");
        }
        self.notify(Change::Tracks);
        Some(t)
    }

    /// Deletes a track. Its versions stay, as ordinary images.
    pub fn delete_track(&self, id: &str) -> Result<(), String> {
        let following = self.devices().iter().filter(|d| d.assignment.as_ref().is_some_and(|a| a.track.as_deref() == Some(id))).count();
        if following > 0 {
            return Err(format!("{following} machines follow {id}"));
        }
        self.store.delete(Table::Tracks, id).map_err(|e| e.to_string())?;
        self.tracks.write().unwrap().remove(id);
        self.notify(Change::Tracks);
        Ok(())
    }

    // ---- Images ----

    pub fn images(&self) -> Vec<Image> {
        self.images.read().unwrap().values().cloned().collect()
    }

    pub fn image(&self, id: &str) -> Option<Image> {
        self.images.read().unwrap().get(id).cloned()
    }

    fn image_title(&self, id: &str) -> String {
        self.image(id).map(|i| i.title).unwrap_or_else(|| id.to_string())
    }

    pub fn put_image(&self, image: Image) -> Result<(), String> {
        if image.id.is_empty() || !image.id.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) {
            return Err("an image id is letters, digits, '-', '_' and '.'".into());
        }
        if builtin_images().iter().any(|b| b.id == image.id) {
            return Err(format!("{:?} is a built-in image", image.id));
        }
        if image.action.is_none() && image.pi.is_none() && image.disk.is_none() {
            return Err(
                "an image needs something to boot: an iPXE action or a disk image for PCs, a Raspberry Pi boot tree, or both".into()
            );
        }
        if image.action.is_some() && image.disk.is_some() {
            return Err("a PC boots either the iPXE action or the disk image, not both".into());
        }
        if let Some(d) = &image.disk {
            self.check_source(&d.source, d.cow)?;
            if !d.source.ends_with(".img") {
                return Err("a PC disk boot needs a disk image (.img)".into());
            }
            if d.kernel.trim().is_empty() {
                return Err("a PC disk boot needs the kernel's path in the boot partition".into());
            }
        }
        if let Some(a) = &image.action {
            for id in crate::catalogue::action_refs(a) {
                match self.catalogue_entry(&id) {
                    None => return Err(format!("{{cached:{id}}}: no such catalogue entry")),
                    Some(_) if crate::cache::is_disk(&id) => {
                        return Err(format!("{{cached:{id}}}: a disk image is a disk source, not a file to fetch"));
                    }
                    Some(_) => {}
                }
            }
        }
        if let Some(pi) = &image.pi {
            self.check_source(&pi.source, pi.cow)?;
            if pi.cow && !pi.source.ends_with(".img") {
                return Err("copy-on-write needs a disk image (.img) source, not a directory".into());
            }
        }
        self.store.put(Table::Images, &image.id, &image).map_err(|e| e.to_string())?;
        self.images.write().unwrap().insert(image.id.clone(), Image { builtin: false, ..image });
        self.notify(Change::Images);
        Ok(())
    }

    /// A source has to exist, or be something the catalogue can fetch. A fetched disk is
    /// never written: that's the copy every machine starts from, verified, and the cache
    /// may delete it.
    fn check_source(&self, source: &str, cow: bool) -> Result<(), String> {
        match self.catalogue_for_source(source) {
            Some(_) if !cow => Err(format!("{source} comes from the catalogue, so an image of it must be copy-on-write")),
            Some(_) => Ok(()),
            None => crate::stores::locate(&self.config, source).map(|_| ()),
        }
    }

    pub fn delete_image(&self, id: &str) -> Result<(), String> {
        match self.image(id) {
            None => return Err(format!("no image {id:?}")),
            Some(i) if i.builtin => return Err(format!("{id:?} is a built-in image")),
            Some(_) => {}
        }
        let in_use = self.devices().iter().filter(|d| d.assignment.as_ref().is_some_and(|a| a.image == id)).count();
        let s = self.settings();
        if in_use > 0 || s.default_image == id || s.pi_default_image == id {
            return Err(format!("{id:?} is in use ({in_use} assignments, or the default)"));
        }
        // A version has to stay for as long as a machine might run it or come back to it.
        let tracks: Vec<Track> = self.tracks().into_iter().filter(|t| t.versions.iter().any(|v| v.image == id)).collect();
        if let Some(t) = tracks.iter().find(|t| t.current == id) {
            return Err(format!("{id:?} is the current version of {}", t.id));
        }
        let needed = |d: &Device| {
            d.last_boot.as_ref().is_some_and(|b| b.image == id) || d.moving.as_ref().is_some_and(|m| m.from == id || m.to == id)
        };
        if !tracks.is_empty()
            && let Some(d) = self.devices().iter().find(|d| needed(d))
        {
            return Err(format!("{} is running {id:?}, or may go back to it", describe(d)));
        }
        self.store.delete(Table::Images, id).map_err(|e| e.to_string())?;
        self.images.write().unwrap().remove(id);
        for t in tracks {
            self.edit_track(&t.id, |t| t.versions.retain(|v| v.image != id));
        }
        self.notify(Change::Images);
        Ok(())
    }

    // ---- The catalogue ----

    pub fn catalogue(&self) -> Vec<crate::catalogue::Entry> {
        self.catalogue.read().unwrap().values().cloned().collect()
    }

    pub fn catalogue_entry(&self, id: &str) -> Option<crate::catalogue::Entry> {
        self.catalogue.read().unwrap().get(id).cloned()
    }

    /// The entry that fetches this image source, if one does.
    pub fn catalogue_for_source(&self, source: &str) -> Option<crate::catalogue::Entry> {
        self.catalogue.read().unwrap().values().find(|e| e.source() == source).cloned()
    }

    pub fn put_catalogue_entry(&self, e: crate::catalogue::Entry) -> Result<(), String> {
        self.store.put(Table::Catalogue, &e.id, &e).map_err(|e| e.to_string())?;
        self.catalogue.write().unwrap().insert(e.id.clone(), e);
        self.cache_changed();
        Ok(())
    }

    /// The images booting this entry's file. It can't go while there are any.
    pub fn catalogue_users(&self, e: &crate::catalogue::Entry) -> Vec<String> {
        let source = e.source();
        self.images()
            .into_iter()
            .filter(|i| {
                i.pi.as_ref().is_some_and(|p| p.source == source)
                    || i.disk.as_ref().is_some_and(|d| d.source == source)
                    || i.action.as_ref().is_some_and(|a| crate::catalogue::action_refs(a).contains(&e.id))
            })
            .map(|i| i.id)
            .collect()
    }

    pub fn delete_catalogue_entry(&self, id: &str) -> Result<(), String> {
        self.store.delete(Table::Catalogue, id).map_err(|e| e.to_string())?;
        self.catalogue.write().unwrap().remove(id);
        self.cache_changed();
        Ok(())
    }

    pub fn cache_changed(&self) {
        self.notify(Change::Catalogue);
    }

    pub fn cache_rows(&self) -> Vec<crate::cache::Row> {
        self.store.all(Table::Cache).unwrap_or_else(|e| {
            tracing::error!("can't read the cache table: {e}");
            Vec::new()
        })
    }

    pub fn cache_row(&self, name: &str) -> Option<crate::cache::Row> {
        self.store.get(Table::Cache, name).unwrap_or_else(|e| {
            tracing::error!(name, "can't read a cache row: {e}");
            None
        })
    }

    pub fn put_cache_row(&self, r: &crate::cache::Row) {
        if let Err(e) = self.store.put(Table::Cache, &r.name, r) {
            tracing::error!(name = r.name, "can't save a cache row: {e}");
        }
    }

    pub fn delete_cache_row(&self, name: &str) {
        if let Err(e) = self.store.delete(Table::Cache, name) {
            tracing::error!(name, "can't delete a cache row: {e}");
        }
    }

    // ---- Settings ----

    pub fn settings(&self) -> Settings {
        self.settings.read().unwrap().clone()
    }

    pub fn network_zone(&self) -> Option<String> {
        self.network_zone.read().unwrap().clone()
    }

    pub fn set_network_zone(&self, zone: String) {
        if self.network_zone().as_ref() == Some(&zone) {
            return;
        }
        if let Err(e) = self.store.put(Table::Meta, "network_time_zone", &zone) {
            tracing::warn!("can't save the network's time zone: {e}");
        }
        self.event(None, "time-zone", format!("The network's time zone is {zone}"));
        *self.network_zone.write().unwrap() = Some(zone);
        self.notify(Change::Settings);
    }

    /// The settings with the time zone actually in force filled in.
    pub fn window_settings(&self) -> Settings {
        let mut s = self.settings();
        if s.time_zone.is_empty() {
            s.time_zone = self.network_zone().unwrap_or_else(|| "UTC".into());
        }
        s
    }

    pub fn put_settings(&self, s: Settings) -> Result<(), String> {
        s.theme.validate()?;
        match self.image(&s.default_image) {
            None => return Err(format!("no image {:?}", s.default_image)),
            Some(i) if i.action.is_none() && i.disk.is_none() && i.id != "menu" => {
                return Err(format!("{:?} is for Raspberry Pis only, so it can't be the PC default", s.default_image));
            }
            Some(_) => {}
        }
        if !s.pi_default_image.is_empty() && self.image(&s.pi_default_image).and_then(|i| i.pi).is_none() {
            return Err(format!("{:?} isn't an image with Raspberry Pi boot files", s.pi_default_image));
        }
        if !(3..=300).contains(&s.poll_secs) {
            return Err("poll_secs must be 3–300".into());
        }
        if s.menu_timeout_secs != 0 && s.menu_timeout_secs < s.poll_secs * 2 {
            return Err("menu_timeout_secs must be 0 (never) or at least two poll intervals".into());
        }
        crate::rollout::check_window(&s)?;
        self.store.put(Table::Meta, "settings", &s).map_err(|e| e.to_string())?;
        *self.settings.write().unwrap() = s;
        self.notify(Change::Settings);
        Ok(())
    }

    // ---- Events ----

    pub fn event(&self, mac: Option<Mac>, kind: &str, message: String) {
        let e = Event { at: now_ms(), mac: mac.map(|m| m.to_string()), kind: kind.into(), message };
        tracing::info!(mac = e.mac.as_deref().unwrap_or("-"), kind, "{}", e.message);
        if let Err(err) = self.store.append_event(&e) {
            tracing::error!("can't save an event: {err}");
        }
        let mut ev = self.events.lock().unwrap();
        ev.push_back(e);
        while ev.len() > EVENTS_IN_MEMORY {
            ev.pop_front();
        }
        drop(ev);
        self.notify(Change::Event);
    }

    pub fn events(&self, mac: Option<&str>, limit: usize) -> Vec<Event> {
        let ev = self.events.lock().unwrap();
        let mut out: Vec<Event> =
            ev.iter().rev().filter(|e| mac.is_none_or(|m| e.mac.as_deref() == Some(m))).take(limit).cloned().collect();
        out.reverse();
        out
    }

    // ---- Resolution ----

    /// One-shot assignment, then the device's assignment, then the site default.
    /// (Class rules slot in between the last two in a later phase.)
    pub fn resolve(&self, dev: &Device) -> Resolved {
        if let Some(a) = &dev.assignment
            && self.image(&a.image).is_some()
        {
            let reason = if a.once { "one-shot assignment" } else { "assigned to this machine" };
            return Resolved { image: a.image.clone(), reason: reason.into(), once: a.once };
        }
        let s = self.settings();
        let image = if self.image(&s.default_image).is_some() { s.default_image } else { "menu".into() };
        Resolved { image, reason: "no assignment; site default".into(), once: false }
    }

    /// What a Raspberry Pi boots: its assignment if that has Pi boot files, else the Pi
    /// default. `None`: nothing to serve it.
    pub fn resolve_pi(&self, dev: &Device) -> Option<Resolved> {
        let has_pi = |id: &str| self.image(id).is_some_and(|i| i.pi.is_some());
        if let Some(a) = &dev.assignment
            && has_pi(&a.image)
        {
            let reason = if a.once { "one-shot assignment" } else { "assigned to this machine" };
            return Some(Resolved { image: a.image.clone(), reason: reason.into(), once: a.once });
        }
        let s = self.settings();
        has_pi(&s.pi_default_image).then(|| Resolved {
            image: s.pi_default_image,
            reason: "no assignment; Raspberry Pi default".into(),
            once: false,
        })
    }

    /// Records that a machine is booting `image` (and uses up a one-shot assignment).
    pub fn record_boot(&self, mac: Mac, image: &str, reason: &str, used_once: bool) {
        let title = self.image_title(image);
        self.update_device(mac, |d| {
            d.menu = None;
            d.waiting = None;
            d.last_boot = Some(BootRecord { at: now_ms(), image: image.into(), title: title.clone(), reason: reason.into() });
            if used_once && d.assignment.as_ref().is_some_and(|a| a.once && a.image == image) {
                d.assignment = None;
            }
        });
        self.event(Some(mac), "boot", format!("Booting {title} ({reason})"));
    }

    /// Notes that a machine's boot is held for a download.
    pub fn note_waiting(&self, mac: Mac, image: &str, detail: &str) {
        let now = now_ms();
        let title = self.image_title(image);
        let (_, _) = self.update_device(mac, |d| {
            d.last_seen = now;
            d.waiting = match d.waiting.take() {
                Some(w) if w.image == image => Some(Waiting { last: now, detail: detail.into(), ..w }),
                _ => Some(Waiting { image: image.into(), since: now, last: now, detail: detail.into() }),
            };
        });
        if self.device(mac).and_then(|d| d.waiting).is_some_and(|w| w.since == now) {
            self.event(Some(mac), "boot", format!("Holding the boot of {title} until its download is ready"));
        }
    }

    /// The machine isn't held any more: the file failed, or it moved on.
    pub fn clear_waiting(&self, mac: Mac) {
        if self.device(mac).is_some_and(|d| d.waiting.is_some()) {
            self.update_device(mac, |d| d.waiting = None);
        }
    }

    // ---- Boot interfaces and admission ----

    /// Re-reads the host's interfaces (addresses, VLANs).
    pub fn refresh_netifs(&self) {
        *self.netifs.write().unwrap() = crate::netif::discover();
    }

    pub fn netif(&self, name: &str) -> Option<crate::netif::NetIf> {
        self.netifs.read().unwrap().iter().find(|i| i.name == name).cloned()
    }

    /// A boot interface's VLAN, if it's a VLAN subinterface.
    pub fn vlan_of(&self, iface: &str) -> Option<u16> {
        self.netif(iface).and_then(|i| i.vlan)
    }

    /// What the UI and the log call a boot interface: its label, and its VLAN.
    pub fn iface_label(&self, iface: &str) -> String {
        let label = self.config.boot_iface(iface).map(|b| b.label().to_string()).unwrap_or_else(|| iface.to_string());
        match self.vlan_of(iface) {
            Some(v) => format!("{label} (VLAN {v})"),
            None => label,
        }
    }

    /// The address a boot interface serves on: the configured one, else the interface's own.
    pub fn boot_address(&self, iface: &str) -> Option<Ipv4Addr> {
        let b = self.config.boot_iface(iface)?;
        b.address.or_else(|| self.netif(iface)?.address())
    }

    /// The URL machines on a boot interface fetch from (`{bootbob}`).
    pub fn boot_url(&self, iface: &str) -> Option<String> {
        let b = self.config.boot_iface(iface)?;
        match &b.url {
            Some(u) => Some(u.trim_end_matches('/').to_string()),
            None => Some(format!("http://{}:{}", self.boot_address(iface)?, b.http_port)),
        }
    }

    /// Admits a machine to a boot interface, or takes that away.
    pub fn set_admitted(&self, mac: Mac, iface: &str, admitted: bool) -> Result<Device, String> {
        if self.config.boot_iface(iface).is_none() {
            return Err(format!("no boot interface {iface:?}"));
        }
        let was = self.device(mac).is_some_and(|d| d.is_admitted(iface));
        let (dev, _) = self.update_device(mac, |d| {
            d.admitted.retain(|a| a != iface);
            if admitted {
                d.admitted.push(iface.to_string());
                d.admitted.sort();
            }
        });
        if was != admitted {
            let label = self.iface_label(iface);
            self.event(
                Some(mac),
                "admitted",
                if admitted { format!("Admitted on {label}") } else { format!("No longer admitted on {label}") },
            );
        }
        Ok(dev)
    }

    /// Records that a machine was seen on a boot interface, at `ip`. Its address is only
    /// believed where it's admitted: a machine claiming its MAC elsewhere doesn't move it.
    /// Logs a machine admitted elsewhere turning up where it isn't (once per visit): that's
    /// either a machine that moved, or something claiming its MAC.
    pub fn note_sighting(&self, mac: Mac, iface: &str, ip: Option<Ipv4Addr>) {
        let now = now_ms();
        let vlan = self.vlan_of(iface);
        let before = self.device(mac);
        let (dev, _) = self.update_device(mac, |d| {
            d.last_seen = now;
            let s = d.seen.entry(iface.to_string()).or_default();
            s.at = now;
            s.vlan = vlan;
            if let Some(ip) = ip {
                s.ip = Some(ip.to_string());
            }
            if let Some(ip) = ip
                && d.speaks_for(iface)
            {
                d.ip = Some(ip.to_string());
            }
        });
        let quiet_for = before.as_ref().and_then(|d| d.seen.get(iface)).map(|s| now.saturating_sub(s.at));
        if !dev.admitted.is_empty() && !dev.is_admitted(iface) && quiet_for.is_none_or(|q| q >= PI_SESSION_GAP_MS) {
            let elsewhere = dev.admitted.iter().map(|a| self.iface_label(a)).collect::<Vec<_>>().join(", ");
            self.event(
                Some(mac),
                "unadmitted",
                format!("Seen on {}, where it isn't admitted (it's admitted on {elsewhere}): public images only", self.iface_label(iface)),
            );
        }
    }

    // ---- The doctor ----

    pub fn expect_probe(&self, xid: u32) {
        self.probes.lock().unwrap().insert(xid, Vec::new());
    }

    pub fn take_probe(&self, xid: u32) -> Vec<crate::doctor::Offer> {
        self.probes.lock().unwrap().remove(&xid).unwrap_or_default()
    }

    pub fn set_report(&self, r: crate::doctor::Report) {
        *self.report.lock().unwrap() = Some(r);
    }

    pub fn report(&self) -> Option<crate::doctor::Report> {
        self.report.lock().unwrap().clone()
    }

    // ---- Addresses ----

    pub fn learn_address(&self, iface: &str, ip: Ipv4Addr, mac: Mac) {
        if ip.is_unspecified() || ip.is_broadcast() {
            return;
        }
        self.addresses.lock().unwrap().insert((iface.to_string(), ip), (mac, now_ms()));
    }

    /// The MAC behind an IP on one boot interface: what its listener learnt, else the
    /// kernel's neighbour table for that interface. Never what was learnt on another.
    pub fn mac_for(&self, iface: &str, ip: Ipv4Addr) -> Option<Mac> {
        if let Some((m, _)) = self.addresses.lock().unwrap().get(&(iface.to_string(), ip)) {
            return Some(*m);
        }
        bootbob_dhcp::neigh::lookup_on(iface, ip)
    }

    /// Which boot interface and machine an IP belongs to, for a question that comes without
    /// an interface (an image store asking about one of its NBD clients). Only when exactly
    /// one boot interface knows it.
    pub fn identify_ip(&self, ip: Ipv4Addr) -> Option<(String, Mac)> {
        let mut found = self.config.boot.iter().filter_map(|b| Some((b.interface.clone(), self.mac_for(&b.interface, ip)?)));
        let first = found.next()?;
        found.next().is_none().then_some(first)
    }

    /// The firmware of whatever is at `ip` on `iface`, as best we know.
    pub fn firmware_for(&self, iface: &str, ip: Ipv4Addr) -> (Option<Mac>, Option<Firmware>) {
        let mac = self.mac_for(iface, ip);
        let fw = mac.and_then(|m| self.device(m)).and_then(|d| d.firmware).and_then(|s| Firmware::from_slug(&s));
        (mac, fw)
    }

    // ---- The DHCP listener ----

    /// What the DHCP listener on boot interface `iface` saw.
    pub fn observe(&self, iface: &str, seen: Seen) {
        let now = now_ms();
        match &seen {
            Seen::Dhcp { src_mac, src_ip, from_server, msg, .. } => tracing::debug!(
                %src_mac, %src_ip, from_server, xid = format!("{:#010x}", msg.xid), kind = ?msg.msg_type(), client = ?msg.mac(),
                "listener: DHCP"
            ),
            Seen::TftpRequest { src_mac, src_ip, path } => tracing::debug!(%src_mac, %src_ip, path, "listener: TFTP request"),
        }
        match seen {
            Seen::Dhcp { src_mac, from_server, msg, .. } => {
                if let Some(l) = self.listeners.write().unwrap().get_mut(iface) {
                    l.dhcp_seen += 1;
                    l.last_seen = Some(now);
                }
                // A reply to one of the doctor's probes: hand it over, and don't treat the
                // probe's made-up MAC as a machine. Probes go out only on the control network,
                // so only answers heard there count: a forged one on a boot VLAN can't set the
                // time zone.
                if from_server
                    && self.config.control.network.as_deref() == Some(iface)
                    && let Some(offers) = self.probes.lock().unwrap().get_mut(&msg.xid)
                {
                    offers.push(crate::doctor::Offer {
                        server: msg.server_id(),
                        src_mac,
                        boot_server: msg.boot_server(),
                        boot_file: msg.boot_file(),
                        time_zone: msg.time_zone(),
                    });
                    return;
                }
                let Some(client) = msg.mac() else { return };
                if from_server {
                    if !matches!(msg.msg_type(), Some(MessageType::Offer | MessageType::Ack)) {
                        return;
                    }
                    self.learn_address(iface, msg.yiaddr, client);
                    // Only machines we already know as netbooters get the offer recorded;
                    // this isn't a DHCP log of every phone on the network.
                    if self.device(client).is_some() {
                        self.update_device(client, |d| {
                            if !msg.yiaddr.is_unspecified() && d.speaks_for(iface) {
                                d.ip = Some(msg.yiaddr.to_string());
                            }
                            d.dhcp.get_or_insert_with(Default::default).offer = Some(OfferSeen {
                                at: now,
                                server: msg.server_id().map(|s| s.to_string()).or(Some(src_mac.to_string())),
                                ip: (!msg.yiaddr.is_unspecified()).then(|| msg.yiaddr.to_string()),
                                boot_server: msg.boot_server(),
                                boot_file: msg.boot_file(),
                            });
                        });
                    }
                    return;
                }
                if !msg.is_pxe() || !matches!(msg.msg_type(), Some(MessageType::Discover | MessageType::Request)) {
                    return;
                }
                if let Some(l) = self.listeners.write().unwrap().get_mut(iface) {
                    l.pxe_seen += 1;
                }
                let arches = msg.arches();
                let fw = arches.first().map(|&a| Firmware::from_arch(a));
                let is_new = self.device(client).is_none();
                let vlan = self.vlan_of(iface);
                let (_, _) = self.update_device(client, |d| {
                    d.last_seen = now;
                    let s = d.seen.entry(iface.to_string()).or_default();
                    s.at = now;
                    s.vlan = vlan;
                    if !d.speaks_for(iface) {
                        return;
                    }
                    if let Some((f, http)) = fw
                        // iPXE re-sends the firmware's arch; the firmware's own is the truth,
                        // but either is fine. A Pi claims BIOS; its MAC says otherwise.
                        && !client.is_raspberry_pi()
                    {
                        d.firmware = Some(f.slug());
                        d.http_boot = http;
                    }
                    if let Some(u) = msg.uuid() {
                        d.uuid = clean_smbios(Some(&u)).or(d.uuid.take());
                    }
                    let dh = d.dhcp.get_or_insert_with(Default::default);
                    dh.last_discover = now;
                    dh.arches = arches.clone();
                    dh.vendor_class = msg.vendor_class();
                    dh.ipxe = msg.is_ipxe();
                });
                if !is_new && msg.msg_type() == Some(MessageType::Discover) && !msg.is_ipxe() {
                    self.event(Some(client), "netboot", format!("Started a network boot (DHCP) on {}", self.iface_label(iface)));
                }
            }
            Seen::TftpRequest { src_mac, src_ip, .. } => {
                self.learn_address(iface, src_ip, src_mac);
            }
        }
    }
}

/// A short human description of a device for log lines.
pub fn describe(d: &Device) -> String {
    let short = d.mac.parse::<Mac>().map(|m| m.short()).unwrap_or_default();
    let mut s = match &d.name {
        Some(n) => format!("{n} ({short})"),
        None => short,
    };
    let model: Vec<&str> = [d.manufacturer.as_deref(), d.product.as_deref()].into_iter().flatten().collect();
    if !model.is_empty() {
        s.push_str(&format!(", {}", model.join(" ")));
    }
    if let Some(f) = &d.firmware {
        s.push_str(&format!(", {f}"));
    }
    s
}
