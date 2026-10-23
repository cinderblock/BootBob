//! What TFTP serves.
//!
//! - The boot file name the network's DHCP server hands out (`boot_filename`, default
//!   `bootbob`): the iPXE build for the requester's firmware. That's how one file name
//!   in UniFi works for BIOS, UEFI x86-64 and arm64 machines alike.
//! - `ipxe/<file>`: a specific iPXE build.
//! - Anything else isn't served yet (Raspberry Pi boot trees come later), but is noted on
//!   the device so the UI can say what it asked for.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use bootbob_dhcp::{Firmware, Mac};
use bootbob_tftp::{Content, Outcome, Request, Resolution, Resolver};

use crate::model::{TftpSeen, now_ms};
use crate::state::State;

pub struct Tftp {
    pub st: Arc<State>,
}

/// The iPXE build file for a firmware.
pub fn ipxe_file(fw: Firmware) -> String {
    match fw {
        Firmware::Bios => "bootbob-bios.kpxe".into(),
        f => format!("bootbob-{}.efi", f.slug()),
    }
}

fn v4(ip: IpAddr) -> Option<Ipv4Addr> {
    match ip {
        IpAddr::V4(a) => Some(a),
        IpAddr::V6(a) => a.to_ipv4_mapped(),
    }
}

impl Tftp {
    /// The requester's MAC and firmware. The listener sees the request frame at the same
    /// moment we do, so give it a moment if it hasn't recorded the address yet.
    async fn identify(&self, ip: Option<Ipv4Addr>) -> (Option<Mac>, Option<Firmware>) {
        let Some(ip) = ip else { return (None, None) };
        let mut found = self.st.firmware_for(ip);
        if found.0.is_none() {
            tokio::time::sleep(Duration::from_millis(30)).await;
            found = self.st.firmware_for(ip);
        }
        found
    }

    fn note(&self, mac: Option<Mac>, ip: Option<Ipv4Addr>, path: &str, served: Option<String>) {
        let Some(mac) = mac else { return };
        // Only machines that netboot become devices; this is one.
        self.st.update_device(mac, |d| {
            let now = now_ms();
            d.last_seen = now;
            if let Some(ip) = ip {
                d.ip = Some(ip.to_string());
            }
            d.tftp = Some(TftpSeen { at: now, path: path.to_string(), served, ok: None });
        });
    }
}

impl Resolver for Tftp {
    async fn resolve(&self, req: &Request) -> Resolution {
        let Some(path) = req.path.clone() else { return Resolution::Denied("bad path".into()) };
        let ip = v4(req.peer.ip());
        let (mac, fw) = self.identify(ip).await;
        let dir = &self.st.config.ipxe_dir;

        let file = if path == self.st.config.boot_filename {
            let default = Firmware::from_slug(&self.st.config.default_firmware).unwrap_or(Firmware::EfiX64);
            let fw = fw.unwrap_or(default);
            ipxe_file(fw)
        } else if let Some(name) = path.strip_prefix("ipxe/") {
            name.to_string()
        } else {
            self.note(mac, ip, &path, None);
            if let Some(m) = mac
                && m.is_raspberry_pi()
            {
                tracing::info!(%m, path, "a Raspberry Pi asked for a file; Pi boot isn't served yet");
            }
            return Resolution::NotFound;
        };

        match Content::open(dir.join(&file)).await {
            Ok(c) => {
                self.note(mac, ip, &path, Some(file.clone()));
                if let Some(m) = mac {
                    self.st.event(Some(m), "tftp", format!("Loading iPXE ({file}) over TFTP"));
                }
                Resolution::Serve(c)
            }
            Err(e) => {
                self.note(mac, ip, &path, None);
                let msg = format!("No iPXE build {file} in {}: {e}", dir.display());
                tracing::warn!("{msg}");
                self.st.event(mac, "error", msg);
                Resolution::NotFound
            }
        }
    }

    fn finished(&self, req: &Request, outcome: &Outcome) {
        let Some(mac) = v4(req.peer.ip()).and_then(|ip| self.st.mac_for(ip)) else { return };
        let ok = matches!(outcome, Outcome::Completed { .. });
        self.st.edit_device(mac, |d| {
            if let Some(t) = &mut d.tftp {
                t.ok = Some(ok);
            }
        });
        if let Outcome::Failed(why) = outcome {
            self.st.event(Some(mac), "error", format!("TFTP transfer of {} failed: {why}", req.raw_path));
        }
    }
}
