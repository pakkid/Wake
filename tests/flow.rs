//! The whole path: phone → API → magic packet → GRUB fetch → back to default.

mod common;

use std::time::Duration;

use common::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use wake::grub::{self, GrubContext};

async fn grub_fetch(addr: std::net::SocketAddr) -> String {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(b"GET /grub/boot.env HTTP/1.1\r\nHost: wake\r\nUser-Agent: GRUB 2.14\r\n\r\n")
        .await
        .unwrap();
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out))
        .await
        .unwrap()
        .unwrap();
    let text = String::from_utf8(out).unwrap();
    let body = text.split("\r\n\r\n").nth(1).unwrap().to_string();
    body.lines()
        .find_map(|l| l.strip_prefix("wake_boot="))
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn phone_to_grub_and_back() {
    let h = harness(&[("GRUB_REPEAT_WINDOW", "1s")]);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(grub::serve(
        listener,
        GrubContext {
            boot: h.state.boot.clone(),
            allowed: h.state.cfg.grub_allowed.clone(),
            monitor: Some(h.state.monitor.clone()),
        },
    ));

    // The phone: tap Windows, tap Wake.
    let res = send(&h.app, post("/api/wake/windows")).await;
    assert_eq!(res.status(), 200);
    assert_eq!(h.wol.sent.lock().unwrap().len(), 1);
    let s = body_json(send(&h.app, get("/api/status")).await).await;
    assert_eq!(s["pc"]["status"], "waking");

    // GRUB asks, possibly more than once during one boot.
    assert_eq!(grub_fetch(addr).await, "1");
    assert_eq!(grub_fetch(addr).await, "1");

    let s = body_json(send(&h.app, get("/api/status")).await).await;
    assert_eq!(s["pc"]["status"], "booting");
    assert_eq!(s["last_boot"]["os"], "windows");
    assert_eq!(s["last_boot"]["from_choice"], true);
    assert_eq!(s["next_boot"]["os"], "linux");
    assert_eq!(s["next_boot"]["explicit"], false);

    // The next boot, after the repeat window: Linux again.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(grub_fetch(addr).await, "0");
}

#[tokio::test]
async fn server_restart_mid_flow_keeps_the_choice() {
    let dir = tempfile::tempdir().unwrap();
    let settings = wake::state::BootSettings::default();
    {
        let boot = wake::state::BootService::load(
            wake::store::Store::new(dir.path()),
            settings,
            wake::new_revision(),
        )
        .await;
        boot.select(wake::state::Os::Windows).await;
    }
    // Container restarts between the wake and GRUB asking.
    let boot = std::sync::Arc::new(
        wake::state::BootService::load(
            wake::store::Store::new(dir.path()),
            settings,
            wake::new_revision(),
        )
        .await,
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(grub::serve(
        listener,
        GrubContext {
            boot,
            allowed: wake::config::AllowList::Any,
            monitor: None,
        },
    ));
    assert_eq!(grub_fetch(addr).await, "1");
}
