//! BootBob: a network boot server with a web UI.
//!
//! `bootbob [--config <file>]` (or `BOOTBOB_CONFIG`). With no config, it serves the web UI
//! and API on :8067 from `./data` and `./ipxe`, and boots nothing: machines netboot only on
//! the `[[boot]]` interfaces a config names.

mod cache;
mod catalogue;
mod config;
mod doctor;
mod fetch;
mod http;
mod mdns;
mod model;
mod nbd;
mod netif;
mod rollout;
mod sparse;
mod state;
mod store;
mod stores;
mod tftp;
mod usb;

#[cfg(test)]
mod tests;

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use tracing_subscriber::EnvFilter;

use crate::config::{BootIface, Config, Service};
use crate::state::State;

/// Re-reads the host's interfaces now and then, so the UI shows VLANs and addresses as they
/// are, and boot interfaces the host brings up later are found.
async fn watch_interfaces(st: Arc<State>) {
    loop {
        tokio::time::sleep(Duration::from_secs(10)).await;
        st.refresh_netifs();
    }
}

/// Serves one boot interface: once the host has it up with an address, the DHCP listener,
/// TFTP, NBD and HTTP for booting machines, each bound to the interface so nothing that
/// arrives on another reaches them. `http_shared`: a control listener already serves its
/// HTTP.
async fn boot_interface(st: Arc<State>, b: BootIface, http_shared: bool) {
    let name = b.interface.clone();
    let mut said = false;
    let ip = loop {
        let present = st.netif(&name).is_some() || !cfg!(target_os = "linux");
        match st.boot_address(&name) {
            Some(ip) if present => break ip,
            _ => {
                if !said {
                    tracing::warn!(iface = name, "boot interface isn't up with an address yet; waiting for the host to bring it up");
                    said = true;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
                st.refresh_netifs();
            }
        }
    };
    let device = netif::device_for(&name);
    let label = st.iface_label(&name);
    tracing::info!(iface = name, label, %ip, ?device, services = ?b.services, "boot interface: serving");

    if b.serves(Service::Tftp) {
        match bootbob_tftp::bind_on(IpAddr::V4(ip), b.tftp_port, device) {
            Ok(sock) => {
                let resolver = Arc::new(tftp::Tftp { st: st.clone(), iface: name.clone() });
                let cfg = bootbob_tftp::Config { device: device.map(String::from), ..Default::default() };
                let n = name.clone();
                tokio::spawn(async move {
                    if let Err(e) = bootbob_tftp::serve(sock, resolver, cfg).await {
                        tracing::error!(iface = n, "TFTP stopped: {e}");
                    }
                });
            }
            // Port 69 needs root or CAP_NET_BIND_SERVICE. Carry on without it: iPXE over
            // HTTP still works.
            Err(e) => tracing::error!(iface = name, "TFTP: can't listen on {ip}:{}, so firmware PXE won't work: {e}", b.tftp_port),
        }
    }

    if b.serves(Service::Nbd) {
        match netif::tcp_listener(SocketAddr::from((ip, b.nbd_port)), device) {
            Ok(l) => {
                let exports = Arc::new(nbd::Nbd { st: st.clone(), iface: name.clone() });
                let live = st.nbd_live.clone();
                let n = name.clone();
                tokio::spawn(async move {
                    if let Err(e) = bootbob_nbd::serve_with(l, exports, live).await {
                        tracing::error!(iface = n, "NBD stopped: {e}");
                    }
                });
            }
            Err(e) => tracing::error!(iface = name, "NBD: can't listen on {ip}:{}, so network roots won't work: {e}", b.nbd_port),
        }
    }

    if b.serves(Service::Http) && !http_shared {
        match netif::tcp_listener(SocketAddr::from((ip, b.http_port)), device) {
            Ok(l) => {
                let app = http::boot_router(st.clone(), &name).into_make_service_with_connect_info::<SocketAddr>();
                let n = name.clone();
                tokio::spawn(async move {
                    if let Err(e) = axum::serve(l, app).await {
                        tracing::error!(iface = n, "boot HTTP stopped: {e}");
                    }
                });
            }
            Err(e) => tracing::error!(iface = name, "HTTP: can't listen on {ip}:{}, so iPXE can't boot here: {e}", b.http_port),
        }
    }

    if b.serves(Service::Listen) {
        if let Some(l) = st.listeners.write().unwrap().get_mut(&name) {
            l.running = true;
        }
        let st2 = st.clone();
        let n = name.clone();
        let res = bootbob_dhcp::listener::listen(&name, move |seen| st2.observe(&n, seen)).await;
        if let Some(l) = st.listeners.write().unwrap().get_mut(&name) {
            l.running = false;
            if let Err(e) = res {
                tracing::error!(iface = name, "DHCP listener stopped: {e}");
                l.error = Some(e.to_string());
            }
        }
    }
}

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
    let http = config.store_http();
    let l = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("NBD: can't listen on {addr}"))?;
    tracing::info!(%addr, main = config.main_url.as_deref().unwrap_or(""), "store: NBD listening");
    let exports = Arc::new(nbd::StoreExports { st: st.clone() });
    let live = st.nbd_live.clone();
    tokio::spawn(async move {
        if let Err(e) = bootbob_nbd::serve_with(l, exports, live).await {
            tracing::error!("NBD stopped: {e}");
        }
    });
    let listener = tokio::net::TcpListener::bind(http).await.with_context(|| format!("HTTP: can't listen on {http}"))?;
    tracing::info!(addr = %http, "store: HTTP listening");
    let live = st.nbd_live.clone();
    // The peer's address decides who may read a whole disk (`http::store_disk`).
    let app = http::store_router(st).into_make_service_with_connect_info::<std::net::SocketAddr>();
    axum::serve(listener, app).with_graceful_shutdown(shutdown_signal(live)).await?;
    Ok(())
}

/// Waits to be told to stop, then lets NBD connections finish what they're doing.
async fn shutdown_signal(live: Arc<bootbob_nbd::Live>) {
    stop_requested().await;
    live.drain().await;
}

async fn stop_requested() {
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

    cache::startup(&st);

    if config.mode == config::Mode::Store {
        return store(config, st).await;
    }

    tokio::spawn(watch_interfaces(st.clone()));

    // Control listeners, each bound to the interface holding its address. One that a boot
    // interface's HTTP shares (same address and port) serves both route sets.
    let mut shared = Vec::new();
    let mut servers = Vec::new();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    for &addr in &config.control.http {
        let sharer = config.boot.iter().find(|b| {
            b.serves(Service::Http) && b.http_port == addr.port() && st.boot_address(&b.interface).map(IpAddr::V4) == Some(addr.ip())
        });
        let (device, app) = match sharer {
            Some(b) => {
                shared.push(b.interface.clone());
                (Some(b.interface.clone()), http::shared_router(st.clone(), &b.interface))
            }
            None => (netif::control_device(addr.ip()), http::control_router(st.clone())),
        };
        if addr.ip().is_unspecified() {
            tracing::warn!(%addr, "control: listening on every interface, boot VLANs included; name an address to keep it to one");
        }
        let l = netif::tcp_listener(addr, device.as_deref().and_then(netif::device_for))
            .with_context(|| format!("control HTTP: can't listen on {addr}"))?;
        tracing::info!(%addr, device = device.as_deref().unwrap_or("-"), also_boot = sharer.is_some(), "control: HTTP listening");
        let mut stop = stop_rx.clone();
        servers.push(tokio::spawn(async move {
            let app = app.into_make_service_with_connect_info::<SocketAddr>();
            let r = axum::serve(l, app)
                .with_graceful_shutdown(async move {
                    let _ = stop.wait_for(|s| *s).await;
                })
                .await;
            if let Err(e) = r {
                tracing::error!(%addr, "control HTTP stopped: {e}");
            }
        }));
    }
    if config.boot.is_empty() {
        tracing::warn!("no [[boot]] interfaces in the config, so nothing can netboot");
    }
    for b in &config.boot {
        tokio::spawn(boot_interface(st.clone(), b.clone(), shared.contains(&b.interface)));
    }

    // Kept for as long as BootBob runs: dropping it stops the advertisement.
    let _mdns = match mdns::advertise(&st) {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("mDNS: can't advertise: {e}");
            None
        }
    };

    tokio::spawn(rollout::run(st.clone()));
    tokio::spawn(doctor::watch_zone(st.clone()));

    // Stop: let NBD connections finish what they're doing, then the HTTP servers.
    shutdown_signal(st.nbd_live.clone()).await;
    let _ = stop_tx.send(true);
    for s in servers {
        let _ = s.await;
    }
    Ok(())
}
