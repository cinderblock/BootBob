use super::*;
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::time::timeout;

struct Files {
    files: HashMap<String, Bytes>,
    seen: Mutex<Vec<Request>>,
    outcomes: Mutex<Vec<Outcome>>,
}

impl Resolver for Files {
    async fn resolve(&self, req: &Request) -> Resolution {
        self.seen.lock().unwrap().push(req.clone());
        let Some(path) = &req.path else { return Resolution::Denied("bad path".into()) };
        match self.files.get(path) {
            Some(b) => Resolution::Serve(Content::Memory(b.clone())),
            None => Resolution::NotFound,
        }
    }

    fn finished(&self, _req: &Request, outcome: &Outcome) {
        self.outcomes.lock().unwrap().push(outcome.clone());
    }
}

async fn server(files: &[(&str, Bytes)], config: Config) -> (SocketAddr, Arc<Files>) {
    let resolver = Arc::new(Files {
        files: files.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
        seen: Mutex::new(Vec::new()),
        outcomes: Mutex::new(Vec::new()),
    });
    let sock = bind("127.0.0.1".parse().unwrap(), 0).await.unwrap();
    let addr = sock.local_addr().unwrap();
    tokio::spawn(serve(sock, resolver.clone(), config));
    (addr, resolver)
}

fn pattern(len: usize) -> Bytes {
    (0..len).map(|i| (i * 7 + i / 251) as u8).collect::<Vec<u8>>().into()
}

#[derive(Debug, Default)]
struct Fetched {
    data: Vec<u8>,
    oack: Option<Vec<(String, String)>>,
    /// Every DATA packet received, including duplicates, as (wire block, length).
    packets: Vec<(u16, usize)>,
}

/// A small windowed TFTP client. `drop(block, n)` is asked about the `n`th arrival of each
/// wire block number and may discard it, to simulate loss.
async fn fetch(
    server: SocketAddr,
    path: &str,
    options: &[(&str, &str)],
    mut drop: impl FnMut(u16, usize) -> bool,
) -> Result<Fetched, (u16, String)> {
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    sock.send_to(&Packet::rrq(path, options), server).await.unwrap();
    let mut out = Fetched::default();
    let mut blksize = 512;
    let mut windowsize = 1;
    let mut expected: u16 = 1;
    let mut in_window = 0;
    let mut arrivals: HashMap<u16, usize> = HashMap::new();
    let mut buf = vec![0u8; 70000];
    let mut tid = None;
    loop {
        let (n, from) = timeout(Duration::from_secs(10), sock.recv_from(&mut buf)).await.expect("server stalled").unwrap();
        if *tid.get_or_insert(from) != from {
            continue;
        }
        match Packet::parse(&buf[..n]).unwrap() {
            Packet::OptionAck { options } => {
                for (k, v) in &options {
                    match k.as_str() {
                        "blksize" => blksize = v.parse().unwrap(),
                        "windowsize" => windowsize = v.parse().unwrap(),
                        _ => {}
                    }
                }
                out.oack = Some(options);
                sock.send_to(&Packet::ack(0), from).await.unwrap();
            }
            Packet::Data { block, data } => {
                out.packets.push((block, data.len()));
                let count = arrivals.entry(block).or_default();
                *count += 1;
                if drop(block, *count) {
                    continue;
                }
                if block != expected {
                    // A gap: ACK the last block we have, so the server resends from there.
                    sock.send_to(&Packet::ack(expected.wrapping_sub(1)), from).await.unwrap();
                    in_window = 0;
                    continue;
                }
                out.data.extend_from_slice(data);
                expected = expected.wrapping_add(1);
                in_window += 1;
                let last = data.len() < blksize;
                if last || in_window == windowsize {
                    sock.send_to(&Packet::ack(block), from).await.unwrap();
                    in_window = 0;
                }
                if last {
                    return Ok(out);
                }
            }
            Packet::Error { code, message } => return Err((code, message)),
            other => panic!("unexpected packet {other:?}"),
        }
    }
}

#[test]
fn cleans_paths() {
    assert_eq!(clean_path("/pxelinux/./ipxe.efi").as_deref(), Some("pxelinux/ipxe.efi"));
    assert_eq!(clean_path("\\boot\\bootbob").as_deref(), Some("boot/bootbob"));
    assert_eq!(clean_path("a//b/").as_deref(), Some("a/b"));
    assert_eq!(clean_path("../etc/passwd"), None);
    assert_eq!(clean_path("a/../../b"), None);
    assert_eq!(clean_path("/"), None);
}

#[tokio::test]
async fn plain_transfer_without_options() {
    let file = pattern(1300);
    let (addr, r) = server(&[("ipxe.efi", file.clone())], Config::default()).await;
    let got = fetch(addr, "/ipxe.efi", &[], |_, _| false).await.unwrap();
    assert_eq!(got.data, file);
    assert!(got.oack.is_none());
    assert_eq!(got.packets, vec![(1, 512), (2, 512), (3, 276)]);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(matches!(r.outcomes.lock().unwrap()[0], Outcome::Completed { bytes: 1300, .. }));
}

#[tokio::test]
async fn exact_multiple_ends_with_an_empty_block() {
    let file = pattern(1024);
    let (addr, _) = server(&[("f", file.clone())], Config::default()).await;
    let got = fetch(addr, "f", &[], |_, _| false).await.unwrap();
    assert_eq!(got.data, file);
    assert_eq!(got.packets, vec![(1, 512), (2, 512), (3, 0)]);
}

#[tokio::test]
async fn empty_file() {
    let (addr, _) = server(&[("f", Bytes::new())], Config::default()).await;
    let got = fetch(addr, "f", &[], |_, _| false).await.unwrap();
    assert!(got.data.is_empty());
    assert_eq!(got.packets, vec![(1, 0)]);
}

#[tokio::test]
async fn negotiates_options() {
    let file = pattern(100_000);
    let (addr, _) = server(&[("k", file.clone())], Config::default()).await;
    let opts = [("blksize", "1024"), ("tsize", "0"), ("windowsize", "8"), ("timeout", "2"), ("bogus", "1")];
    let got = fetch(addr, "k", &opts, |_, _| false).await.unwrap();
    assert_eq!(got.data, file);
    let oack: HashMap<_, _> = got.oack.unwrap().into_iter().collect();
    assert_eq!(oack["blksize"], "1024");
    assert_eq!(oack["tsize"], "100000");
    assert_eq!(oack["windowsize"], "8");
    assert_eq!(oack["timeout"], "2");
    assert!(!oack.contains_key("bogus"));
}

#[tokio::test]
async fn clamps_blksize_and_windowsize() {
    let file = pattern(5000);
    let config = Config { max_blksize: 1468, max_windowsize: 4, ..Config::default() };
    let (addr, _) = server(&[("k", file.clone())], config).await;
    let got = fetch(addr, "k", &[("blksize", "65464"), ("windowsize", "64")], |_, _| false).await.unwrap();
    assert_eq!(got.data, file);
    let oack: HashMap<_, _> = got.oack.unwrap().into_iter().collect();
    assert_eq!(oack["blksize"], "1468");
    assert_eq!(oack["windowsize"], "4");
}

#[tokio::test]
async fn block_numbers_roll_over() {
    // blksize 8 and 70,000 blocks crosses block 65535.
    let file = pattern(8 * 70_000 + 3);
    let (addr, _) = server(&[("big", file.clone())], Config::default()).await;
    let got = fetch(addr, "big", &[("blksize", "8"), ("windowsize", "32")], |_, _| false).await.unwrap();
    assert_eq!(got.data.len(), file.len());
    assert_eq!(got.data, file);
    assert!(got.packets.iter().any(|&(b, _)| b == 0), "block 65536 should go out as 0");
}

#[tokio::test]
async fn recovers_from_a_lost_block_mid_window() {
    let file = pattern(512 * 20);
    let (addr, _) = server(&[("f", file.clone())], Config::default()).await;
    let got = fetch(addr, "f", &[("windowsize", "4")], |block, n| block == 6 && n == 1).await.unwrap();
    assert_eq!(got.data, file);
    assert!(got.packets.iter().filter(|&&(b, _)| b == 6).count() >= 2);
}

#[tokio::test]
async fn retransmits_after_timeout() {
    let file = pattern(512 * 3);
    let (addr, _) = server(&[("f", file.clone())], Config::default()).await;
    // Drop the first copy of the last block: no ACK goes back, so only the timeout helps.
    let got = fetch(addr, "f", &[("timeout", "1")], |block, n| block == 4 && n == 1).await.unwrap();
    assert_eq!(got.data, file);
}

#[tokio::test]
async fn missing_and_bad_paths() {
    let (addr, r) = server(&[("f", pattern(10))], Config::default()).await;
    assert_eq!(fetch(addr, "nope", &[], |_, _| false).await.unwrap_err().0, ErrorCode::FileNotFound as u16);
    assert_eq!(fetch(addr, "../f", &[], |_, _| false).await.unwrap_err().0, ErrorCode::AccessViolation as u16);
    let seen = r.seen.lock().unwrap();
    assert_eq!(seen[1].raw_path, "../f");
    assert_eq!(seen[1].path, None);
}

#[tokio::test]
async fn refuses_writes() {
    let (addr, _) = server(&[], Config::default()).await;
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut wrq = Packet::rrq("x", &[]);
    wrq[1] = 2;
    sock.send_to(&wrq, addr).await.unwrap();
    let mut buf = [0u8; 512];
    let n = timeout(Duration::from_secs(5), sock.recv(&mut buf)).await.unwrap().unwrap();
    assert!(matches!(Packet::parse(&buf[..n]).unwrap(), Packet::Error { code: 2, .. }));
}
