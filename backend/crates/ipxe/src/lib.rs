//! iPXE scripts: the fallback menu every unassigned PC lands on, and the scripts that boot
//! an assigned image.
//!
//! Everything a script says about a machine comes from BootBob's database, which partly
//! comes from the machines themselves (SMBIOS strings) and partly from the admin (names).
//! iPXE has no quoting, so any such text goes through [`text`] first.

use std::fmt::Write;

use serde::{Deserialize, Serialize};

/// What booting an image means, as far as iPXE is concerned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    /// Back to the firmware, which moves on to the next boot entry (normally the local
    /// disk). On BIOS, boot the first hard disk instead.
    LocalDisk,
    Reboot,
    Poweroff,
    Shell,
    /// Chain another iPXE script or EFI binary, e.g. netboot.xyz.
    Chain {
        url: String,
    },
    /// A Linux-style kernel, initrds and command line.
    Kernel {
        kernel: String,
        #[serde(default)]
        initrds: Vec<String>,
        #[serde(default)]
        cmdline: String,
    },
    /// An admin-written iPXE script, used as is.
    Script {
        script: String,
    },
}

/// Removes what iPXE would interpret in displayed text: settings expansion (`$`, braces),
/// line breaks and other controls, and a leading `-` (which would read as an option).
pub fn text(s: &str) -> String {
    let t: String = s.chars().filter(|c| !matches!(c, '$' | '{' | '}' | '\\') && !c.is_control()).collect();
    t.trim().trim_start_matches('-').trim().to_string()
}

/// Theme colours as 0xRRGGBB. Applied with `colour`/`cpair`, which only change anything on
/// a framebuffer console; on a text console iPXE keeps its own palette.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MenuTheme {
    pub background: u32,
    pub text: u32,
    pub accent: u32,
    pub muted: u32,
}

/// The query iPXE sends back about itself. Every value is URI-encoded by iPXE.
pub const IDENTITY_QUERY: &str = "mac=${mac:uristring}&uuid=${uuid:uristring}&serial=${serial:uristring}\
&manufacturer=${manufacturer:uristring}&product=${product:uristring}&asset=${asset:uristring}\
&platform=${platform}&arch=${buildarch}&ip=${ip}";

/// The fallback menu.
#[derive(Debug, Clone)]
pub struct Menu {
    /// BootBob's base URL as the client reaches it, e.g. `http://10.255.0.5:8067`.
    pub base_url: String,
    pub title: String,
    /// Shown under the title: who this machine is to BootBob (code, name, IP).
    pub device_line: String,
    /// Shown under that: why it got the menu, and what happens next.
    pub status_line: String,
    /// How often to poll for an assignment. 0 disables polling.
    pub poll_secs: u32,
    /// Extra entries (images from the catalogue), grouped. Built-in entries (local disk,
    /// shell, reboot, power off) are always added.
    pub groups: Vec<(String, Vec<MenuItem>)>,
    pub theme: Option<MenuTheme>,
}

#[derive(Debug, Clone)]
pub struct MenuItem {
    /// The image's id, sent back when chosen.
    pub id: String,
    pub title: String,
}

fn label(id: &str) -> String {
    let l: String = id.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    format!("img_{l}")
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

fn theme_lines(s: &mut String, t: &MenuTheme) {
    // Palette slots: 0 black → background, 7 white → text, 4 blue → accent, 6 cyan → muted.
    // Pairs: 1 normal, 2 selected, 3 separator (iPXE's CPAIR_NORMAL/SELECT/SEPARATOR).
    // An iPXE without COLOUR_CMD skips the block.
    let _ = writeln!(s, "colour --rgb {:#08x} 0 || goto theme_done", t.background);
    let _ = writeln!(s, "colour --rgb {:#08x} 7 ||", t.text);
    let _ = writeln!(s, "colour --rgb {:#08x} 4 ||", t.accent);
    let _ = writeln!(s, "colour --rgb {:#08x} 6 ||", t.muted);
    s.push_str("cpair --foreground 7 --background 0 1 ||\n");
    s.push_str("cpair --foreground 0 --background 4 2 ||\n");
    s.push_str("cpair --foreground 6 --background 0 3 ||\n");
    s.push_str(":theme_done\n");
}

/// The fallback menu script.
///
/// Choosing an entry reports the choice to BootBob (`/boot/ipxe/chose`), which answers
/// with the script for it, so the web UI knows what the machine did. If BootBob can't be
/// reached, the built-in entries still work locally.
///
/// With polling on, the menu re-fetches `/boot/ipxe?…&poll=1` every `poll_secs` until
/// someone presses a key (iPXE's menu cancels its timeout on a keypress). That's how an
/// assignment made in the web UI reaches a machine already sitting at the menu.
pub fn menu(m: &Menu) -> String {
    let mut s = String::from("#!ipxe\n");
    let base = &m.base_url;
    let _ = writeln!(s, "set bootbob {base}");
    let _ = writeln!(s, "set bootbob-id {IDENTITY_QUERY}");
    if let Some(t) = &m.theme {
        theme_lines(&mut s, t);
    }
    s.push_str(":menu\n");
    let _ = writeln!(s, "menu {}", text(&m.title));
    let _ = writeln!(s, "item --gap {}", text(&m.device_line));
    let _ = writeln!(s, "item --gap {}", text(&m.status_line));
    s.push_str("item --gap\n");
    s.push_str("item refresh Check for an assignment now\n");
    s.push_str("item local Boot from local disk\n");
    for (group, items) in &m.groups {
        if items.is_empty() {
            continue;
        }
        let _ = writeln!(s, "item --gap {}", text(group));
        for it in items {
            let _ = writeln!(s, "item {} {}", label(&it.id), text(&it.title));
        }
    }
    s.push_str("item --gap Power and tools\n");
    s.push_str("item reboot Reboot\n");
    s.push_str("item poweroff Power off\n");
    s.push_str("item shell iPXE shell\n");
    if m.poll_secs > 0 {
        let _ = writeln!(s, "choose --timeout {} --default refresh selected || goto refresh", m.poll_secs * 1000);
    } else {
        s.push_str("choose --default refresh selected || goto refresh\n");
    }
    s.push_str("goto ${selected}\n");

    s.push_str(":refresh\n");
    s.push_str("chain --replace --autofree ${bootbob}/boot/ipxe?${bootbob-id}&poll=1 || goto unreachable\n");

    // Built-ins report the choice, and fall back to doing it locally.
    for (id, fallback) in [
        ("local", "iseq ${platform} pcbios && sanboot --no-describe --drive 0x80 || exit"),
        ("reboot", "reboot"),
        ("poweroff", "poweroff"),
        ("shell", "shell\ngoto menu"),
    ] {
        let _ = writeln!(s, ":{id}");
        let _ = writeln!(s, "chain --replace --autofree ${{bootbob}}/boot/ipxe/chose?${{bootbob-id}}&item={id} || goto {id}_here");
        let _ = writeln!(s, ":{id}_here");
        let _ = writeln!(s, "{fallback}");
    }
    for (_, items) in &m.groups {
        for it in items {
            let _ = writeln!(s, ":{}", label(&it.id));
            let _ = writeln!(
                s,
                "chain --replace --autofree ${{bootbob}}/boot/ipxe/chose?${{bootbob-id}}&item={} || goto unreachable",
                urlencode(&it.id)
            );
        }
    }

    s.push_str(":unreachable\n");
    s.push_str("echo Can't reach BootBob at ${bootbob}; trying again in 5 seconds (Ctrl-C for the menu)\n");
    s.push_str("sleep 5 && goto refresh || goto menu\n");
    s
}

/// The script that performs `action`. `title` is shown while it starts. `base_url` is
/// BootBob's URL, for coming back to the menu if the boot fails: a machine can get here
/// straight from the embedded script, without the menu having set it.
pub fn boot(action: &Action, title: &str, base_url: &str) -> String {
    let mut s = String::from("#!ipxe\n");
    let _ = writeln!(s, "set bootbob {base_url}");
    let _ = writeln!(s, "set bootbob-id {IDENTITY_QUERY}");
    let title = text(title);
    match action {
        Action::LocalDisk => {
            let _ = writeln!(s, "echo {title}");
            s.push_str("iseq ${platform} pcbios && sanboot --no-describe --drive 0x80 || exit\n");
        }
        Action::Reboot => s.push_str("reboot\n"),
        Action::Poweroff => s.push_str("poweroff\n"),
        Action::Shell => s.push_str("shell\n"),
        Action::Chain { url } => {
            let _ = writeln!(s, "echo Booting {title}");
            let _ = writeln!(s, "chain --autofree {url} || goto failed");
        }
        Action::Kernel { kernel, initrds, cmdline } => {
            let _ = writeln!(s, "echo Booting {title}");
            s.push_str("imgfree\n");
            let _ = writeln!(s, "kernel {kernel} {cmdline} || goto failed");
            for i in initrds {
                let _ = writeln!(s, "initrd {i} || goto failed");
            }
            s.push_str("boot || goto failed\n");
        }
        Action::Script { script } => {
            // Used as is, minus a second shebang.
            s.push_str(script.strip_prefix("#!ipxe").unwrap_or(script).trim_start_matches(['\r', '\n']));
            if !s.ends_with('\n') {
                s.push('\n');
            }
            return s;
        }
    }
    if matches!(action, Action::Chain { .. } | Action::Kernel { .. }) {
        s.push_str(":failed\n");
        let _ = writeln!(s, "echo Booting {title} failed; back to the menu in 10 seconds");
        s.push_str("sleep 10\n");
        s.push_str("chain --replace --autofree ${bootbob}/boot/ipxe?${bootbob-id}&failed=1 || shell\n");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Menu {
        Menu {
            base_url: "http://10.255.0.5:8067".into(),
            title: "BootBob".into(),
            device_line: "Device 12ab34 (bench-pc) at 10.255.7.9".into(),
            status_line: "No image assigned: pick one here or in the web UI".into(),
            poll_secs: 10,
            groups: vec![("Tools".into(), vec![MenuItem { id: "netboot.xyz".into(), title: "netboot.xyz".into() }])],
            theme: Some(MenuTheme { background: 0x101418, text: 0xe6e6e6, accent: 0x4fa3ff, muted: 0x7a8794 }),
        }
    }

    #[test]
    fn strips_what_ipxe_would_interpret() {
        assert_eq!(text("${root} pc\n{x}"), "root pcx");
        assert_eq!(text("--timeout 0"), "timeout 0");
        assert_eq!(text("  Dell Inc.  "), "Dell Inc.");
    }

    #[test]
    fn menu_polls_and_has_power_entries() {
        let s = menu(&sample());
        assert!(s.starts_with("#!ipxe\n"));
        assert!(s.contains("choose --timeout 10000 --default refresh selected || goto refresh"));
        assert!(s.contains("item reboot Reboot"));
        assert!(s.contains("item poweroff Power off"));
        assert!(s.contains(":reboot_here\nreboot\n"));
        assert!(s.contains("&poll=1"));
        assert!(s.contains("item img_netboot_xyz netboot.xyz"));
        assert!(s.contains("&item=netboot.xyz"));
        assert!(s.contains("colour --rgb 0x101418 0 || goto theme_done"));
    }

    #[test]
    fn every_goto_has_a_label() {
        let s = menu(&sample());
        let labels: Vec<&str> = s.lines().filter_map(|l| l.strip_prefix(':')).collect();
        for l in s.lines() {
            for target in l.split("goto ").skip(1) {
                let t = target.split_whitespace().next().unwrap();
                if t != "${selected}" {
                    assert!(labels.contains(&t), "goto {t} has no label");
                }
            }
        }
        // ${selected} is one of the item labels.
        for l in s.lines().filter_map(|l| l.strip_prefix("item ")) {
            let id = l.split_whitespace().next().unwrap();
            if id != "--gap" {
                assert!(labels.contains(&id), "item {id} has no label");
            }
        }
    }

    #[test]
    fn no_polling() {
        let s = menu(&Menu { poll_secs: 0, theme: None, ..sample() });
        assert!(s.contains("choose --default refresh selected"));
        assert!(!s.contains("colour"));
    }

    #[test]
    fn boot_scripts() {
        let k = boot(
            &Action::Kernel { kernel: "http://x/vmlinuz".into(), initrds: vec!["http://x/initrd".into()], cmdline: "console=ttyS0".into() },
            "Debian installer",
            "http://b",
        );
        assert!(k.starts_with("#!ipxe\nset bootbob http://b\nset bootbob-id mac="));
        assert!(k.contains("kernel http://x/vmlinuz console=ttyS0 || goto failed"));
        assert!(k.contains("initrd http://x/initrd || goto failed"));
        assert!(k.contains(":failed"));
        assert!(boot(&Action::Reboot, "x", "http://b").ends_with("\nreboot\n"));
        let script = boot(&Action::Script { script: "#!ipxe\r\necho hi".into() }, "x", "http://b");
        assert!(script.ends_with("&ip=${ip}\necho hi\n"), "{script}");
        assert!(boot(&Action::LocalDisk, "Local disk", "http://b").contains("sanboot --no-describe --drive 0x80 || exit"));
    }
}
