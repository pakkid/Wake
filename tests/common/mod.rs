#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, Response};
use futures_util::future::BoxFuture;
use http_body_util::BodyExt;
use tower::ServiceExt;

use wake::api::AppState;
use wake::config::Config;
use wake::probe::Monitor;
use wake::state::BootService;
use wake::wol::{MacAddr, WolSender};

#[derive(Default)]
pub struct MockWol {
    pub sent: Mutex<Vec<MacAddr>>,
    pub fail: bool,
}

impl WolSender for MockWol {
    fn send<'a>(&'a self, macs: &'a [MacAddr]) -> BoxFuture<'a, std::io::Result<()>> {
        Box::pin(async move {
            if self.fail {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NetworkUnreachable,
                    "network is unreachable",
                ));
            }
            self.sent.lock().unwrap().extend_from_slice(macs);
            Ok(())
        })
    }
}

pub struct Harness {
    pub app: Router,
    pub state: AppState,
    pub wol: Arc<MockWol>,
}

pub fn config(extra: &[(&str, &str)]) -> Config {
    let mut vars: HashMap<String, String> = [
        ("PC_NAME", "Desk"),
        ("PC_MAC", "00:11:22:33:44:55"),
        ("DATA_DIR", "/nonexistent"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    for (k, v) in extra {
        vars.insert(k.to_string(), v.to_string());
    }
    Config::from_map(&vars).expect("valid test config")
}

pub fn harness_with(cfg: Config, wol: MockWol, monitor: Option<Monitor>) -> Harness {
    let cfg = Arc::new(cfg);
    let revision = wake::new_revision();
    let boot = Arc::new(BootService::in_memory(
        cfg.boot_settings(),
        revision.clone(),
    ));
    let monitor = Arc::new(monitor.unwrap_or_else(|| {
        Monitor::new(
            None,
            boot.clone(),
            revision.clone(),
            Duration::from_secs(5),
            cfg.wake_timeout,
            cfg.pc_name.clone(),
        )
    }));
    let wol = Arc::new(wol);
    let state = AppState {
        cfg,
        boot,
        monitor,
        wol: wol.clone(),
        revision,
    };
    Harness {
        app: wake::web::app(state.clone()),
        state,
        wol,
    }
}

pub fn harness(extra: &[(&str, &str)]) -> Harness {
    harness_with(config(extra), MockWol::default(), None)
}

pub async fn send(app: &Router, req: Request<Body>) -> Response<Body> {
    app.clone().oneshot(req).await.unwrap()
}

pub fn get(uri: &str) -> Request<Body> {
    Request::get(uri).body(Body::empty()).unwrap()
}

pub fn post(uri: &str) -> Request<Body> {
    Request::post(uri).body(Body::empty()).unwrap()
}

pub async fn body_json(res: Response<Body>) -> serde_json::Value {
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|e| panic!("not JSON ({e}): {}", String::from_utf8_lossy(&bytes)))
}

pub async fn body_text(res: Response<Body>) -> String {
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}
