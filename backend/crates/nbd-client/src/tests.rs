use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use super::*;

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

#[test]
fn takes_debians_nbd_client_arguments() {
    // Exactly what Debian's initramfs NBD script runs.
    let a = parse(&s(&["10.255.0.5", "-N", "desky.img", "/dev/nbd0", "-swap", "-persist", "-systemd-mark", "-b", "512"])).unwrap();
    assert_eq!(a, Args { host: "10.255.0.5".into(), export: "desky.img".into(), port: 10809, index: 0, block_size: 512, monitor: false });
    let a = parse(&s(&["server", "-N", "x", "10810", "/dev/nbd3", "--monitor"])).unwrap();
    assert_eq!((a.port, a.index, a.monitor), (10810, 3, true));
    assert!(parse(&s(&["server", "/dev/nbd0"])).unwrap_err().contains("export"));
    assert!(parse(&s(&["server", "-N", "x"])).unwrap_err().contains("device"));
    assert!(parse(&s(&["server", "-N", "x", "/dev/nbd0", "-b", "3000"])).is_err());
}

struct One(PathBuf);

impl bootbob_nbd::Exports for One {
    fn open(&self, name: &str, _: SocketAddr) -> Result<bootbob_nbd::Export, String> {
        if name != "disk.img" {
            return Err(format!("no export {name:?}"));
        }
        Ok(bootbob_nbd::Export { name: name.into(), path: self.0.clone(), read_only: false, overlay: None })
    }
    fn list(&self, _: SocketAddr) -> Vec<String> {
        vec!["disk.img".into()]
    }
}

#[test]
fn negotiates_with_bootbobs_server() {
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("disk.img");
    std::fs::write(&img, vec![0u8; 1 << 20]).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let port = rt.block_on(async {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(bootbob_nbd::serve(l, Arc::new(One(img))));
        port
    });
    let n = handshake::negotiate("127.0.0.1", port, "disk.img", Duration::from_secs(5)).unwrap();
    assert_eq!(n.size, 1 << 20);
    assert_eq!(n.flags & 1, 1, "HAS_FLAGS");
    assert_eq!(n.flags & 2, 0, "writable");
    let e = handshake::negotiate("127.0.0.1", port, "other.img", Duration::from_secs(5)).err().unwrap();
    assert!(e.to_string().contains("refused"), "{e}");
}
