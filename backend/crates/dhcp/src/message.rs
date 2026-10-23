//! A DHCPv4 message codec: the fixed BOOTP header plus options, kept in order.

use std::net::Ipv4Addr;

use crate::Mac;

const MAGIC: [u8; 4] = [99, 130, 83, 99];
const HEADER: usize = 236;

pub const OPT_PAD: u8 = 0;
pub const OPT_SERVER_NAME: u8 = 66;
pub const OPT_BOOTFILE: u8 = 67;
pub const OPT_MESSAGE_TYPE: u8 = 53;
pub const OPT_SERVER_ID: u8 = 54;
pub const OPT_PARAMS: u8 = 55;
pub const OPT_VENDOR_CLASS: u8 = 60;
pub const OPT_VENDOR_INFO: u8 = 43;
pub const OPT_USER_CLASS: u8 = 77;
pub const OPT_ARCH: u8 = 93;
pub const OPT_NDI: u8 = 94;
pub const OPT_UUID: u8 = 97;
pub const OPT_OVERLOAD: u8 = 52;
pub const OPT_END: u8 = 255;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    Discover,
    Offer,
    Request,
    Decline,
    Ack,
    Nak,
    Release,
    Inform,
    Other(u8),
}

impl MessageType {
    pub fn from_u8(n: u8) -> MessageType {
        match n {
            1 => MessageType::Discover,
            2 => MessageType::Offer,
            3 => MessageType::Request,
            4 => MessageType::Decline,
            5 => MessageType::Ack,
            6 => MessageType::Nak,
            7 => MessageType::Release,
            8 => MessageType::Inform,
            n => MessageType::Other(n),
        }
    }

    pub fn to_u8(self) -> u8 {
        match self {
            MessageType::Discover => 1,
            MessageType::Offer => 2,
            MessageType::Request => 3,
            MessageType::Decline => 4,
            MessageType::Ack => 5,
            MessageType::Nak => 6,
            MessageType::Release => 7,
            MessageType::Inform => 8,
            MessageType::Other(n) => n,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// 1 = request (client to server), 2 = reply.
    pub op: u8,
    pub htype: u8,
    pub hlen: u8,
    pub xid: u32,
    pub secs: u16,
    pub flags: u16,
    pub ciaddr: Ipv4Addr,
    pub yiaddr: Ipv4Addr,
    pub siaddr: Ipv4Addr,
    pub giaddr: Ipv4Addr,
    pub chaddr: [u8; 16],
    pub sname: Vec<u8>,
    pub file: Vec<u8>,
    /// Options in wire order. Options split across several instances (RFC 3396) are
    /// joined when read with [`Message::opt`].
    pub options: Vec<(u8, Vec<u8>)>,
}

fn ip(b: &[u8]) -> Ipv4Addr {
    Ipv4Addr::new(b[0], b[1], b[2], b[3])
}

fn cstr(b: &[u8]) -> Vec<u8> {
    b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())].to_vec()
}

fn read_options(mut b: &[u8], out: &mut Vec<(u8, Vec<u8>)>) {
    while let Some(&code) = b.first() {
        match code {
            OPT_PAD => b = &b[1..],
            OPT_END => break,
            _ => {
                let Some(&len) = b.get(1) else { break };
                let Some(val) = b.get(2..2 + len as usize) else { break };
                out.push((code, val.to_vec()));
                b = &b[2 + len as usize..];
            }
        }
    }
}

impl Message {
    /// An empty client request with the given hardware address.
    pub fn request(xid: u32, mac: Mac) -> Message {
        let mut chaddr = [0u8; 16];
        chaddr[..6].copy_from_slice(&mac.0);
        Message {
            op: 1,
            htype: 1,
            hlen: 6,
            xid,
            secs: 0,
            flags: 0,
            ciaddr: Ipv4Addr::UNSPECIFIED,
            yiaddr: Ipv4Addr::UNSPECIFIED,
            siaddr: Ipv4Addr::UNSPECIFIED,
            giaddr: Ipv4Addr::UNSPECIFIED,
            chaddr,
            sname: Vec::new(),
            file: Vec::new(),
            options: Vec::new(),
        }
    }

    pub fn parse(b: &[u8]) -> Option<Message> {
        if b.len() < HEADER + 4 || b[HEADER..HEADER + 4] != MAGIC {
            return None;
        }
        let mut chaddr = [0u8; 16];
        chaddr.copy_from_slice(&b[28..44]);
        let mut m = Message {
            op: b[0],
            htype: b[1],
            hlen: b[2],
            xid: u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
            secs: u16::from_be_bytes([b[8], b[9]]),
            flags: u16::from_be_bytes([b[10], b[11]]),
            ciaddr: ip(&b[12..16]),
            yiaddr: ip(&b[16..20]),
            siaddr: ip(&b[20..24]),
            giaddr: ip(&b[24..28]),
            chaddr,
            sname: cstr(&b[44..108]),
            file: cstr(&b[108..236]),
            options: Vec::new(),
        };
        read_options(&b[HEADER + 4..], &mut m.options);
        // Option overload (52): the file and/or sname fields carry more options.
        if let Some(&[o]) = m.opt(OPT_OVERLOAD).as_deref() {
            if o & 1 != 0 {
                read_options(&b[108..236], &mut m.options);
                m.file.clear();
            }
            if o & 2 != 0 {
                read_options(&b[44..108], &mut m.options);
                m.sname.clear();
            }
        }
        Some(m)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(300);
        v.extend_from_slice(&[self.op, self.htype, self.hlen, 0]);
        v.extend_from_slice(&self.xid.to_be_bytes());
        v.extend_from_slice(&self.secs.to_be_bytes());
        v.extend_from_slice(&self.flags.to_be_bytes());
        for a in [self.ciaddr, self.yiaddr, self.siaddr, self.giaddr] {
            v.extend_from_slice(&a.octets());
        }
        v.extend_from_slice(&self.chaddr);
        let mut fixed = |s: &[u8], len: usize| {
            let n = s.len().min(len - 1);
            v.extend_from_slice(&s[..n]);
            v.resize(v.len() + len - n, 0);
        };
        fixed(&self.sname, 64);
        fixed(&self.file, 128);
        v.extend_from_slice(&MAGIC);
        for (code, val) in &self.options {
            for chunk in val.chunks(255) {
                v.push(*code);
                v.push(chunk.len() as u8);
                v.extend_from_slice(chunk);
            }
            if val.is_empty() {
                v.extend_from_slice(&[*code, 0]);
            }
        }
        v.push(OPT_END);
        // Some old servers drop requests shorter than a BOOTP packet (300 bytes).
        if v.len() < 300 {
            v.resize(300, 0);
        }
        v
    }

    /// An option's value, with split instances joined (RFC 3396).
    pub fn opt(&self, code: u8) -> Option<Vec<u8>> {
        let mut found = None::<Vec<u8>>;
        for (c, val) in &self.options {
            if *c == code {
                found.get_or_insert_with(Vec::new).extend_from_slice(val);
            }
        }
        found
    }

    pub fn set_opt(&mut self, code: u8, val: impl Into<Vec<u8>>) {
        self.options.retain(|(c, _)| *c != code);
        self.options.push((code, val.into()));
    }

    pub fn msg_type(&self) -> Option<MessageType> {
        self.opt(OPT_MESSAGE_TYPE).and_then(|v| v.first().copied()).map(MessageType::from_u8)
    }

    pub fn mac(&self) -> Option<Mac> {
        (self.htype == 1 && self.hlen == 6).then(|| Mac(self.chaddr[..6].try_into().unwrap()))
    }

    /// The client's architectures (option 93), most preferred first.
    pub fn arches(&self) -> Vec<u16> {
        self.opt(OPT_ARCH).map(|v| v.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes(*c)).collect()).unwrap_or_default()
    }

    fn text(&self, code: u8) -> Option<String> {
        self.opt(code).map(|v| String::from_utf8_lossy(&v).trim_end_matches('\0').to_string())
    }

    pub fn vendor_class(&self) -> Option<String> {
        self.text(OPT_VENDOR_CLASS)
    }

    /// True for a firmware PXE client or iPXE (vendor class starts with `PXEClient`, or
    /// `HTTPClient` for UEFI HTTP boot).
    pub fn is_pxe(&self) -> bool {
        self.vendor_class().is_some_and(|v| v.starts_with("PXEClient") || v.starts_with("HTTPClient"))
    }

    /// iPXE sets user class `iPXE`.
    pub fn is_ipxe(&self) -> bool {
        self.opt(OPT_USER_CLASS).is_some_and(|v| v.windows(4).any(|w| w == b"iPXE"))
    }

    /// The client's SMBIOS UUID (option 97, type 0), in the usual 8-4-4-4-12 form with the
    /// first three groups byte-swapped as SMBIOS and iPXE's `${uuid}` show them.
    pub fn uuid(&self) -> Option<String> {
        let v = self.opt(OPT_UUID)?;
        let u: [u8; 16] = v.get(1..17)?.try_into().ok()?;
        if v[0] != 0 || u.iter().all(|&b| b == 0) {
            return None;
        }
        Some(format!(
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            u[3], u[2], u[1], u[0], u[5], u[4], u[7], u[6], u[8], u[9], u[10], u[11], u[12], u[13], u[14], u[15]
        ))
    }

    pub fn server_id(&self) -> Option<Ipv4Addr> {
        self.opt(OPT_SERVER_ID).filter(|v| v.len() == 4).map(|v| ip(&v))
    }

    /// The boot server a reply names: option 66, else `siaddr`, else `sname`.
    pub fn boot_server(&self) -> Option<String> {
        if let Some(s) = self.text(OPT_SERVER_NAME).filter(|s| !s.is_empty()) {
            return Some(s);
        }
        if !self.siaddr.is_unspecified() {
            return Some(self.siaddr.to_string());
        }
        (!self.sname.is_empty()).then(|| String::from_utf8_lossy(&self.sname).into_owned())
    }

    /// The boot file a reply names: option 67, else the `file` field.
    pub fn boot_file(&self) -> Option<String> {
        if let Some(s) = self.text(OPT_BOOTFILE).filter(|s| !s.is_empty()) {
            return Some(s);
        }
        (!self.file.is_empty()).then(|| String::from_utf8_lossy(&self.file).into_owned())
    }

    pub fn broadcast(&self) -> bool {
        self.flags & 0x8000 != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PXE DISCOVER shaped like a UEFI x64 firmware's (OVMF's option set), built byte by
    /// byte so the parser isn't only tested against our own encoder.
    fn uefi_discover() -> Vec<u8> {
        let mut b = vec![0u8; 236];
        b[..4].copy_from_slice(&[1, 1, 6, 0]);
        b[4..8].copy_from_slice(&0x74a0_c0c5u32.to_be_bytes());
        b[8..10].copy_from_slice(&4u16.to_be_bytes());
        b[10..12].copy_from_slice(&0x8000u16.to_be_bytes());
        b[28..34].copy_from_slice(&[0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
        b.extend_from_slice(&MAGIC);
        b.extend_from_slice(&hex(concat!(
            "350101",
            "390205c0",
            "5d020007",
            "5e03010300",
            "6111000a0b0c0d0e0f00010203040506070809",
            "3c20505845436c69656e743a417263683a30303030373a554e44493a303033303030",
            "ff",
        )));
        b
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn parses_a_pxe_discover() {
        let m = Message::parse(&uefi_discover()).unwrap();
        assert_eq!(m.msg_type(), Some(MessageType::Discover));
        assert_eq!(m.mac().unwrap().to_string(), "52:54:00:12:34:56");
        assert_eq!(m.arches(), vec![7]);
        assert_eq!(m.vendor_class().as_deref(), Some("PXEClient:Arch:00007:UNDI:003000"));
        assert!(m.is_pxe());
        assert!(!m.is_ipxe());
        assert_eq!(m.uuid().as_deref(), Some("0d0c0b0a-0f0e-0100-0203-040506070809"));
        assert!(m.broadcast());
    }

    #[test]
    fn round_trips() {
        let mut m = Message::request(0x1234_5678, "02:bb:00:00:00:01".parse().unwrap());
        m.flags = 0x8000;
        m.set_opt(OPT_MESSAGE_TYPE, [1]);
        m.set_opt(OPT_ARCH, [0, 9]);
        m.set_opt(OPT_VENDOR_CLASS, b"x".repeat(300));
        let wire = m.encode();
        let back = Message::parse(&wire).unwrap();
        assert_eq!(back.xid, 0x1234_5678);
        assert_eq!(back.arches(), vec![9]);
        // Split into two instances on the wire, joined again when read.
        assert_eq!(back.opt(OPT_VENDOR_CLASS).unwrap().len(), 300);
        assert_eq!(back.options.iter().filter(|(c, _)| *c == OPT_VENDOR_CLASS).count(), 2);
    }

    #[test]
    fn reads_boot_options_from_a_reply() {
        let mut m = Message::request(1, "02:bb:00:00:00:01".parse().unwrap());
        m.op = 2;
        m.siaddr = Ipv4Addr::new(10, 255, 0, 5);
        m.file = b"bootbob".to_vec();
        m.set_opt(OPT_MESSAGE_TYPE, [2]);
        let back = Message::parse(&m.encode()).unwrap();
        assert_eq!(back.boot_server().as_deref(), Some("10.255.0.5"));
        assert_eq!(back.boot_file().as_deref(), Some("bootbob"));
        let mut m2 = back.clone();
        m2.set_opt(OPT_SERVER_NAME, *b"pxe.tsl");
        m2.set_opt(OPT_BOOTFILE, *b"other.efi\0");
        assert_eq!(m2.boot_server().as_deref(), Some("pxe.tsl"));
        assert_eq!(m2.boot_file().as_deref(), Some("other.efi"));
    }

    #[test]
    fn rejects_garbage() {
        assert!(Message::parse(&[0; 100]).is_none());
        assert!(Message::parse(&[0; 300]).is_none());
    }
}
