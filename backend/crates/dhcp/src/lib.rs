//! DHCP for a server that doesn't run DHCP.
//!
//! BootBob never answers DHCP by default. It reads DHCP instead:
//!
//! - [`message`]: a small DHCPv4 codec (just what reading PXE traffic and building probes
//!   needs).
//! - [`frame`]: Ethernet/IPv4/UDP frame parsing for the passive listener.
//! - [`listener`]: a receive-only `AF_PACKET` socket (Linux) that reports DHCP and TFTP
//!   requests seen on one interface. It never transmits.
//! - [`neigh`]: the kernel's neighbour table, to map a TFTP client's IP to its MAC.
//! - [`probe`]: client-style `DISCOVER`/`INFORM` packets for the network check.
//!
//! [`Firmware`] turns a PXE client's architecture (option 93) into the iPXE build it needs.

pub mod frame;
pub mod listener;
pub mod message;
pub mod neigh;
pub mod probe;

use std::fmt;
use std::str::FromStr;

pub use message::{Message, MessageType};

/// An Ethernet MAC address.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Mac(pub [u8; 6]);

impl Mac {
    /// The first three bytes, as `aa:bb:cc`.
    pub fn oui(&self) -> String {
        format!("{:02x}:{:02x}:{:02x}", self.0[0], self.0[1], self.0[2])
    }

    /// The last three bytes, as `ddeeff`: short enough to read off a screen.
    pub fn short(&self) -> String {
        format!("{:02x}{:02x}{:02x}", self.0[3], self.0[4], self.0[5])
    }

    /// A Raspberry Pi's built-in Ethernet (Raspberry Pi Foundation / Trading OUIs).
    pub fn is_raspberry_pi(&self) -> bool {
        matches!(self.oui().as_str(), "b8:27:eb" | "dc:a6:32" | "e4:5f:01" | "28:cd:c1" | "d8:3a:dd" | "2c:cf:67" | "88:a2:9e")
    }
}

impl fmt::Display for Mac {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let m = self.0;
        write!(f, "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
    }
}

impl fmt::Debug for Mac {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl FromStr for Mac {
    type Err = String;

    /// Accepts `aa:bb:cc:dd:ee:ff`, `aa-bb-…`, and bare `aabbccddeeff`, any case.
    fn from_str(s: &str) -> Result<Mac, String> {
        let hex: String = s.chars().filter(|c| !matches!(c, ':' | '-' | '.')).collect();
        if hex.len() != 12 {
            return Err(format!("not a MAC address: {s:?}"));
        }
        let mut m = [0u8; 6];
        for (i, byte) in m.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|_| format!("not a MAC address: {s:?}"))?;
        }
        Ok(Mac(m))
    }
}

/// A machine's firmware, as far as choosing a boot file goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Firmware {
    /// Legacy BIOS PXE (also what a Raspberry Pi's bootloader claims to be).
    Bios,
    EfiIa32,
    EfiX64,
    EfiArm32,
    EfiArm64,
    Other(u16),
}

impl Firmware {
    /// From a DHCP option 93 value (RFC 4578 and the IANA registry). The second value is
    /// true for UEFI HTTP boot clients.
    pub fn from_arch(arch: u16) -> (Firmware, bool) {
        match arch {
            0 => (Firmware::Bios, false),
            6 => (Firmware::EfiIa32, false),
            7 | 9 => (Firmware::EfiX64, false),
            10 => (Firmware::EfiArm32, false),
            11 => (Firmware::EfiArm64, false),
            15 => (Firmware::EfiIa32, true),
            16 => (Firmware::EfiX64, true),
            18 => (Firmware::EfiArm32, true),
            19 => (Firmware::EfiArm64, true),
            n => (Firmware::Other(n), false),
        }
    }

    /// From iPXE's `${platform}` and `${buildarch}`.
    pub fn from_ipxe(platform: &str, buildarch: &str) -> Option<Firmware> {
        Some(match (platform, buildarch) {
            ("pcbios", _) => Firmware::Bios,
            ("efi", "x86_64") => Firmware::EfiX64,
            ("efi", "i386") => Firmware::EfiIa32,
            ("efi", "arm64") => Firmware::EfiArm64,
            ("efi", "arm32") => Firmware::EfiArm32,
            _ => return None,
        })
    }

    /// A stable short name, used in the API and in file names.
    pub fn slug(&self) -> String {
        match self {
            Firmware::Bios => "bios".into(),
            Firmware::EfiIa32 => "efi-ia32".into(),
            Firmware::EfiX64 => "efi-x64".into(),
            Firmware::EfiArm32 => "efi-arm32".into(),
            Firmware::EfiArm64 => "efi-arm64".into(),
            Firmware::Other(n) => format!("arch-{n}"),
        }
    }

    pub fn from_slug(s: &str) -> Option<Firmware> {
        Some(match s {
            "bios" => Firmware::Bios,
            "efi-ia32" => Firmware::EfiIa32,
            "efi-x64" => Firmware::EfiX64,
            "efi-arm32" => Firmware::EfiArm32,
            "efi-arm64" => Firmware::EfiArm64,
            s => Firmware::Other(s.strip_prefix("arch-")?.parse().ok()?),
        })
    }

    /// The option 93 value a PXE client of this kind sends.
    pub fn arch(&self) -> u16 {
        match self {
            Firmware::Bios => 0,
            Firmware::EfiIa32 => 6,
            Firmware::EfiX64 => 7,
            Firmware::EfiArm32 => 10,
            Firmware::EfiArm64 => 11,
            Firmware::Other(n) => *n,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macs() {
        let m: Mac = "DC-A6-32-0A-0B-0C".parse().unwrap();
        assert_eq!(m.to_string(), "dc:a6:32:0a:0b:0c");
        assert_eq!(m.short(), "0a0b0c");
        assert!(m.is_raspberry_pi());
        assert_eq!("dca6320a0b0c".parse::<Mac>().unwrap(), m);
        assert!("dc:a6:32".parse::<Mac>().is_err());
        assert!("zz:a6:32:03:fc:36".parse::<Mac>().is_err());
    }

    #[test]
    fn firmware() {
        assert_eq!(Firmware::from_arch(7), (Firmware::EfiX64, false));
        assert_eq!(Firmware::from_arch(16), (Firmware::EfiX64, true));
        assert_eq!(Firmware::from_ipxe("pcbios", "i386"), Some(Firmware::Bios));
        for f in [Firmware::Bios, Firmware::EfiX64, Firmware::EfiArm64, Firmware::Other(41)] {
            assert_eq!(Firmware::from_slug(&f.slug()), Some(f));
            assert_eq!(Firmware::from_arch(f.arch()).0, f);
        }
    }
}
