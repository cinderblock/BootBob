use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use bootbob_dhcp::listener::Seen;
use bootbob_dhcp::message::*;
use bootbob_dhcp::{Firmware, Mac, probe};
use bootbob_tftp::{Resolution, Resolver};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::config::Config;
use crate::http::router;
use crate::model::*;
use crate::state::State;

struct T {
    st: Arc<State>,
    _dir: tempfile::TempDir,
}

fn setup() -> T {
    let dir = tempfile::tempdir().unwrap();
    let ipxe = dir.path().join("ipxe");
    std::fs::create_dir_all(&ipxe).unwrap();
    std::fs::write(ipxe.join("bootbob-efi-x64.efi"), b"EFI-X64").unwrap();
    std::fs::write(ipxe.join("bootbob-bios.kpxe"), b"BIOS").unwrap();
    let config = Config { data_dir: dir.path().join("data"), ipxe_dir: ipxe, ..Config::default() };
    T { st: Arc::new(State::open(config).unwrap()), _dir: dir }
}

const MAC: &str = "52:54:00:12:34:56";

async fn call(st: &Arc<State>, method: Method, uri: &str, body: Option<Value>) -> (StatusCode, String) {
    let mut req = Request::builder().method(method).uri(uri).header("host", "10.255.0.5:8067");
    let body = match body {
        Some(v) => {
            req = req.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let res = router(st.clone()).oneshot(req.body(body).unwrap()).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn get(st: &Arc<State>, uri: &str) -> String {
    let (s, body) = call(st, Method::GET, uri, None).await;
    assert_eq!(s, StatusCode::OK, "{uri}: {body}");
    body
}

async fn json(st: &Arc<State>, uri: &str) -> Value {
    serde_json::from_str(&get(st, uri).await).unwrap()
}

/// What iPXE sends: the MAC as `${netX/mac:hexhyp}`, text as `${…:uristring}`.
fn ident(extra: &str) -> String {
    format!(
        "/boot/ipxe?mac=52-54-00-12-34-56&uuid=0d0c0b0a-0f0e-0100-0203-040506070809&serial=To%20Be%20Filled%20By%20O.E.M.\
         &manufacturer=QEMU&product=Standard%20PC&asset=&platform=efi&arch=x86_64&ip=10.255.7.9{extra}"
    )
}

#[tokio::test]
async fn unknown_machine_gets_the_polling_menu_and_shows_up() {
    let t = setup();
    let script = get(&t.st, &ident("")).await;
    assert!(script.starts_with("#!ipxe\n"));
    assert!(script.contains("set bootbob http://10.255.0.5:8067"));
    assert!(script.contains("choose --timeout 10000"));
    assert!(script.contains("item reboot Reboot"));
    assert!(script.contains("This is 123456, QEMU Standard PC, efi-x64 at 10.255.7.9"));
    assert!(script.contains("item img_netboot_xyz"));

    let d = json(&t.st, &format!("/api/devices/{MAC}")).await;
    assert_eq!(d["state"], "at_menu");
    assert_eq!(d["firmware"], "efi-x64");
    assert_eq!(d["manufacturer"], "QEMU");
    assert_eq!(d["serial"], Value::Null, "SMBIOS filler is dropped");
    assert_eq!(d["ip"], "10.255.7.9");
    assert_eq!(d["code"], "123456");
    assert_eq!(d["next"]["image"], "menu");
    let kinds: Vec<String> = t.st.events(None, 10).into_iter().map(|e| e.kind).collect();
    assert_eq!(kinds, vec!["discovered", "menu"]);

    // Polling doesn't log a new event each time.
    get(&t.st, &ident("&poll=1")).await;
    get(&t.st, &ident("&poll=1")).await;
    assert_eq!(t.st.events(None, 10).len(), 2);
    assert_eq!(t.st.device(MAC.parse().unwrap()).unwrap().menu.unwrap().polls, 2);
}

#[tokio::test]
async fn assigning_a_waiting_machine_boots_it_on_the_next_poll() {
    let t = setup();
    get(&t.st, &ident("")).await;
    let img = json!({
        "title": "Debian installer",
        "group": "Installers",
        "action": {"kind": "kernel", "kernel": "http://x/linux", "initrds": ["http://x/initrd.gz"], "cmdline": "priority=low"}
    });
    let (s, body) = call(&t.st, Method::PUT, "/api/images/debian", Some(img)).await;
    assert_eq!(s, StatusCode::OK, "{body}");

    let (s, body) =
        call(&t.st, Method::PUT, &format!("/api/devices/{MAC}/assignment"), Some(json!({"image": "debian", "once": true}))).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["next"]["image"], "debian");
    assert_eq!(v["next"]["reason"], "one-shot assignment");

    let script = get(&t.st, &ident("&poll=1")).await;
    assert!(script.contains("kernel http://x/linux priority=low || goto failed"), "{script}");
    let d = t.st.device(MAC.parse().unwrap()).unwrap();
    assert!(d.menu.is_none());
    assert_eq!(d.last_boot.unwrap().image, "debian");
    assert!(d.assignment.is_none(), "a one-shot is used up");

    // Back to the menu on the next netboot.
    assert!(get(&t.st, &ident("")).await.contains("choose --timeout"));
}

#[tokio::test]
async fn a_failed_boot_returns_to_the_menu_instead_of_looping() {
    let t = setup();
    t.st.assign(MAC.parse().unwrap(), Some(Assignment { image: "netboot.xyz".into(), once: false, set_at: 0 })).unwrap();
    let first = get(&t.st, &ident("")).await;
    assert!(first.contains("chain --autofree https://boot.netboot.xyz || goto failed"));
    let again = get(&t.st, &ident("&failed=1")).await;
    assert!(again.contains("Booting netboot.xyz (installers and tools from the internet) failed"), "{again}");
    assert!(again.contains("choose --timeout"));
}

#[tokio::test]
async fn choices_at_the_menu_are_recorded() {
    let t = setup();
    get(&t.st, &ident("")).await;
    let script = get(&t.st, &ident("&item=reboot").replace("/boot/ipxe?", "/boot/ipxe/chose?")).await;
    assert!(script.ends_with("\nreboot\n"));
    let d = t.st.device(MAC.parse().unwrap()).unwrap();
    assert_eq!(d.last_boot.unwrap().reason, "chosen at the menu");
    assert!(d.menu.is_none());
}

#[tokio::test]
async fn no_identity_means_no_polling() {
    let t = setup();
    let s = get(&t.st, "/boot/ipxe").await;
    assert!(s.contains("choose --default refresh selected"));
    assert!(!s.contains("--timeout"));
    assert!(t.st.devices().is_empty());
}

#[tokio::test]
async fn image_and_settings_validation() {
    let t = setup();
    let (s, _) = call(&t.st, Method::DELETE, "/api/images/netboot.xyz", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = call(&t.st, Method::PUT, "/api/images/menu", Some(json!({"title": "x", "action": {"kind": "reboot"}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = call(&t.st, Method::PUT, "/api/images/bad%20id", Some(json!({"title": "x", "action": {"kind": "reboot"}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = call(&t.st, Method::PUT, "/api/images/r", Some(json!({"title": "R", "action": {"kind": "reboot"}}))).await;
    assert_eq!(s, StatusCode::OK);
    t.st.assign(MAC.parse().unwrap(), Some(Assignment { image: "r".into(), once: false, set_at: 0 })).unwrap();
    let (s, body) = call(&t.st, Method::DELETE, "/api/images/r", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "in use: {body}");
    let (s, _) = call(&t.st, Method::PUT, &format!("/api/devices/{MAC}/assignment"), Some(json!({"image": "nope"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let mut settings = json(&t.st, "/api/settings").await;
    settings["poll_secs"] = json!(1);
    let (s, _) = call(&t.st, Method::PUT, "/api/settings", Some(settings.clone())).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    settings["poll_secs"] = json!(5);
    settings["theme"]["accent"] = json!("#ff8800");
    let (s, _) = call(&t.st, Method::PUT, "/api/settings", Some(settings)).await;
    assert_eq!(s, StatusCode::OK);
    assert!(get(&t.st, "/theme.css").await.contains("--accent: #ff8800;"));
    t.st.assign(MAC.parse().unwrap(), None).unwrap();
    assert!(get(&t.st, &ident("")).await.contains("colour --rgb 0xff8800 4"));
}

#[tokio::test]
async fn naming_and_forgetting_devices() {
    let t = setup();
    get(&t.st, &ident("")).await;
    let (s, body) = call(&t.st, Method::PATCH, &format!("/api/devices/{MAC}"), Some(json!({"name": " bench-pc "}))).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert!(get(&t.st, &ident("&poll=1")).await.contains("This is bench-pc (123456)"));
    let (s, _) = call(&t.st, Method::PATCH, &format!("/api/devices/{MAC}"), Some(json!({"notes": "on the left"}))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(t.st.device(MAC.parse().unwrap()).unwrap().name.as_deref(), Some("bench-pc"), "absent field left alone");
    let (s, _) = call(&t.st, Method::DELETE, &format!("/api/devices/{MAC}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = call(&t.st, Method::GET, &format!("/api/devices/{MAC}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

fn discover(mac: Mac, fw: Firmware) -> Seen {
    Seen::Dhcp {
        src_mac: mac,
        src_ip: Ipv4Addr::UNSPECIFIED,
        dst_ip: Ipv4Addr::BROADCAST,
        from_server: false,
        msg: probe::discover(1, mac, fw),
    }
}

#[tokio::test]
async fn the_listener_finds_netbooting_machines_only() {
    let t = setup();
    let pc: Mac = "aa:bb:cc:00:00:01".parse().unwrap();
    t.st.observe(discover(pc, Firmware::Bios));
    let d = t.st.device(pc).unwrap();
    assert_eq!(d.firmware.as_deref(), Some("bios"));
    assert_eq!(d.dhcp.as_ref().unwrap().arches, vec![0]);

    // A phone's ordinary DHCP isn't recorded.
    let phone: Mac = "aa:bb:cc:00:00:02".parse().unwrap();
    let mut m = Message::request(2, phone);
    m.set_opt(OPT_MESSAGE_TYPE, [1]);
    t.st.observe(Seen::Dhcp { src_mac: phone, src_ip: Ipv4Addr::UNSPECIFIED, dst_ip: Ipv4Addr::BROADCAST, from_server: false, msg: m });
    assert!(t.st.device(phone).is_none());

    // UniFi's offer to the PC: learn its address and what it was told.
    let mut offer = Message::request(1, pc);
    offer.op = 2;
    offer.yiaddr = Ipv4Addr::new(10, 255, 7, 20);
    offer.siaddr = Ipv4Addr::new(10, 255, 0, 5);
    offer.file = b"bootbob".to_vec();
    offer.set_opt(OPT_MESSAGE_TYPE, [2]);
    offer.set_opt(OPT_SERVER_ID, [10, 255, 0, 1]);
    t.st.observe(Seen::Dhcp {
        src_mac: "f4:e2:c6:00:00:01".parse().unwrap(),
        src_ip: Ipv4Addr::new(10, 255, 0, 1),
        dst_ip: Ipv4Addr::BROADCAST,
        from_server: true,
        msg: offer,
    });
    let d = t.st.device(pc).unwrap();
    let o = d.dhcp.unwrap().offer.unwrap();
    assert_eq!(o.boot_server.as_deref(), Some("10.255.0.5"));
    assert_eq!(o.boot_file.as_deref(), Some("bootbob"));
    assert_eq!(o.server.as_deref(), Some("10.255.0.1"));
    assert_eq!(t.st.mac_for(Ipv4Addr::new(10, 255, 7, 20)), Some(pc));
    assert_eq!(t.st.listener.read().unwrap().pxe_seen, 1);
}

#[tokio::test]
async fn a_machine_that_never_reaches_us_is_flagged() {
    let t = setup();
    let pc: Mac = "aa:bb:cc:00:00:03".parse().unwrap();
    t.st.observe(discover(pc, Firmware::EfiX64));
    // Pretend the discover was a minute ago and nothing followed.
    t.st.edit_device(pc, |d| d.dhcp.as_mut().unwrap().last_discover = now_ms() - 60_000);
    let v = json(&t.st, &format!("/api/devices/{pc}")).await;
    assert_eq!(v["state"], "stuck", "{v}");
}

fn tftp_req(ip: [u8; 4], path: &str) -> bootbob_tftp::Request {
    bootbob_tftp::Request {
        peer: SocketAddr::from((ip, 2000)),
        local: "10.255.0.5:69".parse().unwrap(),
        raw_path: path.into(),
        path: bootbob_tftp::clean_path(path),
    }
}

async fn served(r: Resolution) -> Option<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    match r {
        Resolution::Serve(bootbob_tftp::Content::File { mut file, .. }) => {
            let mut v = Vec::new();
            file.read_to_end(&mut v).await.unwrap();
            Some(v)
        }
        Resolution::Serve(bootbob_tftp::Content::Memory(b)) => Some(b.to_vec()),
        _ => None,
    }
}

#[tokio::test]
async fn one_boot_filename_serves_each_firmware_its_own_ipxe() {
    let t = setup();
    let tftp = crate::tftp::Tftp { st: t.st.clone() };
    let bios: Mac = "aa:bb:cc:00:00:04".parse().unwrap();
    let uefi: Mac = "aa:bb:cc:00:00:05".parse().unwrap();
    t.st.observe(discover(bios, Firmware::Bios));
    t.st.observe(discover(uefi, Firmware::EfiX64));
    t.st.learn_address(Ipv4Addr::new(10, 255, 7, 30), bios);
    t.st.learn_address(Ipv4Addr::new(10, 255, 7, 31), uefi);

    assert_eq!(served(tftp.resolve(&tftp_req([10, 255, 7, 30], "bootbob")).await).await.unwrap(), b"BIOS");
    assert_eq!(served(tftp.resolve(&tftp_req([10, 255, 7, 31], "/bootbob")).await).await.unwrap(), b"EFI-X64");
    // Unknown machines get the default (UEFI x86-64).
    assert_eq!(served(tftp.resolve(&tftp_req([10, 255, 7, 99], "bootbob")).await).await.unwrap(), b"EFI-X64");
    assert_eq!(served(tftp.resolve(&tftp_req([10, 255, 7, 99], "ipxe/bootbob-bios.kpxe")).await).await.unwrap(), b"BIOS");
    assert!(served(tftp.resolve(&tftp_req([10, 255, 7, 30], "start4.elf")).await).await.is_none());
    assert!(served(tftp.resolve(&tftp_req([10, 255, 7, 30], "../x")).await).await.is_none());

    let d = t.st.device(bios).unwrap();
    assert_eq!(d.tftp.unwrap().path, "start4.elf");
}

#[tokio::test]
async fn everything_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config { data_dir: dir.path().to_path_buf(), ..Config::default() };
    let mac: Mac = MAC.parse().unwrap();
    {
        let st = State::open(config.clone()).unwrap();
        st.put_image(Image {
            id: "r".into(),
            title: "R".into(),
            group: String::new(),
            action: Some(bootbob_ipxe::Action::Reboot),
            pi: None,
            disk: None,
            builtin: false,
        })
        .unwrap();
        st.assign(mac, Some(Assignment { image: "r".into(), once: false, set_at: 0 })).unwrap();
        st.put_settings(Settings { poll_secs: 7, ..Settings::default() }).unwrap();
    }
    let st = State::open(config).unwrap();
    assert_eq!(st.device(mac).unwrap().assignment.unwrap().image, "r");
    assert_eq!(st.settings().poll_secs, 7);
    assert!(st.image("r").is_some_and(|i| !i.builtin));
    assert!(st.image("netboot.xyz").is_some_and(|i| i.builtin));
    assert!(st.events(None, 10).iter().any(|e| e.kind == "assigned"));
}

#[tokio::test]
async fn probe_replies_go_to_the_doctor_not_the_device_list() {
    let t = setup();
    let mac = probe::probe_mac(42);
    t.st.expect_probe(42);
    let mut offer = Message::request(42, mac);
    offer.op = 2;
    offer.siaddr = Ipv4Addr::new(10, 255, 0, 5);
    offer.file = b"bootbob".to_vec();
    offer.set_opt(OPT_MESSAGE_TYPE, [2]);
    offer.set_opt(OPT_SERVER_ID, [10, 255, 0, 1]);
    t.st.observe(Seen::Dhcp {
        src_mac: "f4:e2:c6:00:00:01".parse().unwrap(),
        src_ip: Ipv4Addr::new(10, 255, 0, 1),
        dst_ip: Ipv4Addr::BROADCAST,
        from_server: true,
        msg: offer,
    });
    let offers = t.st.take_probe(42);
    assert_eq!(offers.len(), 1);
    assert_eq!(offers[0].boot_file.as_deref(), Some("bootbob"));
    assert!(t.st.device(mac).is_none());
    assert!(t.st.take_probe(42).is_empty());
}

#[tokio::test]
async fn the_doctor_runs_without_a_listener() {
    let t = setup();
    let (s, body) = call(&t.st, Method::POST, "/api/doctor", None).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let r: Value = serde_json::from_str(&body).unwrap();
    let status = |id: &str| r["checks"].as_array().unwrap().iter().find(|c| c["id"] == id).unwrap()["status"].clone();
    assert_eq!(status("ipxe"), "warn", "arm64 build missing in the test dir");
    assert_eq!(status("listener"), "warn");
    assert_eq!(status("dhcp"), "skip");
    assert_eq!(json(&t.st, "/api/doctor").await["checks"], r["checks"]);
}

#[tokio::test]
async fn an_untouched_menu_gives_up_and_boots_the_local_disk() {
    let t = setup();
    let first = get(&t.st, &ident("")).await;
    assert!(first.contains("item --gap Boots from local disk in about 2 min unless someone chooses"), "{first}");
    // Pretend it has been polling for 2 minutes.
    let mac: Mac = MAC.parse().unwrap();
    t.st.edit_device(mac, |d| d.menu.as_mut().unwrap().since -= 121_000);
    let script = get(&t.st, &ident("&poll=1")).await;
    assert!(script.contains("sanboot --no-describe --drive 0x80 || exit"), "{script}");
    let d = t.st.device(mac).unwrap();
    assert_eq!(d.last_boot.unwrap().reason, "nothing chosen at the menu in time");
    assert!(d.menu.is_none());

    // With the timeout off, it waits forever.
    let mut s = t.st.settings();
    s.menu_timeout_secs = 0;
    t.st.put_settings(s.clone()).unwrap();
    get(&t.st, &ident("")).await;
    t.st.edit_device(mac, |d| d.menu.as_mut().unwrap().since -= 3_600_000);
    assert!(get(&t.st, &ident("&poll=1")).await.contains("choose --timeout"));
    s.menu_timeout_secs = 5;
    assert!(t.st.put_settings(s).is_err(), "shorter than two polls");
}

// ---- Raspberry Pi boot ----

const PI: &str = "dc:a6:32:01:02:03";
const PI_IP: [u8; 4] = [10, 255, 7, 40];

/// A Pi boot tree in the images directory, as a directory (disk images are covered by
/// the bootbob-pi crate's tests).
fn pi_tree(t: &T) {
    let tree = t.st.config.images_dir().join("pios");
    std::fs::create_dir_all(tree.join("overlays")).unwrap();
    std::fs::write(tree.join("start4.elf"), b"FIRMWARE").unwrap();
    std::fs::write(tree.join("config.txt"), b"arm_64bit=1").unwrap();
    std::fs::write(tree.join("cmdline.txt"), b"console=tty1 root=/dev/mmcblk0p2\n").unwrap();
    std::fs::write(tree.join("overlays/vc4-kms-v3d.dtbo"), b"DTBO").unwrap();
}

async fn put_pi_image(t: &T, id: &str, pi: Value) -> (StatusCode, String) {
    call(&t.st, Method::PUT, &format!("/api/images/{id}"), Some(json!({"title": "Pi OS", "pi": pi}))).await
}

async fn pi_file(tftp: &crate::tftp::Tftp, ip: [u8; 4], path: &str) -> Option<Vec<u8>> {
    served(tftp.resolve(&tftp_req(ip, path)).await).await
}

#[tokio::test]
async fn an_unassigned_pi_gets_nothing_and_shows_up_waiting() {
    let t = setup();
    let tftp = crate::tftp::Tftp { st: t.st.clone() };
    let pi: Mac = PI.parse().unwrap();
    t.st.learn_address(Ipv4Addr::from(PI_IP), pi);

    assert!(pi_file(&tftp, PI_IP, "6a6f4d1d/start4.elf").await.is_none());
    assert!(pi_file(&tftp, PI_IP, "start4.elf").await.is_none());

    let d = json(&t.st, &format!("/api/devices/{PI}")).await;
    assert_eq!(d["firmware"], "rpi");
    assert_eq!(d["serial"], "6a6f4d1d");
    assert_eq!(d["state"], "pi_waiting", "{d}");
    assert_eq!(d["next"]["title"], "Nothing");
    let msgs: Vec<String> = t.st.events(Some(PI), 10).into_iter().map(|e| e.message).collect();
    assert_eq!(msgs.iter().filter(|m| m.contains("no Pi image assigned")).count(), 1, "{msgs:?}");
}

#[tokio::test]
async fn an_assigned_pi_boots_its_tree_with_overrides() {
    let t = setup();
    pi_tree(&t);
    let tftp = crate::tftp::Tftp { st: t.st.clone() };
    let pi: Mac = PI.parse().unwrap();
    t.st.learn_address(Ipv4Addr::from(PI_IP), pi);

    let (s, body) = put_pi_image(
        &t,
        "pios",
        json!({
            "source": "pios",
            "cmdline": "console=tty1 root=/dev/nbd0p2 nbdroot={server}:10809/{serial} mac={mac} ip={ip}",
            "config": "dtoverlay=disable-wifi"
        }),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let (s, body) = call(&t.st, Method::PUT, &format!("/api/devices/{PI}/assignment"), Some(json!({"image": "pios", "once": true}))).await;
    assert_eq!(s, StatusCode::OK, "{body}");

    assert_eq!(pi_file(&tftp, PI_IP, "6a6f4d1d/start4.elf").await.unwrap(), b"FIRMWARE");
    // The one-shot assignment is used up as the boot starts, but the boot keeps going.
    let d = t.st.device(pi).unwrap();
    assert!(d.assignment.is_none());
    assert_eq!(d.last_boot.as_ref().unwrap().image, "pios");
    assert_eq!(d.pi_session.as_ref().unwrap().image, "pios");

    assert_eq!(
        String::from_utf8(pi_file(&tftp, PI_IP, "6a6f4d1d/cmdline.txt").await.unwrap()).unwrap(),
        "console=tty1 root=/dev/nbd0p2 nbdroot=10.255.0.5:10809/6a6f4d1d mac=dc-a6-32-01-02-03 ip=10.255.7.40\n"
    );
    assert_eq!(
        String::from_utf8(pi_file(&tftp, PI_IP, "6a6f4d1d/CONFIG.TXT").await.unwrap()).unwrap(),
        "arm_64bit=1\n\n# Added by BootBob (image pios)\n[all]\ndtoverlay=disable-wifi\n"
    );
    assert_eq!(pi_file(&tftp, PI_IP, "6a6f4d1d/overlays/vc4-kms-v3d.dtbo").await.unwrap(), b"DTBO");
    assert!(pi_file(&tftp, PI_IP, "6a6f4d1d/start.elf").await.is_none(), "optional files it doesn't have");
    // Without the prefix (the bootloader drops it when nothing is found under it).
    assert_eq!(pi_file(&tftp, PI_IP, "start4.elf").await.unwrap(), b"FIRMWARE");
    let boots = t.st.events(Some(PI), 50).into_iter().filter(|e| e.kind == "boot").count();
    assert_eq!(boots, 1, "one boot, however many files");
    assert_eq!(json(&t.st, &format!("/api/devices/{PI}")).await["state"], "booted");

    // The next boot, after a pause: the one-shot is gone, so nothing.
    t.st.edit_device(pi, |d| d.pi_session.as_mut().unwrap().last -= PI_SESSION_GAP_MS + 1);
    assert!(pi_file(&tftp, PI_IP, "6a6f4d1d/start4.elf").await.is_none());
    assert!(t.st.device(pi).unwrap().pi_session.is_none());
}

#[tokio::test]
async fn a_mac_prefix_identifies_the_pi_and_the_pi_default_applies() {
    let t = setup();
    pi_tree(&t);
    let tftp = crate::tftp::Tftp { st: t.st.clone() };
    put_pi_image(&t, "pios", json!({"source": "pios"})).await;
    let mut s = t.st.settings();
    s.pi_default_image = "pios".into();
    t.st.put_settings(s).unwrap();

    // No IP → MAC knowledge at all: the prefix says who it is.
    let ip = [10, 255, 7, 41];
    assert_eq!(pi_file(&tftp, ip, "dc-a6-32-0a-0b-0c/start4.elf").await.unwrap(), b"FIRMWARE");
    let d = t.st.device("dc:a6:32:0a:0b:0c".parse().unwrap()).unwrap();
    assert_eq!(d.last_boot.unwrap().reason, "no assignment; Raspberry Pi default");
    assert_eq!(d.serial, None);
    // The tree's own cmdline.txt when the image doesn't replace it.
    assert_eq!(pi_file(&tftp, ip, "dc-a6-32-0a-0b-0c/cmdline.txt").await.unwrap(), b"console=tty1 root=/dev/mmcblk0p2\n");
}

#[tokio::test]
async fn pi_images_are_checked_and_kept_apart_from_pcs() {
    let t = setup();
    pi_tree(&t);
    let (s, body) = put_pi_image(&t, "bad", json!({"source": "../etc"})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(body.contains("inside the images directory"), "{body}");
    let (s, body) = put_pi_image(&t, "missing", json!({"source": "nope.img"})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(body.contains("isn't in the images directory"), "{body}");
    let (s, _) = call(&t.st, Method::PUT, "/api/images/empty", Some(json!({"title": "Nothing"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, body) = put_pi_image(&t, "pios", json!({"source": "pios"})).await;
    assert_eq!(s, StatusCode::OK, "{body}");

    // A Pi-only image can't be the PC default, and only Pi images can be the Pi default.
    let mut s = t.st.settings();
    s.default_image = "pios".into();
    assert!(t.st.put_settings(s).unwrap_err().contains("Raspberry Pis only"));
    let mut s = t.st.settings();
    s.pi_default_image = "local".into();
    assert!(t.st.put_settings(s).unwrap_err().contains("Raspberry Pi boot files"));

    // A PC assigned it gets the menu, saying why, and the image isn't in the PC menu.
    get(&t.st, &ident("")).await;
    call(&t.st, Method::PUT, &format!("/api/devices/{MAC}/assignment"), Some(json!({"image": "pios"}))).await;
    let script = get(&t.st, &ident("&poll=1")).await;
    assert!(script.contains("Pi OS is for Raspberry Pis only"), "{script}");
    assert!(!script.contains("img_pios"));

    // The sources list shows the tree and what's in it.
    let v = json(&t.st, "/api/pi/sources").await;
    assert_eq!(v[0]["path"], "pios");
    assert_eq!(v[0]["kind"], "directory");
    assert_eq!(v[0]["boot_files"], json!(["start4.elf", "config.txt", "cmdline.txt"]));
}

// ---- NBD exports ----

#[tokio::test]
async fn nbd_exports_go_only_to_the_machine_assigned_the_disk() {
    use bootbob_nbd::Exports;
    let t = setup();
    let (s, body) = call(&t.st, Method::POST, "/api/disks", Some(json!({"name": "desky.img", "size": 1u64 << 30}))).await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    assert_eq!(std::fs::metadata(t.st.config.images_dir().join("desky.img")).unwrap().len(), 1 << 30);
    let (s, _) = call(&t.st, Method::POST, "/api/disks", Some(json!({"name": "desky.img", "size": 1u64 << 30}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "never overwrites");
    for bad in
        [json!({"name": "../x.img", "size": 1u64 << 30}), json!({"name": "x.img", "size": 4096}), json!({"name": "x", "size": 1u64 << 30})]
    {
        assert_eq!(call(&t.st, Method::POST, "/api/disks", Some(bad.clone())).await.0, StatusCode::BAD_REQUEST, "{bad}");
    }

    let (s, body) = put_pi_image(&t, "desky", json!({"source": "desky.img"})).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let desky: Mac = PI.parse().unwrap();
    let other: Mac = "dc:a6:32:09:09:09".parse().unwrap();
    t.st.learn_address(Ipv4Addr::from(PI_IP), desky);
    t.st.learn_address(Ipv4Addr::new(10, 255, 7, 50), other);
    t.st.assign(desky, Some(Assignment { image: "desky".into(), once: true, set_at: 0 })).unwrap();
    t.st.update_device(other, |_| {});

    let nbd = crate::nbd::Nbd { st: t.st.clone() };
    let peer = |ip: [u8; 4]| SocketAddr::from((ip, 40000));
    assert_eq!(nbd.list(peer(PI_IP)), vec!["desky.img"]);
    assert!(nbd.open("desky.img", peer(PI_IP)).is_ok());
    assert!(nbd.open("desky.img", peer([10, 255, 7, 50])).unwrap_err().contains("isn't the disk"));
    assert!(nbd.open("desky.img", peer([10, 255, 7, 99])).unwrap_err().contains("doesn't know which machine"));
    assert!(nbd.open("nope.img", peer(PI_IP)).unwrap_err().contains("no export"));

    // A one-shot assignment is used up when the boot starts; the running image still counts.
    t.st.record_boot(desky, "desky", "one-shot assignment", true);
    assert!(t.st.device(desky).unwrap().assignment.is_none());
    assert!(nbd.open("desky.img", peer(PI_IP)).is_ok());
}

#[tokio::test]
async fn a_cow_image_gives_each_pi_its_own_overlay() {
    use bootbob_cow::Disk;
    use bootbob_nbd::Exports;
    let t = setup();
    // A tiny "disk image": a bare FAT volume with a Pi boot file, the kind TFTP reads.
    let img = t.st.config.images_dir().join("golden.img");
    {
        let mut disk = std::io::Cursor::new(vec![0u8; 4 << 20]);
        fatfs::format_volume(&mut disk, fatfs::FormatVolumeOptions::new()).unwrap();
        let fs = fatfs::FileSystem::new(&mut disk, fatfs::FsOptions::new()).unwrap();
        std::io::Write::write_all(&mut fs.root_dir().create_file("start4.elf").unwrap(), b"GOLDEN").unwrap();
        drop(fs);
        std::fs::write(&img, disk.into_inner()).unwrap();
    }
    let (s, body) = put_pi_image(&t, "tree-cow", json!({"source": "pios", "cow": true})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
    pi_tree(&t);
    let (s, body) = put_pi_image(&t, "golden", json!({"source": "golden.img", "cow": true})).await;
    assert_eq!(s, StatusCode::OK, "{body}");

    let (a, b): (Mac, Mac) = (PI.parse().unwrap(), "dc:a6:32:00:00:0b".parse().unwrap());
    let (ip_a, ip_b) = (Ipv4Addr::from(PI_IP), Ipv4Addr::new(10, 255, 7, 41));
    t.st.learn_address(ip_a, a);
    t.st.learn_address(ip_b, b);
    for m in [a, b] {
        t.st.assign(m, Some(Assignment { image: "golden".into(), once: false, set_at: 0 })).unwrap();
    }
    let nbd = crate::nbd::Nbd { st: t.st.clone() };
    let peer = |ip: Ipv4Addr| SocketAddr::from((ip, 40000));
    let ex_a = nbd.open("golden.img", peer(ip_a)).unwrap();
    let ex_b = nbd.open("golden.img", peer(ip_b)).unwrap();
    assert!(ex_a.overlay.is_some() && ex_b.overlay.is_some());
    assert_ne!(ex_a.key(), ex_b.key(), "one overlay per machine");
    assert!(json(&t.st, &format!("/api/devices/{PI}/overlays")).await.as_array().unwrap().is_empty(), "nothing written yet");

    // Machine A writes through its overlay; the image and machine B are untouched.
    let before = std::fs::read(&img).unwrap();
    {
        let (dir, name) = ex_a.overlay.clone().unwrap();
        let d = bootbob_cow::Cow::open(&ex_a.path, &dir, &name, false).unwrap();
        d.write_at(b"A was here", 1 << 20).unwrap();
        d.flush().unwrap();
    }
    assert_eq!(std::fs::read(&img).unwrap(), before);
    let v = json(&t.st, &format!("/api/devices/{PI}/overlays")).await;
    assert_eq!(v[0]["source"], "golden.img");
    assert_eq!(v[0]["in_use"], false);
    assert!(v[0]["allocated"].as_u64().unwrap() > 0);

    // TFTP reads A's boot files through its overlay (here unchanged, so still golden),
    // and B's straight from the image.
    let tftp = crate::tftp::Tftp { st: t.st.clone() };
    assert_eq!(pi_file(&tftp, PI_IP, "6a6f4d1d/start4.elf").await.unwrap(), b"GOLDEN");
    assert_eq!(pi_file(&tftp, [10, 255, 7, 41], "start4.elf").await.unwrap(), b"GOLDEN");

    // Reset: refused while connected, otherwise gone.
    let (claim_id, _rx) = {
        let key = ex_a.key();
        // Stand in for a live connection.
        t.st.nbd_live.claim_for_test(key.clone())
    };
    let (s, body) = call(&t.st, Method::DELETE, &format!("/api/devices/{PI}/overlays/golden.img"), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{body}");
    t.st.nbd_live.release_for_test(&ex_a.key(), claim_id);
    let (s, _) = call(&t.st, Method::DELETE, &format!("/api/devices/{PI}/overlays/golden.img"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = call(&t.st, Method::DELETE, &format!("/api/devices/{PI}/overlays/golden.img"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(json(&t.st, &format!("/api/devices/{PI}/overlays")).await.as_array().unwrap().is_empty());
}

// ---- PC disk boot ----

/// A bare FAT "disk image" holding a kernel and initrd, the way a PC golden image's boot
/// partition does.
fn pc_disk(t: &T, name: &str) {
    let mut disk = std::io::Cursor::new(vec![0u8; 4 << 20]);
    fatfs::format_volume(&mut disk, fatfs::FormatVolumeOptions::new()).unwrap();
    {
        let fs = fatfs::FileSystem::new(&mut disk, fatfs::FsOptions::new()).unwrap();
        let root = fs.root_dir();
        std::io::Write::write_all(&mut root.create_file("vmlinuz").unwrap(), b"KERNEL").unwrap();
        std::io::Write::write_all(&mut root.create_file("initrd.img").unwrap(), b"INITRD").unwrap();
    }
    std::fs::write(t.st.config.images_dir().join(name), disk.into_inner()).unwrap();
}

#[tokio::test]
async fn a_pc_boots_a_disk_image_with_its_root_over_nbd() {
    use bootbob_nbd::Exports;
    let t = setup();
    pc_disk(&t, "debian.img");
    let img = json!({
        "title": "Debian (network root)",
        "group": "Systems",
        "disk": {"source": "debian.img", "initrd": "initrd.img", "cmdline": "root=/dev/nbd0p2 nbdroot={server},{disk},nbd0 ip=dhcp", "cow": true}
    });
    let (s, body) = call(&t.st, Method::PUT, "/api/images/debian", Some(img.clone())).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    // Both an action and a disk is ambiguous; a disk needs a disk image.
    let mut both = img.clone();
    both["action"] = json!({"kind": "reboot"});
    assert_eq!(call(&t.st, Method::PUT, "/api/images/both", Some(both)).await.0, StatusCode::BAD_REQUEST);

    // It's in the PC menu, and an assigned PC gets kernel + initrd from the image.
    let first = get(&t.st, &ident("")).await;
    assert!(first.contains("item img_debian"), "{first}");
    call(&t.st, Method::PUT, &format!("/api/devices/{MAC}/assignment"), Some(json!({"image": "debian"}))).await;
    let script = get(&t.st, &ident("&poll=1")).await;
    assert!(
        script.contains("kernel http://10.255.0.5:8067/boot/disk/debian/vmlinuz?mac=52-54-00-12-34-56 root=/dev/nbd0p2 nbdroot=10.255.0.5,debian.img,nbd0 ip=dhcp || goto failed"),
        "{script}"
    );
    assert!(script.contains("initrd http://10.255.0.5:8067/boot/disk/debian/initrd.img?mac=52-54-00-12-34-56 || goto failed"), "{script}");

    // The files themselves, straight from the image's boot partition.
    assert_eq!(get(&t.st, "/boot/disk/debian/vmlinuz?mac=52-54-00-12-34-56").await, "KERNEL");
    assert_eq!(get(&t.st, "/boot/disk/debian/initrd.img").await, "INITRD");
    assert_eq!(call(&t.st, Method::GET, "/boot/disk/debian/nope", None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&t.st, Method::GET, "/boot/disk/nope/vmlinuz", None).await.0, StatusCode::NOT_FOUND);

    // NBD: the assigned PC gets the disk, through its own overlay; nobody else does.
    let mac: Mac = MAC.parse().unwrap();
    t.st.learn_address(Ipv4Addr::new(10, 255, 7, 9), mac);
    let nbd = crate::nbd::Nbd { st: t.st.clone() };
    let ex = nbd.open("debian.img", SocketAddr::from(([10, 255, 7, 9], 40000))).unwrap();
    assert!(ex.overlay.is_some());
    let other: Mac = "52:54:00:00:00:99".parse().unwrap();
    t.st.learn_address(Ipv4Addr::new(10, 255, 7, 99), other);
    t.st.update_device(other, |_| {});
    assert!(nbd.open("debian.img", SocketAddr::from(([10, 255, 7, 99], 40000))).is_err());
}

#[tokio::test]
async fn disk_images_can_be_uploaded_and_deleted() {
    let t = setup();
    let up = |name: &str, body: &'static [u8]| {
        let st = t.st.clone();
        let uri = format!("/api/disks/{name}");
        async move {
            let req = Request::builder().method(Method::PUT).uri(uri).body(Body::from(body)).unwrap();
            router(st).oneshot(req).await.unwrap().status()
        }
    };
    assert_eq!(up("small.img", b"0123456789").await, StatusCode::CREATED);
    assert_eq!(std::fs::read(t.st.config.images_dir().join("small.img")).unwrap(), b"0123456789");
    assert_eq!(up("small.img", b"again").await, StatusCode::CONFLICT, "never overwrites");
    assert_eq!(up("bad%20name.img", b"x").await, StatusCode::BAD_REQUEST);
    assert_eq!(up("empty.img", b"").await, StatusCode::BAD_REQUEST);
    assert!(!t.st.config.images_dir().join("empty.img").exists());
    assert!(!t.st.config.images_dir().join(".empty.img.upload").exists());

    // In use by an image: kept. Unused: deleted.
    pc_disk(&t, "used.img");
    call(&t.st, Method::PUT, "/api/images/used", Some(json!({"title": "Used", "disk": {"source": "used.img"}}))).await;
    assert_eq!(call(&t.st, Method::DELETE, "/api/disks/used.img", None).await.0, StatusCode::CONFLICT);
    assert_eq!(call(&t.st, Method::DELETE, "/api/disks/small.img", None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&t.st, Method::DELETE, "/api/disks/small.img", None).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn serves_a_usb_stick_that_knows_this_server() {
    let t = setup();
    // No USB build yet: a clear 404.
    assert_eq!(call(&t.st, Method::GET, "/files/usb/bootbob-usb.img", None).await.0, StatusCode::NOT_FOUND);
    let script = include_str!("../../../../ipxe/usb.ipxe");
    let ph = script.lines().find_map(|l| l.strip_prefix("set bootbob-url ")).unwrap().trim();
    std::fs::write(t.st.config.ipxe_dir.join("bootbob-usb-efi-x64.efi"), [b"MZ-x64-".as_slice(), ph.as_bytes(), b"-end"].concat()).unwrap();

    let req = Request::builder().uri("/files/usb/bootbob-usb.img").header("host", "10.255.0.5:8067").body(Body::empty()).unwrap();
    let res = router(t.st.clone()).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let img = res.into_body().collect().await.unwrap().to_bytes().to_vec();
    assert_eq!(&img[510..512], &[0x55, 0xaa]);
    assert_eq!(img[446 + 4], 0xef, "an EFI system partition");
    let start = u32::from_le_bytes(img[454..458].try_into().unwrap()) as usize * 512;
    let mut part = std::io::Cursor::new(img[start..].to_vec());
    let fs = fatfs::FileSystem::new(&mut part, fatfs::FsOptions::new()).unwrap();
    let mut efi = Vec::new();
    std::io::Read::read_to_end(&mut fs.root_dir().open_file("EFI/BOOT/BOOTX64.EFI").unwrap(), &mut efi).unwrap();
    let efi = String::from_utf8_lossy(&efi);
    assert!(efi.starts_with("MZ-x64-http://10.255.0.5:8067 "), "{efi}");
    assert!(!efi.contains("PLACEHOLDER"));
    assert!(fs.root_dir().open_file("EFI/BOOT/BOOTAA64.EFI").is_err(), "no arm64 build here");
}
