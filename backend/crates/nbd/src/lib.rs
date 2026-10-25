//! An NBD server: disk image files served as network block devices.
//!
//! This is how a diskless machine gets its root filesystem. Its initramfs runs
//! `nbd-client`, which connects here and asks for an export by name; the kernel then sees
//! `/dev/nbd0` as an ordinary disk, partitions and all.
//!
//! The protocol is the fixed-newstyle handshake (`NBD_OPT_GO`, `NBD_OPT_INFO`,
//! `NBD_OPT_EXPORT_NAME`, `NBD_OPT_LIST`, `NBD_OPT_ABORT`) and simple replies for
//! `READ`, `WRITE`, `FLUSH` and `DISC`, with `FUA` on writes. Requests on one connection
//! are handled in order; writes are durable once acknowledged with `FUA`, or after a
//! `FLUSH`.
//!
//! Which client may use which export is the [`Exports`] implementation's call. One
//! export has one connection at a time: a new connection to an export ends the old one,
//! because the usual reason for a second connection is that the machine rebooted
//! without saying goodbye, and its old TCP connection is dead but not yet noticed.

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

const NBDMAGIC: u64 = 0x4e42_444d_4147_4943; // "NBDMAGIC"
const IHAVEOPT: u64 = 0x4948_4156_454f_5054; // "IHAVEOPT"
const OPT_REPLY_MAGIC: u64 = 0x0003_e889_0455_65a9;
const REQUEST_MAGIC: u32 = 0x2560_9513;
const SIMPLE_REPLY_MAGIC: u32 = 0x6744_6698;

const FLAG_FIXED_NEWSTYLE: u16 = 1;
const FLAG_NO_ZEROES: u16 = 2;

const OPT_EXPORT_NAME: u32 = 1;
const OPT_ABORT: u32 = 2;
const OPT_LIST: u32 = 3;
const OPT_INFO: u32 = 6;
const OPT_GO: u32 = 7;

const REP_ACK: u32 = 1;
const REP_SERVER: u32 = 2;
const REP_INFO: u32 = 3;
const REP_ERR_UNSUP: u32 = 0x8000_0001;
const REP_ERR_POLICY: u32 = 0x8000_0002;
const REP_ERR_INVALID: u32 = 0x8000_0003;
const REP_ERR_UNKNOWN: u32 = 0x8000_0006;

const INFO_EXPORT: u16 = 0;
const INFO_BLOCK_SIZE: u16 = 3;

const TFLAG_HAS_FLAGS: u16 = 1;
const TFLAG_READ_ONLY: u16 = 2;
const TFLAG_SEND_FLUSH: u16 = 4;
const TFLAG_SEND_FUA: u16 = 8;

const CMD_READ: u16 = 0;
const CMD_WRITE: u16 = 1;
const CMD_DISC: u16 = 2;
const CMD_FLUSH: u16 = 3;
const CMD_FLAG_FUA: u16 = 1;

const EPERM: u32 = 1;
const EIO: u32 = 5;
const EINVAL: u32 = 22;
const ENOSPC: u32 = 28;

/// The largest single read or write accepted (the Linux client sends at most 32 MiB).
pub const MAX_REQUEST: u32 = 32 << 20;
/// Option data larger than this is a broken or hostile client.
const MAX_OPTION: u32 = 64 << 10;

/// An export a client may use.
#[derive(Debug, Clone)]
pub struct Export {
    pub name: String,
    pub path: PathBuf,
    pub read_only: bool,
}

/// Bytes moved on one connection, reported when it ends.
#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    pub read: u64,
    pub written: u64,
}

/// Decides which client gets which export.
pub trait Exports: Send + Sync + 'static {
    /// The export `name` for `peer`, or why not (shown to the client as a policy error).
    fn open(&self, name: &str, peer: SocketAddr) -> Result<Export, String>;
    /// The export names `peer` may use (`NBD_OPT_LIST`).
    fn list(&self, peer: SocketAddr) -> Vec<String>;
    fn connected(&self, _export: &Export, _peer: SocketAddr) {}
    fn disconnected(&self, _export: &Export, _peer: SocketAddr, _stats: Stats, _why: &str) {}
}

/// The live connection per export file, so a new one can end the old.
#[derive(Default)]
struct Live {
    next: AtomicU64,
    by_path: Mutex<HashMap<PathBuf, (u64, oneshot::Sender<()>)>>,
}

/// Serves NBD on `listener` until it fails.
pub async fn serve<E: Exports>(listener: TcpListener, exports: Arc<E>) -> io::Result<()> {
    let live = Arc::new(Live::default());
    loop {
        let (sock, peer) = listener.accept().await?;
        let (exports, live) = (exports.clone(), live.clone());
        tokio::spawn(async move {
            if let Err(e) = connection(sock, peer, exports, live).await {
                tracing::debug!(%peer, "NBD connection ended: {e}");
            }
        });
    }
}

async fn connection<E: Exports>(mut sock: TcpStream, peer: SocketAddr, exports: Arc<E>, live: Arc<Live>) -> io::Result<()> {
    sock.set_nodelay(true)?;
    let Some((export, file, size)) = negotiate(&mut sock, peer, &*exports).await? else { return Ok(()) };

    let id = live.next.fetch_add(1, Ordering::Relaxed);
    let (kick_tx, mut kick_rx) = oneshot::channel();
    if let Some((_, old)) = live.by_path.lock().unwrap().insert(export.path.clone(), (id, kick_tx)) {
        let _ = old.send(());
    }
    exports.connected(&export, peer);
    tracing::info!(%peer, export = export.name, size, "NBD export in use");

    let mut stats = Stats::default();
    let result = tokio::select! {
        r = transmission(&mut sock, &file, size, export.read_only, &mut stats) => r,
        _ = &mut kick_rx => Err(io::Error::other("replaced by a newer connection to the same export")),
    };
    // Whatever happened, what was written is on disk before anyone else gets the export.
    let f = file.clone();
    let _ = tokio::task::spawn_blocking(move || f.sync_data()).await;
    {
        let mut by = live.by_path.lock().unwrap();
        if by.get(&export.path).is_some_and(|(i, _)| *i == id) {
            by.remove(&export.path);
        }
    }
    let why = match &result {
        Ok(()) => "disconnected".to_string(),
        Err(e) => e.to_string(),
    };
    exports.disconnected(&export, peer, stats, &why);
    result
}

async fn opt_reply(sock: &mut TcpStream, opt: u32, kind: u32, data: &[u8]) -> io::Result<()> {
    let mut b = Vec::with_capacity(20 + data.len());
    b.extend_from_slice(&OPT_REPLY_MAGIC.to_be_bytes());
    b.extend_from_slice(&opt.to_be_bytes());
    b.extend_from_slice(&kind.to_be_bytes());
    b.extend_from_slice(&(data.len() as u32).to_be_bytes());
    b.extend_from_slice(data);
    sock.write_all(&b).await
}

fn tflags(read_only: bool) -> u16 {
    TFLAG_HAS_FLAGS | TFLAG_SEND_FLUSH | TFLAG_SEND_FUA | if read_only { TFLAG_READ_ONLY } else { 0 }
}

/// Opens an export's file. `Err` is the message for the client.
fn open_file(export: &Export) -> Result<(Arc<File>, u64), String> {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(!export.read_only)
        .open(&export.path)
        .map_err(|e| format!("can't open {}: {e}", export.name))?;
    let size = f.metadata().map_err(|e| e.to_string())?.len();
    Ok((Arc::new(f), size))
}

/// The handshake. `None`: the client went away or aborted without choosing an export.
async fn negotiate<E: Exports>(sock: &mut TcpStream, peer: SocketAddr, exports: &E) -> io::Result<Option<(Export, Arc<File>, u64)>> {
    let mut hello = Vec::with_capacity(18);
    hello.extend_from_slice(&NBDMAGIC.to_be_bytes());
    hello.extend_from_slice(&IHAVEOPT.to_be_bytes());
    hello.extend_from_slice(&(FLAG_FIXED_NEWSTYLE | FLAG_NO_ZEROES).to_be_bytes());
    sock.write_all(&hello).await?;
    let client = sock.read_u32().await?;
    if client & !u32::from(FLAG_FIXED_NEWSTYLE | FLAG_NO_ZEROES) != 0 {
        return Err(io::Error::other(format!("client flags {client:#x} we don't know")));
    }
    let no_zeroes = client & u32::from(FLAG_NO_ZEROES) != 0;

    loop {
        if sock.read_u64().await? != IHAVEOPT {
            return Err(io::Error::other("bad option magic"));
        }
        let opt = sock.read_u32().await?;
        let len = sock.read_u32().await?;
        if len > MAX_OPTION {
            return Err(io::Error::other("option too long"));
        }
        let mut data = vec![0u8; len as usize];
        sock.read_exact(&mut data).await?;

        match opt {
            OPT_EXPORT_NAME => {
                // The old way: no way to refuse politely, so a refusal closes the connection.
                let name = String::from_utf8_lossy(&data).into_owned();
                let export = match exports.open(&name, peer) {
                    Ok(e) => e,
                    Err(why) => {
                        tracing::info!(%peer, name, "NBD export refused: {why}");
                        return Ok(None);
                    }
                };
                let Ok((file, size)) = open_file(&export) else { return Ok(None) };
                let mut b = Vec::with_capacity(134);
                b.extend_from_slice(&size.to_be_bytes());
                b.extend_from_slice(&tflags(export.read_only).to_be_bytes());
                if !no_zeroes {
                    b.extend_from_slice(&[0u8; 124]);
                }
                sock.write_all(&b).await?;
                return Ok(Some((export, file, size)));
            }
            OPT_ABORT => {
                let _ = opt_reply(sock, opt, REP_ACK, &[]).await;
                return Ok(None);
            }
            OPT_LIST => {
                if !data.is_empty() {
                    opt_reply(sock, opt, REP_ERR_INVALID, b"NBD_OPT_LIST takes no data").await?;
                    continue;
                }
                for name in exports.list(peer) {
                    let mut d = (name.len() as u32).to_be_bytes().to_vec();
                    d.extend_from_slice(name.as_bytes());
                    opt_reply(sock, opt, REP_SERVER, &d).await?;
                }
                opt_reply(sock, opt, REP_ACK, &[]).await?;
            }
            OPT_INFO | OPT_GO => {
                let parsed = (|| {
                    let n = u32::from_be_bytes(data.get(0..4)?.try_into().ok()?) as usize;
                    let name = String::from_utf8_lossy(data.get(4..4 + n)?).into_owned();
                    let count = u16::from_be_bytes(data.get(4 + n..6 + n)?.try_into().ok()?) as usize;
                    (data.len() == 6 + n + 2 * count).then_some(name)
                })();
                let Some(name) = parsed else {
                    opt_reply(sock, opt, REP_ERR_INVALID, b"malformed request").await?;
                    continue;
                };
                let export = match exports.open(&name, peer) {
                    Ok(e) => e,
                    Err(why) => {
                        tracing::info!(%peer, name, "NBD export refused: {why}");
                        let kind = if why.contains("no export") { REP_ERR_UNKNOWN } else { REP_ERR_POLICY };
                        opt_reply(sock, opt, kind, why.as_bytes()).await?;
                        continue;
                    }
                };
                let (file, size) = match open_file(&export) {
                    Ok(v) => v,
                    Err(why) => {
                        opt_reply(sock, opt, REP_ERR_UNKNOWN, why.as_bytes()).await?;
                        continue;
                    }
                };
                let mut info = INFO_EXPORT.to_be_bytes().to_vec();
                info.extend_from_slice(&size.to_be_bytes());
                info.extend_from_slice(&tflags(export.read_only).to_be_bytes());
                opt_reply(sock, opt, REP_INFO, &info).await?;
                let mut bs = INFO_BLOCK_SIZE.to_be_bytes().to_vec();
                for v in [1u32, 4096, MAX_REQUEST] {
                    bs.extend_from_slice(&v.to_be_bytes());
                }
                opt_reply(sock, opt, REP_INFO, &bs).await?;
                opt_reply(sock, opt, REP_ACK, &[]).await?;
                if opt == OPT_GO {
                    return Ok(Some((export, file, size)));
                }
            }
            _ => opt_reply(sock, opt, REP_ERR_UNSUP, b"not supported").await?,
        }
    }
}

#[cfg(unix)]
fn read_at(f: &File, buf: &mut [u8], off: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.read_exact_at(buf, off)
}

#[cfg(unix)]
fn write_at(f: &File, buf: &[u8], off: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.write_all_at(buf, off)
}

#[cfg(windows)]
fn read_at(f: &File, mut buf: &mut [u8], mut off: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match f.seek_read(buf, off)? {
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            n => {
                buf = &mut buf[n..];
                off += n as u64;
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn write_at(f: &File, mut buf: &[u8], mut off: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = f.seek_write(buf, off)?;
        buf = &buf[n..];
        off += n as u64;
    }
    Ok(())
}

async fn reply(sock: &mut TcpStream, error: u32, handle: u64, data: &[u8]) -> io::Result<()> {
    let mut b = Vec::with_capacity(16 + data.len());
    b.extend_from_slice(&SIMPLE_REPLY_MAGIC.to_be_bytes());
    b.extend_from_slice(&error.to_be_bytes());
    b.extend_from_slice(&handle.to_be_bytes());
    b.extend_from_slice(data);
    sock.write_all(&b).await
}

fn errno(e: &io::Error) -> u32 {
    match e.raw_os_error() {
        Some(n) if n > 0 => n as u32,
        _ => EIO,
    }
}

async fn transmission(sock: &mut TcpStream, file: &Arc<File>, size: u64, read_only: bool, stats: &mut Stats) -> io::Result<()> {
    let mut hdr = [0u8; 28];
    loop {
        sock.read_exact(&mut hdr).await?;
        let magic = u32::from_be_bytes(hdr[0..4].try_into().unwrap());
        if magic != REQUEST_MAGIC {
            return Err(io::Error::other(format!("bad request magic {magic:#x}")));
        }
        let flags = u16::from_be_bytes(hdr[4..6].try_into().unwrap());
        let cmd = u16::from_be_bytes(hdr[6..8].try_into().unwrap());
        let handle = u64::from_be_bytes(hdr[8..16].try_into().unwrap());
        let offset = u64::from_be_bytes(hdr[16..24].try_into().unwrap());
        let len = u32::from_be_bytes(hdr[24..28].try_into().unwrap());
        let in_bounds = offset.checked_add(len as u64).is_some_and(|end| end <= size);

        match cmd {
            CMD_READ => {
                if !in_bounds || len > MAX_REQUEST {
                    reply(sock, EINVAL, handle, &[]).await?;
                    continue;
                }
                let f = file.clone();
                let r = tokio::task::spawn_blocking(move || {
                    let mut buf = vec![0u8; len as usize];
                    read_at(&f, &mut buf, offset).map(|_| buf)
                })
                .await
                .map_err(io::Error::other)?;
                match r {
                    Ok(buf) => {
                        stats.read += len as u64;
                        reply(sock, 0, handle, &buf).await?;
                    }
                    Err(e) => reply(sock, errno(&e), handle, &[]).await?,
                }
            }
            CMD_WRITE => {
                if len > MAX_REQUEST {
                    return Err(io::Error::other("write too large"));
                }
                // Always take the payload off the wire, or the stream loses its place.
                let mut buf = vec![0u8; len as usize];
                sock.read_exact(&mut buf).await?;
                if read_only {
                    reply(sock, EPERM, handle, &[]).await?;
                    continue;
                }
                if !in_bounds {
                    reply(sock, ENOSPC, handle, &[]).await?;
                    continue;
                }
                let f = file.clone();
                let fua = flags & CMD_FLAG_FUA != 0;
                let r = tokio::task::spawn_blocking(move || {
                    write_at(&f, &buf, offset)?;
                    if fua { f.sync_data() } else { Ok(()) }
                })
                .await
                .map_err(io::Error::other)?;
                match r {
                    Ok(()) => {
                        stats.written += len as u64;
                        reply(sock, 0, handle, &[]).await?;
                    }
                    Err(e) => reply(sock, errno(&e), handle, &[]).await?,
                }
            }
            CMD_FLUSH => {
                let f = file.clone();
                let r = tokio::task::spawn_blocking(move || f.sync_data()).await.map_err(io::Error::other)?;
                reply(sock, r.as_ref().map(|_| 0).unwrap_or_else(errno), handle, &[]).await?;
            }
            CMD_DISC => return Ok(()),
            _ => reply(sock, EINVAL, handle, &[]).await?,
        }
    }
}

#[cfg(test)]
mod tests;
