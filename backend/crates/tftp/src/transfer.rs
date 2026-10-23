//! One read transfer, from option negotiation to the last ACK.

use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use tokio::io::AsyncReadExt;
use tokio::net::UdpSocket;
use tokio::time::timeout;

use crate::packet::{ErrorCode, Packet};
use crate::{Config, Content, Outcome, Request, Resolution, Resolver};

/// Options as negotiated for this transfer.
struct Negotiated {
    blksize: usize,
    windowsize: usize,
    timeout: Duration,
    /// The OACK to send, or `None` when the client asked for nothing we accept.
    oack: Option<Vec<(String, String)>>,
}

fn negotiate(requested: &[(String, String)], size: u64, config: &Config) -> Negotiated {
    let mut n = Negotiated { blksize: 512, windowsize: 1, timeout: config.timeout, oack: None };
    let mut acked = Vec::new();
    for (k, v) in requested {
        let Ok(val) = v.parse::<u64>() else { continue };
        match k.as_str() {
            "blksize" if (8..=65464).contains(&val) => {
                let b = val.min(config.max_blksize as u64);
                n.blksize = b as usize;
                acked.push((k.clone(), b.to_string()));
            }
            "windowsize" if (1..=65535).contains(&val) => {
                let w = val.min(config.max_windowsize as u64);
                n.windowsize = w as usize;
                acked.push((k.clone(), w.to_string()));
            }
            "timeout" if (1..=255).contains(&val) => {
                n.timeout = Duration::from_secs(val);
                acked.push((k.clone(), v.clone()));
            }
            "tsize" => acked.push((k.clone(), size.to_string())),
            _ => {}
        }
    }
    if !acked.is_empty() {
        n.oack = Some(acked);
    }
    n
}

/// Reads the content one block at a time.
struct Blocks {
    content: Content,
    offset: u64,
}

impl Blocks {
    async fn next(&mut self, blksize: usize) -> io::Result<Bytes> {
        match &mut self.content {
            Content::Memory(b) => {
                let start = (self.offset as usize).min(b.len());
                let end = (start + blksize).min(b.len());
                self.offset = end as u64;
                Ok(b.slice(start..end))
            }
            Content::File { file, .. } => {
                let mut buf = BytesMut::zeroed(blksize);
                let mut filled = 0;
                while filled < blksize {
                    let n = file.read(&mut buf[filled..]).await?;
                    if n == 0 {
                        break;
                    }
                    filled += n;
                }
                buf.truncate(filled);
                self.offset += filled as u64;
                Ok(buf.freeze())
            }
        }
    }
}

pub(crate) async fn run<R: Resolver>(req: Request, options: Vec<(String, String)>, resolver: Arc<R>, config: Config) {
    let sock = match UdpSocket::bind(SocketAddr::new(req.local.ip(), 0)).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(peer = %req.peer, "TFTP: can't open a transfer socket: {e}");
            return;
        }
    };
    if let Err(e) = sock.connect(req.peer).await {
        tracing::warn!(peer = %req.peer, "TFTP: can't connect the transfer socket: {e}");
        return;
    }
    let content = match resolver.resolve(&req).await {
        Resolution::Serve(c) => c,
        Resolution::NotFound => {
            tracing::debug!(peer = %req.peer, path = %req.raw_path, "TFTP: not found");
            let _ = sock.send(&Packet::error(ErrorCode::FileNotFound, "file not found")).await;
            return;
        }
        Resolution::Denied(why) => {
            tracing::debug!(peer = %req.peer, path = %req.raw_path, "TFTP: denied: {why}");
            let _ = sock.send(&Packet::error(ErrorCode::AccessViolation, &why)).await;
            return;
        }
    };
    let started = Instant::now();
    let outcome = match send(&sock, content, &options, &config).await {
        Ok(bytes) => Outcome::Completed { bytes, elapsed: started.elapsed() },
        Err(o) => o,
    };
    match &outcome {
        Outcome::Completed { bytes, elapsed } => {
            tracing::debug!(peer = %req.peer, path = %req.raw_path, bytes, ?elapsed, "TFTP: sent")
        }
        Outcome::Failed(why) => tracing::info!(peer = %req.peer, path = %req.raw_path, "TFTP: failed: {why}"),
        Outcome::Cancelled(why) => tracing::debug!(peer = %req.peer, path = %req.raw_path, "TFTP: cancelled by the client: {why}"),
    }
    resolver.finished(&req, &outcome);
}

/// What the client sent back while we waited.
enum Reply {
    Ack(u16),
    Error(String),
    Timeout,
}

async fn wait(sock: &UdpSocket, dur: Duration, buf: &mut [u8]) -> Result<Reply, String> {
    let deadline = Instant::now() + dur;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match timeout(left, sock.recv(buf)).await {
            Err(_) => return Ok(Reply::Timeout),
            Ok(Err(e)) if e.kind() == io::ErrorKind::ConnectionReset => {
                return Err("the client went away (port unreachable)".into());
            }
            Ok(Err(e)) => return Err(format!("receive failed: {e}")),
            Ok(Ok(n)) => match Packet::parse(&buf[..n]) {
                Ok(Packet::Ack { block }) => return Ok(Reply::Ack(block)),
                Ok(Packet::Error { code, message }) => return Ok(Reply::Error(format!("client error {code}: {message}"))),
                // Anything else on this port is noise; keep waiting.
                _ => {}
            },
        }
    }
}

async fn send(sock: &UdpSocket, content: Content, options: &[(String, String)], config: &Config) -> Result<u64, Outcome> {
    let size = content.size();
    let n = negotiate(options, size, config);
    let mut buf = vec![0u8; 1024];
    let failed = Outcome::Failed;

    if let Some(oack) = &n.oack {
        let pkt = Packet::oack(oack);
        let mut tries = 0;
        loop {
            sock.send(&pkt).await.map_err(|e| failed(format!("send failed: {e}")))?;
            match wait(sock, n.timeout, &mut buf).await.map_err(failed)? {
                Reply::Ack(0) => break,
                Reply::Ack(_) => {}
                // UEFI firmware asks once just to learn the size (tsize), then aborts
                // and asks again. Not a failure.
                Reply::Error(e) => return Err(Outcome::Cancelled(e)),
                Reply::Timeout => {
                    tries += 1;
                    if tries > config.retries {
                        return Err(failed("no ACK for the option acknowledgement".into()));
                    }
                }
            }
        }
    }
    send_data(sock, content, &n, config, &mut buf).await.map_err(failed)
}

async fn send_data(sock: &UdpSocket, content: Content, n: &Negotiated, config: &Config, buf: &mut [u8]) -> Result<u64, String> {
    let io_err = |e: io::Error| format!("send failed: {e}");
    let mut blocks = Blocks { content, offset: 0 };
    // Blocks base..next have been sent and not yet acknowledged; `window` holds them.
    // Block indices are absolute (from 1); the wire number is the index mod 65536, so a
    // transfer past block 65535 rolls over to 0, as iPXE and the Pi bootloader expect.
    let mut base: u64 = 1;
    let mut next: u64 = 1;
    let mut window: VecDeque<Bytes> = VecDeque::new();
    let mut last_sent = false;
    let mut tries = 0;
    loop {
        while !last_sent && (next - base) < n.windowsize as u64 {
            let block = blocks.next(n.blksize).await.map_err(|e| format!("read failed: {e}"))?;
            last_sent = block.len() < n.blksize;
            sock.send(&Packet::data(next as u16, &block)).await.map_err(io_err)?;
            window.push_back(block);
            next += 1;
        }
        match wait(sock, n.timeout, buf).await? {
            Reply::Ack(num) => {
                // Find the newest in-flight block with this wire number. Older or duplicate
                // ACKs match nothing and are ignored, which avoids the Sorcerer's Apprentice
                // bug of answering every duplicate with a duplicate.
                let acked = (base..next).rev().find(|&i| i as u16 == num);
                let Some(acked) = acked else { continue };
                for _ in base..=acked {
                    window.pop_front();
                }
                base = acked + 1;
                tries = 0;
                if last_sent && base == next {
                    return Ok(blocks.offset);
                }
                // A partial ACK means the client missed something after `acked`: resend the
                // rest of the window (RFC 7440).
                for (i, block) in window.iter().enumerate() {
                    sock.send(&Packet::data((base + i as u64) as u16, block)).await.map_err(io_err)?;
                }
            }
            Reply::Error(e) => return Err(e),
            Reply::Timeout => {
                tries += 1;
                if tries > config.retries {
                    return Err(format!("timed out at block {base}"));
                }
                for (i, block) in window.iter().enumerate() {
                    sock.send(&Packet::data((base + i as u64) as u16, block)).await.map_err(io_err)?;
                }
            }
        }
    }
}
