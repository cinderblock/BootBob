//! Walks rollouts through a night, with `tick` given the clock, so every timeout can pass.

use super::*;
use crate::rollout::{self, REBOOT, STAY, check, tick};

const A: &str = "52:54:00:00:00:0a";
const B: &str = "52:54:00:00:00:0b";
const MIN: Millis = 60_000;
const CMDLINE: &str = "root=/dev/nbd0p2 nbdroot={server},{disk},nbd0 ip=dhcp bootbob.url={bootbob}";

fn mac(s: &str) -> Mac {
    s.parse().unwrap()
}

fn on_track() -> Option<Assignment> {
    Some(Assignment { image: String::new(), once: false, set_at: 0, track: Some("golden".into()) })
}

/// A track on `golden-1.img`, a second version waiting, and machines A and B following it,
/// up and well on the first one. The window is closed until `run_now`.
async fn fleet() -> T {
    let t = setup();
    pc_disk(&t, "golden-1.img");
    pc_disk(&t, "golden-2.img");
    let img = json!({"title": "Golden", "disk": {"source": "golden-1.img", "initrd": "initrd.img", "cmdline": CMDLINE, "cow": true}});
    let (s, body) = call(&t.st, Method::PUT, "/api/images/golden-1", Some(img)).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let (s, body) = call(&t.st, Method::PUT, "/api/tracks/golden", Some(json!({"title": "Golden", "image": "golden-1"}))).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    close_window(&t);
    for m in [A, B] {
        t.st.assign(mac(m), on_track()).unwrap();
        t.st.record_boot(mac(m), "golden-1", "assigned", false);
        assert_eq!(check(&t.st, mac(m), "running", ""), STAY);
    }
    let (s, body) = call(&t.st, Method::POST, "/api/tracks/golden/versions", Some(json!({"source": "golden-2.img"}))).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    t
}

/// The daily window, as far from now as it can be.
fn close_window(t: &T) {
    let now = jiff::Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC);
    let s = Settings { rollout_start: format!("{:02}:00", (now.hour() + 12) % 24), rollout_minutes: 60, ..t.st.settings() };
    t.st.put_settings(s).unwrap();
}

fn assigned(t: &T, m: &str) -> String {
    t.st.device(mac(m)).unwrap().assignment.unwrap().image
}

fn moving(t: &T, m: &str) -> bool {
    t.st.device(mac(m)).unwrap().moving.is_some()
}

/// A machine rebooting onto what it's assigned.
fn boots(t: &T, m: &str) {
    let image = assigned(t, m);
    t.st.record_boot(mac(m), &image, "assigned", false);
}

/// Goes after `f`, so a check-in lands after a boot recorded in the same millisecond.
fn later() {
    std::thread::sleep(std::time::Duration::from_millis(3));
}

fn messages(t: &T) -> String {
    t.st.events(None, 500).into_iter().map(|e| e.message).collect::<Vec<_>>().join("\n")
}

#[test]
fn the_window_is_local_time_across_daylight_saving() {
    let s = Settings { rollout_start: "05:00".into(), rollout_minutes: 60, time_zone: "America/Los_Angeles".into(), ..Default::default() };
    let ms = |at: &str| at.parse::<jiff::Timestamp>().unwrap().as_millisecond() as Millis;
    // 5 AM PDT is 12:00Z, and 5 AM PST is 13:00Z.
    assert!(rollout::in_window(&s, ms("2026-07-01T12:30:00Z")));
    assert!(!rollout::in_window(&s, ms("2026-07-01T13:30:00Z")));
    assert!(!rollout::in_window(&s, ms("2026-12-01T12:30:00Z")));
    assert!(rollout::in_window(&s, ms("2026-12-01T13:30:00Z")));
    // The morning the clocks went back: 5 AM comes after the change.
    assert_eq!(rollout::next_window(&s, ms("2026-11-01T08:00:00Z")), Some(ms("2026-11-01T13:00:00Z")));
    assert_eq!(rollout::next_window(&s, ms("2026-11-01T13:30:00Z")), Some(ms("2026-11-02T13:00:00Z")));

    // A window running past midnight still covers the small hours.
    let late = Settings { rollout_start: "23:30".into(), rollout_minutes: 120, time_zone: "UTC".into(), ..Default::default() };
    assert!(rollout::in_window(&late, ms("2026-07-02T01:00:00Z")));
    assert!(!rollout::in_window(&late, ms("2026-07-02T01:30:00Z")));
}

#[tokio::test]
async fn window_settings_are_checked() {
    let t = setup();
    let mut s = json(&t.st, "/api/settings").await;
    assert_eq!(s["rollout_start"], "05:00");
    for (k, v) in [
        ("rollout_start", json!("25:00")),
        ("rollout_start", json!("5am")),
        ("time_zone", json!("Mars/Base")),
        ("rollout_minutes", json!(0)),
    ] {
        let mut bad = s.clone();
        bad[k] = v;
        assert_eq!(call(&t.st, Method::PUT, "/api/settings", Some(bad)).await.0, StatusCode::BAD_REQUEST);
    }
    s["time_zone"] = json!("America/Los_Angeles");
    assert_eq!(call(&t.st, Method::PUT, "/api/settings", Some(s)).await.0, StatusCode::OK);
}

#[tokio::test]
async fn an_empty_time_zone_means_the_networks() {
    let t = setup();
    assert_eq!(t.st.window_settings().time_zone, "UTC", "nothing known yet");

    t.st.set_network_zone("America/Los_Angeles".into());
    assert_eq!(t.st.window_settings().time_zone, "America/Los_Angeles");
    let mut s = json(&t.st, "/api/settings").await;
    assert_eq!(s["time_zone"], "", "the setting itself stays empty");
    assert_eq!(s["network_time_zone"], "America/Los_Angeles");

    // A setting beats the network, and what the UI sends back doesn't become one.
    s["time_zone"] = json!("Europe/Paris");
    assert_eq!(call(&t.st, Method::PUT, "/api/settings", Some(s)).await.0, StatusCode::OK);
    assert_eq!(t.st.window_settings().time_zone, "Europe/Paris");
    assert_eq!(t.st.network_zone().as_deref(), Some("America/Los_Angeles"));
}

#[tokio::test]
async fn only_disposable_roots_get_on_a_track() {
    let t = setup();
    pc_disk(&t, "pet.img");
    let image = |cmdline: &str, cow: bool| json!({"title": "x", "disk": {"source": "pet.img", "cmdline": cmdline, "cow": cow}});
    let track = |image: &str| Some(json!({"title": "T", "image": image}));

    assert_eq!(call(&t.st, Method::PUT, "/api/images/pet", Some(image(CMDLINE, false))).await.0, StatusCode::OK);
    let (s, body) = call(&t.st, Method::PUT, "/api/tracks/t", track("pet")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(body.contains("copy-on-write"), "{body}");

    // A hand-written export would boot the old root under the new kernel.
    let by_hand = CMDLINE.replace("{disk}", "pet.img");
    assert_eq!(call(&t.st, Method::PUT, "/api/images/named", Some(image(&by_hand, true))).await.0, StatusCode::OK);
    let (s, body) = call(&t.st, Method::PUT, "/api/tracks/t", track("named")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(body.contains("{disk}"), "{body}");

    let no_way_home = CMDLINE.replace(" bootbob.url={bootbob}", "");
    assert_eq!(call(&t.st, Method::PUT, "/api/images/quiet", Some(image(&no_way_home, true))).await.0, StatusCode::OK);
    let (s, body) = call(&t.st, Method::PUT, "/api/tracks/t", track("quiet")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(body.contains("{bootbob}"), "{body}");

    assert_eq!(call(&t.st, Method::PUT, "/api/tracks/t", track("netboot.xyz")).await.0, StatusCode::BAD_REQUEST);
    assert!(json(&t.st, "/api/tracks").await.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn following_a_track_is_something_a_machine_opts_into() {
    let t = fleet().await;
    // On the same image as A and B, but it never said its root is disposable.
    let pet = "52:54:00:00:00:0c";
    t.st.assign(mac(pet), Some(Assignment { image: "golden-1".into(), once: false, set_at: 0, track: None })).unwrap();
    t.st.record_boot(mac(pet), "golden-1", "assigned", false);
    later();
    check(&t.st, mac(pet), "running", "");
    rollout::run_now(&t.st, "golden").unwrap();
    for _ in 0..4 {
        tick(&t.st, now_ms());
        for m in [A, B] {
            if moving(&t, m) {
                later();
                boots(&t, m);
                later();
                check(&t.st, mac(m), "running", "");
            }
        }
    }
    assert_eq!(assigned(&t, A), "golden-2");
    assert_eq!(assigned(&t, B), "golden-2");
    assert_eq!(assigned(&t, pet), "golden-1");
    assert_eq!(check(&t.st, mac(pet), "running", ""), STAY);

    let bad = Assignment { image: "netboot.xyz".into(), once: false, set_at: 0, track: Some("golden".into()) };
    assert!(t.st.assign(mac(pet), Some(bad)).unwrap_err().contains("isn't a version"));
    let once = Assignment { image: String::new(), once: true, set_at: 0, track: Some("golden".into()) };
    assert!(t.st.assign(mac(pet), Some(once)).is_err());
    let none = Assignment { image: String::new(), once: false, set_at: 0, track: Some("nope".into()) };
    assert!(t.st.assign(mac(pet), Some(none)).unwrap_err().contains("no track"));
    assert!(t.st.assign(mac(pet), Some(Assignment { image: String::new(), once: false, set_at: 0, track: None })).is_err());
}

#[tokio::test]
async fn nothing_moves_outside_the_window() {
    let t = fleet().await;
    tick(&t.st, now_ms());
    assert!(!moving(&t, A) && !moving(&t, B));
    assert_eq!(assigned(&t, A), "golden-1", "a machine rebooting by chance stays on its version");
    assert_eq!(check(&t.st, mac(A), "running", ""), STAY);
    let v = json(&t.st, "/api/tracks").await;
    assert_eq!(v[0]["open"], false);
    assert!(v[0]["next_window"].is_u64());

    // Inside the daily window, it does.
    let mut s = t.st.settings();
    let now = jiff::Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC);
    s.rollout_start = format!("{:02}:{:02}", now.hour(), now.minute());
    t.st.put_settings(s).unwrap();
    tick(&t.st, now_ms());
    assert!(moving(&t, A));
}

#[tokio::test]
async fn one_machine_at_a_time_and_each_keeps_what_it_had() {
    use bootbob_nbd::Exports;
    let t = fleet().await;
    let ip = Ipv4Addr::new(10, 255, 7, 77);
    t.st.learn_address(ip, mac(A));
    let nbd = crate::nbd::Nbd { st: t.st.clone() };
    let peer = SocketAddr::from((ip, 40000));

    // What A has written on version 1.
    let v1 = nbd.open("golden-1.img", peer).unwrap();
    let (dir, name) = v1.overlay.clone().unwrap();
    let cow = bootbob_cow::Cow::open(&v1.path, &dir, &name, false).unwrap();
    bootbob_cow::Disk::write_at(&cow, b"A's state", 1 << 20).unwrap();
    bootbob_cow::Disk::flush(&cow).unwrap();
    drop(cow);
    let snapshot = |d: &std::path::Path| {
        let mut files: Vec<_> = std::fs::read_dir(d).unwrap().map(|e| e.unwrap().path()).collect();
        files.sort();
        files.into_iter().map(|f| (f.clone(), std::fs::read(f).unwrap())).collect::<Vec<_>>()
    };
    let before = snapshot(&dir);

    rollout::run_now(&t.st, "golden").unwrap();
    tick(&t.st, now_ms());
    assert_eq!(assigned(&t, A), "golden-2");
    assert_eq!(assigned(&t, B), "golden-1", "one at a time");
    assert!(nbd.open("golden-1.img", peer).is_ok(), "the disk it's still running from stays open to it");
    assert_eq!(check(&t.st, mac(B), "running", ""), STAY);
    assert_eq!(check(&t.st, mac(A), "running", ""), REBOOT);
    tick(&t.st, now_ms());
    assert_eq!(assigned(&t, B), "golden-1", "still A's turn");

    later();
    boots(&t, A);
    assert_eq!(check(&t.st, mac(A), "starting", ""), STAY, "booted: no more reboots");
    tick(&t.st, now_ms());
    assert_eq!(assigned(&t, B), "golden-1", "not until A is healthy");

    later();
    assert_eq!(check(&t.st, mac(A), "running", ""), STAY);
    tick(&t.st, now_ms());
    assert!(!moving(&t, A));
    assert_eq!(assigned(&t, B), "golden-2", "A proved it, so B goes");

    // A's new root is a new overlay, and its old one is exactly as it was.
    let v2 = nbd.open("golden-2.img", peer).unwrap();
    assert_ne!(v2.overlay.as_ref().unwrap().1, name);
    assert_eq!(snapshot(&dir), before);

    // Rollback: the same thing in reverse, and A has its state back.
    later();
    boots(&t, B);
    later();
    check(&t.st, mac(B), "running", "");
    tick(&t.st, now_ms());
    let (s, body) = call(&t.st, Method::POST, "/api/tracks/golden/release", Some(json!({"image": "golden-1", "now": true}))).await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{body}");
    tick(&t.st, now_ms());
    assert_eq!(assigned(&t, A), "golden-1");
    assert_eq!(assigned(&t, B), "golden-2");
    assert_eq!(check(&t.st, mac(A), "running", ""), REBOOT);
    later();
    boots(&t, A);
    later();
    check(&t.st, mac(A), "running", "");
    assert_eq!(snapshot(&dir), before, "an overlay is never reset by a rollout");
    let back = nbd.open("golden-1.img", peer).unwrap();
    let (dir, name) = back.overlay.clone().unwrap();
    let cow = bootbob_cow::Cow::open(&back.path, &dir, &name, true).unwrap();
    let mut buf = [0u8; 9];
    bootbob_cow::Disk::read_at(&cow, &mut buf, 1 << 20).unwrap();
    assert_eq!(&buf, b"A's state");
}

#[tokio::test]
async fn a_busy_machine_is_left_alone() {
    let t = fleet().await;
    check(&t.st, mac(A), "running", "login");
    rollout::run_now(&t.st, "golden").unwrap();
    tick(&t.st, now_ms());
    assert!(!moving(&t, A), "busy");
    assert!(moving(&t, B), "the next one doesn't wait for it");

    // Busy between being picked and hearing about it: it's never told to reboot.
    assert_eq!(check(&t.st, mac(B), "running", "apt"), STAY);
    later();
    tick(&t.st, now_ms());
    assert!(!moving(&t, B));
    assert_eq!(assigned(&t, B), "golden-1", "back where it was");
    assert!(messages(&t).contains("became busy"));

    // Idle again, it takes its turn in the same window.
    later();
    check(&t.st, mac(B), "running", "");
    tick(&t.st, now_ms());
    assert!(moving(&t, A) || moving(&t, B));
}

#[tokio::test]
async fn a_machine_that_never_reboots_goes_back_and_waits_for_the_next_window() {
    let t = fleet().await;
    check(&t.st, mac(A), "running", "login");
    rollout::run_now(&t.st, "golden").unwrap();
    let start = now_ms();
    tick(&t.st, start);
    assert!(moving(&t, B));
    tick(&t.st, start + 9 * MIN);
    assert!(moving(&t, B));
    tick(&t.st, start + 11 * MIN);
    assert!(!moving(&t, B));
    assert_eq!(assigned(&t, B), "golden-1", "a power cycle can't land it on the new version unseen");
    assert!(t.st.track("golden").unwrap().halted.is_none(), "it hasn't failed: nothing happened to it");
    assert!(t.st.device(mac(B)).unwrap().not_before.unwrap() > start + 11 * MIN);
    assert_eq!(check(&t.st, mac(B), "running", ""), STAY, "not told again");
}

#[tokio::test]
async fn a_machine_that_comes_back_unhealthy_halts_the_track_and_goes_back() {
    let t = fleet().await;
    rollout::run_now(&t.st, "golden").unwrap();
    let start = now_ms();
    tick(&t.st, start);
    assert!(moving(&t, A));
    later();
    boots(&t, A);
    later();
    assert_eq!(check(&t.st, mac(A), "degraded", ""), STAY);
    tick(&t.st, now_ms());
    assert!(t.st.track("golden").unwrap().halted.is_none(), "it gets its time");

    tick(&t.st, now_ms() + 16 * MIN);
    let halted = t.st.track("golden").unwrap().halted.unwrap();
    assert!(halted.why.contains("degraded"), "{}", halted.why);
    assert_eq!(assigned(&t, A), "golden-1", "back on the version it came from");
    assert_eq!(assigned(&t, B), "golden-1", "and the next one never went");
    assert_eq!(check(&t.st, mac(A), "degraded", ""), REBOOT, "it's alive, so it can take itself back");

    // That tick ran 16 minutes ahead of the clock the boot is recorded on.
    t.st.update_device(mac(A), |d| d.moving.as_mut().unwrap().started = now_ms());
    later();
    boots(&t, A);
    later();
    check(&t.st, mac(A), "running", "");
    tick(&t.st, now_ms());
    assert!(!moving(&t, A));
    assert!(messages(&t).contains("Back on golden-1, healthy"));
    tick(&t.st, now_ms() + 30 * MIN);
    assert!(!moving(&t, A) && !moving(&t, B), "halted until a person says otherwise");

    // A person says otherwise.
    assert_eq!(call(&t.st, Method::POST, "/api/tracks/golden/run", None).await.0, StatusCode::NO_CONTENT);
    tick(&t.st, now_ms());
    assert!(moving(&t, A) || moving(&t, B));
}

#[tokio::test]
async fn a_machine_that_never_comes_back_is_reported_not_retried() {
    let t = fleet().await;
    rollout::run_now(&t.st, "golden").unwrap();
    tick(&t.st, now_ms());
    later();
    boots(&t, A);
    let later_on = now_ms() + 16 * MIN;
    tick(&t.st, later_on);
    assert!(t.st.track("golden").unwrap().halted.unwrap().why.contains("said nothing"));
    assert_eq!(assigned(&t, A), "golden-1", "its next boot is the old version");
    tick(&t.st, later_on + 11 * MIN);
    assert!(!moving(&t, A));
    assert!(messages(&t).contains("needs a power cycle"));
}

#[tokio::test]
async fn a_healthy_report_from_before_the_reboot_proves_nothing() {
    let t = fleet().await;
    rollout::run_now(&t.st, "golden").unwrap();
    tick(&t.st, now_ms());
    check(&t.st, mac(A), "running", "");
    later();
    boots(&t, A);
    tick(&t.st, now_ms());
    assert!(moving(&t, A));
}

#[tokio::test]
async fn a_machine_nobody_can_hear_gets_the_version_when_it_next_boots() {
    let t = fleet().await;
    rollout::run_now(&t.st, "golden").unwrap();
    let hours_later = now_ms() + 60 * MIN;
    t.st.edit_track("golden", |t| t.open_until = Some(hours_later + MIN));
    tick(&t.st, hours_later);
    assert_eq!(assigned(&t, A), "golden-2");
    assert_eq!(assigned(&t, B), "golden-2");
    assert!(!moving(&t, A) && !moving(&t, B), "there's nothing to wait for");
}

#[tokio::test]
async fn no_machine_reboots_into_a_disk_that_isnt_there() {
    let t = fleet().await;
    std::fs::remove_file(t.st.config.images_dir().join("golden-2.img")).unwrap();
    rollout::run_now(&t.st, "golden").unwrap();
    tick(&t.st, now_ms());
    tick(&t.st, now_ms());
    assert!(!moving(&t, A) && !moving(&t, B));
    assert_eq!(assigned(&t, A), "golden-1");
    assert!(t.st.track("golden").unwrap().blocked.is_some());
    assert_eq!(messages(&t).matches("can't start").count(), 1, "said once");

    pc_disk(&t, "golden-2.img");
    tick(&t.st, now_ms());
    assert!(moving(&t, A) || moving(&t, B));
    assert!(t.st.track("golden").unwrap().blocked.is_none());
}

#[tokio::test]
async fn a_person_assigning_a_machine_ends_its_move() {
    let t = fleet().await;
    rollout::run_now(&t.st, "golden").unwrap();
    tick(&t.st, now_ms());
    t.st.assign(mac(A), Some(Assignment { image: "netboot.xyz".into(), once: false, set_at: 0, track: None })).unwrap();
    assert!(!moving(&t, A));
    assert_eq!(check(&t.st, mac(A), "running", ""), STAY);
    tick(&t.st, now_ms());
    assert_eq!(assigned(&t, A), "netboot.xyz", "it doesn't follow the track any more");
}

#[tokio::test]
async fn stopping_by_hand_stops_the_next_machine_not_the_one_rebooting() {
    let t = fleet().await;
    rollout::run_now(&t.st, "golden").unwrap();
    tick(&t.st, now_ms());
    assert_eq!(call(&t.st, Method::POST, "/api/tracks/golden/halt", None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(check(&t.st, mac(A), "running", ""), REBOOT);
    later();
    boots(&t, A);
    later();
    check(&t.st, mac(A), "running", "");
    tick(&t.st, now_ms());
    assert!(!moving(&t, A));
    assert_eq!(assigned(&t, B), "golden-1");
}

#[tokio::test]
async fn versions_stay_while_anything_could_need_them() {
    let t = fleet().await;
    let delete = |id: &'static str| {
        let st = t.st.clone();
        async move { call(&st, Method::DELETE, &format!("/api/images/{id}"), None).await.0 }
    };
    assert_eq!(delete("golden-2").await, StatusCode::BAD_REQUEST, "current");
    assert_eq!(delete("golden-1").await, StatusCode::BAD_REQUEST, "assigned");

    rollout::run_now(&t.st, "golden").unwrap();
    for _ in 0..3 {
        tick(&t.st, now_ms());
        for m in [A, B] {
            if moving(&t, m) {
                later();
                boots(&t, m);
                later();
                check(&t.st, mac(m), "running", "");
            }
        }
    }
    assert_eq!(assigned(&t, B), "golden-2");
    // Nobody's on version 1 now, and that's when it can go.
    assert_eq!(delete("golden-1").await, StatusCode::NO_CONTENT);
    assert_eq!(t.st.track("golden").unwrap().versions.len(), 1);

    let (s, _) = call(&t.st, Method::DELETE, "/api/tracks/golden", None).await;
    assert_eq!(s, StatusCode::CONFLICT, "machines follow it");
}

#[tokio::test]
async fn the_version_a_machine_is_leaving_stays_until_it_has_left() {
    let t = fleet().await;
    t.st.remove_device(mac(B));
    rollout::run_now(&t.st, "golden").unwrap();
    tick(&t.st, now_ms());
    // Assigned the new version, but its root is still on the old disk.
    assert_eq!(assigned(&t, A), "golden-2");
    let (s, body) = call(&t.st, Method::DELETE, "/api/images/golden-1", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("running"), "{body}");
    later();
    boots(&t, A);
    later();
    check(&t.st, mac(A), "running", "");
    tick(&t.st, now_ms());
    assert_eq!(call(&t.st, Method::DELETE, "/api/images/golden-1", None).await.0, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn a_version_is_a_new_disk_that_a_machine_can_boot() {
    let t = fleet().await;
    let post = |source: &str| call(&t.st, Method::POST, "/api/tracks/golden/versions", Some(json!({"source": source})));
    let (s, body) = post("golden-2.img").await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(body.contains("never changes"), "{body}");
    assert_eq!(post("nothing-here.img").await.0, StatusCode::BAD_REQUEST);
    assert_eq!(post("not-a-disk").await.0, StatusCode::BAD_REQUEST);

    // A disk with no kernel on it would strand every machine that rebooted into it.
    let empty = {
        let mut disk = std::io::Cursor::new(vec![0u8; 4 << 20]);
        fatfs::format_volume(&mut disk, fatfs::FormatVolumeOptions::new()).unwrap();
        disk.into_inner()
    };
    std::fs::write(t.st.config.images_dir().join("empty.img"), empty).unwrap();
    assert_eq!(post("empty.img").await.0, StatusCode::BAD_REQUEST);
    assert!(t.st.image("empty").is_none());
    assert_eq!(t.st.track("golden").unwrap().current, "golden-2");

    let v = t.st.image("golden-2").unwrap();
    assert_eq!(v.disk.as_ref().unwrap().source, "golden-2.img");
    assert_eq!(v.disk.as_ref().unwrap().cmdline, CMDLINE, "the rest is copied");
}

#[tokio::test]
async fn a_rollout_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config { data_dir: dir.path().join("data"), ..Config::default() };
    let st = Arc::new(State::open(config.clone()).unwrap());
    std::fs::create_dir_all(config.images_dir()).unwrap();
    let t = T { st, _dir: dir };
    pc_disk(&t, "golden-1.img");
    let img = json!({"title": "Golden", "disk": {"source": "golden-1.img", "cmdline": CMDLINE, "cow": true}});
    assert_eq!(call(&t.st, Method::PUT, "/api/images/golden-1", Some(img)).await.0, StatusCode::OK);
    t.st.put_track("golden", "Golden", Some("golden-1")).unwrap();
    t.st.assign(mac(A), on_track()).unwrap();
    let T { st, _dir } = t;
    drop(st);
    let st = State::open(config).unwrap();
    assert_eq!(st.track("golden").unwrap().current, "golden-1");
    assert_eq!(st.device(mac(A)).unwrap().assignment.unwrap().track.as_deref(), Some("golden"));
}

#[tokio::test]
async fn pis_get_the_disk_and_the_way_home_in_their_command_line() {
    let t = setup();
    let tftp = crate::tftp::Tftp { st: t.st.clone() };
    t.st.learn_address(Ipv4Addr::from(PI_IP), PI.parse().unwrap());
    pc_disk(&t, "golden-1.img");
    let pi = json!({"source": "golden-1.img", "cow": true, "cmdline": "nbdroot={server},{disk},nbd0 bootbob.url={bootbob}"});
    let (s, body) = put_pi_image(&t, "golden-1", pi).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    t.st.assign(PI.parse().unwrap(), Some(Assignment { image: "golden-1".into(), once: false, set_at: 0, track: None })).unwrap();
    let cmdline = String::from_utf8(pi_file(&tftp, PI_IP, "cmdline.txt").await.unwrap()).unwrap();
    assert_eq!(cmdline, "nbdroot=10.255.0.5,golden-1.img,nbd0 bootbob.url=http://10.255.0.5:8067\n");
}

#[tokio::test]
async fn pcs_get_the_way_home_in_their_command_line() {
    let t = fleet().await;
    t.st.record_boot(mac(A), "golden-1", "assigned", false);
    let script = get(&t.st, "/boot/ipxe?mac=52-54-00-00-00-0a&ip=10.255.7.9&platform=efi&arch=x86_64").await;
    assert!(script.contains("nbdroot=10.255.0.5,golden-1.img,nbd0"), "{script}");
    assert!(script.contains("bootbob.url=http://10.255.0.5:8067"), "{script}");
}

#[tokio::test]
async fn the_check_in_is_one_word_and_a_stranger_stays() {
    let t = fleet().await;
    let word = |q: &'static str| {
        let st = t.st.clone();
        async move { get(&st, &format!("/boot/rollout?{q}")).await }
    };
    assert_eq!(word("mac=nonsense").await, "stay\n");
    assert_eq!(word("mac=52-54-00-99-99-99&health=running").await, "stay\n");
    assert!(t.st.device(mac("52:54:00:99:99:99")).is_none(), "checking in doesn't make a machine");
    rollout::run_now(&t.st, "golden").unwrap();
    tick(&t.st, now_ms());
    assert_eq!(word("mac=52-54-00-00-00-0a&health=running").await, "reboot\n");
    assert_eq!(word("mac=52-54-00-00-00-0a&health=running&busy=login").await, "stay\n");
    let d = t.st.device(mac(A)).unwrap().check.unwrap();
    assert_eq!((d.health.as_str(), d.busy.as_str()), ("running", "login"));
}
