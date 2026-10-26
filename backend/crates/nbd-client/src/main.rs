//! `bootbob-nbd-client`: an NBD client for network root filesystems that rides out the
//! server restarting.
//!
//! Debian's `nbd-client` sets the device up and exits, so when the server goes away (a
//! BootBob upgrade, a reboot of the server) nothing reconnects: in-flight I/O fails and
//! the root filesystem drops to read-only. This client connects the device through
//! netlink with a long dead-connection timeout, so the kernel *holds* I/O while the
//! server is gone, and then stays running to hand the kernel a new connection as soon as
//! the server is back. To the machine, a server restart is a pause.
//!
//! It takes Debian's `nbd-client` arguments, so the NBD root script in Debian's
//! initramfs can use it unchanged: put it in the initramfs and add
//! `NBDCLIENT=/usr/sbin/bootbob-nbd-client` to the kernel command line (the kernel hands
//! unknown `NAME=value` arguments to init as environment variables).
//!
//!   bootbob-nbd-client <host> -N <export> [port] /dev/nbdX [-b <block size>] [...]
//!
//! Other `nbd-client` flags (`-swap`, `-persist`, `-systemd-mark`, …) are accepted and
//! ignored: this client always persists, and always marks itself for systemd.

// Off Linux it only builds (for the workspace tests); nothing in it runs.
#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]

#[cfg(target_os = "linux")]
mod netlink;

mod handshake;

use std::time::Duration;

/// How long the kernel holds I/O waiting for the server: an hour. Long enough for a
/// server upgrade or reboot; a dead server for longer than that is a real failure.
const DEAD_CONN_TIMEOUT: u64 = 3600;
/// A request unanswered this long marks the connection dead (and is retried on the next
/// one, not failed).
const REQUEST_TIMEOUT: u64 = 30;

/// Logs a line. Never fails and never panics: the reconnector's stderr is whatever the
/// initramfs left it (it can be a pipe that's gone after the switch to the real root),
/// and `eprintln!` panics when stderr fails, which once killed the reconnector the moment
/// it tried to say the connection was lost. The reconnector points stderr at the kernel
/// log (`/dev/kmsg`), so these lines land in `dmesg` and on the console.
macro_rules! log {
    ($($t:tt)*) => {{
        use std::io::Write;
        // One write per line: on /dev/kmsg every write is a separate log record.
        let line = format!("bootbob-nbd-client: {}\n", format_args!($($t)*));
        let _ = std::io::stderr().write_all(line.as_bytes());
    }};
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Args {
    host: String,
    export: String,
    port: u16,
    index: u32,
    block_size: u64,
    /// Internal: run as the resident reconnector.
    monitor: bool,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut host = None;
    let mut export = None;
    let mut port = 10809u16;
    let mut device = None;
    let mut block_size = 512u64;
    let mut monitor = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-N" | "-name" => export = Some(it.next().ok_or("-N needs an export name")?.clone()),
            "-b" | "-block-size" => {
                block_size = it.next().ok_or("-b needs a size")?.parse().map_err(|_| "bad block size")?;
            }
            "-t" | "-timeout" => {
                it.next();
            }
            "--monitor" => monitor = true,
            a if a.starts_with('-') => {} // -swap, -persist, -systemd-mark, -nofork, …
            a if a.starts_with("/dev/nbd") => device = Some(a.to_string()),
            a if host.is_some() && a.parse::<u16>().is_ok() => port = a.parse().unwrap(),
            a if host.is_none() => host = Some(a.to_string()),
            a => return Err(format!("unexpected argument {a:?}")),
        }
    }
    let device = device.ok_or("no /dev/nbdX device given")?;
    let index = device["/dev/nbd".len()..].parse().map_err(|_| format!("bad device {device}"))?;
    if !matches!(block_size, 512 | 1024 | 2048 | 4096) {
        return Err("block size must be 512, 1024, 2048 or 4096".into());
    }
    Ok(Args { host: host.ok_or("no server given")?, export: export.ok_or("no export name given (-N)")?, port, index, block_size, monitor })
}

#[cfg(target_os = "linux")]
fn run(a: &Args) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;

    let mut nl = netlink::Genl::open().map_err(|e| format!("netlink: {e}"))?;
    let (family, group) = nl.nbd_family().map_err(|e| e.to_string())?;

    if a.monitor {
        // The resident half: wait for the link to die, reconnect, repeat.
        nl.join(group).map_err(|e| format!("joining the nbd netlink group: {e}"))?;
        loop {
            nl.wait_link_dead(a.index).map_err(|e| format!("netlink: {e}"))?;
            log!("/dev/nbd{}: connection to {} lost; reconnecting", a.index, a.host);
            let mut delay = Duration::from_millis(500);
            loop {
                match handshake::negotiate(&a.host, a.port, &a.export, Duration::from_secs(10)) {
                    Ok(n) => match nl.reconfigure(family, a.index, n.sock.as_raw_fd()) {
                        Ok(()) => {
                            log!("/dev/nbd{}: reconnected", a.index);
                            break;
                        }
                        Err(e) => log!("reconfigure: {e}"),
                    },
                    Err(e) => log!("{}:{}: {e}", a.host, a.port),
                }
                std::thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_secs(5));
            }
        }
    }

    // The first half: connect the device, start the resident half, return.
    let n = handshake::negotiate(&a.host, a.port, &a.export, Duration::from_secs(30)).map_err(|e| e.to_string())?;
    nl.connect(family, a.index, n.size, a.block_size, n.flags, REQUEST_TIMEOUT, DEAD_CONN_TIMEOUT, n.sock.as_raw_fd())
        .map_err(|e| format!("connecting /dev/nbd{}: {e}", a.index))?;
    log!("/dev/nbd{} is {} from {}:{} ({} bytes)", a.index, a.export, a.host, a.port, n.size);

    // argv[0] starting with '@' tells systemd not to kill it when switching root or at
    // shutdown: the root filesystem needs it until the very end.
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg0("@bootbob-nbd-client")
        .args([a.host.as_str(), "-N", a.export.as_str(), &a.port.to_string(), &format!("/dev/nbd{}", a.index), "--monitor"])
        .stdin(std::process::Stdio::null());
    // Its own session, and stdout/stderr on the kernel log (see `log!`).
    // SAFETY: setsid, open and dup2 are async-signal-safe; nothing else runs between fork
    // and exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            let mut fd = libc::open(c"/dev/kmsg".as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
            if fd < 0 {
                fd = libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
            }
            if fd >= 0 {
                libc::dup2(fd, 1);
                libc::dup2(fd, 2);
            }
            Ok(())
        });
    }
    cmd.spawn().map_err(|e| format!("starting the reconnector: {e}"))?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn run(_: &Args) -> Result<(), String> {
    Err("bootbob-nbd-client needs Linux (the kernel's nbd driver)".into())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") || args.is_empty() {
        eprintln!("usage: bootbob-nbd-client <host> -N <export> [port] /dev/nbdX [-b <block size>]");
        std::process::exit(if args.is_empty() { 1 } else { 0 });
    }
    let a = match parse(&args) {
        Ok(a) => a,
        Err(e) => {
            log!("{e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = run(&a) {
        log!("{e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests;
