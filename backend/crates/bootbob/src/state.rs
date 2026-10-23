//! BootBob's working state: devices, images and settings in memory, written through to the
//! store, with a change feed for the web UI.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::Ipv4Addr;
use std::sync::{Mutex, RwLock};

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
    Device { mac: String },
    DeviceRemoved { mac: String },
    Images,
    Settings,
    Event,
}

/// Why a machine gets what it gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub image: String,
    pub reason: String,
    /// The image came from a one-shot assignment, which booting it uses up.
    pub once: bool,
}

/// The DHCP listener's state, for the UI and the network check.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ListenerStatus {
    pub interface: Option<String>,
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
    events: Mutex<VecDeque<Event>>,
    /// IP → MAC, learnt from DHCP replies and TFTP request frames.
    addresses: Mutex<HashMap<Ipv4Addr, (Mac, Millis)>>,
    pub listener: RwLock<ListenerStatus>,
    /// The doctor's probes in flight, by DHCP transaction id, with the offers they got.
    probes: Mutex<HashMap<u32, Vec<crate::doctor::Offer>>>,
    report: Mutex<Option<crate::doctor::Report>>,
    changes: broadcast::Sender<Change>,
}

impl State {
    pub fn open(config: Config) -> anyhow::Result<State> {
        std::fs::create_dir_all(&config.data_dir)?;
        let store = Store::open(&config.data_dir.join("bootbob.redb"))?;
        let devices = store.all::<Device>(Table::Devices)?.into_iter().filter_map(|d| Some((d.mac.parse::<Mac>().ok()?, d))).collect();
        let mut images: BTreeMap<String, Image> = store.all::<Image>(Table::Images)?.into_iter().map(|i| (i.id.clone(), i)).collect();
        for b in builtin_images() {
            images.insert(b.id.clone(), b);
        }
        let settings = store.get::<Settings>(Table::Meta, "settings")?.unwrap_or_default();
        let events = store.recent_events::<Event>(EVENTS_IN_MEMORY)?.into();
        let (changes, _) = broadcast::channel(256);
        Ok(State {
            listener: RwLock::new(ListenerStatus { interface: config.interface.clone(), ..Default::default() }),
            config,
            store,
            devices: RwLock::new(devices),
            images: RwLock::new(images),
            settings: RwLock::new(settings),
            events: Mutex::new(events),
            addresses: Mutex::new(HashMap::new()),
            probes: Mutex::new(HashMap::new()),
            report: Mutex::new(None),
            changes,
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
        if let Some(a) = &assignment
            && self.image(&a.image).is_none()
        {
            return Err(format!("no image {:?}", a.image));
        }
        let msg = match &assignment {
            Some(a) => format!("{} {}", if a.once { "Next boot only:" } else { "Assigned" }, self.image_title(&a.image)),
            None => "Assignment cleared".to_string(),
        };
        let (dev, _) = self.update_device(mac, |d| {
            d.assignment = assignment.map(|a| Assignment { set_at: now_ms(), ..a });
        });
        self.event(Some(mac), "assigned", msg);
        Ok(dev)
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
        if image.action.is_none() {
            return Err("an image needs an action".into());
        }
        self.store.put(Table::Images, &image.id, &image).map_err(|e| e.to_string())?;
        self.images.write().unwrap().insert(image.id.clone(), Image { builtin: false, ..image });
        self.notify(Change::Images);
        Ok(())
    }

    pub fn delete_image(&self, id: &str) -> Result<(), String> {
        match self.image(id) {
            None => return Err(format!("no image {id:?}")),
            Some(i) if i.builtin => return Err(format!("{id:?} is a built-in image")),
            Some(_) => {}
        }
        let in_use = self.devices().iter().filter(|d| d.assignment.as_ref().is_some_and(|a| a.image == id)).count();
        if in_use > 0 || self.settings().default_image == id {
            return Err(format!("{id:?} is in use ({in_use} assignments, or the default)"));
        }
        self.store.delete(Table::Images, id).map_err(|e| e.to_string())?;
        self.images.write().unwrap().remove(id);
        self.notify(Change::Images);
        Ok(())
    }

    // ---- Settings ----

    pub fn settings(&self) -> Settings {
        self.settings.read().unwrap().clone()
    }

    pub fn put_settings(&self, s: Settings) -> Result<(), String> {
        s.theme.validate()?;
        if self.image(&s.default_image).is_none() {
            return Err(format!("no image {:?}", s.default_image));
        }
        if !(3..=300).contains(&s.poll_secs) {
            return Err("poll_secs must be 3–300".into());
        }
        if s.menu_timeout_secs != 0 && s.menu_timeout_secs < s.poll_secs * 2 {
            return Err("menu_timeout_secs must be 0 (never) or at least two poll intervals".into());
        }
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

    /// Records that a machine is booting `image` (and uses up a one-shot assignment).
    pub fn record_boot(&self, mac: Mac, image: &str, reason: &str, used_once: bool) {
        let title = self.image_title(image);
        self.update_device(mac, |d| {
            d.menu = None;
            d.last_boot = Some(BootRecord { at: now_ms(), image: image.into(), title: title.clone(), reason: reason.into() });
            if used_once && d.assignment.as_ref().is_some_and(|a| a.once && a.image == image) {
                d.assignment = None;
            }
        });
        self.event(Some(mac), "boot", format!("Booting {title} ({reason})"));
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

    pub fn learn_address(&self, ip: Ipv4Addr, mac: Mac) {
        if ip.is_unspecified() || ip.is_broadcast() {
            return;
        }
        self.addresses.lock().unwrap().insert(ip, (mac, now_ms()));
    }

    /// The MAC behind an IP: what the listener learnt, else the kernel's neighbour table.
    pub fn mac_for(&self, ip: Ipv4Addr) -> Option<Mac> {
        if let Some((m, _)) = self.addresses.lock().unwrap().get(&ip) {
            return Some(*m);
        }
        bootbob_dhcp::neigh::lookup(ip)
    }

    /// The firmware of whatever is at `ip`, as best we know.
    pub fn firmware_for(&self, ip: Ipv4Addr) -> (Option<Mac>, Option<Firmware>) {
        let mac = self.mac_for(ip);
        let fw = mac.and_then(|m| self.device(m)).and_then(|d| d.firmware).and_then(|s| Firmware::from_slug(&s));
        (mac, fw)
    }

    // ---- The DHCP listener ----

    pub fn observe(&self, seen: Seen) {
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
                {
                    let mut l = self.listener.write().unwrap();
                    l.dhcp_seen += 1;
                    l.last_seen = Some(now);
                }
                // A reply to one of the doctor's probes: hand it over, and don't treat the
                // probe's made-up MAC as a machine.
                if from_server && let Some(offers) = self.probes.lock().unwrap().get_mut(&msg.xid) {
                    offers.push(crate::doctor::Offer {
                        server: msg.server_id(),
                        src_mac,
                        boot_server: msg.boot_server(),
                        boot_file: msg.boot_file(),
                    });
                    return;
                }
                let Some(client) = msg.mac() else { return };
                if from_server {
                    if !matches!(msg.msg_type(), Some(MessageType::Offer | MessageType::Ack)) {
                        return;
                    }
                    self.learn_address(msg.yiaddr, client);
                    // Only machines we already know as netbooters get the offer recorded;
                    // this isn't a DHCP log of every phone on the network.
                    if self.device(client).is_some() {
                        self.update_device(client, |d| {
                            if !msg.yiaddr.is_unspecified() {
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
                self.listener.write().unwrap().pxe_seen += 1;
                let arches = msg.arches();
                let fw = arches.first().map(|&a| Firmware::from_arch(a));
                let is_new = self.device(client).is_none();
                let (_, _) = self.update_device(client, |d| {
                    d.last_seen = now;
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
                    self.event(Some(client), "netboot", "Started a network boot (DHCP)".into());
                }
            }
            Seen::TftpRequest { src_mac, src_ip, .. } => {
                self.learn_address(src_ip, src_mac);
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
