//! REST API, live status stream and request guards.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::WatchStream;

use crate::config::Config;
use crate::error::ApiError;
use crate::probe::{Monitor, PcStatus};
use crate::state::{BootService, BootSnapshot, Os};
use crate::wol::WolSender;
use crate::{Millis, Revision, now_ms};

const SESSION_COOKIE: &str = "wake_session";

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub boot: Arc<BootService>,
    pub monitor: Arc<Monitor>,
    pub wol: Arc<dyn WolSender>,
    pub revision: Revision,
}

#[derive(Debug, Clone, Serialize)]
pub struct PcInfo {
    pub name: String,
    pub status: PcStatus,
    pub label: &'static str,
    pub probing: bool,
    pub last_seen: Option<Millis>,
}

/// Read-only facts about the setup, for the settings sheet.
#[derive(Debug, Clone, Serialize)]
pub struct SetupInfo {
    pub version: &'static str,
    pub pc_macs: Vec<String>,
    pub pc_ip: Option<String>,
    pub probes: Vec<String>,
    pub wol_target: String,
    pub grub_port: u16,
    pub choice_ttl_ms: Option<Millis>,
    pub repeat_window_ms: Millis,
}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub pc: PcInfo,
    #[serde(flatten)]
    pub boot: BootSnapshot,
    pub wake_timeout_ms: Millis,
    pub server_time: Millis,
    pub auth_required: bool,
    pub setup: SetupInfo,
}

pub async fn status(st: &AppState) -> Status {
    let now = now_ms();
    let pc = st.monitor.snapshot();
    let cfg = &st.cfg;
    Status {
        pc: PcInfo {
            name: cfg.pc_name.clone(),
            status: pc.status,
            label: pc.label,
            probing: pc.probing,
            last_seen: pc.last_seen,
        },
        boot: st.boot.snapshot_at(now).await,
        wake_timeout_ms: cfg.wake_timeout.as_millis() as Millis,
        server_time: now,
        auth_required: cfg.wake_token.is_some(),
        setup: SetupInfo {
            version: crate::web::VERSION,
            pc_macs: cfg.pc_macs.iter().map(ToString::to_string).collect(),
            pc_ip: cfg.pc_ip.map(|ip| ip.to_string()),
            probes: cfg.probes.iter().map(ToString::to_string).collect(),
            wol_target: format!("{}:{}", cfg.wol_broadcast, cfg.wol_port),
            grub_port: cfg.grub_port,
            choice_ttl_ms: cfg.boot_choice_ttl.map(|d| d.as_millis() as Millis),
            repeat_window_ms: cfg.grub_repeat_window.as_millis() as Millis,
        },
    }
}

async fn get_status(State(st): State<AppState>) -> Json<Status> {
    Json(status(&st).await)
}

fn parse_os(raw: &str) -> Result<Os, ApiError> {
    match raw.to_ascii_lowercase().as_str() {
        "linux" => Ok(Os::Linux),
        "windows" => Ok(Os::Windows),
        _ => Err(ApiError::bad_request(format!(
            "'{raw}' isn't an OS Wake knows. Use linux or windows."
        ))),
    }
}

async fn set_boot(
    State(st): State<AppState>,
    Path(target): Path<String>,
) -> Result<Json<Status>, ApiError> {
    if target.eq_ignore_ascii_case("default") {
        st.boot.clear().await;
    } else {
        st.boot.select(parse_os(&target)?).await;
    }
    Ok(Json(status(&st).await))
}

#[derive(Debug, Serialize)]
pub struct WakeResponse {
    pub ok: bool,
    pub already_online: bool,
    pub next_boot: Os,
    pub message: String,
    pub status: Status,
}

async fn wake(st: &AppState, os: Option<Os>) -> Result<Json<WakeResponse>, ApiError> {
    if let Some(os) = os {
        st.boot.select(os).await;
    }
    let next = st.boot.peek_at(now_ms()).await;
    let already_online = st.monitor.is_online();
    let result = st.wol.send(&st.cfg.pc_macs).await;
    st.boot
        .record_wol_at(
            next,
            result.as_ref().map(|_| ()).map_err(|e| e.to_string()),
            now_ms(),
        )
        .await;
    st.monitor.nudge().await;
    if let Err(e) = result {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "wol_failed",
            format!("Couldn't send the magic packet: {e}. The {next} choice is still saved."),
        ));
    }
    let name = &st.cfg.pc_name;
    let message = if already_online {
        format!("{name} is already on. {next} will be used the next time it restarts.")
    } else {
        format!("Magic packet sent. {name} should start {next}.")
    };
    Ok(Json(WakeResponse {
        ok: true,
        already_online,
        next_boot: next,
        message,
        status: status(st).await,
    }))
}

async fn wake_current(State(st): State<AppState>) -> Result<Json<WakeResponse>, ApiError> {
    wake(&st, None).await
}

async fn wake_os(
    State(st): State<AppState>,
    Path(os): Path<String>,
) -> Result<Json<WakeResponse>, ApiError> {
    let os = parse_os(&os)?;
    wake(&st, Some(os)).await
}

async fn events(State(st): State<AppState>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = WatchStream::new(st.revision.subscribe()).then(move |_| {
        let st = st.clone();
        async move {
            let s = status(&st).await;
            Ok(Event::default()
                .event("status")
                .json_data(s)
                .unwrap_or_else(|_| Event::default().comment("encode failed")))
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[derive(Deserialize)]
struct SessionRequest {
    token: String,
}

fn hex(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn session_cookie(value: &str, max_age: u64) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}"
    ))
    .expect("cookie is ASCII")
}

async fn create_session(
    State(st): State<AppState>,
    body: Result<Json<SessionRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(req) =
        body.map_err(|e| ApiError::bad_request(format!("Expected {{\"token\": \"…\"}}: {e}")))?;
    let Some(token) = &st.cfg.wake_token else {
        return Ok(StatusCode::NO_CONTENT.into_response());
    };
    if !constant_time_eq(req.token.as_bytes(), token.as_bytes()) {
        tracing::warn!("wrong token offered to /api/session");
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "wrong_token",
            "That token doesn't match WAKE_TOKEN.",
        ));
    }
    let mut res = StatusCode::NO_CONTENT.into_response();
    res.headers_mut().insert(
        header::SET_COOKIE,
        session_cookie(&hex(token), 365 * 24 * 3600),
    );
    Ok(res)
}

async fn delete_session() -> Response {
    let mut res = StatusCode::NO_CONTENT.into_response();
    res.headers_mut()
        .insert(header::SET_COOKIE, session_cookie("", 0));
    res
}

pub fn authorized(cfg: &Config, headers: &HeaderMap) -> bool {
    let Some(token) = &cfg.wake_token else {
        return true;
    };
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| constant_time_eq(t.trim().as_bytes(), token.as_bytes()));
    if bearer {
        return true;
    }
    let expected = hex(token);
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .any(|(k, v)| k == SESSION_COOKIE && constant_time_eq(v.as_bytes(), expected.as_bytes()))
}

async fn require_auth(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let open = req.uri().path() == "/api/session";
    if open || authorized(&st.cfg, req.headers()) {
        next.run(req).await
    } else {
        ApiError::unauthorized().into_response()
    }
}

/// Rejects state-changing requests that a browser sent from another site.
/// `curl` sends no `Origin`, so scripts keep working without extra headers.
pub async fn same_origin_only(req: Request, next: Next) -> Response {
    if matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        return next.run(req).await;
    }
    let headers = req.headers();
    let cross_site = headers
        .get("sec-fetch-site")
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"cross-site"));
    let origin_mismatch = headers.get(header::ORIGIN).is_some_and(|origin| {
        let origin = origin.to_str().unwrap_or("null");
        let origin_host = origin.split_once("://").map(|(_, h)| h);
        let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
        match (origin_host, host) {
            (Some(o), Some(h)) => !o.eq_ignore_ascii_case(h),
            _ => true,
        }
    });
    if cross_site || origin_mismatch {
        tracing::warn!(method = %req.method(), path = req.uri().path(), "blocked a cross-site request");
        return ApiError::forbidden("Cross-site requests can't control Wake.").into_response();
    }
    next.run(req).await
}

async fn no_store(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

async fn api_not_found() -> ApiError {
    ApiError::not_found()
}

pub fn routes(state: AppState) -> Router {
    Router::new()
        .route("/api/status", get(get_status))
        .route("/api/events", get(events))
        .route("/api/boot/{target}", post(set_boot))
        .route("/api/wake", post(wake_current))
        .route("/api/wake/{os}", post(wake_os))
        .route("/api/session", post(create_session).delete(delete_session))
        .route("/api/{*rest}", get(api_not_found).post(api_not_found))
        .layer(middleware::from_fn_with_state(state.clone(), require_auth))
        .layer(middleware::from_fn(no_store))
        .route("/healthz", get(|| async { "ok\n" }))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_works() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }

    #[test]
    fn parse_os_rejects_unknown() {
        assert_eq!(parse_os("Windows").unwrap(), Os::Windows);
        assert!(
            parse_os("0").is_err(),
            "the API takes names, not GRUB values"
        );
        assert!(parse_os("").is_err());
    }
}
