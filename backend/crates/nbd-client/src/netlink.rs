//! The kernel's NBD netlink interface (generic netlink family `nbd`), just the parts a
//! root-filesystem client needs: connect a device with a dead-connection timeout, hear
//! when its link dies, and hand it a new socket.
//!
//! With `NBD_ATTR_DEAD_CONN_TIMEOUT` set, the kernel doesn't fail I/O when the server
//! goes away: it holds it, multicasts `NBD_CMD_LINK_DEAD`, and resumes when
//! `NBD_CMD_RECONFIGURE` brings a new socket. Without it (and with the old ioctl
//! interface), in-flight I/O fails and ext4 goes read-only.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

const NETLINK_GENERIC: i32 = 16;
const GENL_ID_CTRL: u16 = 0x10;
const CTRL_CMD_GETFAMILY: u8 = 3;
const CTRL_ATTR_FAMILY_ID: u16 = 1;
const CTRL_ATTR_FAMILY_NAME: u16 = 2;
const CTRL_ATTR_MCAST_GROUPS: u16 = 7;
const CTRL_ATTR_MCAST_GRP_NAME: u16 = 1;
const CTRL_ATTR_MCAST_GRP_ID: u16 = 2;
const NLM_F_REQUEST: u16 = 1;
const NLM_F_ACK: u16 = 4;
const NLMSG_ERROR: u16 = 2;
const NLA_F_NESTED: u16 = 0x8000;
const SOL_NETLINK: i32 = 270;
const NETLINK_ADD_MEMBERSHIP: i32 = 1;

pub const NBD_CMD_CONNECT: u8 = 1;
pub const NBD_CMD_RECONFIGURE: u8 = 3;
pub const NBD_CMD_LINK_DEAD: u8 = 4;
const NBD_ATTR_INDEX: u16 = 1;
const NBD_ATTR_SIZE_BYTES: u16 = 2;
const NBD_ATTR_BLOCK_SIZE_BYTES: u16 = 3;
const NBD_ATTR_TIMEOUT: u16 = 4;
const NBD_ATTR_SERVER_FLAGS: u16 = 5;
const NBD_ATTR_SOCKETS: u16 = 7;
const NBD_ATTR_DEAD_CONN_TIMEOUT: u16 = 8;
const NBD_SOCK_ITEM: u16 = 1;
const NBD_SOCK_FD: u16 = 1;

/// Netlink attributes, built up and then framed.
#[derive(Default)]
pub struct Attrs(Vec<u8>);

impl Attrs {
    fn raw(&mut self, kind: u16, payload: &[u8]) -> &mut Self {
        let len = 4 + payload.len();
        self.0.extend_from_slice(&(len as u16).to_ne_bytes());
        self.0.extend_from_slice(&kind.to_ne_bytes());
        self.0.extend_from_slice(payload);
        while !self.0.len().is_multiple_of(4) {
            self.0.push(0);
        }
        self
    }
    pub fn u32(&mut self, kind: u16, v: u32) -> &mut Self {
        self.raw(kind, &v.to_ne_bytes())
    }
    pub fn u64(&mut self, kind: u16, v: u64) -> &mut Self {
        self.raw(kind, &v.to_ne_bytes())
    }
    pub fn str(&mut self, kind: u16, v: &str) -> &mut Self {
        let mut b = v.as_bytes().to_vec();
        b.push(0);
        self.raw(kind, &b)
    }
    pub fn nested(&mut self, kind: u16, inner: &Attrs) -> &mut Self {
        self.raw(kind | NLA_F_NESTED, &inner.0)
    }
}

/// Walks attributes: (type without flags, payload).
fn parse_attrs(mut b: &[u8]) -> Vec<(u16, &[u8])> {
    let mut out = Vec::new();
    while b.len() >= 4 {
        let len = u16::from_ne_bytes([b[0], b[1]]) as usize;
        let kind = u16::from_ne_bytes([b[2], b[3]]) & !NLA_F_NESTED & 0x3fff;
        if len < 4 || len > b.len() {
            break;
        }
        out.push((kind, &b[4..len]));
        let next = (len + 3) & !3;
        b = &b[next.min(b.len())..];
    }
    out
}

/// One netlink message: type, sequence number, generic-netlink command, attributes.
type Msg = (u16, u32, u8, Vec<u8>);

/// A generic netlink socket.
pub struct Genl {
    fd: OwnedFd,
    seq: u32,
}

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(r) }
}

impl Genl {
    pub fn open() -> io::Result<Genl> {
        let fd = cvt(unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, NETLINK_GENERIC) })?;
        // SAFETY: a fresh descriptor we own.
        let fd = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
        Ok(Genl { fd, seq: 1 })
    }

    fn send(&mut self, family: u16, cmd: u8, version: u8, attrs: &Attrs, flags: u16) -> io::Result<u32> {
        self.seq += 1;
        let len = 16 + 4 + attrs.0.len();
        let mut m = Vec::with_capacity(len);
        m.extend_from_slice(&(len as u32).to_ne_bytes());
        m.extend_from_slice(&family.to_ne_bytes());
        m.extend_from_slice(&(NLM_F_REQUEST | flags).to_ne_bytes());
        m.extend_from_slice(&self.seq.to_ne_bytes());
        m.extend_from_slice(&0u32.to_ne_bytes());
        m.extend_from_slice(&[cmd, version, 0, 0]);
        m.extend_from_slice(&attrs.0);
        let n = unsafe { libc::send(self.fd.as_raw_fd(), m.as_ptr().cast(), m.len(), 0) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(self.seq)
    }

    /// The next datagram, split into (type, generic-netlink cmd, attributes bytes) per
    /// message.
    fn recv(&self) -> io::Result<Vec<Msg>> {
        let mut buf = vec![0u8; 65536];
        let n = unsafe { libc::recv(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut b = &buf[..n as usize];
        let mut out = Vec::new();
        while b.len() >= 16 {
            let len = u32::from_ne_bytes(b[0..4].try_into().unwrap()) as usize;
            let kind = u16::from_ne_bytes([b[4], b[5]]);
            let seq = u32::from_ne_bytes(b[8..12].try_into().unwrap());
            if len < 16 || len > b.len() {
                break;
            }
            let body = &b[16..len];
            if kind == NLMSG_ERROR {
                let code = i32::from_ne_bytes(body[0..4].try_into().unwrap());
                out.push((kind, seq, 0, code.to_ne_bytes().to_vec()));
            } else if body.len() >= 4 {
                out.push((kind, seq, body[0], body[4..].to_vec()));
            }
            b = &b[((len + 3) & !3).min(b.len())..];
        }
        Ok(out)
    }

    /// Sends a request and waits for its acknowledgement.
    fn request(&mut self, family: u16, cmd: u8, attrs: &Attrs) -> io::Result<()> {
        let seq = self.send(family, cmd, 1, attrs, NLM_F_ACK)?;
        loop {
            for (kind, s, _, body) in self.recv()? {
                if kind == NLMSG_ERROR && s == seq {
                    let code = i32::from_ne_bytes(body[0..4].try_into().unwrap());
                    return if code == 0 { Ok(()) } else { Err(io::Error::from_raw_os_error(-code)) };
                }
            }
        }
    }

    /// The `nbd` family's id and its multicast group's id.
    pub fn nbd_family(&mut self) -> io::Result<(u16, u32)> {
        let mut a = Attrs::default();
        a.str(CTRL_ATTR_FAMILY_NAME, "nbd");
        let seq = self.send(GENL_ID_CTRL, CTRL_CMD_GETFAMILY, 1, &a, 0)?;
        loop {
            for (kind, s, _, body) in self.recv()? {
                if s != seq {
                    continue;
                }
                if kind == NLMSG_ERROR {
                    let code = i32::from_ne_bytes(body[0..4].try_into().unwrap());
                    return Err(if code == -libc::ENOENT {
                        io::Error::other("the kernel has no nbd netlink family (modprobe nbd?)")
                    } else {
                        io::Error::from_raw_os_error(-code)
                    });
                }
                let mut family = None;
                let mut group = None;
                for (t, p) in parse_attrs(&body) {
                    match t {
                        CTRL_ATTR_FAMILY_ID if p.len() >= 2 => family = Some(u16::from_ne_bytes([p[0], p[1]])),
                        CTRL_ATTR_MCAST_GROUPS => {
                            for (_, g) in parse_attrs(p) {
                                let fields = parse_attrs(g);
                                let name = fields.iter().find(|(t, _)| *t == CTRL_ATTR_MCAST_GRP_NAME).map(|(_, v)| *v);
                                let id = fields.iter().find(|(t, _)| *t == CTRL_ATTR_MCAST_GRP_ID).map(|(_, v)| *v);
                                if name.is_some_and(|n| n.starts_with(b"nbd_mc_group"))
                                    && let Some(id) = id.filter(|v| v.len() >= 4)
                                {
                                    group = Some(u32::from_ne_bytes(id[0..4].try_into().unwrap()));
                                }
                            }
                        }
                        _ => {}
                    }
                }
                return match (family, group) {
                    (Some(f), Some(g)) => Ok((f, g)),
                    _ => Err(io::Error::other("the nbd netlink family has no multicast group")),
                };
            }
        }
    }

    pub fn join(&self, group: u32) -> io::Result<()> {
        cvt(unsafe {
            libc::setsockopt(
                self.fd.as_raw_fd(),
                SOL_NETLINK,
                NETLINK_ADD_MEMBERSHIP,
                (&group as *const u32).cast(),
                std::mem::size_of::<u32>() as libc::socklen_t,
            )
        })?;
        Ok(())
    }

    fn sockets(fd: RawFd) -> Attrs {
        let mut item = Attrs::default();
        item.u32(NBD_SOCK_FD, fd as u32);
        let mut socks = Attrs::default();
        socks.nested(NBD_SOCK_ITEM, &item);
        socks
    }

    /// Connects `/dev/nbd<index>` to `sock`.
    #[allow(clippy::too_many_arguments)]
    pub fn connect(
        &mut self,
        family: u16,
        index: u32,
        size: u64,
        block_size: u64,
        server_flags: u16,
        timeout_secs: u64,
        dead_conn_timeout_secs: u64,
        sock: RawFd,
    ) -> io::Result<()> {
        let mut a = Attrs::default();
        a.u32(NBD_ATTR_INDEX, index)
            .u64(NBD_ATTR_SIZE_BYTES, size)
            .u64(NBD_ATTR_BLOCK_SIZE_BYTES, block_size)
            .u64(NBD_ATTR_SERVER_FLAGS, server_flags as u64)
            .u64(NBD_ATTR_TIMEOUT, timeout_secs)
            .u64(NBD_ATTR_DEAD_CONN_TIMEOUT, dead_conn_timeout_secs)
            .nested(NBD_ATTR_SOCKETS, &Self::sockets(sock));
        self.request(family, NBD_CMD_CONNECT, &a)
    }

    /// Gives `/dev/nbd<index>` a new socket after its link died.
    pub fn reconfigure(&mut self, family: u16, index: u32, sock: RawFd) -> io::Result<()> {
        let mut a = Attrs::default();
        a.u32(NBD_ATTR_INDEX, index).nested(NBD_ATTR_SOCKETS, &Self::sockets(sock));
        self.request(family, NBD_CMD_RECONFIGURE, &a)
    }

    /// Blocks until the kernel reports `/dev/nbd<index>`'s link dead (on a socket that
    /// has [`Genl::join`]ed the nbd group).
    pub fn wait_link_dead(&self, index: u32) -> io::Result<()> {
        loop {
            for (_, _, cmd, body) in self.recv()? {
                if cmd != NBD_CMD_LINK_DEAD {
                    continue;
                }
                let idx = parse_attrs(&body)
                    .iter()
                    .find(|(t, _)| *t == NBD_ATTR_INDEX)
                    .and_then(|(_, p)| p.get(0..4).map(|v| u32::from_ne_bytes(v.try_into().unwrap())));
                if idx.is_none_or(|i| i == index) {
                    return Ok(());
                }
            }
        }
    }
}
