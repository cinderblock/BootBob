//! The NBD server with no policy at all, for trying it against a real kernel
//! (`tools/test-trim.sh`). Anyone can open anything, so it's only for a private test network.
//!
//!   serve_dir <dir> <address>
//!
//! Export `<name>.img` is the file itself; `<name>.cow` is an overlay of it in
//! `<dir>/overlays`. SIGTERM stops it the way BootBob stops: answer what was sent, then exit.
//!
//! `TRIM_STALL=<n>:<seconds>` holds every nth TRIM that long. With more seconds than the
//! client's request timeout, the kernel gives up on the TRIM while it's still running here
//! and comes back on a new connection, which is the case the ordering in the crate docs is
//! there for.

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bootbob_cow::Disk;
use bootbob_nbd::{Export, Exports, Live, Stats};

struct Dir {
    dir: PathBuf,
    stall: Option<Arc<Stall>>,
}

struct Stall {
    every: u64,
    hold: Duration,
    seen: AtomicU64,
}

/// A disk whose TRIMs stall.
struct Stalling {
    inner: Arc<dyn Disk>,
    stall: Arc<Stall>,
}

impl Disk for Stalling {
    fn size(&self) -> u64 {
        self.inner.size()
    }
    fn read_only(&self) -> bool {
        self.inner.read_only()
    }
    fn read_at(&self, buf: &mut [u8], off: u64) -> io::Result<()> {
        self.inner.read_at(buf, off)
    }
    fn write_at(&self, buf: &[u8], off: u64) -> io::Result<()> {
        self.inner.write_at(buf, off)
    }
    fn flush(&self) -> io::Result<()> {
        self.inner.flush()
    }
    fn can_discard(&self) -> bool {
        self.inner.can_discard()
    }
    fn discard(&self, off: u64, len: u64) -> io::Result<()> {
        let n = self.stall.seen.fetch_add(1, Ordering::Relaxed) + 1;
        if n.is_multiple_of(self.stall.every) {
            println!("stalling TRIM {n} ({len} bytes at {off})");
            std::thread::sleep(self.stall.hold);
            println!("TRIM {n} goes ahead");
        }
        self.inner.discard(off, len)
    }
}

impl Exports for Dir {
    fn open(&self, name: &str, _: SocketAddr) -> Result<Export, String> {
        let (stem, cow) = match (name.strip_suffix(".img"), name.strip_suffix(".cow")) {
            (Some(s), _) => (s, false),
            (_, Some(s)) => (s, true),
            _ => return Err(format!("no export {name:?}")),
        };
        if stem.contains(['/', '\\']) {
            return Err(format!("no export {name:?}"));
        }
        let overlay = cow.then(|| (self.dir.join("overlays"), stem.to_string()));
        Ok(Export { name: name.to_string(), path: self.dir.join(format!("{stem}.img")), read_only: false, overlay })
    }

    fn list(&self, _: SocketAddr) -> Vec<String> {
        Vec::new()
    }

    fn open_disk(&self, export: &Export) -> Result<Arc<dyn Disk>, String> {
        let inner = bootbob_nbd::open_disk(export)?;
        println!("{} opened: trim {}", export.name, inner.can_discard());
        Ok(match &self.stall {
            Some(stall) => Arc::new(Stalling { inner, stall: stall.clone() }),
            None => inner,
        })
    }

    fn connected(&self, export: &Export, peer: SocketAddr) {
        println!("{peer} connected to {}", export.name);
    }

    fn disconnected(&self, export: &Export, peer: SocketAddr, stats: Stats, why: &str) {
        println!("{peer} left {}: {why}: {stats:?}", export.name);
    }
}

async fn stop_requested() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let mut args = std::env::args().skip(1);
    let (Some(dir), Some(addr)) = (args.next(), args.next()) else {
        eprintln!("usage: serve_dir <dir> <address>");
        std::process::exit(2);
    };
    let stall = std::env::var("TRIM_STALL").ok().map(|v| {
        let (every, secs) = v.split_once(':').expect("TRIM_STALL=<n>:<seconds>");
        Arc::new(Stall {
            every: every.parse().expect("TRIM_STALL's n"),
            hold: Duration::from_secs(secs.parse().expect("TRIM_STALL's seconds")),
            seen: AtomicU64::new(0),
        })
    });
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("serving {dir} on {addr}");
    let live = Arc::new(Live::default());
    let exports = Arc::new(Dir { dir: dir.into(), stall });
    let server = tokio::spawn(bootbob_nbd::serve_with(listener, exports, live.clone()));
    stop_requested().await;
    println!("draining");
    live.drain().await;
    server.abort();
    Ok(())
}
