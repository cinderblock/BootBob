//! BootBob: a network boot server with a web UI.
//!
//! `bootbob [--config <file>]` (or `BOOTBOB_CONFIG`). With no config, it serves HTTP on
//! :8067 and TFTP on :69 from `./data` and `./ipxe`, with the DHCP listener off.

mod config;
mod doctor;
mod http;
mod mdns;
mod model;
mod nbd;
mod state;
mod store;
mod stores;
mod tftp;
mod usb;

#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::state::State;

fn config_path() -> anyhow::Result<Option<PathBuf>> {
    let mut args = std::env::args().skip(1);
    let mut path = std::env::var_os("BOOTBOB_CONFIG").map(PathBuf::from);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" | "-c" => path = Some(args.next().context("--config needs a file")?.into()),
            "--version" | "-V" => {
                println!("bootbob {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--help" | "-h" => {
                println!("usage: bootbob [--config <file>]");
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument {other:?} (try --help)"),
        }
    }
    Ok(path)
}

/// An image store: NBD for its disk images (each opened only when the main server says
/// so) and its small HTTP API. No TFTP, DHCP listener, mDNS or web UI.
async fn store(config: Config, st: Arc<State>) -> anyhow::Result<()> {
    anyhow::ensure!(config.main_url.is_some(), "store mode needs main_url (the main BootBob server)");
    anyhow::ensure!(config.store_name.is_some(), "store mode needs store_name (what the main server's stores call it)");
    let addr = config.nbd.context("store mode needs nbd (the address machines reach its disks on)")?;
    let l = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("NBD: can't listen on {addr}"))?;
    tracing::info!(%addr, main = config.main_url.as_deref().unwrap_or(""), "store: NBD listening");
    let exports = Arc::new(nbd::StoreExports { st: st.clone() });
    let live = st.nbd_live.clone();
    tokio::spawn(async move {
        if let Err(e) = bootbob_nbd::serve_with(l, exports, live).await {
            tracing::error!("NBD stopped: {e}");
        }
    });
    let listener = tokio::net::TcpListener::bind(config.http).await.with_context(|| format!("HTTP: can't listen on {}", config.http))?;
    tracing::info!(addr = %config.http, "store: HTTP listening");
    axum::serve(listener, http::store_router(st)).with_graceful_shutdown(shutdown_signal()).await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = ctrl_c.await;
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))).init();
    let config = Config::load(config_path()?.as_deref())?;
    let st = Arc::new(State::open(config.clone()).context("opening the database")?);
    tracing::info!(version = env!("CARGO_PKG_VERSION"), data = %config.data_dir.display(), mode = ?config.mode, "BootBob starting");

    if config.mode == config::Mode::Store {
        return store(config, st).await;
    }

    if let Some(addr) = config.tftp {
        match bootbob_tftp::bind(addr.ip(), addr.port()).await {
            Ok(sock) => {
                tracing::info!(%addr, "TFTP: listening");
                let resolver = Arc::new(tftp::Tftp { st: st.clone() });
                tokio::spawn(async move {
                    if let Err(e) = bootbob_tftp::serve(sock, resolver, bootbob_tftp::Config::default()).await {
                        tracing::error!("TFTP stopped: {e}");
                    }
                });
            }
            // Port 69 needs root or CAP_NET_BIND_SERVICE. Carry on without it: iPXE over
            // HTTP still works.
            Err(e) => tracing::error!(%addr, "TFTP: can't listen, so firmware PXE won't work: {e}"),
        }
    }

    if let Some(addr) = config.nbd {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => {
                tracing::info!(%addr, "NBD: listening");
                let exports = Arc::new(nbd::Nbd { st: st.clone() });
                let live = st.nbd_live.clone();
                tokio::spawn(async move {
                    if let Err(e) = bootbob_nbd::serve_with(l, exports, live).await {
                        tracing::error!("NBD stopped: {e}");
                    }
                });
            }
            Err(e) => tracing::error!(%addr, "NBD: can't listen, so network roots won't work: {e}"),
        }
    }

    // Kept for as long as BootBob runs: dropping it stops the advertisement.
    let _mdns = match mdns::advertise(&config) {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("mDNS: can't advertise: {e}");
            None
        }
    };

    if let Some(iface) = config.interface.clone() {
        let st2 = st.clone();
        tokio::spawn(async move {
            st2.listener.write().unwrap().running = true;
            let st3 = st2.clone();
            let res = bootbob_dhcp::listener::listen(&iface, move |seen| st3.observe(seen)).await;
            let mut l = st2.listener.write().unwrap();
            l.running = false;
            if let Err(e) = res {
                tracing::error!(iface, "DHCP listener stopped: {e}");
                l.error = Some(e.to_string());
            }
        });
    }

    let listener = tokio::net::TcpListener::bind(config.http).await.with_context(|| format!("HTTP: can't listen on {}", config.http))?;
    tracing::info!(addr = %config.http, "HTTP: listening");
    axum::serve(listener, http::router(st)).with_graceful_shutdown(shutdown_signal()).await?;
    Ok(())
}
