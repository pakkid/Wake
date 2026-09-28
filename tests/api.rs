//! REST API and web page, driven in-process through the router.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::*;
use wake::probe::{Monitor, Probe, Prober};

#[tokio::test]
async fn status_reports_defaults() {
    let h = harness(&[]);
    let res = send(&h.app, get("/api/status")).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CACHE_CONTROL], "no-store");
    let s = body_json(res).await;
    assert_eq!(s["pc"]["name"], "Desk");
    assert_eq!(s["pc"]["status"], "unknown");
    assert_eq!(s["next_boot"]["os"], "linux");
    assert_eq!(s["next_boot"]["explicit"], false);
    assert_eq!(s["default_boot"], "linux");
    assert_eq!(s["auth_required"], false);
    assert_eq!(s["setup"]["grub_port"], 8081);
}

#[tokio::test]
async fn select_and_reset_boot_choice() {
    let h = harness(&[]);
    let s = body_json(send(&h.app, post("/api/boot/windows")).await).await;
    assert_eq!(s["next_boot"]["os"], "windows");
    assert_eq!(s["next_boot"]["explicit"], true);
    assert!(s["next_boot"]["expires_at"].is_number());

    let s = body_json(send(&h.app, post("/api/boot/Linux")).await).await;
    assert_eq!(s["next_boot"]["os"], "linux");
    assert_eq!(s["next_boot"]["explicit"], true);

    let s = body_json(send(&h.app, post("/api/boot/default")).await).await;
    assert_eq!(s["next_boot"]["explicit"], false);
}

#[tokio::test]
async fn invalid_os_is_rejected_without_changing_state() {
    let h = harness(&[]);
    send(&h.app, post("/api/boot/windows")).await;
    for bad in ["macos", "0", "1", "%20"] {
        let res = send(&h.app, post(&format!("/api/boot/{bad}"))).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{bad}");
        let body = body_json(res).await;
        assert_eq!(body["error"], "bad_request");
    }
    let res = send(&h.app, post("/api/wake/templeos")).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert!(
        h.wol.sent.lock().unwrap().is_empty(),
        "no packet for a bad request"
    );
    let s = body_json(send(&h.app, get("/api/status")).await).await;
    assert_eq!(s["next_boot"]["os"], "windows");
}

#[tokio::test]
async fn get_on_control_endpoints_is_not_allowed() {
    let h = harness(&[]);
    let res = send(&h.app, get("/api/wake/windows")).await;
    assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert!(h.wol.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn wake_with_os_selects_and_sends() {
    let h = harness(&[]);
    let res = send(&h.app, post("/api/wake/windows")).await;
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_json(res).await;
    assert_eq!(body["ok"], true);
    assert_eq!(body["next_boot"], "windows");
    assert_eq!(body["already_online"], false);
    assert_eq!(body["status"]["last_wol"]["ok"], true);
    assert_eq!(body["status"]["pc"]["status"], "waking");
    assert_eq!(h.wol.sent.lock().unwrap().len(), 1);
    assert_eq!(
        h.wol.sent.lock().unwrap()[0].to_string(),
        "00:11:22:33:44:55"
    );
}

#[tokio::test]
async fn wake_uses_current_choice() {
    let h = harness(&[]);
    send(&h.app, post("/api/boot/windows")).await;
    let body = body_json(send(&h.app, post("/api/wake")).await).await;
    assert_eq!(body["next_boot"], "windows");
    let body = body_json(send(&h.app, post("/api/wake/linux")).await).await;
    assert_eq!(body["next_boot"], "linux");
}

#[tokio::test]
async fn wol_failure_is_reported_and_choice_kept() {
    let h = harness_with(
        config(&[]),
        MockWol {
            fail: true,
            ..Default::default()
        },
        None,
    );
    let res = send(&h.app, post("/api/wake/windows")).await;
    assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
    let body = body_json(res).await;
    assert_eq!(body["error"], "wol_failed");
    assert!(body["message"].as_str().unwrap().contains("still saved"));
    let s = body_json(send(&h.app, get("/api/status")).await).await;
    assert_eq!(s["next_boot"]["os"], "windows");
    assert_eq!(s["last_wol"]["ok"], false);
}

#[tokio::test]
async fn wake_when_already_online_says_so() {
    // A refused TCP connection counts as "host is up", so a closed local port
    // makes the PC look online.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let cfg = config(&[("PC_IP", "127.0.0.1")]);
    let revision = wake::new_revision();
    let boot = Arc::new(wake::state::BootService::in_memory(
        cfg.boot_settings(),
        revision.clone(),
    ));
    let monitor = Monitor::new(
        Some(Prober::new(
            "127.0.0.1".parse().unwrap(),
            vec![Probe::Tcp(port)],
        )),
        boot.clone(),
        revision.clone(),
        Duration::from_secs(5),
        cfg.wake_timeout,
        "Desk".into(),
    );
    monitor.tick(wake::now_ms()).await;
    assert!(monitor.is_online());
    let h = harness_with(cfg, MockWol::default(), Some(monitor));
    // The harness builds its own BootService; the monitor only supplies reachability here.
    let body = body_json(send(&h.app, post("/api/wake/windows")).await).await;
    assert_eq!(body["already_online"], true);
    assert!(body["message"].as_str().unwrap().contains("already on"));
    assert_eq!(
        h.wol.sent.lock().unwrap().len(),
        1,
        "still sends, harmlessly"
    );
}

#[tokio::test]
async fn token_protects_the_api() {
    let h = harness(&[("WAKE_TOKEN", "s3cret token")]);
    assert_eq!(
        send(&h.app, get("/api/status")).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let res = send(&h.app, post("/api/wake/windows")).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert!(h.wol.sent.lock().unwrap().is_empty());

    let with_bearer = |uri: &str, token: &str| {
        Request::post(uri)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        send(&h.app, with_bearer("/api/boot/windows", "wrong"))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(&h.app, with_bearer("/api/boot/windows", "s3cret token"))
            .await
            .status(),
        StatusCode::OK
    );

    // Health checks stay open for Docker.
    assert_eq!(send(&h.app, get("/healthz")).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn session_cookie_unlocks_the_browser() {
    let h = harness(&[("WAKE_TOKEN", "s3cret")]);
    let login = |token: &str| {
        Request::post("/api/session")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(format!(r#"{{"token":"{token}"}}"#)))
            .unwrap()
    };
    assert_eq!(
        send(&h.app, login("nope")).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let res = send(&h.app, login("s3cret")).await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    let cookie = res.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .to_string();
    assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
    let pair = cookie.split(';').next().unwrap().to_string();

    let req = Request::get("/api/status")
        .header(header::COOKIE, pair)
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&h.app, req).await.status(), StatusCode::OK);

    let bad_json = Request::post("/api/session")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{"))
        .unwrap();
    assert_eq!(
        send(&h.app, bad_json).await.status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn locked_page_reveals_nothing() {
    let h = harness(&[("WAKE_TOKEN", "s3cret")]);
    send(
        &h.app,
        Request::post("/api/boot/windows")
            .header(header::AUTHORIZATION, "Bearer s3cret")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let html = body_text(send(&h.app, get("/")).await).await;
    assert!(html.contains("data-locked"));
    assert!(html.contains(r#"<script type="application/json" id="initial">null</script>"#));
    assert!(
        html.contains(r#"data-next="linux""#),
        "the pending Windows choice isn't leaked"
    );
    assert!(!html.contains("Wake Desk into Windows"));
}

#[tokio::test]
async fn cross_site_posts_are_blocked() {
    let h = harness(&[]);
    let evil = Request::post("/api/wake/windows")
        .header(header::HOST, "wake.lan:8080")
        .header(header::ORIGIN, "http://evil.example")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&h.app, evil).await.status(), StatusCode::FORBIDDEN);

    let fetch_meta = Request::post("/api/wake/windows")
        .header("sec-fetch-site", "cross-site")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        send(&h.app, fetch_meta).await.status(),
        StatusCode::FORBIDDEN
    );

    let null_origin = Request::post("/api/boot/windows")
        .header(header::HOST, "wake.lan:8080")
        .header(header::ORIGIN, "null")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        send(&h.app, null_origin).await.status(),
        StatusCode::FORBIDDEN
    );
    assert!(h.wol.sent.lock().unwrap().is_empty());

    let same = Request::post("/api/wake/windows")
        .header(header::HOST, "wake.lan:8080")
        .header(header::ORIGIN, "http://wake.lan:8080")
        .header("sec-fetch-site", "same-origin")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&h.app, same).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn unknown_api_paths_are_json_404s() {
    let h = harness(&[]);
    let res = send(&h.app, get("/api/nope")).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(res).await["error"], "not_found");
    assert_eq!(
        send(&h.app, get("/nope")).await.status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn page_and_assets_are_served() {
    let h = harness(&[]);
    let res = send(&h.app, get("/")).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CACHE_CONTROL], "no-store");
    assert!(
        res.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("default-src 'self'")
    );
    let html = body_text(res).await;
    assert!(html.contains("<h1 class=\"masthead-name\">Wake</h1>"));
    assert!(html.contains("Wake Desk into Linux"));

    let css = send(&h.app, get("/static/css/wake.css")).await;
    assert_eq!(css.status(), StatusCode::OK);
    assert_eq!(css.headers()[header::CONTENT_TYPE], "text/css");

    let manifest = send(&h.app, get("/manifest.webmanifest")).await;
    assert_eq!(
        manifest.headers()[header::CONTENT_TYPE],
        "application/manifest+json"
    );
    let m = body_json(manifest).await;
    assert_eq!(m["display"], "standalone");

    assert_eq!(
        send(&h.app, get("/static/../Cargo.toml")).await.status(),
        StatusCode::NOT_FOUND
    );
}
