//! A read-only TFTP server whose files are resolved per request.
//!
//! Every read request goes through a [`Resolver`], which sees the client's address and the
//! requested path and returns the bytes to send. That is what lets BootBob hand each
//! machine a different file under the same name (the right iPXE build for its firmware, or
//! a Pi boot tree generated from its assigned image).
//!
//! Supported: RFC 1350 reads (octet; netascii is served as octet), and the option
//! extensions RFC 2347 (negotiation), 2348 (`blksize`), 2349 (`timeout`, `tsize`) and
//! 7440 (`windowsize`). Writes are refused.

mod packet;
mod transfer;

use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::net::UdpSocket;

pub use packet::{ErrorCode, Packet};

/// A read request, as the resolver sees it.
#[derive(Debug, Clone)]
pub struct Request {
    pub peer: SocketAddr,
    /// The server address the request arrived on, when known.
    pub local: SocketAddr,
    /// The path as the client sent it.
    pub raw_path: String,
    /// The cleaned path: no leading `/`, `\` turned into `/`, no empty or `.` segments.
    /// `None` when the path tried to climb out (`..`) or was empty.
    pub path: Option<String>,
}

/// The bytes to send.
#[derive(Debug)]
pub enum Content {
    Memory(Bytes),
    File { file: tokio::fs::File, size: u64 },
}

impl Content {
    pub fn size(&self) -> u64 {
        match self {
            Content::Memory(b) => b.len() as u64,
            Content::File { size, .. } => *size,
        }
    }

    /// Opens a file from disk.
    pub async fn open(path: impl AsRef<std::path::Path>) -> io::Result<Content> {
        let file = tokio::fs::File::open(path).await?;
        let size = file.metadata().await?.len();
        Ok(Content::File { file, size })
    }
}

/// What to do with a read request.
#[derive(Debug)]
pub enum Resolution {
    Serve(Content),
    NotFound,
    Denied(String),
}

/// How a transfer ended, reported back to the resolver.
#[derive(Debug, Clone)]
pub enum Outcome {
    Completed { bytes: u64, elapsed: Duration },
    Failed(String),
}

/// Decides what each read request gets.
pub trait Resolver: Send + Sync + 'static {
    fn resolve(&self, req: &Request) -> impl Future<Output = Resolution> + Send;

    /// Called once per served request when the transfer ends.
    fn finished(&self, _req: &Request, _outcome: &Outcome) {}
}

#[derive(Debug, Clone)]
pub struct Config {
    /// The largest `blksize` granted. 1468 fills a 1500-byte Ethernet frame without IP
    /// fragmentation (1500 - 20 IP - 8 UDP - 4 TFTP).
    pub max_blksize: u16,
    /// The largest `windowsize` granted.
    pub max_windowsize: u16,
    /// Retransmit timeout when the client doesn't ask for one.
    pub timeout: Duration,
    /// Retransmissions of one window before giving up.
    pub retries: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config { max_blksize: 1468, max_windowsize: 32, timeout: Duration::from_secs(1), retries: 5 }
    }
}

/// Cleans a client path. See [`Request::path`].
pub fn clean_path(raw: &str) -> Option<String> {
    let mut parts = Vec::new();
    for seg in raw.split(['/', '\\']) {
        match seg {
            "" | "." => {}
            ".." => return None,
            s => parts.push(s),
        }
    }
    if parts.is_empty() { None } else { Some(parts.join("/")) }
}

/// Serves read requests arriving on `socket` until the socket fails.
pub async fn serve<R: Resolver>(socket: UdpSocket, resolver: Arc<R>, config: Config) -> io::Result<()> {
    let local = socket.local_addr()?;
    let mut buf = vec![0u8; 2048];
    loop {
        let (n, peer) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            // A previous reply to a closed port surfaces as ConnectionReset on Windows.
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => continue,
            Err(e) => return Err(e),
        };
        match Packet::parse(&buf[..n]) {
            Ok(Packet::Read { path, mode, options }) => {
                if !mode.eq_ignore_ascii_case("octet") && !mode.eq_ignore_ascii_case("netascii") {
                    let _ = socket.send_to(&Packet::error(ErrorCode::IllegalOperation, "unsupported mode"), peer).await;
                    continue;
                }
                let req = Request { peer, local, path: clean_path(&path), raw_path: path };
                let resolver = resolver.clone();
                let config = config.clone();
                tokio::spawn(async move {
                    transfer::run(req, options, resolver, config).await;
                });
            }
            Ok(Packet::Write { .. }) => {
                let _ = socket.send_to(&Packet::error(ErrorCode::AccessViolation, "read-only server"), peer).await;
            }
            Ok(_) => {
                let _ = socket.send_to(&Packet::error(ErrorCode::UnknownTid, "no transfer here"), peer).await;
            }
            Err(e) => tracing::debug!(%peer, "bad TFTP packet: {e}"),
        }
    }
}

/// Binds a socket for [`serve`].
pub async fn bind(ip: IpAddr, port: u16) -> io::Result<UdpSocket> {
    UdpSocket::bind(SocketAddr::new(ip, port)).await
}

#[cfg(test)]
mod tests;
