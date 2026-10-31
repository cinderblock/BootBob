use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::AtomicBool;

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

/// Holes need Linux here. Off Linux a disk doesn't offer them, and these tests have nothing
/// to test; on Linux they must run.
fn no_holes() -> bool {
    !cfg!(target_os = "linux")
}

fn allocated(p: &Path) -> u64 {
    bootbob_cow::allocated(p).unwrap()
}

#[tokio::test]
async fn trim_and_write_zeroes_are_offered_only_where_they_can_work() {
    let dir = tempfile::tempdir().unwrap();
    let addr = server(dir.path()).await;
    let mut c = Client::connect(addr).await;
    let (_, flags) = c.go("ro").await.unwrap();
    assert_eq!(flags & (TFLAG_SEND_TRIM | TFLAG_SEND_WRITE_ZEROES), 0, "read-only");
    assert_eq!(c.request(CMD_TRIM, 0, 0, 512, &[]).await.0, EPERM);
    assert_eq!(c.request(CMD_WRITE_ZEROES, 0, 0, 512, &[]).await.0, EPERM);

    for export in ["disk", "cow"] {
        let mut c = Client::connect(addr).await;
        let (_, flags) = c.go(export).await.unwrap();
        assert_ne!(flags & TFLAG_SEND_WRITE_ZEROES, 0, "{export}");
        assert_eq!(flags & TFLAG_SEND_TRIM != 0, !no_holes(), "{export}");
    }
}

#[tokio::test]
async fn trim_gives_a_plain_disk_space_back_and_reads_zeroes() {
    let dir = tempfile::tempdir().unwrap();
    let addr = server(dir.path()).await;
    let img = dir.path().join("disk.img");
    let mut c = Client::connect(addr).await;
    c.go("disk").await.unwrap();
    if no_holes() {
        return;
    }
    let ones = vec![1u8; 1 << 19];
    assert_eq!(c.request(CMD_WRITE, 0, 1 << 19, 1 << 19, &ones).await.0, 0);
    assert_eq!(c.request(CMD_FLUSH, 0, 0, 0, &[]).await.0, 0);
    let before = allocated(&img);

    // Partial sectors at both ends: those bytes are zero, and their neighbours survive.
    let (off, len) = ((1 << 19) + 100, (1 << 18) + 7);
    assert_eq!(c.request(CMD_TRIM, 0, off, len, &[]).await.0, 0);
    assert_eq!(c.request(CMD_FLUSH, 0, 0, 0, &[]).await.0, 0);
    assert!(allocated(&img) <= before - (1 << 18) + 8192, "{before} -> {}", allocated(&img));
    let mut want = ones.clone();
    want[100..100 + len as usize].fill(0);
    assert_eq!(c.request(CMD_READ, 0, 1 << 19, 1 << 19, &[]).await, (0, want));

    // Past the end refuses and changes nothing, and the connection carries on.
    assert_eq!(c.request(CMD_TRIM, 0, (1 << 20) - 2, 4, &[]).await.0, EINVAL);
    assert_eq!(c.request(CMD_WRITE_ZEROES, 0, (1 << 20) - 2, 4, &[]).await.0, ENOSPC);
    assert_eq!(c.request(CMD_READ, 0, (1 << 20) - 2, 2, &[]).await, (0, vec![1, 1]));

    // With FUA, and a write straight after: the write stays.
    assert_eq!(c.request(CMD_TRIM, CMD_FLAG_FUA, 900_000, 4096, &[]).await.0, 0);
    assert_eq!(c.request(CMD_WRITE, 0, 900_100, 3, b"new").await.0, 0);
    let got = c.request(CMD_READ, 0, 900_098, 7, &[]).await.1;
    assert_eq!(got, [0, 0, b'n', b'e', b'w', 0, 0]);
}

#[tokio::test]
async fn a_trim_can_be_bigger_than_any_write() {
    let dir = tempfile::tempdir().unwrap();
    let addr = server(dir.path()).await;
    let big = 8u64 << 30;
    std::fs::File::options().write(true).open(dir.path().join("disk.img")).unwrap().set_len(big).unwrap();
    let mut c = Client::connect(addr).await;
    assert_eq!(c.go("disk").await.unwrap().0, big);
    if no_holes() {
        return;
    }
    assert_eq!(c.request(CMD_WRITE, 0, 5 << 30, 4, b"data").await.0, 0);
    // What Linux sends for a discard of a whole disk: just under 4 GiB at a time.
    assert_eq!(c.request(CMD_TRIM, 0, 5 << 29, u32::MAX - 511, &[]).await.0, 0);
    assert_eq!(c.request(CMD_READ, 0, 5 << 30, 4, &[]).await, (0, vec![0; 4]));
}

#[tokio::test]
async fn write_zeroes_makes_a_hole_unless_told_to_keep_the_space() {
    let dir = tempfile::tempdir().unwrap();
    let addr = server(dir.path()).await;
    let img = dir.path().join("disk.img");
    let mut c = Client::connect(addr).await;
    c.go("disk").await.unwrap();
    let ones = vec![1u8; 1 << 18];
    assert_eq!(c.request(CMD_WRITE, 0, 0, 1 << 18, &ones).await.0, 0);

    assert_eq!(c.request(CMD_WRITE_ZEROES, CMD_FLAG_NO_HOLE, 8192, 8192, &[]).await.0, 0);
    let got = c.request(CMD_READ, 0, 8190, 8196, &[]).await.1;
    assert_eq!((&got[..2], &got[2..8194], &got[8194..]), (&[1, 1][..], &[0u8; 8192][..], &[1, 1][..]));
    assert_eq!(c.request(CMD_FLUSH, 0, 0, 0, &[]).await.0, 0);
    if no_holes() {
        return;
    }
    let before = allocated(&img);
    assert_ne!(before, 0);
    assert_eq!(c.request(CMD_WRITE_ZEROES, 0, 0, 1 << 17, &[]).await.0, 0);
    assert_eq!(c.request(CMD_FLUSH, 0, 0, 0, &[]).await.0, 0);
    assert!(allocated(&img) <= before - (1 << 16), "{before} -> {}", allocated(&img));
    assert_eq!(c.request(CMD_READ, 0, 4096, 4, &[]).await, (0, vec![0; 4]));
}

#[tokio::test]
async fn a_trimmed_overlay_reads_zeroes_not_the_base_and_gives_space_back() {
    let dir = tempfile::tempdir().unwrap();
    let addr = server(dir.path()).await;
    let mut c = Client::connect(addr).await;
    c.go("cow").await.unwrap();
    if no_holes() {
        return;
    }
    let port = c.s.local_addr().unwrap().port();
    let (ovl, _) = bootbob_cow::overlay_paths(&dir.path().join("overlays"), &format!("port-{port}"));

    // Trimmed over the base's data: zeroes, and the base doesn't show through.
    assert_eq!(c.request(CMD_READ, 0, 8192, 2, &[]).await, (0, vec![0, 1]));
    assert_eq!(c.request(CMD_TRIM, 0, 4096, 16384, &[]).await.0, 0);
    assert_eq!(c.request(CMD_READ, 0, 8192, 2, &[]).await, (0, vec![0, 0]));

    // Trimmed over what the machine wrote: the same, and the space comes back.
    assert_eq!(c.request(CMD_WRITE, CMD_FLAG_FUA, 32768, 16384, &vec![7u8; 16384]).await.0, 0);
    let before = allocated(&ovl);
    assert_eq!(c.request(CMD_TRIM, CMD_FLAG_FUA, 32768, 16384, &[]).await.0, 0);
    assert!(allocated(&ovl) <= before - 12288, "{before} -> {}", allocated(&ovl));
    assert_eq!(c.request(CMD_READ, 0, 32768, 4, &[]).await, (0, vec![0; 4]));

    // A partial sector: the rest of the chunk is still the base's.
    assert_eq!(c.request(CMD_TRIM, 0, 50000 + 256, 4, &[]).await.0, 0);
    let got = c.request(CMD_READ, 0, 50000 + 254, 8, &[]).await.1;
    assert_eq!(got, [0x4e, 0x4f, 0, 0, 0, 0, 0x54, 0x55]);

    let base = std::fs::read(dir.path().join("ro.img")).unwrap();
    assert_eq!(&base[8192..8194], &[0, 1], "the base is untouched");
}

/// Puts a request on the wire without waiting for its reply.
async fn send_trim(s: &mut TcpStream, handle: u64, offset: u64, len: u32) {
    s.write_u32(REQUEST_MAGIC).await.unwrap();
    s.write_u16(0).await.unwrap();
    s.write_u16(CMD_TRIM).await.unwrap();
    s.write_u64(handle).await.unwrap();
    s.write_u64(offset).await.unwrap();
    s.write_u32(len).await.unwrap();
}

/// Holds one discard until the test lets it go, the way a slow hole punch on a busy disk
/// would. It gives up after a while, so a failing test fails rather than hangs.
#[derive(Default)]
struct Stall {
    armed: AtomicBool,
    entered: AtomicBool,
    released: AtomicBool,
}

impl Stall {
    fn hold(&self) {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.entered.store(true, Ordering::SeqCst);
            let started = Instant::now();
            while !self.released.load(Ordering::SeqCst) && started.elapsed() < Duration::from_secs(15) {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }

    async fn wait_entered(&self) {
        while !self.entered.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
    }
}

struct StallingDisk {
    inner: Arc<dyn Disk>,
    stall: Arc<Stall>,
}

impl Disk for StallingDisk {
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
        self.stall.hold();
        self.inner.discard(off, len)
    }
}

/// `disk` and `cow`, the same disk for every connection (unlike [`Dir`]), which stall.
struct Stalling {
    dir: PathBuf,
    stall: Arc<Stall>,
}

impl Exports for Stalling {
    fn open(&self, name: &str, _: SocketAddr) -> Result<Export, String> {
        let cow = name == "cow";
        Ok(Export {
            name: name.into(),
            path: self.dir.join(if cow { "ro.img" } else { "disk.img" }),
            read_only: false,
            overlay: cow.then(|| (self.dir.join("overlays"), "shared".to_string())),
        })
    }
    fn list(&self, _: SocketAddr) -> Vec<String> {
        Vec::new()
    }
    fn open_disk(&self, export: &Export) -> Result<Arc<dyn Disk>, String> {
        Ok(Arc::new(StallingDisk { inner: open_disk(export)?, stall: self.stall.clone() }))
    }
}

struct StallFixture {
    _dir: tempfile::TempDir,
    addr: SocketAddr,
    live: Arc<Live>,
    stall: Arc<Stall>,
}

async fn stalling_server() -> StallFixture {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("disk.img"), vec![1u8; 1 << 20]).unwrap();
    std::fs::write(dir.path().join("ro.img"), vec![2u8; 1 << 20]).unwrap();
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let stall = Arc::new(Stall { armed: AtomicBool::new(true), ..Stall::default() });
    let live = Arc::new(Live::default());
    tokio::spawn(serve_with(l, Arc::new(Stalling { dir: dir.path().to_path_buf(), stall: stall.clone() }), live.clone()));
    StallFixture { _dir: dir, addr, live, stall }
}

/// This is the one that matters most: a TRIM landing after a newer write destroys it. The old
/// connection's TRIM is running when the client gives up and comes back. If the new
/// connection could start, its write would land, and then the old TRIM would punch a hole
/// through it.
#[tokio::test(flavor = "multi_thread")]
async fn a_stale_trim_can_never_land_after_a_newer_write() {
    if no_holes() {
        return;
    }
    for export in ["disk", "cow"] {
        let t = stalling_server().await;
        let mut old = Client::connect(t.addr).await;
        old.go(export).await.unwrap();
        send_trim(&mut old.s, 9, 0, 1 << 18).await;
        t.stall.wait_entered().await;

        // The TRIM is running. The client comes back; the handshake must not finish.
        let (addr, name) = (t.addr, export.to_string());
        let mut newer = tokio::spawn(async move {
            let mut c = Client::connect(addr).await;
            c.go(&name).await.unwrap();
            c
        });
        let waited = tokio::time::timeout(Duration::from_millis(300), &mut newer).await;
        let started_early = waited.is_ok();
        t.stall.release();
        assert!(!started_early, "{export}: the new connection started while the old TRIM was running");

        // Now the TRIM has finished, and only then can the write go.
        let mut c = tokio::time::timeout(Duration::from_secs(5), newer).await.expect("the TRIM is done, so this goes ahead").unwrap();
        let data = vec![9u8; 8192];
        assert_eq!(c.request(CMD_WRITE, CMD_FLAG_FUA, 4096, 8192, &data).await.0, 0);
        assert_eq!(c.request(CMD_READ, 0, 4096, 8192, &[]).await, (0, data));
        // What the old connection did is what the new one sees: zeroes, through an overlay
        // too, whose map it read when it opened the disk.
        assert_eq!(c.request(CMD_READ, 0, 65536, 4, &[]).await, (0, vec![0; 4]), "{export}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restart_waits_for_a_replaced_connection_that_is_still_trimming() {
    if no_holes() {
        return;
    }
    let t = stalling_server().await;
    let mut old = Client::connect(t.addr).await;
    old.go("disk").await.unwrap();
    send_trim(&mut old.s, 9, 0, 4096).await;
    t.stall.wait_entered().await;

    // Replaced, so it no longer holds the export by name, but its TRIM is still running.
    let addr = t.addr;
    tokio::spawn(async move {
        let mut c = Client::connect(addr).await;
        let _ = c.go("disk").await;
        std::future::pending::<()>().await;
    });
    while t.live.next.load(Ordering::Relaxed) < 2 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let live = t.live.clone();
    let drained = tokio::spawn(async move { live.drain().await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let early = drained.is_finished();
    t.stall.release();
    assert!(!early, "a TRIM is running");
    tokio::time::timeout(DRAIN_LIMIT + Duration::from_secs(2), drained).await.unwrap().unwrap();
}

#[tokio::test]
async fn a_trim_across_a_restart_is_finished_or_safely_sent_again() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("disk.img"), vec![1u8; 1 << 20]).unwrap();
    std::fs::write(dir.path().join("ro.img"), vec![2u8; 1 << 20]).unwrap();
    if no_holes() {
        return;
    }
    for export in ["disk", "cow"] {
        // Overlays are per client port here, so this one names its own.
        struct One(PathBuf);
        impl Exports for One {
            fn open(&self, name: &str, _: SocketAddr) -> Result<Export, String> {
                let cow = name == "cow";
                Ok(Export {
                    name: name.into(),
                    path: self.0.join(if cow { "ro.img" } else { "disk.img" }),
                    read_only: false,
                    overlay: cow.then(|| (self.0.join("overlays"), "pi".to_string())),
                })
            }
            fn list(&self, _: SocketAddr) -> Vec<String> {
                Vec::new()
            }
        }
        async fn start(dir: &Path, live: Arc<Live>) -> (SocketAddr, tokio::task::JoinHandle<io::Result<()>>) {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = l.local_addr().unwrap();
            (addr, tokio::spawn(serve_with(l, Arc::new(One(dir.to_path_buf())), live)))
        }

        let live = Arc::new(Live::default());
        let (addr, server) = start(dir.path(), live.clone()).await;
        let mut c = Client::connect(addr).await;
        c.go(export).await.unwrap();
        // TRIMs and a write to the same blocks on the wire together when the restart comes.
        for i in 0..8u64 {
            send_trim(&mut c.s, 100 + i, i * 8192, 8192).await;
        }
        c.s.write_u32(REQUEST_MAGIC).await.unwrap();
        c.s.write_u16(0).await.unwrap();
        c.s.write_u16(CMD_WRITE).await.unwrap();
        c.s.write_u64(200).await.unwrap();
        c.s.write_u64(8192).await.unwrap();
        c.s.write_u32(4).await.unwrap();
        c.s.write_all(b"kept").await.unwrap();

        live.drain().await;
        for i in (100..108).chain([200]) {
            assert_eq!(c.s.read_u32().await.unwrap(), SIMPLE_REPLY_MAGIC);
            assert_eq!(c.s.read_u32().await.unwrap(), 0);
            assert_eq!(c.s.read_u64().await.unwrap(), i, "in order");
        }
        assert_eq!(c.s.read_u8().await.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        server.await.unwrap().unwrap();

        // A new server on the same files. The client lost the replies to the last TRIM and
        // sends it again, which is safe, and the data written after that one survives.
        let (addr, _server) = start(dir.path(), Arc::new(Live::default())).await;
        let mut c = Client::connect(addr).await;
        c.go(export).await.unwrap();
        assert_eq!(c.request(CMD_READ, 0, 8192, 4, &[]).await, (0, b"kept".to_vec()), "{export}");
        assert_eq!(c.request(CMD_READ, 0, 0, 4, &[]).await, (0, vec![0; 4]), "{export}");
        assert_eq!(c.request(CMD_READ, 0, 60000, 4, &[]).await, (0, vec![0; 4]), "{export}");
        assert_eq!(c.request(CMD_TRIM, 0, 7 * 8192, 8192, &[]).await.0, 0);
        assert_eq!(c.request(CMD_READ, 0, 8192, 4, &[]).await, (0, b"kept".to_vec()), "{export}");
        assert_eq!(c.request(CMD_READ, 0, 80000, 4, &[]).await.1, if export == "cow" { vec![2u8; 4] } else { vec![1u8; 4] });
    }
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
