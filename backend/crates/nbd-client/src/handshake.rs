//! The client half of the NBD fixed-newstyle handshake: connect, ask for an export with
//! `NBD_OPT_GO`, and learn its size and transmission flags. After it, the socket is
//! handed to the kernel, which speaks the transmission phase itself.

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

const NBDMAGIC: u64 = 0x4e42_444d_4147_4943;
const IHAVEOPT: u64 = 0x4948_4156_454f_5054;
const OPT_REPLY_MAGIC: u64 = 0x0003_e889_0455_65a9;
const FLAG_FIXED_NEWSTYLE: u16 = 1;
const FLAG_NO_ZEROES: u16 = 2;
const OPT_GO: u32 = 7;
const REP_ACK: u32 = 1;
const REP_INFO: u32 = 3;
const INFO_EXPORT: u16 = 0;

/// An export, negotiated and ready for the kernel.
pub struct Negotiated {
    pub sock: TcpStream,
    pub size: u64,
    /// The transmission flags (`NBD_FLAG_*`), as the kernel wants them.
    pub flags: u16,
}

fn err(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

fn u32_from(s: &mut TcpStream) -> io::Result<u32> {
    let mut b = [0u8; 4];
    s.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}

fn u64_from(s: &mut TcpStream) -> io::Result<u64> {
    let mut b = [0u8; 8];
    s.read_exact(&mut b)?;
    Ok(u64::from_be_bytes(b))
}

/// Connects to `host:port` and negotiates `export`. `timeout` bounds the connect and
/// each read of the handshake (the kernel sets its own timeouts afterwards).
pub fn negotiate(host: &str, port: u16, export: &str, timeout: Duration) -> io::Result<Negotiated> {
    let addr = (host, port).to_socket_addrs()?.next().ok_or_else(|| err(format!("can't resolve {host}")))?;
    let mut s = TcpStream::connect_timeout(&addr, timeout)?;
    s.set_read_timeout(Some(timeout))?;
    s.set_nodelay(true)?;

    if u64_from(&mut s)? != NBDMAGIC || u64_from(&mut s)? != IHAVEOPT {
        return Err(err("not an NBD server (or not a newstyle one)"));
    }
    let mut f = [0u8; 2];
    s.read_exact(&mut f)?;
    if u16::from_be_bytes(f) & FLAG_FIXED_NEWSTYLE == 0 {
        return Err(err("the server doesn't speak fixed newstyle"));
    }
    s.write_all(&u32::from(FLAG_FIXED_NEWSTYLE | FLAG_NO_ZEROES).to_be_bytes())?;

    // NBD_OPT_GO: name, then no information requests.
    let mut data = (export.len() as u32).to_be_bytes().to_vec();
    data.extend_from_slice(export.as_bytes());
    data.extend_from_slice(&0u16.to_be_bytes());
    let mut msg = IHAVEOPT.to_be_bytes().to_vec();
    msg.extend_from_slice(&OPT_GO.to_be_bytes());
    msg.extend_from_slice(&(data.len() as u32).to_be_bytes());
    msg.extend_from_slice(&data);
    s.write_all(&msg)?;

    let mut info = None;
    loop {
        if u64_from(&mut s)? != OPT_REPLY_MAGIC {
            return Err(err("bad option reply"));
        }
        let _opt = u32_from(&mut s)?;
        let kind = u32_from(&mut s)?;
        let len = u32_from(&mut s)?;
        if len > 64 << 10 {
            return Err(err("option reply too long"));
        }
        let mut d = vec![0u8; len as usize];
        s.read_exact(&mut d)?;
        match kind {
            REP_INFO if d.len() >= 12 && u16_at(&d, 0) == INFO_EXPORT => {
                info = Some((u64::from_be_bytes(d[2..10].try_into().unwrap()), u16_at(&d, 10)));
            }
            REP_INFO => {}
            REP_ACK => break,
            k if k & 0x8000_0000 != 0 => {
                let why = String::from_utf8_lossy(&d);
                return Err(err(format!("the server refused export {export:?}: {why}")));
            }
            k => return Err(err(format!("unexpected option reply {k:#x}"))),
        }
    }
    let (size, flags) = info.ok_or_else(|| err("the server didn't say the export's size"))?;
    s.set_read_timeout(None)?;
    Ok(Negotiated { sock: s, size, flags })
}
