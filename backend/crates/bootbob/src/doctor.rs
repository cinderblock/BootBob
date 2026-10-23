//! The network check: is everything set up for machines to netboot from BootBob, and if
//! not, what exactly to change. BootBob only ever suggests fixes; it never changes the
//! network.
//!
//! The DHCP checks send client-style `DISCOVER`s dressed as each firmware (from a locally
//! administered MAC, never a `REQUEST`, so no lease is bound) and read the offers through
//! the passive listener.

use std::collections::BTreeSet;
use std::net::Ipv4Addr;
use std::time::Duration;

use bootbob_dhcp::{Firmware, Mac, probe};
use serde::Serialize;

use crate::model::{Millis, now_ms};
use crate::state::State;
use crate::tftp::ipxe_file;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    Warn,
    Fail,
    Skip,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub id: String,
    pub title: String,
    pub status: Status,
    pub detail: String,
    /// What to change, when something's wrong.
    pub fix: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub at: Millis,
    pub checks: Vec<Check>,
}

/// A DHCP server's answer to a probe.
#[derive(Debug, Clone)]
pub struct Offer {
    pub server: Option<Ipv4Addr>,
    pub src_mac: Mac,
    pub boot_server: Option<String>,
    pub boot_file: Option<String>,
}

impl Offer {
    fn sender(&self) -> String {
        self.server.map(|s| s.to_string()).unwrap_or_else(|| self.src_mac.to_string())
    }
}

fn check(id: &str, title: &str, status: Status, detail: impl Into<String>, fix: Option<String>) -> Check {
    Check { id: id.into(), title: title.into(), status, detail: detail.into(), fix }
}

fn network_boot_fix(server: Option<&str>, file: &str) -> String {
    format!(
        "On the network's DHCP server, set the boot server (next-server, option 66) to {} and the boot file \
         (option 67) to \"{file}\". On a UniFi gateway that's the network's \"Network Boot\" setting; in this lab it \
         goes through the ops repo (ops/unifi).",
        server.unwrap_or("BootBob's address")
    )
}

/// Judges the offers one firmware's probe got.
pub fn evaluate_offers(fw: Firmware, offers: &[Offer], expect_server: Option<&str>, expect_file: &str) -> Check {
    let id = format!("dhcp-{}", fw.slug());
    let title = format!("DHCP tells {} machines to boot from BootBob", fw.slug());
    let fix = || Some(network_boot_fix(expect_server, expect_file));
    if offers.is_empty() {
        return check(
            &id,
            &title,
            Status::Fail,
            "No DHCP server answered a netboot request. Either nothing on this network hands out addresses to new \
             machines, or the switch drops DHCP from this port.",
            Some(
                "Check that the DHCP server serves this network and that the switch's DHCP guard allows client \
                  requests through to it."
                    .into(),
            ),
        );
    }
    let good = offers.iter().find(|o| {
        let server_ok = match (expect_server, &o.boot_server) {
            (Some(want), Some(got)) => want == got,
            (None, Some(_)) => true,
            (_, None) => false,
        };
        server_ok && o.boot_file.as_deref() == Some(expect_file)
    });
    match good {
        Some(o) => check(
            &id,
            &title,
            Status::Ok,
            format!("{} offers boot server {} and file \"{expect_file}\".", o.sender(), o.boot_server.as_deref().unwrap_or("?")),
            None,
        ),
        None => {
            let seen: Vec<String> = offers
                .iter()
                .map(|o| {
                    format!(
                        "{} offers boot server {} and file {}",
                        o.sender(),
                        o.boot_server.as_deref().unwrap_or("(none)"),
                        o.boot_file.as_deref().map(|f| format!("\"{f}\"")).unwrap_or_else(|| "(none)".into())
                    )
                })
                .collect();
            check(&id, &title, Status::Fail, format!("{}.", seen.join("; ")), fix())
        }
    }
}

/// More than one server handing out boot options is a rogue boot server (or a forgotten
/// one), whichever of them is right.
pub fn evaluate_rogue(offers: &[Offer]) -> Check {
    let servers: BTreeSet<String> =
        offers.iter().filter(|o| o.boot_server.is_some() || o.boot_file.is_some()).map(|o| o.sender()).collect();
    if servers.len() > 1 {
        check(
            "dhcp-rogue",
            "Only one server hands out boot options",
            Status::Warn,
            format!("Boot options came from {} servers: {}.", servers.len(), servers.into_iter().collect::<Vec<_>>().join(", ")),
            Some("Find the extra DHCP or proxyDHCP server and turn its boot options off.".into()),
        )
    } else {
        check("dhcp-rogue", "Only one server hands out boot options", Status::Ok, "No other boot servers answered.", None)
    }
}

/// BootBob's own address, as machines should be told it.
fn our_address(st: &State) -> Option<String> {
    if let Some(t) = st.config.tftp
        && !t.ip().is_unspecified()
    {
        return Some(t.ip().to_string());
    }
    let url = st.config.public_url.as_deref()?;
    let host = url.split("://").nth(1)?.split(['/', ':']).next()?;
    host.parse::<Ipv4Addr>().ok().map(|ip| ip.to_string())
}

pub async fn run(st: &State) -> Report {
    let mut checks = Vec::new();
    let file = st.config.boot_filename.clone();
    let ours = our_address(st);

    // iPXE builds.
    let dir = &st.config.ipxe_dir;
    let missing: Vec<String> =
        [Firmware::Bios, Firmware::EfiX64, Firmware::EfiArm64].into_iter().map(ipxe_file).filter(|f| !dir.join(f).is_file()).collect();
    checks.push(match missing.len() {
        0 => check(
            "ipxe",
            "iPXE builds are installed",
            Status::Ok,
            format!("BIOS, UEFI x86-64 and arm64 builds in {}.", dir.display()),
            None,
        ),
        _ => check(
            "ipxe",
            "iPXE builds are installed",
            if missing.iter().any(|f| f.contains("x64") || f.contains("bios")) { Status::Fail } else { Status::Warn },
            format!("Missing from {}: {}.", dir.display(), missing.join(", ")),
            Some("Build them with ipxe/build.sh (the Docker image has them), and point ipxe_dir at them.".into()),
        ),
    });

    checks.push(match st.config.tftp {
        Some(a) => check("tftp", "TFTP is on", Status::Ok, format!("Listening on {a}."), None),
        None => check(
            "tftp",
            "TFTP is on",
            Status::Fail,
            "TFTP is off, so firmware PXE can't load iPXE.",
            Some("Set tftp in the config (e.g. tftp = \"10.255.0.5:69\").".into()),
        ),
    });

    let listener = st.listener.read().unwrap().clone();
    let probing = match (&listener.interface, listener.running) {
        (None, _) => {
            checks.push(check(
                "listener",
                "The DHCP listener is watching the LAN",
                Status::Warn,
                "Off. Without it BootBob can't tell BIOS from UEFI machines (they all get the default build), can't see \
                 machines that fail to reach it, and can't run the DHCP checks.",
                Some("Set interface in the config to the LAN interface (e.g. interface = \"eno1\").".into()),
            ));
            None
        }
        (Some(i), false) => {
            checks.push(check(
                "listener",
                "The DHCP listener is watching the LAN",
                Status::Fail,
                format!("Not running on {i}: {}.", listener.error.as_deref().unwrap_or("unknown error")),
                Some("It needs Linux and CAP_NET_RAW (in Docker: --cap-add NET_RAW).".into()),
            ));
            None
        }
        (Some(i), true) => {
            checks.push(check(
                "listener",
                "The DHCP listener is watching the LAN",
                Status::Ok,
                format!("Watching {i}: {} DHCP messages seen, {} from netbooting machines.", listener.dhcp_seen, listener.pxe_seen),
                None,
            ));
            Some(i.clone())
        }
    };

    match probing {
        Some(iface) => {
            let mut all = Vec::new();
            for (n, fw) in [Firmware::EfiX64, Firmware::Bios].into_iter().enumerate() {
                let xid = (now_ms() as u32).wrapping_add(n as u32);
                let mac = probe::probe_mac(xid & 0x00ff_ffff);
                st.expect_probe(xid);
                let sent = probe::send(&iface, &probe::discover(xid, mac, fw));
                if let Err(e) = sent {
                    st.take_probe(xid);
                    checks.push(check(
                        &format!("dhcp-{}", fw.slug()),
                        &format!("DHCP tells {} machines to boot from BootBob", fw.slug()),
                        Status::Skip,
                        format!("Couldn't send the probe: {e}."),
                        None,
                    ));
                    continue;
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
                let offers = st.take_probe(xid);
                checks.push(evaluate_offers(fw, &offers, ours.as_deref(), &file));
                all.extend(offers);
            }
            if !all.is_empty() {
                checks.push(evaluate_rogue(&all));
            }
        }
        None => checks.push(check(
            "dhcp",
            "DHCP tells machines to boot from BootBob",
            Status::Skip,
            "Needs the DHCP listener.",
            Some(network_boot_fix(ours.as_deref(), &file)),
        )),
    }

    let stuck: Vec<String> =
        st.devices().into_iter().map(|d| crate::http::view(st, d)).filter(|v| v.state == "stuck").map(|v| v.code).collect();
    checks.push(if stuck.is_empty() {
        check("stuck", "Machines that try to netboot reach BootBob", Status::Ok, "None stuck.", None)
    } else {
        check(
            "stuck",
            "Machines that try to netboot reach BootBob",
            Status::Warn,
            format!("Tried to netboot but never reached BootBob: {}. Their details show what DHCP told them.", stuck.join(", ")),
            Some(network_boot_fix(ours.as_deref(), &file)),
        )
    });

    let report = Report { at: now_ms(), checks };
    let bad = report.checks.iter().filter(|c| matches!(c.status, Status::Fail | Status::Warn)).count();
    st.event(None, "doctor", if bad == 0 { "Network check: all good".into() } else { format!("Network check: {bad} problems") });
    st.set_report(report.clone());
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(server: [u8; 4], boot_server: Option<&str>, file: Option<&str>) -> Offer {
        Offer {
            server: Some(server.into()),
            src_mac: Mac([2; 6]),
            boot_server: boot_server.map(Into::into),
            boot_file: file.map(Into::into),
        }
    }

    #[test]
    fn no_offer_fails() {
        let c = evaluate_offers(Firmware::EfiX64, &[], Some("10.255.0.5"), "bootbob");
        assert_eq!(c.status, Status::Fail);
        assert!(c.detail.contains("No DHCP server answered"));
    }

    #[test]
    fn missing_boot_options_get_the_exact_fix() {
        let c = evaluate_offers(Firmware::Bios, &[offer([10, 255, 0, 1], None, None)], Some("10.255.0.5"), "bootbob");
        assert_eq!(c.status, Status::Fail);
        assert!(c.detail.contains("10.255.0.1 offers boot server (none) and file (none)"));
        let fix = c.fix.unwrap();
        assert!(fix.contains("10.255.0.5") && fix.contains("\"bootbob\""), "{fix}");
    }

    #[test]
    fn wrong_server_or_file_fails_and_right_passes() {
        let wrong = [offer([10, 255, 0, 1], Some("10.255.0.9"), Some("bootbob"))];
        assert_eq!(evaluate_offers(Firmware::EfiX64, &wrong, Some("10.255.0.5"), "bootbob").status, Status::Fail);
        let wrong_file = [offer([10, 255, 0, 1], Some("10.255.0.5"), Some("ipxe.efi"))];
        assert_eq!(evaluate_offers(Firmware::EfiX64, &wrong_file, Some("10.255.0.5"), "bootbob").status, Status::Fail);
        let right = [offer([10, 255, 0, 1], Some("10.255.0.5"), Some("bootbob"))];
        assert_eq!(evaluate_offers(Firmware::EfiX64, &right, Some("10.255.0.5"), "bootbob").status, Status::Ok);
        // Not knowing our own address, any boot server with the right file passes.
        assert_eq!(evaluate_offers(Firmware::EfiX64, &wrong, None, "bootbob").status, Status::Ok);
    }

    #[test]
    fn two_boot_servers_is_a_warning() {
        let offers = [offer([10, 255, 0, 1], Some("10.255.0.5"), Some("bootbob")), offer([10, 255, 0, 77], Some("10.255.0.77"), Some("x"))];
        assert_eq!(evaluate_rogue(&offers).status, Status::Warn);
        assert_eq!(evaluate_rogue(&offers[..1]).status, Status::Ok);
        // A plain DHCP server with no boot options isn't a boot server.
        let plain = [offers[0].clone(), offer([10, 255, 0, 2], None, None)];
        assert_eq!(evaluate_rogue(&plain).status, Status::Ok);
    }
}
