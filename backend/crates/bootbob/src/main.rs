//! BootBob: a network boot server with a web UI.
//!
//! `bootbob [--config <file>]` (or `BOOTBOB_CONFIG`). With no config, it serves HTTP on
//! :8067 and TFTP on :69 from `./data` and `./ipxe`, with the DHCP listener off.

mod config;
mod doctor;
mod http;
mod model;
mod nbd;
mod state;
mod store;
mod tftp;

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
    tracing::info!(version = env!("CARGO_PKG_VERSION"), data = %config.data_dir.display(), "BootBob starting");

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
