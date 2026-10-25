//! What BootBob stores: devices, images, settings and the event log.

use std::time::{SystemTime, UNIX_EPOCH};

use bootbob_ipxe::Action;
use serde::{Deserialize, Serialize};

/// Milliseconds since the Unix epoch.
pub type Millis = u64;

pub fn now_ms() -> Millis {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// A machine BootBob has seen. Keyed by MAC.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Device {
    pub mac: String,
    pub name: Option<String>,
    pub notes: Option<String>,
    pub first_seen: Millis,
    pub last_seen: Millis,
    pub ip: Option<String>,
    /// `bios`, `efi-x64`, … (see `bootbob_dhcp::Firmware::slug`).
    pub firmware: Option<String>,
    pub http_boot: bool,
    pub uuid: Option<String>,
    pub serial: Option<String>,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub asset: Option<String>,
    pub assignment: Option<Assignment>,
    pub last_boot: Option<BootRecord>,
    /// Set while the machine sits at the fallback menu (cleared when it boots something).
    pub menu: Option<MenuState>,
    pub dhcp: Option<DhcpSeen>,
    pub tftp: Option<TftpSeen>,
    /// The last time it fetched an iPXE script from us.
    pub last_script: Option<Millis>,
    /// A Raspberry Pi's boot in progress: the image it started with, so every file of one
    /// boot comes from the same image even if the assignment changes (or was one-shot).
    pub pi_session: Option<PiSession>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PiSession {
    pub image: String,
    pub started: Millis,
    /// The last TFTP request in it. A gap longer than [`PI_SESSION_GAP_MS`] ends it.
    pub last: Millis,
}

/// A Pi fetches its boot files back to back; this long with no request means the boot
/// is over (or never started), and the next request begins a new one.
pub const PI_SESSION_GAP_MS: Millis = 60_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assignment {
    pub image: String,
    /// Boot it once, then go back to the normal resolution.
    #[serde(default)]
    pub once: bool,
    #[serde(default)]
    pub set_at: Millis,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootRecord {
    pub at: Millis,
    pub image: String,
    pub title: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MenuState {
    pub since: Millis,
    pub last_poll: Millis,
    pub polls: u32,
}

/// What the DHCP listener saw from this machine.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DhcpSeen {
    pub last_discover: Millis,
    pub arches: Vec<u16>,
    pub vendor_class: Option<String>,
    /// The request came from iPXE rather than the firmware.
    pub ipxe: bool,
    /// The last offer or ack a DHCP server sent it, if we saw it.
    pub offer: Option<OfferSeen>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OfferSeen {
    pub at: Millis,
    pub server: Option<String>,
    pub ip: Option<String>,
    pub boot_server: Option<String>,
    pub boot_file: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TftpSeen {
    pub at: Millis,
    pub path: String,
    /// The file actually sent (e.g. which iPXE build), if any.
    pub served: Option<String>,
    pub ok: Option<bool>,
}

/// Something a machine can boot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Image {
    /// From the URL in `PUT /api/images/{id}`, so optional in the body.
    #[serde(default)]
    pub id: String,
    pub title: String,
    /// The fallback menu's section for it. Empty: not listed in the menu.
    #[serde(default)]
    pub group: String,
    /// What a PC (iPXE) does with it. `None` for the fallback menu itself, and for images
    /// only Raspberry Pis can boot.
    pub action: Option<Action>,
    /// What a Raspberry Pi boots: its boot files over TFTP.
    #[serde(default)]
    pub pi: Option<PiBoot>,
    #[serde(default, skip_deserializing)]
    pub builtin: bool,
}

/// A Raspberry Pi boot tree, served over TFTP to the Pis assigned it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PiBoot {
    /// A directory or a disk image (`.img`) in the images directory, relative to it. A
    /// disk image's FAT boot partition is read in place.
    pub source: String,
    /// Replaces the tree's `cmdline.txt`. `{serial}`, `{mac}`, `{ip}` and `{server}` are
    /// filled in per Pi. Empty: the tree's own.
    #[serde(default)]
    pub cmdline: String,
    /// Lines added to the end of the tree's `config.txt`, same placeholders.
    #[serde(default)]
    pub config: String,
    /// Each machine gets its own copy-on-write overlay of the disk image, which is then
    /// never written: one golden image for any number of Pis. Needs a disk image source.
    #[serde(default)]
    pub cow: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub at: Millis,
    pub mac: Option<String>,
    pub kind: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// What machines with no assignment get.
    pub default_image: String,
    /// What Raspberry Pis with no assignment get. Empty: nothing, and they keep asking
    /// (with `BOOT_ORDER` network-only they retry until one is assigned).
    pub pi_default_image: String,
    /// How often the fallback menu checks for an assignment.
    pub poll_secs: u32,
    /// After this long at the menu untouched, boot the local disk, so a PC that merely
    /// tries the network before its disk still starts normally. 0: wait forever.
    pub menu_timeout_secs: u32,
    pub theme: Theme,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            default_image: "menu".into(),
            pi_default_image: String::new(),
            poll_secs: 10,
            menu_timeout_secs: 120,
            theme: Theme::default(),
        }
    }
}

/// The one theme shared by the web UI and the boot menu. Colours are `#rrggbb`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Theme {
    pub background: String,
    pub surface: String,
    pub text: String,
    pub muted: String,
    pub accent: String,
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            background: "#101418".into(),
            surface: "#1a2027".into(),
            text: "#e6e9ec".into(),
            muted: "#8794a1".into(),
            accent: "#4fa3ff".into(),
        }
    }
}

pub fn parse_colour(s: &str) -> Option<u32> {
    let h = s.strip_prefix('#')?;
    (h.len() == 6).then(|| u32::from_str_radix(h, 16).ok()).flatten()
}

impl Theme {
    pub fn validate(&self) -> Result<(), String> {
        for (k, v) in [
            ("background", &self.background),
            ("surface", &self.surface),
            ("text", &self.text),
            ("muted", &self.muted),
            ("accent", &self.accent),
        ] {
            parse_colour(v).ok_or_else(|| format!("theme.{k} must be #rrggbb, not {v:?}"))?;
        }
        Ok(())
    }

    pub fn menu_theme(&self) -> Option<bootbob_ipxe::MenuTheme> {
        Some(bootbob_ipxe::MenuTheme {
            background: parse_colour(&self.background)?,
            text: parse_colour(&self.text)?,
            accent: parse_colour(&self.accent)?,
            muted: parse_colour(&self.muted)?,
        })
    }

    pub fn css(&self) -> String {
        format!(
            ":root {{\n  --bg: {};\n  --surface: {};\n  --text: {};\n  --muted: {};\n  --accent: {};\n}}\n",
            self.background, self.surface, self.text, self.muted, self.accent
        )
    }
}

/// The images every BootBob has. They live in code, so they improve with releases.
pub fn builtin_images() -> Vec<Image> {
    let b = |id: &str, title: &str, group: &str, action: Option<Action>| Image {
        id: id.into(),
        title: title.into(),
        group: group.into(),
        action,
        pi: None,
        builtin: true,
    };
    vec![
        b("menu", "Fallback menu", "", None),
        b("local", "Boot from local disk", "", Some(Action::LocalDisk)),
        b("reboot", "Reboot", "", Some(Action::Reboot)),
        b("poweroff", "Power off", "", Some(Action::Poweroff)),
        b("shell", "iPXE shell", "", Some(Action::Shell)),
        b(
            "netboot.xyz",
            "netboot.xyz (installers and tools from the internet)",
            "Tools",
            Some(Action::Chain { url: "https://boot.netboot.xyz".into() }),
        ),
    ]
}

/// SMBIOS fields full of vendor filler are as good as empty.
pub fn clean_smbios(s: Option<&str>) -> Option<String> {
    let s = s?.trim();
    let junk = [
        "",
        "to be filled by o.e.m.",
        "default string",
        "not specified",
        "none",
        "system serial number",
        "system product name",
        "system manufacturer",
        "o.e.m.",
        "0",
        "00000000-0000-0000-0000-000000000000",
        "ffffffff-ffff-ffff-ffff-ffffffffffff",
        "03000200-0400-0500-0006-000700080009",
    ];
    (!junk.contains(&s.to_ascii_lowercase().as_str())).then(|| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smbios_filler_is_dropped() {
        assert_eq!(clean_smbios(Some("To Be Filled By O.E.M.")), None);
        assert_eq!(clean_smbios(Some(" Dell Inc. ")).as_deref(), Some("Dell Inc."));
        assert_eq!(clean_smbios(None), None);
    }

    #[test]
    fn theme() {
        let t = Theme::default();
        assert!(t.validate().is_ok());
        assert_eq!(t.menu_theme().unwrap().accent, 0x4fa3ff);
        assert!(t.css().contains("--accent: #4fa3ff;"));
        assert!(Theme { accent: "blue".into(), ..t }.validate().is_err());
    }
}
