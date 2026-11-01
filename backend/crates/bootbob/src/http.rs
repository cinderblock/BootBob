//! HTTP: iPXE scripts for machines, the JSON API and live feed for the web UI, and the UI.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Path, Query, State as Ax};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use bootbob_dhcp::{Firmware, Mac};
use bootbob_ipxe::{Menu, MenuItem};
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::BroadcastStream;
use tower_http::services::{ServeDir, ServeFile};

use crate::catalogue::Hold;
use crate::model::*;
use crate::state::{State, describe};
use crate::tftp::PI_FIRMWARE;

type St = Arc<State>;

pub fn router(st: St) -> Router {
    let mut r = Router::new()
        .route("/boot/ipxe", get(boot_ipxe))
        .route("/boot/ipxe/chose", get(boot_chose))
        .route("/boot/disk/{image}/{*path}", get(disk_file))
        .route("/boot/rollout", get(boot_rollout))
        .route("/files/usb/bootbob-usb.img", get(usb_stick))
        .route("/api/status", get(status))
        .route("/api/devices", get(list_devices))
        .route("/api/devices/{mac}", get(get_device).patch(patch_device).delete(delete_device))
        .route("/api/devices/{mac}/assignment", put(put_assignment).delete(delete_assignment))
        .route("/api/devices/{mac}/overlays", get(list_overlays))
        .route("/api/devices/{mac}/overlays/{source}", axum::routing::delete(reset_overlay))
        .route("/api/images", get(list_images))
        .route("/api/images/{id}", put(put_image).delete(delete_image))
        .route("/api/tracks", get(list_tracks))
        .route("/api/tracks/{id}", put(put_track).delete(delete_track))
        .route("/api/tracks/{id}/versions", axum::routing::post(add_version))
        .route("/api/tracks/{id}/release", axum::routing::post(release_version))
        .route("/api/tracks/{id}/run", axum::routing::post(run_track))
        .route("/api/tracks/{id}/halt", axum::routing::post(halt_track))
        .route("/api/pi/sources", get(pi_sources))
        .route("/api/store/allow", get(store_allow))
        .route("/api/disks", axum::routing::post(create_disk))
        .route("/api/disks/{name}", put(upload_disk).delete(delete_disk))
        .route("/api/catalogue", get(list_catalogue))
        .route("/api/catalogue/pi-os", get(pi_os))
        .route("/api/catalogue/{id}", put(put_catalogue).delete(delete_catalogue))
        .route("/api/catalogue/{id}/fetch", axum::routing::post(fetch_catalogue))
        .route("/api/cache", get(list_cache))
        .route("/api/cache/{name}", axum::routing::post(ensure_cache).delete(remove_cache))
        .route("/api/cache/{name}/pin", put(pin_cache))
        .route("/api/settings", get(get_settings).put(put_settings))
        .route("/api/events", get(list_events))
        .route("/api/doctor", get(last_report).post(run_doctor))
        .route("/api/stream", get(stream))
        .route("/theme.css", get(theme_css))
        .nest_service("/files/ipxe", ServeDir::new(&st.config.ipxe_dir))
        .nest_service("/files/cache", ServeDir::new(crate::cache::files_dir(&st)))
        .nest_service("/files/tools", ServeDir::new(st.config.tools_dir()));
    r = match st.config.web_dir.clone().filter(|d| d.join("index.html").exists()) {
        Some(dir) => r.fallback_service(ServeDir::new(&dir).fallback(ServeFile::new(dir.join("index.html")))),
        None => r.fallback(no_ui),
    };
    r.with_state(st)
}

async fn no_ui() -> Html<&'static str> {
    Html(
        "<!doctype html><title>BootBob</title><p>BootBob is running, but its web UI isn't installed \
         (<code>web_dir</code> in the config). The API is at <a href=\"/api/status\">/api/status</a>.",
    )
}

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}

fn not_found(what: &str) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, format!("no such {what}"))
}

fn parse_mac(s: &str) -> Result<Mac, ApiError> {
    s.parse().map_err(bad)
}

fn ipxe(body: String) -> Response {
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], body).into_response()
}

/// BootBob's URL as this client reached it.
fn base_url(st: &State, headers: &HeaderMap) -> String {
    if let Some(u) = &st.config.public_url {
        return u.trim_end_matches('/').to_string();
    }
    match headers.get(header::HOST).and_then(|h| h.to_str().ok()) {
        Some(host) => format!("http://{host}"),
        None => format!("http://{}", st.config.http),
    }
}

// ---- iPXE ----

/// What iPXE tells us about itself (see `bootbob_ipxe::IDENTITY_QUERY`).
#[derive(Debug, Default, Deserialize)]
struct Identity {
    mac: Option<String>,
    uuid: Option<String>,
    serial: Option<String>,
    manufacturer: Option<String>,
    product: Option<String>,
    asset: Option<String>,
    platform: Option<String>,
    arch: Option<String>,
    ip: Option<String>,
    poll: Option<String>,
    failed: Option<String>,
    item: Option<String>,
}

/// Records what a machine says about itself on every script fetch.
fn record_identity(st: &State, mac: Mac, q: &Identity) -> Device {
    st.update_device(mac, |d| {
        let now = now_ms();
        d.last_seen = now;
        d.last_script = Some(now);
        if let Some(ip) = q.ip.as_deref().filter(|s| s.parse::<std::net::Ipv4Addr>().is_ok_and(|ip| !ip.is_unspecified())) {
            d.ip = Some(ip.to_string());
        }
        if let (Some(p), Some(a)) = (&q.platform, &q.arch)
            && let Some(f) = Firmware::from_ipxe(p, a)
        {
            d.firmware = Some(f.slug());
        }
        for (field, val) in [
            (&mut d.uuid, &q.uuid),
            (&mut d.serial, &q.serial),
            (&mut d.manufacturer, &q.manufacturer),
            (&mut d.product, &q.product),
            (&mut d.asset, &q.asset),
        ] {
            if let Some(v) = clean_smbios(val.as_deref()) {
                *field = Some(v);
            }
        }
    })
    .0
}

/// The fallback menu for `dev`. `poll` makes it check back for an assignment.
fn menu_script(st: &State, dev: &Device, base: String, status_line: String, poll: bool) -> String {
    let poll_secs = if poll { st.settings().poll_secs } else { 0 };
    menu_with(st, dev, base, status_line, poll_secs, None)
}

/// [`menu_script`], checking back every `poll_secs`, at `refresh` if that's given.
fn menu_with(st: &State, dev: &Device, base: String, status_line: String, poll_secs: u32, refresh: Option<String>) -> String {
    let settings = st.settings();
    let mut groups: Vec<(String, Vec<MenuItem>)> = Vec::new();
    for img in st.images() {
        if img.group.is_empty() || (img.action.is_none() && img.disk.is_none()) {
            continue;
        }
        let item = MenuItem { id: img.id.clone(), title: img.title.clone() };
        match groups.iter_mut().find(|(g, _)| *g == img.group) {
            Some((_, items)) => items.push(item),
            None => groups.push((img.group.clone(), vec![item])),
        }
    }
    let ip = dev.ip.as_deref().map(|ip| format!(" at {ip}")).unwrap_or_default();
    bootbob_ipxe::menu(&Menu {
        base_url: base,
        title: "BootBob".into(),
        device_line: format!("This is {}{ip}", describe(dev)),
        status_line,
        poll_secs,
        refresh,
        groups,
        theme: settings.theme.menu_theme(),
    })
}

/// How often a machine waiting for a download checks back. Each check is instant: iPXE gives
/// up on a silent connection after 120 s, so a boot is never held by not answering.
const WAIT_POLL_SECS: u32 = 5;

/// Whether a PC can boot `image` yet. `Some`: the script to answer with instead. `refresh`
/// is where it should check back, if not `/boot/ipxe`.
async fn held(st: &St, mac: Mac, dev: &Device, image: &Image, base: &str, refresh: Option<String>) -> Option<String> {
    let entries = crate::catalogue::for_pc(st, image);
    if entries.is_empty() {
        return None;
    }
    let st2 = st.clone();
    let hold = tokio::task::spawn_blocking(move || crate::catalogue::hold(&st2, &entries)).await;
    match hold.unwrap_or_else(|e| Hold::Failed(e.to_string())) {
        Hold::Go => None,
        Hold::Wait(detail) => {
            st.note_waiting(mac, &image.id, &detail);
            let status = format!("Waiting to boot {}\n{detail}\nIt boots by itself once that's done.", image.title);
            Some(menu_with(st, dev, base.to_string(), status, WAIT_POLL_SECS, refresh))
        }
        Hold::Failed(why) => {
            let first = dev.waiting.is_some() || !dev.last_script.is_some_and(|t| now_ms().saturating_sub(t) < 60_000);
            st.clear_waiting(mac);
            if first {
                st.event(Some(mac), "error", format!("Can't boot {}: {why}", image.title));
            }
            Some(menu_script(st, dev, base.to_string(), format!("Can't boot {}.\n{why}", image.title), true))
        }
    }
}

/// `95 s` → `about 2 min`; under a minute, seconds.
fn human_secs(s: u64) -> String {
    if s < 60 { format!("{s} s") } else { format!("about {} min", s.div_ceil(60)) }
}

/// A machine with no usable identity: a menu without polling.
fn anonymous_menu(st: &State, base: String) -> String {
    let dev = Device { mac: "unknown".into(), ..Default::default() };
    menu_script(st, &dev, base, "BootBob couldn't tell which machine this is".into(), false)
}

async fn boot_ipxe(Ax(st): Ax<St>, headers: HeaderMap, Query(q): Query<Identity>) -> Response {
    let base = base_url(&st, &headers);
    let Some(mac) = q.mac.as_deref().and_then(|m| m.parse::<Mac>().ok()) else {
        // Worth seeing: an iPXE that isn't ours, or a broken identity query.
        st.event(
            None,
            "error",
            format!("A machine asked for a boot script without a usable MAC (mac={:?})", q.mac.as_deref().unwrap_or("")),
        );
        return ipxe(anonymous_menu(&st, base));
    };
    let polling = q.poll.is_some();
    let dev = record_identity(&st, mac, &q);
    let res = st.resolve(&dev);

    // A boot that failed comes back here; going straight back to the same image would
    // loop forever, so it gets the menu.
    let failed = q.failed.is_some();
    if res.image == "menu" || failed {
        let now = now_ms();
        let settings = st.settings();
        let recently_at_menu = dev.menu.as_ref().is_some_and(|m| now.saturating_sub(m.last_poll) < settings.poll_secs as u64 * 3000);
        st.update_device(mac, |d| match (&mut d.menu, polling || recently_at_menu) {
            (Some(m), true) => {
                m.last_poll = now;
                m.polls += 1;
            }
            _ => d.menu = Some(MenuState { since: now, last_poll: now, polls: 0 }),
        });
        // Untouched for too long: boot the local disk, as the machine would have without
        // BootBob. (A keypress stops the polling, so a menu someone is using never gets here.)
        let since = st.device(mac).and_then(|d| d.menu).map(|m| m.since).unwrap_or(now);
        let timeout_ms = settings.menu_timeout_secs as u64 * 1000;
        if polling && timeout_ms > 0 && now.saturating_sub(since) >= timeout_ms {
            st.record_boot(mac, "local", "nothing chosen at the menu in time", false);
            return ipxe(bootbob_ipxe::boot(&bootbob_ipxe::Action::LocalDisk, "Nothing chosen; booting from local disk", &base));
        }
        let then = if timeout_ms > 0 {
            let left = (settings.menu_timeout_secs as u64).saturating_sub(now.saturating_sub(since) / 1000);
            format!("\nBoots from local disk in {} unless someone chooses (a key stops the clock).", human_secs(left))
        } else {
            String::new()
        };
        let status = if failed {
            let what = dev.last_boot.as_ref().map(|b| b.title.clone()).unwrap_or_else(|| "the last image".into());
            if !polling {
                st.event(Some(mac), "failed", format!("Booting {what} failed; back at the menu"));
            }
            format!("Booting {what} failed.\nChoose something here, or reassign it in the web UI.{then}")
        } else {
            if !polling && !recently_at_menu {
                st.event(Some(mac), "menu", format!("At the fallback menu ({})", res.reason));
            }
            format!("No image assigned: choose one here, or assign one in the web UI.{then}")
        };
        let dev = st.device(mac).unwrap_or(dev);
        return ipxe(menu_script(&st, &dev, base, status, true));
    }

    if let Some(i) = st.image(&res.image)
        && let Some(script) = held(&st, mac, &dev, &i, &base, None).await
    {
        return ipxe(script);
    }
    let Some(action) = st.image(&res.image).and_then(|i| pc_action(&st, &i, mac, &base)) else {
        let why = match st.image(&res.image) {
            Some(i) if i.pi.is_some() => format!(
                "{} is for Raspberry Pis only.
Choose something here, or reassign it in the web UI.",
                i.title
            ),
            _ => format!("Image {} has nothing to boot", res.image),
        };
        return ipxe(menu_script(&st, &dev, base, why, true));
    };
    st.record_boot(mac, &res.image, &res.reason, res.once);
    let title = st.image(&res.image).map(|i| i.title).unwrap_or_default();
    ipxe(bootbob_ipxe::boot(&action, &title, &base))
}

/// BootBob's address for NBD, as a machine reaches it: the configured NBD address,
/// else the host in `base` (the URL the machine fetched its script from).
fn nbd_server(st: &State, base: &str) -> String {
    match st.config.nbd {
        Some(a) if !a.ip().is_unspecified() => a.ip().to_string(),
        _ => {
            let host = base.split("://").nth(1).unwrap_or(base);
            let host = host.split('/').next().unwrap_or(host);
            host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host).to_string()
        }
    }
}

/// What a PC does with an image: its iPXE action, or for a disk image, its kernel and
/// initrd from the image's boot partition (through this machine's copy-on-write overlay
/// when it has one) with the command line filled in. `None`: nothing for PCs.
pub(crate) fn pc_action(st: &State, img: &Image, mac: Mac, base: &str) -> Option<bootbob_ipxe::Action> {
    if let Some(a) = &img.action {
        return Some(crate::catalogue::fill(st, a, base));
    }
    let d = img.disk.as_ref()?;
    let m = mac.0;
    let dashed = format!("{:02x}-{:02x}-{:02x}-{:02x}-{:02x}-{:02x}", m[0], m[1], m[2], m[3], m[4], m[5]);
    let url = |f: &str| format!("{base}/boot/disk/{}/{}?mac={dashed}", img.id, f.trim_start_matches('/'));
    let ip = st.device(mac).and_then(|d| d.ip).unwrap_or_default();
    let cmdline = d
        .cmdline
        .replace("{server}", &crate::stores::nbd_host(&st.config, &d.source, nbd_server(st, base)))
        .replace("{disk}", crate::stores::export_name(&d.source))
        .replace("{bootbob}", base)
        .replace("{mac}", &dashed)
        .replace("{ip}", &ip);
    let initrds = if d.initrd.trim().is_empty() { vec![] } else { vec![url(d.initrd.trim())] };
    Some(bootbob_ipxe::Action::Kernel { kernel: url(d.kernel.trim()), initrds, cmdline })
}

#[derive(Deserialize)]
struct RolloutQuery {
    mac: String,
    #[serde(default)]
    health: String,
    #[serde(default)]
    busy: String,
}

/// A running machine checking in: one word back, for a shell script. Anything else it gets,
/// or nothing, means stay.
async fn boot_rollout(Ax(st): Ax<St>, Query(q): Query<RolloutQuery>) -> Response {
    let answer = match q.mac.parse::<Mac>() {
        Ok(mac) => crate::rollout::check(&st, mac, &q.health, &q.busy),
        Err(_) => crate::rollout::STAY,
    };
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], format!("{answer}\n")).into_response()
}

#[derive(Deserialize)]
struct MacQuery {
    mac: Option<String>,
}

/// A file from a PC disk image's boot partition: the kernel and initrd iPXE asks for.
/// Read through the machine's overlay when the image is copy-on-write and it has one
/// (a kernel it installed lives there).
async fn disk_file(Ax(st): Ax<St>, Path((image, path)): Path<(String, String)>, Query(q): Query<MacQuery>) -> Response {
    let Some(d) = st.image(&image).and_then(|i| i.disk) else {
        return not_found("disk image").into_response();
    };
    let Some(path) = bootbob_tftp::clean_path(&path) else { return bad("bad path").into_response() };
    let mac = q.mac.as_deref().and_then(|m| m.parse::<Mac>().ok());
    let look = move || -> std::io::Result<Option<Vec<u8>>> {
        let found = crate::stores::boot_file(&st, &d.source, d.cow, mac, &path)?;
        Ok(match found {
            Some(bootbob_pi::Found::Bytes(b)) => Some(b),
            Some(bootbob_pi::Found::Path { path, .. }) => Some(std::fs::read(path)?),
            None => None,
        })
    };
    match tokio::task::spawn_blocking(look).await {
        Ok(Ok(Some(bytes))) => ([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response(),
        Ok(Ok(None)) => not_found("file in the boot partition").into_response(),
        Ok(Err(e)) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("can't read {image}: {e}")).into_response(),
        Err(e) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// A USB boot stick that finds this server (see `usb.rs`).
async fn usb_stick(Ax(st): Ax<St>, headers: HeaderMap) -> Response {
    let url = base_url(&st, &headers);
    let dir = st.config.ipxe_dir.clone();
    match tokio::task::spawn_blocking(move || crate::usb::stick(&dir, &url)).await {
        Ok(Ok(img)) => (
            [(header::CONTENT_TYPE, "application/octet-stream"), (header::CONTENT_DISPOSITION, "attachment; filename=\"bootbob-usb.img\"")],
            img,
        )
            .into_response(),
        Ok(Err(e)) => ApiError(StatusCode::NOT_FOUND, e).into_response(),
        Err(e) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn boot_chose(Ax(st): Ax<St>, headers: HeaderMap, Query(q): Query<Identity>) -> Response {
    let base = base_url(&st, &headers);
    let Some(mac) = q.mac.as_deref().and_then(|m| m.parse::<Mac>().ok()) else {
        return ipxe(anonymous_menu(&st, base));
    };
    let dev = record_identity(&st, mac, &q);
    let item = q.item.unwrap_or_default();
    if let Some(i) = st.image(&item) {
        let again = format!("/boot/ipxe/chose?${{bootbob-id}}&item={item}");
        if let Some(script) = held(&st, mac, &dev, &i, &base, Some(again)).await {
            return ipxe(script);
        }
    }
    match st.image(&item).and_then(|i| pc_action(&st, &i, mac, &base).map(|a| (i.title, a))) {
        Some((title, action)) => {
            st.record_boot(mac, &item, "chosen at the menu", false);
            ipxe(bootbob_ipxe::boot(&action, &title, &base))
        }
        None => ipxe(menu_script(&st, &dev, base, format!("There's no {item:?} any more; choose again"), true)),
    }
}

// ---- API ----

#[derive(Serialize)]
struct StatusOut {
    version: &'static str,
    boot_filename: String,
    tftp: Option<String>,
    http: String,
    public_url: Option<String>,
    listener: crate::state::ListenerStatus,
    ipxe_builds: Vec<String>,
    devices: usize,
}

async fn status(Ax(st): Ax<St>) -> Json<StatusOut> {
    let mut builds: Vec<String> = std::fs::read_dir(&st.config.ipxe_dir)
        .map(|rd| rd.filter_map(|e| e.ok()?.file_name().into_string().ok()).filter(|n| n.starts_with("bootbob-")).collect())
        .unwrap_or_default();
    builds.sort();
    Json(StatusOut {
        version: env!("CARGO_PKG_VERSION"),
        boot_filename: st.config.boot_filename.clone(),
        tftp: st.config.tftp.map(|a| a.to_string()),
        http: st.config.http.to_string(),
        public_url: st.config.public_url.clone(),
        listener: st.listener.read().unwrap().clone(),
        ipxe_builds: builds,
        devices: st.devices().len(),
    })
}

/// A device as the UI shows it: the stored record plus what it means right now.
#[derive(Serialize)]
pub struct DeviceView {
    #[serde(flatten)]
    device: Device,
    pub(crate) code: String,
    /// `fetching`, `at_menu`, `menu_idle`, `pi_waiting`, `stuck`, `booted` or `seen`.
    pub(crate) state: &'static str,
    summary: String,
    /// What it would get if it netbooted now.
    next: Next,
}

#[derive(Serialize)]
struct Next {
    image: String,
    title: String,
    reason: String,
}

pub fn view(st: &State, d: Device) -> DeviceView {
    let now = now_ms();
    let poll_ms = st.settings().poll_secs as u64 * 1000;
    let contact = d.last_script.unwrap_or(0).max(d.tftp.as_ref().map(|t| t.at).unwrap_or(0));
    let is_pi = d.firmware.as_deref() == Some(PI_FIRMWARE);
    // A machine still asking: a Pi's retries come well inside this.
    let waiting = d.waiting.as_ref().filter(|w| now.saturating_sub(w.last) < 120_000);
    let (state, summary) = if let Some(w) = waiting {
        let title = st.image(&w.image).map(|i| i.title).unwrap_or_else(|| w.image.clone());
        ("fetching", format!("Waiting to boot {title}. {}", w.detail.replace('\n', ", ")))
    } else if is_pi && d.pi_session.is_none() && d.tftp.as_ref().is_some_and(|t| now.saturating_sub(t.at) < 300_000) {
        ("pi_waiting", "A Raspberry Pi asking to netboot, with no Pi image to give it".to_string())
    } else if let Some(m) = &d.menu {
        if now.saturating_sub(m.last_poll) <= poll_ms * 5 / 2 + 2000 {
            ("at_menu", "Waiting at the fallback menu".to_string())
        } else {
            ("menu_idle", "At the fallback menu but not checking in: someone may be at the keyboard, or it's off".to_string())
        }
    } else if let Some(dh) = d.dhcp.as_ref().filter(|dh| dh.last_discover > contact + 15_000 && now > dh.last_discover + 15_000) {
        let offer = match dh.offer.as_ref().filter(|o| o.at + 60_000 >= dh.last_discover) {
            Some(o) => format!(
                "; its DHCP offer named boot server {} and file {}",
                o.boot_server.as_deref().unwrap_or("(none)"),
                o.boot_file.as_deref().unwrap_or("(none)")
            ),
            None => String::new(),
        };
        ("stuck", format!("Tried to netboot but never reached BootBob{offer}"))
    } else if let Some(b) = &d.last_boot {
        ("booted", format!("Booted {} ({})", b.title, b.reason))
    } else {
        ("seen", "Seen on the network".to_string())
    };
    let r = if is_pi {
        st.resolve_pi(&d).unwrap_or_else(|| crate::state::Resolved {
            image: String::new(),
            reason: "no Pi image assigned, and no Raspberry Pi default".into(),
            once: false,
        })
    } else {
        st.resolve(&d)
    };
    let title = match st.image(&r.image) {
        Some(i) => i.title,
        None if r.image.is_empty() => "Nothing".into(),
        None => r.image.clone(),
    };
    DeviceView {
        code: d.mac.parse::<Mac>().map(|m| m.short()).unwrap_or_default(),
        state,
        summary,
        next: Next { image: r.image, title, reason: r.reason },
        device: d,
    }
}

async fn list_devices(Ax(st): Ax<St>) -> Json<Vec<DeviceView>> {
    let mut v: Vec<DeviceView> = st.devices().into_iter().map(|d| view(&st, d)).collect();
    v.sort_by_key(|d| std::cmp::Reverse(d.device.last_seen));
    Json(v)
}

async fn get_device(Ax(st): Ax<St>, Path(mac): Path<String>) -> Result<Json<DeviceView>, ApiError> {
    let d = st.device(parse_mac(&mac)?).ok_or_else(|| not_found("device"))?;
    Ok(Json(view(&st, d)))
}

#[derive(Deserialize)]
struct DevicePatch {
    #[serde(default, with = "double_option")]
    name: Option<Option<String>>,
    #[serde(default, with = "double_option")]
    notes: Option<Option<String>>,
}

/// `absent` → leave alone, `null` → clear, a value → set.
mod double_option {
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
        Ok(Some(Option::<String>::deserialize(d)?))
    }
}

fn tidy(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

async fn patch_device(Ax(st): Ax<St>, Path(mac): Path<String>, Json(p): Json<DevicePatch>) -> Result<Json<DeviceView>, ApiError> {
    let d = st
        .edit_device(parse_mac(&mac)?, |d| {
            if let Some(n) = p.name {
                d.name = tidy(n);
            }
            if let Some(n) = p.notes {
                d.notes = tidy(n);
            }
        })
        .ok_or_else(|| not_found("device"))?;
    Ok(Json(view(&st, d)))
}

async fn delete_device(Ax(st): Ax<St>, Path(mac): Path<String>) -> Result<StatusCode, ApiError> {
    if st.remove_device(parse_mac(&mac)?) { Ok(StatusCode::NO_CONTENT) } else { Err(not_found("device")) }
}

async fn put_assignment(Ax(st): Ax<St>, Path(mac): Path<String>, Json(a): Json<Assignment>) -> Result<Json<DeviceView>, ApiError> {
    let d = st.assign(parse_mac(&mac)?, Some(a)).map_err(bad)?;
    Ok(Json(view(&st, d)))
}

async fn delete_assignment(Ax(st): Ax<St>, Path(mac): Path<String>) -> Result<Json<DeviceView>, ApiError> {
    let mac = parse_mac(&mac)?;
    st.device(mac).ok_or_else(|| not_found("device"))?;
    let d = st.assign(mac, None).map_err(bad)?;
    Ok(Json(view(&st, d)))
}

async fn list_images(Ax(st): Ax<St>) -> Json<Vec<Image>> {
    Json(st.images())
}

async fn put_image(Ax(st): Ax<St>, Path(id): Path<String>, Json(mut img): Json<Image>) -> Result<Json<Image>, ApiError> {
    img.id = id;
    st.put_image(img.clone()).map_err(bad)?;
    Ok(Json(img))
}

async fn list_tracks(Ax(st): Ax<St>) -> Json<Vec<crate::rollout::TrackView>> {
    Json(st.tracks().into_iter().map(|t| crate::rollout::view(&st, t)).collect())
}

#[derive(Deserialize)]
struct TrackBody {
    title: String,
    /// The image a new track starts from.
    image: Option<String>,
}

async fn put_track(Ax(st): Ax<St>, Path(id): Path<String>, Json(b): Json<TrackBody>) -> Result<Json<Track>, ApiError> {
    st.put_track(&id, &b.title, b.image.as_deref()).map(Json).map_err(bad)
}

async fn delete_track(Ax(st): Ax<St>, Path(id): Path<String>) -> Result<StatusCode, ApiError> {
    st.track(&id).ok_or_else(|| not_found("track"))?;
    st.delete_track(&id).map_err(|e| ApiError(StatusCode::CONFLICT, e))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct NewVersion {
    /// A disk image, as an image's source names it: `uberfall:golden-20261108.img`.
    source: String,
    title: Option<String>,
}

async fn add_version(Ax(st): Ax<St>, Path(id): Path<String>, Json(v): Json<NewVersion>) -> Result<Json<Image>, ApiError> {
    tokio::task::spawn_blocking(move || crate::rollout::add_version(&st, &id, &v.source, v.title))
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map(Json)
        .map_err(bad)
}

#[derive(Deserialize)]
struct Release {
    image: String,
    /// Move machines now rather than at the next window.
    #[serde(default)]
    now: bool,
}

/// Makes a version current: how a rollback is done.
async fn release_version(Ax(st): Ax<St>, Path(id): Path<String>, Json(r): Json<Release>) -> Result<StatusCode, ApiError> {
    crate::rollout::release(&st, &id, &r.image, r.now).map_err(bad)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn run_track(Ax(st): Ax<St>, Path(id): Path<String>) -> Result<StatusCode, ApiError> {
    crate::rollout::run_now(&st, &id).map_err(bad)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Stops a track moving any more machines. One already rebooting carries on.
async fn halt_track(Ax(st): Ax<St>, Path(id): Path<String>) -> Result<StatusCode, ApiError> {
    st.track(&id).ok_or_else(|| not_found("track"))?;
    crate::rollout::halt(&st, &id, "stopped by hand");
    Ok(StatusCode::NO_CONTENT)
}

async fn list_overlays(Ax(st): Ax<St>, Path(mac): Path<String>) -> Result<Json<Vec<crate::nbd::OverlayInfo>>, ApiError> {
    let mac = parse_mac(&mac)?;
    Ok(Json(tokio::task::spawn_blocking(move || crate::nbd::overlays(&st, mac)).await.unwrap_or_default()))
}

async fn reset_overlay(Ax(st): Ax<St>, Path((mac, source)): Path<(String, String)>) -> Result<StatusCode, ApiError> {
    let mac = parse_mac(&mac)?;
    let r = tokio::task::spawn_blocking(move || crate::nbd::reset_overlay(&st, mac, &source))
        .await
        .unwrap_or_else(|e| Err((500, e.to_string())));
    match r {
        Ok(true) => Ok(StatusCode::NO_CONTENT),
        Ok(false) => Err(not_found("overlay")),
        Err((code, msg)) => Err(ApiError(StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), msg)),
    }
}

/// The images directory's sources, then each store's disk images (`<store>:<disk>`).
async fn pi_sources(Ax(st): Ax<St>) -> Json<Vec<bootbob_pi::SourceInfo>> {
    let config = st.config.clone();
    Json(
        tokio::task::spawn_blocking(move || {
            let mut out = bootbob_pi::scan(&config.images_dir());
            if config.mode == crate::config::Mode::Server {
                for s in &config.stores {
                    out.extend(crate::stores::sources(s));
                }
            }
            out
        })
        .await
        .unwrap_or_default(),
    )
}

#[derive(Deserialize)]
struct AllowQuery {
    store: String,
    ip: std::net::Ipv4Addr,
    export: String,
}

/// A store asking whether a machine may open one of its disks (see stores.rs).
async fn store_allow(Ax(st): Ax<St>, Query(q): Query<AllowQuery>) -> Result<Json<crate::stores::Allow>, ApiError> {
    if !st.config.stores.iter().any(|s| s.name == q.store) {
        return Err(ApiError(StatusCode::FORBIDDEN, format!("no store {:?}", q.store)));
    }
    crate::nbd::allow_for_store(&st, &q.store, q.ip, &q.export).map(Json).map_err(|e| ApiError(StatusCode::FORBIDDEN, e))
}

// ---- Store mode ----

/// An image store's API: its disk images (list, upload, create, delete), their boot
/// files, and machines' overlays. The main server and people use it; machines use its
/// NBD server.
pub fn store_router(st: St) -> Router {
    Router::new()
        .route("/api/status", get(store_status))
        .route("/api/pi/sources", get(local_sources))
        .route("/api/disks", axum::routing::post(create_disk))
        .route("/api/disks/{name}", put(upload_disk).delete(delete_disk))
        .route("/store/boot/{disk}/{*path}", get(store_boot_file))
        .route("/api/overlays/{mac}", get(store_overlays))
        .route("/api/overlays/{mac}/{disk}", axum::routing::delete(store_reset_overlay))
        .route("/api/cache", get(list_cache))
        .route("/api/cache/{name}", axum::routing::post(ensure_cache).delete(remove_cache))
        .route("/api/cache/{name}/pin", put(pin_cache))
        .route("/store/digest/{disk}", get(store_digest))
        .route("/store/disk/{disk}", get(store_disk))
        .nest_service("/files/cache", ServeDir::new(crate::cache::files_dir(&st)))
        .with_state(st)
}

#[derive(Serialize)]
struct StoreStatus {
    mode: &'static str,
    version: &'static str,
    name: Option<String>,
    main_url: Option<String>,
    nbd: Option<String>,
    disks: usize,
}

async fn store_status(Ax(st): Ax<St>) -> Json<StoreStatus> {
    let dir = st.config.images_dir();
    let disks = tokio::task::spawn_blocking(move || bootbob_pi::scan(&dir).len()).await.unwrap_or(0);
    Json(StoreStatus {
        mode: "store",
        version: env!("CARGO_PKG_VERSION"),
        name: st.config.store_name.clone(),
        main_url: st.config.main_url.clone(),
        nbd: st.config.nbd.map(|a| a.to_string()),
        disks,
    })
}

async fn local_sources(Ax(st): Ax<St>) -> Json<Vec<bootbob_pi::SourceInfo>> {
    let dir = st.config.images_dir();
    Json(tokio::task::spawn_blocking(move || bootbob_pi::scan(&dir)).await.unwrap_or_default())
}

#[derive(Deserialize)]
struct StoreBootQuery {
    mac: Option<String>,
    cow: Option<String>,
}

/// A file from one of this store's disk images' boot partitions, for the main server.
async fn store_boot_file(Ax(st): Ax<St>, Path((disk, path)): Path<(String, String)>, Query(q): Query<StoreBootQuery>) -> Response {
    if crate::nbd::check_disk_name(&disk).is_err() {
        return bad("bad disk name").into_response();
    }
    let Ok(src) = bootbob_pi::resolve_source(&st.config.images_dir(), &disk) else {
        return not_found("disk image").into_response();
    };
    let Some(path) = bootbob_tftp::clean_path(&path) else { return bad("bad path").into_response() };
    let mac = q.mac.as_deref().and_then(|m| m.parse::<Mac>().ok());
    let cow = q.cow.as_deref() == Some("1");
    let look = move || -> std::io::Result<Option<Vec<u8>>> {
        Ok(match crate::stores::local_boot_file(&st, &src, &disk, cow, mac, &path)? {
            Some(bootbob_pi::Found::Bytes(b)) => Some(b),
            Some(bootbob_pi::Found::Path { path, .. }) => Some(std::fs::read(path)?),
            None => None,
        })
    };
    match tokio::task::spawn_blocking(look).await {
        Ok(Ok(Some(bytes))) => ([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response(),
        Ok(Ok(None)) => not_found("file in the boot partition").into_response(),
        Ok(Err(e)) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        Err(e) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn store_overlays(Ax(st): Ax<St>, Path(mac): Path<String>) -> Result<Json<Vec<crate::nbd::OverlayInfo>>, ApiError> {
    let mac = parse_mac(&mac)?;
    Ok(Json(tokio::task::spawn_blocking(move || crate::nbd::local_overlays(&st, mac)).await.unwrap_or_default()))
}

async fn store_reset_overlay(Ax(st): Ax<St>, Path((mac, disk)): Path<(String, String)>) -> Result<StatusCode, ApiError> {
    let mac = parse_mac(&mac)?;
    if disk.contains(':') {
        return Err(bad("a store's own disks have no store prefix"));
    }
    match crate::nbd::reset_overlay(&st, mac, &disk) {
        Ok(true) => Ok(StatusCode::NO_CONTENT),
        Ok(false) => Err(not_found("overlay")),
        Err((code, msg)) => Err(ApiError(StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), msg)),
    }
}

#[derive(Deserialize)]
struct NewDisk {
    name: String,
    /// Bytes.
    size: u64,
}

async fn create_disk(Ax(st): Ax<St>, Json(d): Json<NewDisk>) -> Result<StatusCode, ApiError> {
    crate::nbd::create_disk(&st, &d.name, d.size).map_err(bad)?;
    Ok(StatusCode::CREATED)
}

/// Uploads a disk image (the request body, streamed to disk with its zeros as holes;
/// zstd compressed with `Content-Encoding: zstd`). Never overwrites one.
async fn upload_disk(Ax(st): Ax<St>, Path(name): Path<String>, headers: HeaderMap, body: axum::body::Body) -> Result<StatusCode, ApiError> {
    crate::nbd::check_disk_name(&name).map_err(bad)?;
    let packing = match headers.get(header::CONTENT_ENCODING).map(|v| v.as_bytes()) {
        None | Some(b"identity") => crate::sparse::Packing::Raw,
        Some(b"zstd") => crate::sparse::Packing::Zstd,
        Some(other) => {
            let other = String::from_utf8_lossy(other);
            return Err(ApiError(StatusCode::UNSUPPORTED_MEDIA_TYPE, format!("an upload can be zstd compressed, not {other}")));
        }
    };
    let dir = st.config.images_dir();
    let path = dir.join(&name);
    if path.exists() {
        return Err(ApiError(StatusCode::CONFLICT, format!("{name} already exists; delete it first")));
    }
    let tmp = dir.join(format!(".{name}.upload"));
    let src = tokio_util::io::StreamReader::new(body.into_data_stream().map(|c| c.map_err(std::io::Error::other)));
    let src = tokio_util::io::SyncIoBridge::new(src);
    let to = tmp.clone();
    let res = tokio::task::spawn_blocking(move || crate::sparse::receive(src, packing, &to)).await;
    let res = res.map_err(|e| e.to_string()).and_then(|r| r.map_err(|e| format!("can't store {name}: {e}")));
    let n = match res {
        Ok(0) => Err("the upload was empty".to_string()),
        other => other,
    }
    .inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
    .map_err(bad)?;
    if path.exists() {
        let _ = std::fs::remove_file(&tmp);
        return Err(ApiError(StatusCode::CONFLICT, format!("{name} appeared during the upload")));
    }
    std::fs::rename(&tmp, &path).map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("can't store {name}: {e}")))?;
    st.event(None, "disk", format!("Uploaded disk {name} ({} MB)", n.div_ceil(1 << 20)));
    Ok(StatusCode::CREATED)
}

/// Deletes a disk image no image uses, with any machines' overlays of it.
async fn delete_disk(Ax(st): Ax<St>, Path(name): Path<String>) -> Result<StatusCode, ApiError> {
    match crate::nbd::delete_disk(&st, &name) {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err((code, msg)) => Err(ApiError(StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), msg)),
    }
}

async fn delete_image(Ax(st): Ax<St>, Path(id): Path<String>) -> Result<StatusCode, ApiError> {
    st.delete_image(&id).map_err(bad)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- The catalogue and the cache ----

async fn list_catalogue(Ax(st): Ax<St>) -> Result<Json<crate::catalogue::Overview>, ApiError> {
    tokio::task::spawn_blocking(move || crate::catalogue::overview(&st))
        .await
        .map(Json)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// Raspberry Pi's own image list, for adding entries from.
async fn pi_os() -> Result<Json<Vec<crate::catalogue::PiOsImage>>, ApiError> {
    match tokio::task::spawn_blocking(crate::catalogue::pi_os_images).await {
        Ok(Ok(v)) => Ok(Json(v)),
        Ok(Err(e)) => Err(ApiError(StatusCode::BAD_GATEWAY, e)),
        Err(e) => Err(ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

async fn put_catalogue(
    Ax(st): Ax<St>,
    Path(id): Path<String>,
    Json(mut e): Json<crate::catalogue::Entry>,
) -> Result<Json<crate::catalogue::Entry>, ApiError> {
    e.id = id;
    e.title = e.title.trim().to_string();
    e.added_at = st.catalogue_entry(&e.id).map_or_else(now_ms, |old| old.added_at);
    crate::catalogue::validate(&st, &e).map_err(bad)?;
    if let Some(old) = st.catalogue_entry(&e.id)
        && old.store != e.store
    {
        return Err(bad("an entry can't move to another store; delete it and add it again"));
    }
    st.put_catalogue_entry(e.clone()).map_err(bad)?;
    let (st2, e2) = (st.clone(), e.clone());
    tokio::task::spawn_blocking(move || match &e2.store {
        None => {
            crate::cache::pin(&st2, &e2.id, e2.pinned);
            Ok(())
        }
        Some(name) => st2.config.stores.iter().find(|s| &s.name == name).map_or(Ok(()), |s| crate::stores::cache_pin(s, &e2.id, e2.pinned)),
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| ApiError(StatusCode::BAD_GATEWAY, e))?;
    Ok(Json(e))
}

/// Deletes an entry and its cached file. Refused while an image boots it.
async fn delete_catalogue(Ax(st): Ax<St>, Path(id): Path<String>) -> Result<StatusCode, ApiError> {
    let e = st.catalogue_entry(&id).ok_or_else(|| not_found("catalogue entry"))?;
    let users = st.catalogue_users(&e);
    if !users.is_empty() {
        return Err(ApiError(StatusCode::CONFLICT, format!("{id} is used by image(s) {}", users.join(", "))));
    }
    let st2 = st.clone();
    let removed = tokio::task::spawn_blocking(move || match &e.store {
        None => crate::cache::remove(&st2, &e.id),
        Some(name) => match st2.config.stores.iter().find(|s| &s.name == name) {
            Some(s) => crate::stores::cache_remove(s, &e.id),
            None => Ok(false),
        },
    })
    .await
    .unwrap_or_else(|e| Err((500, e.to_string())));
    removed.map_err(|(code, msg)| ApiError(StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), msg))?;
    st.delete_catalogue_entry(&id).map_err(bad)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Fetches an entry now, without a machine waiting, or tries a failed one again.
async fn fetch_catalogue(Ax(st): Ax<St>, Path(id): Path<String>) -> Result<Json<crate::cache::Status>, ApiError> {
    let e = st.catalogue_entry(&id).ok_or_else(|| not_found("catalogue entry"))?;
    tokio::task::spawn_blocking(move || crate::catalogue::ensure(&st, &e, true))
        .await
        .map(Json)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

async fn list_cache(Ax(st): Ax<St>) -> Result<Json<crate::cache::Listing>, ApiError> {
    tokio::task::spawn_blocking(move || crate::cache::list(&st))
        .await
        .map(Json)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// The main server telling this host to have a file (a store's half of `ensure`).
async fn ensure_cache(
    Ax(st): Ax<St>,
    Path(name): Path<String>,
    Json(want): Json<crate::cache::Want>,
) -> Result<Json<crate::cache::Status>, ApiError> {
    tokio::task::spawn_blocking(move || crate::cache::ensure(&st, &name, &want))
        .await
        .map(Json)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

async fn remove_cache(Ax(st): Ax<St>, Path(name): Path<String>) -> Result<StatusCode, ApiError> {
    crate::cache::check_name(&name).map_err(bad)?;
    let r = tokio::task::spawn_blocking(move || crate::cache::remove(&st, &name)).await.unwrap_or_else(|e| Err((500, e.to_string())));
    match r {
        Ok(true) => Ok(StatusCode::NO_CONTENT),
        Ok(false) => Err(not_found("cached file")),
        Err((code, msg)) => Err(ApiError(StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), msg)),
    }
}

#[derive(Deserialize)]
struct Pin {
    pinned: bool,
}

async fn pin_cache(Ax(st): Ax<St>, Path(name): Path<String>, Json(p): Json<Pin>) -> Result<StatusCode, ApiError> {
    if crate::cache::pin(&st, &name, p.pinned) { Ok(StatusCode::NO_CONTENT) } else { Err(not_found("cached file")) }
}

/// Whether `peer` is the main server: only it may read a whole disk. A disk here can be a
/// machine's root, and NBD's check (`/api/store/allow`) is what normally guards those.
fn from_main(st: &State, peer: std::net::SocketAddr) -> bool {
    let host = st.config.main_url.as_deref().and_then(|u| u.split("://").nth(1)).and_then(|h| h.split(['/', ':']).next());
    host.is_some_and(|h| h.parse::<std::net::IpAddr>().is_ok_and(|ip| ip == peer.ip().to_canonical()))
}

/// A disk this store has, that nothing is connected to: `(path)`, or the refusal.
fn whole_disk(st: &State, peer: std::net::SocketAddr, disk: &str) -> Result<std::path::PathBuf, ApiError> {
    if !from_main(st, peer) {
        return Err(ApiError(StatusCode::FORBIDDEN, "only the main server may read a whole disk".into()));
    }
    crate::nbd::check_disk_name(disk).map_err(bad)?;
    let path = st.config.images_dir().join(disk);
    if !path.is_file() {
        return Err(not_found("disk image"));
    }
    if st.nbd_live.in_use(&path) {
        return Err(ApiError(StatusCode::CONFLICT, format!("{disk} is in use over NBD")));
    }
    Ok(path)
}

/// The SHA-256 of a disk, which the main server checks its copy against (tiering).
async fn store_digest(
    Ax(st): Ax<St>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    Path(disk): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let path = whole_disk(&st, peer, &disk)?;
    match tokio::task::spawn_blocking(move || crate::fetch::sha256_file(&path)).await {
        Ok(Ok(sha256)) => Ok(Json(serde_json::json!({ "sha256": sha256 }))),
        Ok(Err(e)) => Err(ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
        Err(e) => Err(ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

/// A whole disk, with ranges, for the main server to keep a copy of (tiering).
async fn store_disk(
    Ax(st): Ax<St>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    Path(disk): Path<String>,
    req: axum::extract::Request,
) -> Result<Response, ApiError> {
    let path = whole_disk(&st, peer, &disk)?;
    let res = tower::ServiceExt::oneshot(ServeFile::new(path), req).await;
    res.map(IntoResponse::into_response).map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

async fn get_settings(Ax(st): Ax<St>) -> Json<Settings> {
    Json(st.settings())
}

async fn put_settings(Ax(st): Ax<St>, Json(s): Json<Settings>) -> Result<Json<Settings>, ApiError> {
    st.put_settings(s.clone()).map_err(bad)?;
    Ok(Json(s))
}

#[derive(Deserialize)]
struct EventQuery {
    mac: Option<String>,
    limit: Option<usize>,
}

async fn list_events(Ax(st): Ax<St>, Query(q): Query<EventQuery>) -> Result<Json<Vec<Event>>, ApiError> {
    let mac = q.mac.as_deref().map(parse_mac).transpose()?.map(|m| m.to_string());
    Ok(Json(st.events(mac.as_deref(), q.limit.unwrap_or(200).min(500))))
}

async fn stream(Ax(st): Ax<St>) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let s = BroadcastStream::new(st.subscribe()).map(|c| {
        Ok(match c {
            Ok(change) => SseEvent::default().json_data(change).unwrap_or_default(),
            // Missed some: tell the UI to refetch everything.
            Err(_) => SseEvent::default().data(r#"{"what":"resync"}"#),
        })
    });
    Sse::new(s).keep_alive(KeepAlive::default())
}

/// The last network check, or `null` before the first.
async fn last_report(Ax(st): Ax<St>) -> Json<Option<crate::doctor::Report>> {
    Json(st.report())
}

/// Runs the network check (a few seconds: it waits for DHCP offers).
async fn run_doctor(Ax(st): Ax<St>) -> Json<crate::doctor::Report> {
    Json(crate::doctor::run(&st).await)
}

async fn theme_css(Ax(st): Ax<St>) -> Response {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8"), (header::CACHE_CONTROL, "no-cache")], st.settings().theme.css()).into_response()
}
