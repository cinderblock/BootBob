//! Moving machines onto a new version of a golden image.
//!
//! A [`Track`] is a series of versions of one golden image, each an ordinary copy-on-write
//! image over its own dated disk file. A machine follows one by opting in, which is what says
//! its root is disposable: its assignment names a version as usual, plus the track. Everything
//! that serves a boot sees only an ordinary assignment.
//!
//! Overlays are named after their disk, so a machine on a new version gets a new overlay by
//! itself and the old one stays as it was. **Nothing here resets or deletes an overlay**, which
//! is also why going back is the same move in reverse, with the machine's state intact.
//!
//! BootBob can't reboot a machine. A timer in the image asks `/boot/rollout` every couple of
//! minutes, saying how it is, and is told `stay` or `reboot`. So everything is decided here: one
//! machine at a time, inside the window, stopping at the first one that doesn't come back
//! healthy. See [`tick`], which holds no state of its own, so a restart loses nothing.

use std::sync::Arc;
use std::time::Duration;

use bootbob_dhcp::Mac;
use jiff::Timestamp;
use jiff::civil::Time;
use jiff::tz::TimeZone;
use serde::Serialize;

use crate::catalogue::Hold;
use crate::model::*;
use crate::state::{State, describe};

/// How often the host looks at the tracks.
const TICK: Duration = Duration::from_secs(30);
/// A machine told to reboot that hasn't by now goes back in the queue.
const REBOOT_MS: Millis = 10 * 60_000;

/// A machine quiet for this long isn't waited for: it gets the new version when it next boots.
const STALE_MS: Millis = 30 * 60_000;
/// What a healthy machine says: `systemctl is-system-running`.
const HEALTHY: &str = "running";

pub const STAY: &str = "stay";
pub const REBOOT: &str = "reboot";

/// Runs [`tick`] for as long as BootBob runs.
pub async fn run(st: Arc<State>) {
    let mut every = tokio::time::interval(TICK);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        every.tick().await;
        let st = st.clone();
        // It may ask a store whether a disk is there.
        if let Err(e) = tokio::task::spawn_blocking(move || tick(&st, now_ms())).await {
            tracing::error!("the rollout tick failed: {e}");
        }
    }
}

// ---- The window ----

fn parse_time(s: &str) -> Option<Time> {
    let (h, m) = s.split_once(':')?;
    Time::new(h.parse().ok()?, m.parse().ok()?, 0, 0).ok()
}

/// Whether `s`'s window settings make sense.
pub fn check_window(s: &Settings) -> Result<(), String> {
    parse_time(&s.rollout_start).ok_or_else(|| format!("rollout_start must be HH:MM, not {:?}", s.rollout_start))?;
    if !s.time_zone.is_empty() {
        TimeZone::get(&s.time_zone).map_err(|_| format!("no time zone {:?}", s.time_zone))?;
    }
    if !(1..=1440).contains(&s.rollout_minutes) {
        return Err("rollout_minutes must be 1–1440".into());
    }
    if !(1..=240).contains(&s.rollout_health_minutes) {
        return Err("rollout_health_minutes must be 1–240".into());
    }
    Ok(())
}

/// When the windows on and after the day before `at` start, soonest first, in the time zone's
/// own rules, so 5 AM stays 5 AM across daylight saving.
fn window_starts(s: &Settings, at: Millis) -> Vec<Millis> {
    let (Some(time), Ok(tz)) = (parse_time(&s.rollout_start), TimeZone::get(&s.time_zone)) else { return Vec::new() };
    let Ok(now) = Timestamp::from_millisecond(at as i64) else { return Vec::new() };
    let today = now.to_zoned(tz.clone()).date();
    (-1..=2)
        .filter_map(|days| {
            let date = today.checked_add(jiff::Span::new().days(days)).ok()?;
            Some(date.to_datetime(time).to_zoned(tz.clone()).ok()?.timestamp().as_millisecond() as Millis)
        })
        .collect()
}

/// Whether `at` is inside the daily window.
pub fn in_window(s: &Settings, at: Millis) -> bool {
    let len = s.rollout_minutes as Millis * 60_000;
    window_starts(s, at).into_iter().any(|start| start <= at && at < start + len)
}

/// When the next window starts.
pub fn next_window(s: &Settings, at: Millis) -> Option<Millis> {
    window_starts(s, at).into_iter().find(|start| *start > at)
}

fn is_open(st: &State, t: &Track, now: Millis) -> bool {
    t.open_until.is_some_and(|u| now < u) || in_window(&st.window_settings(), now)
}

// ---- What a machine hears ----

fn booted<'a>(d: &'a Device, m: &Move) -> Option<&'a BootRecord> {
    d.last_boot.as_ref().filter(|b| b.image == m.to && b.at >= m.started)
}

/// A machine checking in: records how it is, and tells it what to do. It reboots only when it
/// says it isn't busy, because that's its last word on the subject.
pub fn check(st: &State, mac: Mac, health: &str, busy: &str) -> &'static str {
    let clip = |s: &str| s.chars().take(80).collect::<String>();
    let c = Check { at: now_ms(), health: clip(health), busy: clip(busy) };
    let Some(d) = st.edit_device(mac, |d| {
        d.last_seen = c.at;
        d.check = Some(c.clone());
    }) else {
        return STAY;
    };
    match &d.moving {
        Some(m) if booted(&d, m).is_none() && busy.is_empty() => REBOOT,
        _ => STAY,
    }
}

// ---- The tick ----

/// The machines following a track.
fn followers(st: &State, track: &str) -> Vec<(Mac, Device)> {
    st.devices()
        .into_iter()
        .filter(|d| d.assignment.as_ref().is_some_and(|a| a.track.as_deref() == Some(track)))
        .filter_map(|d| Some((d.mac.parse().ok()?, d)))
        .collect()
}

fn set_image(d: &mut Device, image: &str) {
    if let Some(a) = &mut d.assignment {
        a.image = image.to_string();
        a.set_at = now_ms();
    }
}

/// Puts a machine that hasn't rebooted back where it was. It was never told otherwise as far
/// as anything it's running is concerned.
fn give_back(st: &State, mac: Mac, m: &Move, not_before: Option<Millis>) {
    st.update_device(mac, |d| {
        set_image(d, &m.from);
        d.moving = None;
        if not_before.is_some() {
            d.not_before = not_before;
        }
    });
}

/// Looks at a machine in flight. `true` when it's finished with, one way or another.
fn settle(st: &State, t: &Track, mac: Mac, d: &Device, m: &Move, now: Millis) -> bool {
    let name = describe(d);
    if let Some(b) = booted(d, m) {
        if d.check.as_ref().is_some_and(|c| c.at > b.at && c.health == HEALTHY) {
            st.update_device(mac, |d| d.moving = None);
            let done = if m.back { "Back on" } else { "Now on" };
            st.event(Some(mac), "rollout", format!("{done} {}, healthy", m.to));
            return true;
        }
        let health_ms = st.settings().rollout_health_minutes as Millis * 60_000;
        if now.saturating_sub(b.at) < health_ms {
            return false;
        }
        if m.back {
            st.update_device(mac, |d| d.moving = None);
            st.event(Some(mac), "error", format!("{name} is back on {} but isn't healthy. It needs a person", m.to));
            return true;
        }
        // Keep the track where it is: this may be the machine, not the version.
        let said = d.check.as_ref().filter(|c| c.at > b.at).map_or("nothing".to_string(), |c| format!("{:?}", c.health));
        let why = format!("{name} booted {} but said {said} after {} min", m.to, health_ms / 60_000);
        st.update_device(mac, |d| {
            set_image(d, &m.from);
            d.moving = Some(Move { from: m.to.clone(), to: m.from.clone(), started: now, back: true, ..m.clone() });
        });
        halt(st, &t.id, &why);
        return false;
    }
    if !m.back && d.check.as_ref().is_some_and(|c| c.at >= m.started && !c.busy.is_empty()) {
        give_back(st, mac, m, None);
        st.event(Some(mac), "rollout", format!("Left on {}: it became busy before rebooting", m.from));
        return true;
    }
    if now.saturating_sub(m.started) < REBOOT_MS {
        return false;
    }
    if m.back {
        st.update_device(mac, |d| d.moving = None);
        st.event(Some(mac), "error", format!("{name} hasn't rebooted. It needs a power cycle, and then boots {}", m.to));
    } else {
        // Assigned its old version again, so a power cycle can't land it on the new one.
        give_back(st, mac, m, Some(now + st.settings().rollout_minutes as Millis * 60_000));
        st.event(Some(mac), "rollout", format!("Left on {}: it didn't reboot in {} min", m.from, REBOOT_MS / 60_000));
    }
    true
}

/// Stops a track moving any more machines, until a person says otherwise.
pub fn halt(st: &State, id: &str, why: &str) {
    st.edit_track(id, |t| t.halted = Some(Halt { at: now_ms(), why: why.into() }));
    st.event(None, "error", format!("Rollout of {id} halted: {why}"));
}

/// Whether a machine rebooting into `image` would find it: `Ok(None)` when it would, `Ok(Some)`
/// while it's still arriving. Blocking: it may ask a store.
pub fn ready(st: &Arc<State>, image: &Image) -> Result<Option<String>, String> {
    let entries = if image.pi.is_some() { crate::catalogue::for_pi(st, image) } else { crate::catalogue::for_pc(st, image) };
    match crate::catalogue::hold(st, &entries) {
        Hold::Go => {}
        Hold::Wait(what) => return Ok(Some(what)),
        Hold::Failed(why) => return Err(why),
    }
    let (source, file) = match (&image.pi, &image.disk) {
        (Some(p), _) => (&p.source, "config.txt"),
        (_, Some(d)) => (&d.source, d.kernel.as_str()),
        _ => return Err(format!("{} has no disk", image.id)),
    };
    match crate::stores::boot_file(st, source, false, None, file) {
        Ok(Some(_)) => Ok(None),
        Ok(None) => Err(format!("{source} has no {file}")),
        Err(e) => Err(format!("can't read {file} from {source}: {e}")),
    }
}

/// Moves the next machine, if one can go.
fn advance(st: &Arc<State>, t: &Track, now: Millis) {
    let on = |d: &Device| {
        d.assignment.as_ref().is_some_and(|a| a.image == t.current) && d.last_boot.as_ref().is_some_and(|b| b.image == t.current)
    };
    let waiting: Vec<_> = followers(st, &t.id).into_iter().filter(|(_, d)| !on(d)).collect();
    if waiting.is_empty() {
        return;
    }
    let Some(target) = st.image(&t.current) else { return };
    let blocked = match ready(st, &target) {
        Ok(None) => None,
        Ok(Some(what)) => Some(format!("waiting for {what}")),
        Err(why) => Some(why),
    };
    if blocked != t.blocked {
        st.edit_track(&t.id, |t| t.blocked = blocked.clone());
        if let Some(why) = &blocked {
            st.event(None, "rollout", format!("Rollout of {} can't start: {why}", t.id));
        }
    }
    if blocked.is_some() {
        return;
    }
    for (mac, d) in waiting {
        let assigned = d.assignment.as_ref().is_some_and(|a| a.image == t.current);
        let check = d.check.as_ref().filter(|c| now.saturating_sub(c.at) < STALE_MS);
        let (Some(check), Some(running)) = (check, &d.last_boot) else {
            // Off, or nothing in it answers: a move is just what it boots next.
            if !assigned {
                st.update_device(mac, |d| set_image(d, &t.current));
                st.event(Some(mac), "rollout", format!("Boots {} next time", t.current));
            }
            continue;
        };
        if !check.busy.is_empty() || d.not_before.is_some_and(|n| now < n) {
            continue;
        }
        let from = match d.assignment.as_ref().filter(|a| a.image != t.current) {
            Some(a) => a.image.clone(),
            None => running.image.clone(),
        };
        st.update_device(mac, |d| {
            set_image(d, &t.current);
            d.not_before = None;
            d.moving = Some(Move { track: t.id.clone(), from: from.clone(), to: t.current.clone(), started: now, back: false });
        });
        st.event(Some(mac), "rollout", format!("Moving from {from} to {}: it reboots at its next check-in", t.current));
        return;
    }
}

/// One look at every track. `now` is a parameter so tests can walk through a night.
pub fn tick(st: &Arc<State>, now: Millis) {
    for t in st.tracks() {
        let mut flying = false;
        for (mac, d) in followers(st, &t.id) {
            if let Some(m) = d.moving.as_ref().filter(|m| m.track == t.id) {
                flying |= !settle(st, &t, mac, &d, m, now);
            }
        }
        let Some(t) = st.track(&t.id) else { continue };
        if !flying && t.halted.is_none() && is_open(st, &t, now) {
            advance(st, &t, now);
        }
    }
}

// ---- What a person does ----

/// A track for the API: where its window stands.
#[derive(Debug, Clone, Serialize)]
pub struct TrackView {
    #[serde(flatten)]
    pub track: Track,
    pub open: bool,
    pub next_window: Option<Millis>,
}

pub fn view(st: &State, track: Track) -> TrackView {
    let now = now_ms();
    TrackView { open: is_open(st, &track, now), next_window: next_window(&st.window_settings(), now), track }
}

/// Adds a version: a copy of the current one over a new disk, which becomes current. The next
/// window does the rest. Refused unless a machine could boot it. Blocking.
pub fn add_version(st: &Arc<State>, id: &str, source: &str, title: Option<String>) -> Result<Image, String> {
    let t = st.track(id).ok_or_else(|| format!("no track {id:?}"))?;
    let base = st.image(&t.current).ok_or_else(|| format!("{id}'s current version {:?} is gone", t.current))?;
    let disk = crate::stores::export_name(source);
    let stem = disk.strip_suffix(".img").ok_or("a version is a disk image (.img)")?;
    if st.image(stem).is_some() {
        return Err(format!("there's already an image {stem:?}: a version's disk never changes, so give this one a new name"));
    }
    let mut image =
        Image { id: stem.into(), title: title.unwrap_or_else(|| format!("{} ({stem})", t.title)), group: String::new(), ..base };
    if let Some(p) = &mut image.pi {
        p.source = source.into();
    }
    if let Some(d) = &mut image.disk {
        d.source = source.into();
    }
    st.check_version(&image)?;
    if st.catalogue_for_source(source).is_none() {
        crate::stores::locate(&st.config, source)?;
    }
    ready(st, &image)?;
    st.put_image(image.clone())?;
    st.edit_track(id, |t| {
        t.versions.push(Version { image: image.id.clone(), added_at: now_ms() });
        t.current = image.id.clone();
        t.halted = None;
        t.blocked = None;
    });
    st.event(None, "rollout", format!("{id}: new version {}, for the next window", image.id));
    Ok(image)
}

/// Makes a version current, new or old, and clears a halt. `now` opens the window straight
/// away, for as long as a window lasts.
pub fn release(st: &State, id: &str, image: &str, now: bool) -> Result<(), String> {
    let t = st.track(id).ok_or_else(|| format!("no track {id:?}"))?;
    if !t.versions.iter().any(|v| v.image == image) {
        return Err(format!("{image:?} isn't a version of {id}"));
    }
    let until = now.then(|| now_ms() + st.settings().rollout_minutes as Millis * 60_000);
    st.edit_track(id, |t| {
        t.current = image.into();
        t.halted = None;
        t.blocked = None;
        t.open_until = until.or(t.open_until);
    });
    st.event(None, "rollout", format!("{id}: {image} is current{}", if now { ", moving machines now" } else { "" }));
    Ok(())
}

/// Clears a halt and opens the window now, whatever the time.
pub fn run_now(st: &State, id: &str) -> Result<(), String> {
    let t = st.track(id).ok_or_else(|| format!("no track {id:?}"))?;
    release(st, id, &t.current, true)
}
