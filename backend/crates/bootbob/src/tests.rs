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
