//! Boot interfaces and admission: who a machine is comes from the interface it asked on,
//! and a machine that isn't admitted there is held at the menu with public images only.

use bootbob_nbd::Exports;

use super::*;

/// The untrusted VLAN in these tests, beside [`IF`].
const CI: &str = "eth0.5";
const DESKY_IP: Ipv4Addr = Ipv4Addr::new(10, 255, 7, 40);
const CI_IP: Ipv4Addr = Ipv4Addr::new(10, 5, 0, 40);

/// A server with the LAN ([`IF`], 10.255.0.5) and the CI VLAN ([`CI`], 10.5.0.5).
fn two_networks() -> T {
    let dir = tempfile::tempdir().unwrap();
    let ipxe = dir.path().join("ipxe");
    std::fs::create_dir_all(&ipxe).unwrap();
    std::fs::write(ipxe.join("bootbob-efi-x64.efi"), b"EFI-X64").unwrap();
    let mut boot = boot();
    boot.push(BootIface { label: Some("Hardware CI".into()), ..BootIface::new(CI, Some(Ipv4Addr::new(10, 5, 0, 5))) });
    let config = Config { data_dir: dir.path().join("data"), ipxe_dir: ipxe, boot, ..Config::default() };
    T { st: Arc::new(State::open(config).unwrap()), _dir: dir }
}

/// A GET on boot interface `iface`'s routes, from `peer`, with nothing learnt for it: the
/// interface's neighbour table is whatever the test set up.
async fn get_on(st: &Arc<State>, iface: &str, peer: Ipv4Addr, uri: &str) -> (StatusCode, String) {
    let mut req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    req.extensions_mut().insert(axum::extract::ConnectInfo(SocketAddr::from((peer, 40000))));
    let res = crate::http::boot_router(st.clone(), iface).oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn ipxe_query(mac: &str, extra: &str) -> String {
    format!("/boot/ipxe?mac={mac}&platform=efi&arch=x86_64{extra}")
}

/// A PC disk image `<name>.img` and an image of it, `public` or not, listed in the menu.
async fn disk_image(t: &T, name: &str, public: bool) {
    pc_disk(t, &format!("{name}.img"));
    let (s, body) = call(
        &t.st,
        Method::PUT,
        &format!("/api/images/{name}"),
        Some(
            json!({"title": name, "group": "Systems", "public": public, "disk": {"source": format!("{name}.img"), "initrd": "initrd.img"}}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn a_machine_that_isnt_admitted_is_held_with_public_images_only() {
    let t = setup();
    disk_image(&t, "secret", false).await;
    disk_image(&t, "rescue", true).await;
    // Even the site default: it isn't admitted.
    let mut s = t.st.settings();
    s.default_image = "secret".into();
    t.st.put_settings(s).unwrap();

    let menu = get(&t.st, &ident("")).await;
    assert!(menu.contains("isn't admitted on eth0"), "{menu}");
    assert!(menu.contains("rescue") && !menu.contains("secret"), "{menu}");
    assert!(menu.contains("choose --timeout"), "it keeps checking back");
    let mac: Mac = MAC.parse().unwrap();
    assert!(t.st.device(mac).unwrap().admitted.is_empty());
    assert!(t.st.events(Some(MAC), 10).iter().any(|e| e.kind == "menu" && e.message.contains("not admitted on eth0")));

    // Someone at the keyboard can't pick what the menu didn't list, but can pick a public one.
    let refused = get(&t.st, &format!("{}&item=secret", ident("").replace("/boot/ipxe?", "/boot/ipxe/chose?"))).await;
    assert!(refused.contains("There's no \"secret\" here"), "{refused}");
    let (s, _) = call(&t.st, Method::GET, "/boot/disk/secret/vmlinuz?mac=52-54-00-12-34-56&ip=10.255.7.9", None).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "nor fetch its kernel");
    let chosen = get(&t.st, &format!("{}&item=rescue", ident("").replace("/boot/ipxe?", "/boot/ipxe/chose?"))).await;
    assert!(chosen.contains("/boot/disk/rescue/vmlinuz"), "{chosen}");
    assert_eq!(get(&t.st, "/boot/disk/rescue/vmlinuz?mac=52-54-00-12-34-56&ip=10.255.7.9").await, "KERNEL");

    // Admitted: the default applies, and the menu would list everything.
    let (s, body) = call(&t.st, Method::PUT, &format!("/api/devices/{MAC}/admitted/{IF}"), None).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let script = get(&t.st, &ident("")).await;
    assert!(script.contains("/boot/disk/secret/vmlinuz"), "{script}");
    assert_eq!(call(&t.st, Method::PUT, &format!("/api/devices/{MAC}/admitted/nope0"), None).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn assigning_a_machine_admits_it_where_it_was_seen() {
    let t = two_networks();
    let mac: Mac = MAC.parse().unwrap();
    // Never seen: the first boot interface.
    t.st.assign(mac, Some(Assignment { image: "local".into(), once: false, set_at: 0, track: None })).unwrap();
    assert_eq!(t.st.device(mac).unwrap().admitted, vec![IF.to_string()]);

    // Seen on the CI VLAN, then assigned: admitted there too.
    let ci_box: Mac = "dc:a6:32:0c:0c:0c".parse().unwrap();
    t.st.learn_address(CI, CI_IP, ci_box);
    let (_, menu) = get_on(&t.st, CI, CI_IP, &ipxe_query("dc-a6-32-0c-0c-0c", "")).await;
    assert!(menu.contains("isn't admitted on Hardware CI"), "{menu}");
    t.st.assign(ci_box, Some(Assignment { image: "local".into(), once: false, set_at: 0, track: None })).unwrap();
    assert_eq!(t.st.device(ci_box).unwrap().admitted, vec![CI.to_string()]);
    let (_, script) = get_on(&t.st, CI, CI_IP, &ipxe_query("dc-a6-32-0c-0c-0c", "")).await;
    assert!(script.contains("sanboot") || script.contains("exit"), "it boots what it's assigned: {script}");
}

#[tokio::test]
async fn a_machine_claiming_another_mac_is_refused() {
    let t = two_networks();
    pc_disk(&t, "desky.img");
    call(&t.st, Method::PUT, "/api/images/desky", Some(json!({"title": "Desky", "disk": {"source": "desky.img"}}))).await;
    let desky: Mac = PI.parse().unwrap();
    t.st.learn_address(IF, DESKY_IP, desky);
    t.st.assign(desky, Some(Assignment { image: "desky".into(), once: true, set_at: 0, track: None })).unwrap();

    // On the same network, something else naming desky in its query: the neighbour table
    // says who it is.
    let impostor: Mac = "02:00:00:00:00:66".parse().unwrap();
    let impostor_ip = Ipv4Addr::new(10, 255, 7, 66);
    t.st.learn_address(IF, impostor_ip, impostor);
    let (_, script) = get_on(&t.st, IF, impostor_ip, &ipxe_query("dc-a6-32-01-02-03", "")).await;
    assert!(script.contains("couldn't tell which machine"), "{script}");
    assert!(t.st.events(None, 20).iter().any(|e| e.kind == "refused" && e.message.contains("said it was dc:a6:32:01:02:03")));
    let d = t.st.device(desky).unwrap();
    assert!(d.assignment.is_some() && d.last_boot.is_none(), "its one-shot assignment is untouched");

    // An address nobody on the interface has: nothing.
    let (_, script) = get_on(&t.st, IF, Ipv4Addr::new(10, 255, 7, 77), &ipxe_query("dc-a6-32-01-02-03", "")).await;
    assert!(script.contains("couldn't tell which machine"), "{script}");

    // The check-in: a stranger can't make desky reboot or count as healthy.
    let (_, answer) = get_on(&t.st, IF, impostor_ip, "/boot/rollout?mac=dc:a6:32:01:02:03&health=running").await;
    assert_eq!(answer.trim(), crate::rollout::STAY);
    assert!(t.st.device(desky).unwrap().check.is_none());
}

#[tokio::test]
async fn a_machine_admitted_elsewhere_is_a_stranger_here() {
    let t = two_networks();
    pi_tree(&t);
    let (s, body) = call(&t.st, Method::POST, "/api/disks", Some(json!({"name": "desky.img", "size": 1u64 << 30}))).await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    put_pi_image(&t, "pios", json!({"source": "pios"})).await;
    put_pi_image(&t, "desky", json!({"source": "desky.img"})).await;
    let desky: Mac = PI.parse().unwrap();
    t.st.learn_address(IF, DESKY_IP, desky);
    t.st.assign(desky, Some(Assignment { image: "desky".into(), once: false, set_at: 0, track: None })).unwrap();
    let lan_nbd = crate::nbd::Nbd { st: t.st.clone(), iface: IF.into() };
    assert!(lan_nbd.open("desky.img", SocketAddr::from((DESKY_IP, 40000))).is_ok());

    // On the CI VLAN, something with desky's MAC: it really is that MAC on that network, but
    // desky isn't admitted there.
    t.st.learn_address(CI, CI_IP, desky);
    let ci_nbd = crate::nbd::Nbd { st: t.st.clone(), iface: CI.into() };
    assert!(ci_nbd.open("desky.img", SocketAddr::from((CI_IP, 40000))).unwrap_err().contains("isn't the disk"));
    assert!(ci_nbd.list(SocketAddr::from((CI_IP, 40000))).is_empty());
    let ci_tftp = crate::tftp::Tftp { st: t.st.clone(), iface: CI.into() };
    assert!(pi_file(&ci_tftp, CI_IP.octets(), "start4.elf").await.is_none());
    assert!(pi_file(&ci_tftp, CI_IP.octets(), "dc-a6-32-01-02-03/start4.elf").await.is_none());
    let (_, menu) = get_on(&t.st, CI, CI_IP, &ipxe_query("dc-a6-32-01-02-03", "")).await;
    assert!(menu.contains("isn't admitted on Hardware CI"), "{menu}");
    let d = t.st.device(desky).unwrap();
    assert_ne!(d.ip.as_deref(), Some("10.5.0.40"), "what's seen on the VLAN doesn't move it");
    assert_eq!(d.seen[CI].ip.as_deref(), Some("10.5.0.40"));
    assert!(
        t.st.events(Some(PI), 20).iter().any(|e| e.kind == "unadmitted" && e.message.contains("it's admitted on eth0")),
        "{:?}",
        t.st.events(Some(PI), 20)
    );

    // Still fine on the LAN.
    let lan_tftp = crate::tftp::Tftp { st: t.st.clone(), iface: IF.into() };
    assert!(lan_nbd.open("desky.img", SocketAddr::from((DESKY_IP, 40000))).is_ok());
    let _ = lan_tftp;
}

#[tokio::test]
async fn a_pi_naming_another_pi_gets_nothing() {
    let t = setup();
    pi_tree(&t);
    put_pi_image(&t, "pios", json!({"source": "pios"})).await;
    let desky: Mac = PI.parse().unwrap();
    t.st.assign(desky, Some(Assignment { image: "pios".into(), once: false, set_at: 0, track: None })).unwrap();
    let tftp = crate::tftp::Tftp { st: t.st.clone(), iface: IF.into() };
    let other: Mac = "dc:a6:32:09:09:09".parse().unwrap();
    t.st.learn_address(IF, Ipv4Addr::new(10, 255, 7, 50), other);
    assert!(pi_file(&tftp, [10, 255, 7, 50], "dc-a6-32-01-02-03/start4.elf").await.is_none());
    assert!(t.st.events(None, 20).iter().any(|e| e.kind == "refused"));
    // Desky itself, under its own prefix, is served.
    t.st.learn_address(IF, DESKY_IP, desky);
    assert_eq!(pi_file(&tftp, DESKY_IP.octets(), "dc-a6-32-01-02-03/start4.elf").await.as_deref(), Some(&b"FIRMWARE"[..]));
}

#[tokio::test]
async fn an_unadmitted_pi_gets_nothing_even_with_a_pi_default() {
    let t = setup();
    pi_tree(&t);
    put_pi_image(&t, "pios", json!({"source": "pios"})).await;
    let mut s = t.st.settings();
    s.pi_default_image = "pios".into();
    t.st.put_settings(s).unwrap();
    let tftp = crate::tftp::Tftp { st: t.st.clone(), iface: IF.into() };
    let pi: Mac = PI.parse().unwrap();
    t.st.learn_address(IF, Ipv4Addr::from(PI_IP), pi);
    assert!(pi_file(&tftp, PI_IP, "start4.elf").await.is_none());
    let d = t.st.device(pi).unwrap();
    assert_eq!(d.firmware.as_deref(), Some(crate::tftp::PI_FIRMWARE));
    t.st.set_admitted(pi, IF, true).unwrap();
    assert_eq!(pi_file(&tftp, PI_IP, "start4.elf").await.as_deref(), Some(&b"FIRMWARE"[..]));
}

#[tokio::test]
async fn machines_known_before_admission_are_admitted_on_the_first_boot_interface() {
    let dir = tempfile::tempdir().unwrap();
    let mac: Mac = MAC.parse().unwrap();
    {
        // Before: no boot interfaces, so no admission yet.
        let st = State::open(Config { data_dir: dir.path().to_path_buf(), ..Config::default() }).unwrap();
        st.update_device(mac, |d| d.name = Some("desky".into()));
    }
    let mut both = boot();
    both.push(BootIface::new(CI, Some(Ipv4Addr::new(10, 5, 0, 5))));
    let st = State::open(Config { data_dir: dir.path().to_path_buf(), boot: both.clone(), ..Config::default() }).unwrap();
    assert_eq!(st.device(mac).unwrap().admitted, vec![IF.to_string()]);
    st.set_admitted(mac, IF, false).unwrap();
    drop(st);
    // Only once: taking it away sticks.
    let st = State::open(Config { data_dir: dir.path().to_path_buf(), boot: both, ..Config::default() }).unwrap();
    assert!(st.device(mac).unwrap().admitted.is_empty());
}

#[tokio::test]
async fn status_lists_boot_interfaces() {
    let t = two_networks();
    let s = json(&t.st, "/api/status").await;
    let ifaces = s["interfaces"].as_array().unwrap();
    assert_eq!(ifaces.len(), 2);
    assert_eq!(ifaces[1]["interface"], CI);
    assert_eq!(ifaces[1]["label"], "Hardware CI");
    assert_eq!(ifaces[1]["address"], "10.5.0.5");
    assert_eq!(ifaces[1]["url"], "http://10.5.0.5:8067");
}

#[tokio::test]
async fn the_control_plane_and_boot_routes_are_apart() {
    let t = setup();
    let control = crate::http::control_router(t.st.clone());
    let res = control.oneshot(Request::builder().uri("/boot/ipxe?mac=52-54-00-12-34-56").body(Body::empty()).unwrap()).await.unwrap();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    assert!(!body.starts_with(b"#!ipxe"), "no boot routes on the control plane");
    let (s, _) = get_on(&t.st, IF, PEER, "/api/devices").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "no API on a boot interface");
}
