//! What TFTP serves.
//!
//! - The boot file name the network's DHCP server hands out (`boot_filename`, default
//!   `bootbob`): the iPXE build for the requester's firmware. That's how one file name
//!   in UniFi works for BIOS, UEFI x86-64 and arm64 machines alike.
//! - `ipxe/<file>`: a specific iPXE build.
//! - Raspberry Pi boot files: whatever a Pi asks for (under its serial or MAC prefix, or
//!   none) comes from the boot tree of the image it's assigned. A Pi with nothing
//!   assigned gets nothing, and the UI shows it waiting.
//! - Anything else isn't served, but is noted on the device so the UI can say what it
//!   asked for.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use bootbob_dhcp::{Firmware, Mac};
use bootbob_pi::{Found, Prefix};
use bootbob_tftp::{Content, Outcome, Request, Resolution, Resolver};
use bytes::Bytes;

use crate::model::{Device, PI_SESSION_GAP_MS, PiBoot, PiSession, TftpSeen, now_ms};
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

/// What a device's `firmware` says for a Raspberry Pi (its bootloader isn't a PXE
/// firmware, so it has no [`Firmware`]).
pub const PI_FIRMWARE: &str = "rpi";

fn v4(ip: IpAddr) -> Option<Ipv4Addr> {
    match ip {
        IpAddr::V4(a) => Some(a),
        IpAddr::V6(a) => a.to_ipv4_mapped(),
    }
}

/// Fills in a Pi image's `{serial}`, `{mac}`, `{ip}` and `{server}` placeholders.
pub fn fill(template: &str, serial: &str, mac: Mac, ip: &str, server: &str) -> String {
    let m = mac.0;
    let mac = format!("{:02x}-{:02x}-{:02x}-{:02x}-{:02x}-{:02x}", m[0], m[1], m[2], m[3], m[4], m[5]);
    template.replace("{serial}", serial).replace("{mac}", &mac).replace("{ip}", ip).replace("{server}", server)
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

    fn note(&self, mac: Option<Mac>, ip: Option<Ipv4Addr>, path: &str, served: Option<String>, also: impl FnOnce(&mut Device)) {
        let Some(mac) = mac else { return };
        // Only machines that netboot become devices; this is one.
        self.st.update_device(mac, |d| {
            let now = now_ms();
            d.last_seen = now;
            if let Some(ip) = ip {
                d.ip = Some(ip.to_string());
            }
            d.tftp = Some(TftpSeen { at: now, path: path.to_string(), served, ok: None });
            also(d);
        });
    }

    /// The image a Pi's request belongs to: the boot in progress, or a new one (which
    /// records the boot and uses up a one-shot assignment). `None`: nothing to serve it.
    fn pi_image(&self, mac: Mac, dev: &Device) -> Option<(String, PiBoot, bool)> {
        let now = now_ms();
        if let Some(s) = &dev.pi_session
            && now.saturating_sub(s.last) < PI_SESSION_GAP_MS
            && let Some(pi) = self.st.image(&s.image).and_then(|i| i.pi)
        {
            return Some((s.image.clone(), pi, false));
        }
        let r = self.st.resolve_pi(dev)?;
        let pi = self.st.image(&r.image)?.pi?;
        self.st.record_boot(mac, &r.image, &r.reason, r.once);
        Some((r.image, pi, true))
    }

    async fn pi(&self, req: &Request, mac: Option<Mac>, ip: Option<Ipv4Addr>, prefix: Option<Prefix>, file: &str) -> Resolution {
        let path = req.path.clone().unwrap_or_default();
        let Some(mac) = mac else {
            tracing::debug!(path, "a Pi-style request from a machine we can't identify");
            return Resolution::NotFound;
        };
        let dev = self.st.device(mac).unwrap_or_else(|| Device { mac: mac.to_string(), ..Default::default() });
        let serial = match &prefix {
            Some(Prefix::Serial(s)) => Some(s.clone()),
            _ => dev.serial.clone().filter(|_| dev.firmware.as_deref() == Some(PI_FIRMWARE)),
        };
        let now = now_ms();
        let mark = |d: &mut Device, image: Option<&str>| {
            d.firmware = Some(PI_FIRMWARE.into());
            if let Some(s) = &serial {
                d.serial = Some(s.clone());
            }
            d.pi_session = image.map(|image| match d.pi_session.take() {
                Some(s) if s.image == image && now.saturating_sub(s.last) < PI_SESSION_GAP_MS => PiSession { last: now, ..s },
                _ => PiSession { image: image.into(), started: now, last: now },
            });
        };

        let Some((image, pi, new)) = self.pi_image(mac, &dev) else {
            let fresh = dev.tftp.as_ref().is_none_or(|t| now.saturating_sub(t.at) >= PI_SESSION_GAP_MS);
            self.note(Some(mac), ip, &path, None, |d| mark(d, None));
            if fresh {
                self.st.event(Some(mac), "netboot", "A Raspberry Pi asked to netboot, but has no Pi image assigned".into());
            }
            return Resolution::NotFound;
        };
        if new {
            tracing::info!(%mac, image, "Pi boot started");
        }

        let source = match bootbob_pi::resolve_source(&self.st.config.images_dir(), &pi.source) {
            Ok(s) => s,
            Err(e) => {
                self.note(Some(mac), ip, &path, None, |d| mark(d, Some(&image)));
                if new {
                    self.st.event(Some(mac), "error", format!("Can't boot {image}: {e}"));
                }
                return Resolution::NotFound;
            }
        };
        let server = match req.local.ip() {
            a if !a.is_unspecified() => a.to_string(),
            _ => self.st.config.tftp.map(|t| t.ip().to_string()).unwrap_or_default(),
        };
        let ip_s = ip.map(|i| i.to_string()).unwrap_or_default();
        let fill = |t: &str| fill(t, serial.as_deref().unwrap_or(""), mac, &ip_s, &server);

        let lower = file.to_ascii_lowercase();
        let content = if lower == "cmdline.txt" && !pi.cmdline.trim().is_empty() {
            Ok(Some(Content::Memory(Bytes::from(format!("{}\n", fill(pi.cmdline.trim()))))))
        } else {
            let (src, f) = (source.clone(), file.to_string());
            // A copy-on-write image: the machine's own view of it, once it has one (a
            // kernel it installed lives in its overlay). Until then, the image itself.
            let cow = pi.cow.then(|| (crate::nbd::overlay_dir(&self.st, mac), crate::nbd::overlay_name(&pi.source).to_string()));
            let look = move || match cow {
                Some((dir, name)) if bootbob_cow::Cow::exists(&dir, &name) => {
                    bootbob_pi::find_on(&bootbob_cow::Cow::open(&src, &dir, &name, true)?, &f)
                }
                _ => bootbob_pi::find(&src, &f),
            };
            match tokio::task::spawn_blocking(look).await {
                Ok(Ok(found)) => {
                    let found = if lower == "config.txt" && !pi.config.trim().is_empty() {
                        let base = match found {
                            Some(Found::Bytes(b)) => b,
                            Some(Found::Path { path, .. }) => tokio::fs::read(path).await.unwrap_or_default(),
                            None => Vec::new(),
                        };
                        let mut text = String::from_utf8_lossy(&base).into_owned();
                        if !text.is_empty() && !text.ends_with('\n') {
                            text.push('\n');
                        }
                        text.push_str(&format!("\n# Added by BootBob (image {image})\n[all]\n{}\n", fill(pi.config.trim())));
                        Some(Found::Bytes(text.into_bytes()))
                    } else {
                        found
                    };
                    match found {
                        Some(Found::Bytes(b)) => Ok(Some(Content::Memory(Bytes::from(b)))),
                        Some(Found::Path { path, .. }) => Content::open(path).await.map(Some),
                        None => Ok(None),
                    }
                }
                Ok(Err(e)) => Err(e),
                Err(e) => Err(std::io::Error::other(e)),
            }
        };

        match content {
            Ok(Some(c)) => {
                self.note(Some(mac), ip, &path, Some(format!("{image}: {file}")), |d| mark(d, Some(&image)));
                Resolution::Serve(c)
            }
            // Normal: the bootloader asks for plenty of optional files.
            Ok(None) => {
                self.note(Some(mac), ip, &path, None, |d| mark(d, Some(&image)));
                Resolution::NotFound
            }
            Err(e) => {
                self.note(Some(mac), ip, &path, None, |d| mark(d, Some(&image)));
                self.st.event(Some(mac), "error", format!("Can't read {file} from {}: {e}", pi.source));
                Resolution::NotFound
            }
        }
    }
}

impl Resolver for Tftp {
    async fn resolve(&self, req: &Request) -> Resolution {
        let Some(path) = req.path.clone() else { return Resolution::Denied("bad path".into()) };
        let ip = v4(req.peer.ip());
        let (mut mac, fw) = self.identify(ip).await;
        let (prefix, file) = bootbob_pi::split_prefix(&path);
        // A Pi asking under its MAC says who it is.
        if let Some(Prefix::Mac(m)) = &prefix {
            mac = Some(*m);
        }
        let dir = &self.st.config.ipxe_dir;

        let file = if path == self.st.config.boot_filename {
            let default = Firmware::from_slug(&self.st.config.default_firmware).unwrap_or(Firmware::EfiX64);
            let fw = fw.unwrap_or(default);
            ipxe_file(fw)
        } else if let Some(name) = path.strip_prefix("ipxe/") {
            name.to_string()
        } else if prefix.is_some() || mac.is_some_and(|m| m.is_raspberry_pi()) {
            return self.pi(req, mac, ip, prefix.clone(), file).await;
        } else {
            self.note(mac, ip, &path, None, |_| {});
            return Resolution::NotFound;
        };

        match Content::open(dir.join(&file)).await {
            Ok(c) => {
                // The event comes when the transfer finishes (see `finished`).
                self.note(mac, ip, &path, Some(file.clone()), |_| {});
                Resolution::Serve(c)
            }
            Err(e) => {
                self.note(mac, ip, &path, None, |_| {});
                let msg = format!("No iPXE build {file} in {}: {e}", dir.display());
                tracing::warn!("{msg}");
                self.st.event(mac, "error", msg);
                Resolution::NotFound
            }
        }
    }

    fn finished(&self, req: &Request, outcome: &Outcome) {
        let mac = req
            .path
            .as_deref()
            .and_then(|p| match bootbob_pi::split_prefix(p).0 {
                Some(Prefix::Mac(m)) => Some(m),
                _ => None,
            })
            .or_else(|| v4(req.peer.ip()).and_then(|ip| self.st.mac_for(ip)));
        let Some(mac) = mac else { return };
        let ok = match outcome {
            Outcome::Completed { .. } => Some(true),
            Outcome::Failed(_) => Some(false),
            // A size query (UEFI asks once for tsize, then again for real): no verdict yet.
            Outcome::Cancelled(_) => None,
        };
        let Some(dev) = self.st.edit_device(mac, |d| {
            if let Some(t) = &mut d.tftp {
                t.ok = ok;
            }
        }) else {
            return;
        };
        let pi = dev.firmware.as_deref() == Some(PI_FIRMWARE);
        let what = dev.tftp.and_then(|t| t.served).unwrap_or_else(|| req.raw_path.clone());
        match outcome {
            // A Pi fetches dozens of small files per boot; only the big ones are worth a line.
            Outcome::Completed { bytes, .. } if pi && *bytes < 1 << 20 => {}
            Outcome::Completed { bytes, elapsed } => self.st.event(
                Some(mac),
                "tftp",
                format!("Sent {what} over TFTP ({} KB in {:.1} s)", bytes.div_ceil(1024), elapsed.as_secs_f64()),
            ),
            Outcome::Failed(why) => self.st.event(Some(mac), "error", format!("TFTP transfer of {what} failed: {why}")),
            Outcome::Cancelled(_) => {}
        }
    }
}
