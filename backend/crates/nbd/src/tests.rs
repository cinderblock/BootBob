use std::net::SocketAddr;
use std::path::Path;

use super::*;

/// Exports `disk.img` (read-write) and `ro.img` (read-only) from a directory, to
/// anyone but 127.0.0.2.
struct Dir(PathBuf);

impl Exports for Dir {
    fn open(&self, name: &str, peer: SocketAddr) -> Result<Export, String> {
        if peer.ip().to_string() == "127.0.0.2" {
            return Err("not for you".into());
        }
        match name {
            "disk" | "ro" => {
                Ok(Export { name: name.into(), path: self.0.join(format!("{name}.img")), read_only: name == "ro", overlay: None })
            }
            // A copy-on-write view of ro.img, one overlay per client port (the test's stand-in
            // for "per machine").
            "cow" => Ok(Export {
                name: name.into(),
                path: self.0.join("ro.img"),
                read_only: false,
                overlay: Some((self.0.join("overlays"), format!("port-{}", peer.port()))),
            }),
            _ => Err(format!("no export {name:?}")),
        }
    }
    fn list(&self, _: SocketAddr) -> Vec<String> {
        vec!["disk".into(), "ro".into(), "cow".into()]
    }
}

async fn server(dir: &Path) -> SocketAddr {
    std::fs::write(dir.join("disk.img"), vec![0u8; 1 << 20]).unwrap();
    let ro: Vec<u8> = (0..65536u32).map(|i| i as u8).collect();
    std::fs::write(dir.join("ro.img"), ro).unwrap();
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let ex = Arc::new(Dir(dir.to_path_buf()));
    tokio::spawn(serve(l, ex));
    addr
}

/// A minimal client, speaking the protocol the way the Linux `nbd-client` does.
struct Client {
    s: TcpStream,
    handle: u64,
}

impl Client {
    async fn connect(addr: SocketAddr) -> Client {
        let mut s = TcpStream::connect(addr).await.unwrap();
        assert_eq!(s.read_u64().await.unwrap(), NBDMAGIC);
        assert_eq!(s.read_u64().await.unwrap(), IHAVEOPT);
        assert_eq!(s.read_u16().await.unwrap(), FLAG_FIXED_NEWSTYLE | FLAG_NO_ZEROES);
        s.write_u32(3).await.unwrap();
        Client { s, handle: 0 }
    }

    async fn option(&mut self, opt: u32, data: &[u8]) {
        self.s.write_u64(IHAVEOPT).await.unwrap();
        self.s.write_u32(opt).await.unwrap();
        self.s.write_u32(data.len() as u32).await.unwrap();
        self.s.write_all(data).await.unwrap();
    }

    async fn opt_reply(&mut self) -> (u32, Vec<u8>) {
        assert_eq!(self.s.read_u64().await.unwrap(), OPT_REPLY_MAGIC);
        let _opt = self.s.read_u32().await.unwrap();
        let kind = self.s.read_u32().await.unwrap();
        let len = self.s.read_u32().await.unwrap();
        let mut d = vec![0u8; len as usize];
        self.s.read_exact(&mut d).await.unwrap();
        (kind, d)
    }

    /// `NBD_OPT_GO`: `Ok((size, flags))`, or the error reply.
    async fn go(&mut self, name: &str) -> Result<(u64, u16), (u32, String)> {
        let mut d = (name.len() as u32).to_be_bytes().to_vec();
        d.extend_from_slice(name.as_bytes());
        d.extend_from_slice(&0u16.to_be_bytes());
        self.option(OPT_GO, &d).await;
        let mut out = None;
        loop {
            let (kind, d) = self.opt_reply().await;
            match kind {
                REP_INFO if u16::from_be_bytes([d[0], d[1]]) == INFO_EXPORT => {
                    out = Some((u64::from_be_bytes(d[2..10].try_into().unwrap()), u16::from_be_bytes([d[10], d[11]])));
                }
                REP_INFO => {}
                REP_ACK => return Ok(out.unwrap()),
                err => return Err((err, String::from_utf8_lossy(&d).into_owned())),
            }
        }
    }

    async fn request(&mut self, cmd: u16, flags: u16, offset: u64, len: u32, data: &[u8]) -> (u32, Vec<u8>) {
        self.handle += 1;
        self.s.write_u32(REQUEST_MAGIC).await.unwrap();
        self.s.write_u16(flags).await.unwrap();
        self.s.write_u16(cmd).await.unwrap();
        self.s.write_u64(self.handle).await.unwrap();
        self.s.write_u64(offset).await.unwrap();
        self.s.write_u32(len).await.unwrap();
        self.s.write_all(data).await.unwrap();
        assert_eq!(self.s.read_u32().await.unwrap(), SIMPLE_REPLY_MAGIC);
        let err = self.s.read_u32().await.unwrap();
        assert_eq!(self.s.read_u64().await.unwrap(), self.handle);
        let mut d = vec![0u8; if cmd == CMD_READ && err == 0 { len as usize } else { 0 }];
        self.s.read_exact(&mut d).await.unwrap();
        (err, d)
    }
}

#[tokio::test]
async fn a_restart_answers_what_was_sent_before_it_closes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("disk.img"), vec![0u8; 1 << 20]).unwrap();
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let live = Arc::new(Live::default());
    let server = tokio::spawn(serve_with(l, Arc::new(Dir(dir.path().to_path_buf())), live.clone()));

    let mut c = Client::connect(addr).await;
    c.go("disk").await.unwrap();

    // Several writes on the wire when the restart comes, as the kernel sends them: not
    // one waits for the last one's reply.
    let writes = 20u64;
    for i in 0..writes {
        c.s.write_u32(REQUEST_MAGIC).await.unwrap();
        c.s.write_u16(0).await.unwrap();
        c.s.write_u16(CMD_WRITE).await.unwrap();
        c.s.write_u64(100 + i).await.unwrap();
        c.s.write_u64(4096 * i).await.unwrap();
        c.s.write_u32(5).await.unwrap();
        c.s.write_all(b"drain").await.unwrap();
    }
    let started = Instant::now();
    live.drain().await;
    assert!(started.elapsed() < DRAIN_LIMIT, "it went quiet, so no need to wait it out");
    assert!(!live.in_use(&dir.path().join("disk.img")));

    for i in 0..writes {
        assert_eq!(c.s.read_u32().await.unwrap(), SIMPLE_REPLY_MAGIC);
        assert_eq!(c.s.read_u32().await.unwrap(), 0);
        assert_eq!(c.s.read_u64().await.unwrap(), 100 + i);
    }
    assert_eq!(c.s.read_u8().await.unwrap_err().kind(), io::ErrorKind::UnexpectedEof, "then it closes cleanly");
    let on_disk = std::fs::read(dir.path().join("disk.img")).unwrap();
    assert_eq!(&on_disk[4096 * (writes as usize - 1)..][..5], b"drain");

    server.await.unwrap().unwrap();
    assert!(TcpStream::connect(addr).await.is_err(), "no new connections");
}

#[tokio::test]
async fn reads_writes_and_flushes_through_to_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let addr = server(dir.path()).await;
    let mut c = Client::connect(addr).await;
    let (size, flags) = c.go("disk").await.unwrap();
    assert_eq!(size, 1 << 20);
    assert_eq!(flags & TFLAG_READ_ONLY, 0);
    assert_ne!(flags & TFLAG_SEND_FLUSH, 0);

    assert_eq!(c.request(CMD_WRITE, 0, 4096, 5, b"hello").await.0, 0);
    assert_eq!(c.request(CMD_WRITE, CMD_FLAG_FUA, 8192, 3, b"FUA").await.0, 0);
    assert_eq!(c.request(CMD_READ, 0, 4094, 9, &[]).await, (0, b"\0\0hello\0\0".to_vec()));
    assert_eq!(c.request(CMD_FLUSH, 0, 0, 0, &[]).await.0, 0);
    let on_disk = std::fs::read(dir.path().join("disk.img")).unwrap();
    assert_eq!(&on_disk[4096..4101], b"hello");
    assert_eq!(&on_disk[8192..8195], b"FUA");

    // Out of bounds, and an unknown command, are errors that keep the connection going.
    assert_eq!(c.request(CMD_READ, 0, (1 << 20) - 2, 4, &[]).await.0, EINVAL);
    assert_eq!(c.request(CMD_WRITE, 0, (1 << 20) - 2, 4, b"1234").await.0, ENOSPC);
    assert_eq!(c.request(99, 0, 0, 0, &[]).await.0, EINVAL);
    assert_eq!(c.request(CMD_READ, 0, 4096, 5, &[]).await, (0, b"hello".to_vec()));

    c.s.write_u32(REQUEST_MAGIC).await.unwrap();
    c.s.write_all(&[0, 0, 0, 2]).await.unwrap(); // flags 0, DISC
    c.s.write_all(&[0u8; 20]).await.unwrap();
    let mut b = [0u8; 1];
    assert_eq!(c.s.read(&mut b).await.unwrap(), 0, "closed after DISC");
}

#[tokio::test]
async fn read_only_exports_refuse_writes() {
    let dir = tempfile::tempdir().unwrap();
    let addr = server(dir.path()).await;
    let mut c = Client::connect(addr).await;
    let (size, flags) = c.go("ro").await.unwrap();
    assert_eq!(size, 65536);
    assert_ne!(flags & TFLAG_READ_ONLY, 0);
    assert_eq!(c.request(CMD_WRITE, 0, 0, 2, b"no").await.0, EPERM);
    assert_eq!(c.request(CMD_READ, 0, 256, 4, &[]).await, (0, vec![0, 1, 2, 3]));
}

#[tokio::test]
async fn list_info_unknown_and_refused() {
    let dir = tempfile::tempdir().unwrap();
    let addr = server(dir.path()).await;
    let mut c = Client::connect(addr).await;
    c.option(OPT_LIST, &[]).await;
    let mut names = Vec::new();
    loop {
        match c.opt_reply().await {
            (REP_SERVER, d) => names.push(String::from_utf8(d[4..].to_vec()).unwrap()),
            (REP_ACK, _) => break,
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(names, vec!["disk", "ro", "cow"]);
    assert_eq!(c.go("nope").await.unwrap_err().0, REP_ERR_UNKNOWN);
    c.option(42, &[]).await;
    assert_eq!(c.opt_reply().await.0, REP_ERR_UNSUP);
    // Still negotiating after those: GO works.
    assert!(c.go("disk").await.is_ok());

    let mut s = TcpStream::connect(addr).await.unwrap();
    let _ = s.read_u64().await;
    drop(s);

    // Refused by policy (a different source address).
    let sock = tokio::net::TcpSocket::new_v4().unwrap();
    if sock.bind("127.0.0.2:0".parse().unwrap()).is_ok()
        && let Ok(s) = sock.connect(addr).await
    {
        let mut c = Client { s, handle: 0 };
        assert_eq!(c.s.read_u64().await.unwrap(), NBDMAGIC);
        c.s.read_u64().await.unwrap();
        c.s.read_u16().await.unwrap();
        c.s.write_u32(3).await.unwrap();
        let (code, msg) = c.go("disk").await.unwrap_err();
        assert_eq!((code, msg.as_str()), (REP_ERR_POLICY, "not for you"));
    }
}

#[tokio::test]
async fn old_style_export_name_works() {
    let dir = tempfile::tempdir().unwrap();
    let addr = server(dir.path()).await;
    let mut c = Client::connect(addr).await;
    c.option(OPT_EXPORT_NAME, b"ro").await;
    assert_eq!(c.s.read_u64().await.unwrap(), 65536);
    assert_ne!(c.s.read_u16().await.unwrap() & TFLAG_READ_ONLY, 0);
    // NO_ZEROES was agreed, so the next thing is ours to send.
    assert_eq!(c.request(CMD_READ, 0, 0, 2, &[]).await, (0, vec![0, 1]));
}

#[tokio::test]
async fn a_new_connection_to_an_export_ends_the_old_one() {
    let dir = tempfile::tempdir().unwrap();
    let addr = server(dir.path()).await;
    let mut old = Client::connect(addr).await;
    old.go("disk").await.unwrap();
    assert_eq!(old.request(CMD_WRITE, 0, 0, 3, b"old").await.0, 0);

    let mut new = Client::connect(addr).await;
    new.go("disk").await.unwrap();
    assert_eq!(new.request(CMD_READ, 0, 0, 3, &[]).await, (0, b"old".to_vec()));
    let mut b = [0u8; 1];
    let n = tokio::time::timeout(std::time::Duration::from_secs(5), old.s.read(&mut b)).await.unwrap();
    assert!(matches!(n, Ok(0) | Err(_)), "the old connection is closed");
}

#[tokio::test]
async fn a_cow_export_writes_to_the_overlay_and_leaves_the_base_alone() {
    let dir = tempfile::tempdir().unwrap();
    let addr = server(dir.path()).await;
    let mut c = Client::connect(addr).await;
    let (size, flags) = c.go("cow").await.unwrap();
    assert_eq!(size, 65536);
    assert_eq!(flags & TFLAG_READ_ONLY, 0, "writable, through the overlay");
    assert_eq!(c.request(CMD_READ, 0, 256, 4, &[]).await, (0, vec![0, 1, 2, 3]));
    assert_eq!(c.request(CMD_WRITE, CMD_FLAG_FUA, 258, 2, b"XY").await.0, 0);
    assert_eq!(c.request(CMD_READ, 0, 256, 5, &[]).await, (0, vec![0, 1, b'X', b'Y', 4]));
    assert_eq!(c.request(CMD_FLUSH, 0, 0, 0, &[]).await.0, 0);
    let base = std::fs::read(dir.path().join("ro.img")).unwrap();
    assert_eq!(&base[256..261], &[0, 1, 2, 3, 4], "the base is untouched");
    let port = c.s.local_addr().unwrap().port();
    assert!(bootbob_cow::Cow::exists(&dir.path().join("overlays"), &format!("port-{port}")));

    // Another client has its own overlay: it sees the base.
    let mut other = Client::connect(addr).await;
    other.go("cow").await.unwrap();
    assert_eq!(other.request(CMD_READ, 0, 256, 5, &[]).await, (0, vec![0, 1, 2, 3, 4]));
}
