//! The GRUB port over a real TCP socket, byte for byte.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use wake::config::AllowList;
use wake::grub::{self, GrubContext};
use wake::state::{BootService, BootSettings, Os};

async fn start(allowed: AllowList) -> (SocketAddr, Arc<BootService>) {
    let boot = Arc::new(BootService::in_memory(
        BootSettings::default(),
        wake::new_revision(),
    ));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(grub::serve(
        listener,
        GrubContext {
            boot: boot.clone(),
            allowed,
            monitor: None,
        },
    ));
    (addr, boot)
}

async fn raw(addr: SocketAddr, request: &[u8]) -> Vec<u8> {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(request).await.unwrap();
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut out))
        .await
        .expect("server closes the connection")
        .unwrap();
    out
}

/// What GRUB 2.14's http.c sends.
fn grub_get(addr: SocketAddr, path: &str) -> Vec<u8> {
    format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nUser-Agent: GRUB 2.14\r\n\r\n").into_bytes()
}

fn split(resp: &[u8]) -> (String, Vec<u8>) {
    let end = resp
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("header end")
        + 4;
    (
        String::from_utf8(resp[..end].to_vec()).unwrap(),
        resp[end..].to_vec(),
    )
}

#[tokio::test]
async fn answers_exactly_what_grub_parses() {
    let (addr, boot) = start(AllowList::Any).await;
    boot.select(Os::Windows).await;
    let (head, body) = split(&raw(addr, &grub_get(addr, "/grub/boot.env")).await);
    // GRUB compares these case-sensitively.
    assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
    assert!(head.contains("\r\nContent-Length: 1024\r\n"), "{head}");
    assert!(head.contains("\r\nConnection: close\r\n"));
    assert_eq!(body.len(), 1024);
    assert!(body.starts_with(b"# GRUB Environment Block\nwake_boot=1\n"));
    assert!(body.ends_with(b"###"));
}

#[tokio::test]
async fn one_shot_with_repeats_and_preview() {
    let (addr, boot) = start(AllowList::Any).await;
    let fetch = |path: &'static str| async move {
        let (_, body) = split(&raw(addr, &grub_get(addr, path)).await);
        String::from_utf8(body).unwrap()
    };
    boot.select(Os::Windows).await;
    // The preview never uses the choice up.
    assert!(fetch("/grub/preview.env").await.contains("wake_boot=1\n"));
    assert!(fetch("/grub/preview.env").await.contains("wake_boot=1\n"));
    assert!(boot.snapshot_at(wake::now_ms()).await.next_boot.explicit);
    // The real fetch consumes it; GRUB's repeat fetches in the same boot agree.
    for _ in 0..3 {
        assert!(fetch("/grub/boot.env").await.contains("wake_boot=1\n"));
    }
    assert!(
        !boot.snapshot_at(wake::now_ms()).await.next_boot.explicit,
        "consumed"
    );
    // With nothing waiting, the preview shows the default.
    assert!(fetch("/grub/preview.env").await.contains("wake_boot=0\n"));
}

#[tokio::test]
async fn range_request_after_seek() {
    let (addr, boot) = start(AllowList::Any).await;
    boot.select(Os::Windows).await;
    let req = format!("GET /grub/boot.env HTTP/1.1\r\nHost: {addr}\r\nRange: bytes=25-\r\n\r\n");
    let (head, body) = split(&raw(addr, req.as_bytes()).await);
    assert!(head.starts_with("HTTP/1.1 206 Partial Content\r\n"));
    assert!(head.contains("Content-Range: bytes 25-1023/1024\r\n"));
    assert!(head.contains("Content-Length: 999\r\n"));
    assert!(body.starts_with(b"wake_boot=1\n"));
}

#[tokio::test]
async fn rejects_what_it_should() {
    let (addr, _) = start(AllowList::Any).await;
    let status = |resp: Vec<u8>| String::from_utf8_lossy(&resp[..resp.len().min(40)]).to_string();
    assert!(status(raw(addr, &grub_get(addr, "/api/status")).await).starts_with("HTTP/1.1 404 "));
    assert!(
        status(raw(addr, b"POST /grub/boot.env HTTP/1.1\r\n\r\n").await)
            .starts_with("HTTP/1.1 405 ")
    );
    assert!(status(raw(addr, b"HELLO\r\n\r\n").await).starts_with("HTTP/1.1 400 "));
    assert!(
        status(raw(addr, b"\x16\x03\x01\x02\x00\x01\x00\r\n\r\n").await)
            .starts_with("HTTP/1.1 400 ")
    );
    let huge = format!(
        "GET /grub/boot.env HTTP/1.1\r\nX: {}\r\n\r\n",
        "a".repeat(5000)
    );
    assert!(status(raw(addr, huge.as_bytes()).await).starts_with("HTTP/1.1 431 "));
    // HTTP/1.0 clients still get an answer.
    assert!(
        status(raw(addr, b"GET /grub/boot.env HTTP/1.0\r\n\r\n").await)
            .starts_with("HTTP/1.1 200 ")
    );
}

#[tokio::test]
async fn allowlist_blocks_other_hosts_without_consuming() {
    let (addr, boot) = start(AllowList::Only(vec!["10.9.9.9".parse().unwrap()])).await;
    boot.select(Os::Windows).await;
    let resp = raw(addr, &grub_get(addr, "/grub/boot.env")).await;
    assert!(resp.starts_with(b"HTTP/1.1 403 "));
    assert!(
        boot.snapshot_at(wake::now_ms()).await.next_boot.explicit,
        "choice still waiting"
    );
}

#[tokio::test]
async fn slow_clients_are_dropped() {
    let (addr, _) = start(AllowList::Any).await;
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(b"GET /grub/boot.env HTTP/1.1\r\n")
        .await
        .unwrap();
    let mut out = Vec::new();
    let n = tokio::time::timeout(Duration::from_secs(8), s.read_to_end(&mut out))
        .await
        .expect("closed after the read timeout")
        .unwrap();
    assert_eq!(n, 0, "no answer to an unfinished request");
}
