//! Client-style probes for the network check.
//!
//! A probe asks the network's DHCP server what it would tell a netbooting machine. It
//! sends a `DISCOVER` dressed as a given firmware, or an `INFORM` (which asks for options
//! without allocating anything). It never sends a `REQUEST`, so no lease is ever bound.

use std::net::Ipv4Addr;

use crate::message::*;
use crate::{Firmware, Mac};

/// A locally administered, unicast MAC for probe number `n`: `02:62:62:xx:xx:xx` ("bb").
/// Never a real vendor address, so it can't collide with a real machine.
pub fn probe_mac(n: u32) -> Mac {
    let b = n.to_be_bytes();
    Mac([0x02, 0x62, 0x62, b[1], b[2], b[3]])
}

fn pxe_options(m: &mut Message, firmware: Firmware) {
    let arch = firmware.arch();
    m.set_opt(OPT_ARCH, arch.to_be_bytes());
    // UNDI 3.16, as UEFI firmware reports it.
    m.set_opt(OPT_NDI, [1, 3, 16]);
    m.set_opt(OPT_VENDOR_CLASS, format!("PXEClient:Arch:{arch:05}:UNDI:003016").into_bytes());
    m.set_opt(
        OPT_PARAMS,
        [1, 3, 6, OPT_VENDOR_INFO, OPT_SERVER_ID, OPT_VENDOR_CLASS, OPT_SERVER_NAME, OPT_BOOTFILE, 128, 129, 130, 131, 132, 133, 134, 135],
    );
}

/// A broadcast `DISCOVER` that looks like `firmware`'s PXE ROM.
pub fn discover(xid: u32, mac: Mac, firmware: Firmware) -> Message {
    let mut m = Message::request(xid, mac);
    m.flags = 0x8000;
    m.set_opt(OPT_MESSAGE_TYPE, [MessageType::Discover.to_u8()]);
    pxe_options(&mut m, firmware);
    m
}

/// An `INFORM` from a machine that already has `addr`, asking for the PXE options.
pub fn inform(xid: u32, mac: Mac, addr: Ipv4Addr, firmware: Firmware) -> Message {
    let mut m = Message::request(xid, mac);
    m.ciaddr = addr;
    m.set_opt(OPT_MESSAGE_TYPE, [MessageType::Inform.to_u8()]);
    pxe_options(&mut m, firmware);
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_looks_like_firmware() {
        let m = Message::parse(&discover(9, probe_mac(1), Firmware::EfiX64).encode()).unwrap();
        assert_eq!(m.msg_type(), Some(MessageType::Discover));
        assert_eq!(m.arches(), vec![7]);
        assert_eq!(m.vendor_class().as_deref(), Some("PXEClient:Arch:00007:UNDI:003016"));
        assert!(m.broadcast());
        assert_eq!(m.mac().unwrap().to_string(), "02:62:62:00:00:01");
    }

    #[test]
    fn never_a_request() {
        for f in [Firmware::Bios, Firmware::EfiX64, Firmware::EfiArm64] {
            assert_ne!(discover(1, probe_mac(1), f).msg_type(), Some(MessageType::Request));
            assert_ne!(inform(1, probe_mac(1), Ipv4Addr::LOCALHOST, f).msg_type(), Some(MessageType::Request));
        }
    }
}
