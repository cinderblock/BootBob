//! The kernel's IPv4 neighbour (ARP) table.
//!
//! A TFTP request carries only an IP address. A client ARPs for the server before sending
//! it, and Linux records the sender of an ARP request aimed at one of its addresses, so the
//! table usually already knows the client's MAC by the time the request is read.

use std::net::Ipv4Addr;

use crate::Mac;

/// Parses `/proc/net/arp` text into (IP, MAC, interface) entries, skipping incomplete ones.
pub fn parse_proc_arp(text: &str) -> Vec<(Ipv4Addr, Mac, String)> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            let ip = f.first()?.parse().ok()?;
            let flags = u32::from_str_radix(f.get(2)?.trim_start_matches("0x"), 16).ok()?;
            // ATF_COM (0x2): the entry is complete.
            if flags & 0x2 == 0 {
                return None;
            }
            let mac: Mac = f.get(3)?.parse().ok()?;
            (mac.0 != [0; 6]).then(|| (ip, mac, f.get(5).unwrap_or(&"").to_string()))
        })
        .collect()
}

/// Looks up an IP in the kernel's table. Always `None` off Linux.
pub fn lookup(ip: Ipv4Addr) -> Option<Mac> {
    let text = std::fs::read_to_string("/proc/net/arp").ok()?;
    parse_proc_arp(&text).into_iter().find(|(i, _, _)| *i == ip).map(|(_, m, _)| m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses() {
        let text = "IP address       HW type     Flags       HW address            Mask     Device\n\
                    10.255.7.45      0x1         0x2         dc:a6:32:03:fc:36     *        eno1\n\
                    10.255.0.1       0x1         0x0         00:00:00:00:00:00     *        eno1\n";
        let e = parse_proc_arp(text);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].0, Ipv4Addr::new(10, 255, 7, 45));
        assert_eq!(e[0].1.to_string(), "dc:a6:32:03:fc:36");
        assert_eq!(e[0].2, "eno1");
    }
}
