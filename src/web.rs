//! The phone-first web page, its static assets and the combined router.

use askama::Template;
use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use rust_embed::RustEmbed;

use crate::api::{self, AppState};

#[derive(RustEmbed)]
#[folder = "static/"]
struct Assets;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A short hash of every embedded asset, used as the `?v=` cache-buster so a
/// redeploy with changed CSS or JS is picked up even if VERSION didn't change.
pub fn asset_version() -> &'static str {
    static HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    HASH.get_or_init(|| {
        let mut names: Vec<_> = Assets::iter().collect();
        names.sort();
        // FNV-1a over each file's name and SHA-256.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for name in names {
            let digest = Assets::get(&name).map(|f| f.metadata.sha256_hash());
            for b in name.bytes().chain(digest.into_iter().flatten()) {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
        }
        format!("{h:016x}")[..10].to_string()
    })
}

#[derive(Template)]
#[template(path = "index.html")]
struct Index {
    pc_name: String,
    status_key: &'static str,
    status_label: &'static str,
    next_key: &'static str,
    next_label: &'static str,
    wake_label: String,
    version: &'static str,
    /// True when a token is required and this browser hasn't unlocked yet.
    locked: bool,
    /// Initial status JSON for the page script; `<` is escaped so it can't end the tag.
    initial_json: String,
}

async fn index(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let locked = !api::authorized(&st.cfg, &headers);
    let s = api::status(&st).await;
    // A locked page shows nothing about the PC until it's unlocked.
    let next = if locked {
        st.cfg.default_boot
    } else {
        s.boot.next_boot.os
    };
    let (status_key, status_label) = if locked {
        ("unknown", "Locked")
    } else {
        (s.pc.status.as_str(), s.pc.label)
    };
    let initial_json = if locked {
        "null".to_string()
    } else {
        serde_json::to_string(&s)
            .unwrap_or_else(|_| "null".into())
            .replace('<', "\\u003c")
    };
    let page = Index {
        pc_name: st.cfg.pc_name.clone(),
        status_key,
        status_label,
        next_key: next.as_str(),
        next_label: next.label(),
        wake_label: format!("Wake {} into {}", st.cfg.pc_name, next.label()),
        version: asset_version(),
        locked,
        initial_json,
    };
    match page.render() {
        Ok(html) => {
            let mut res = Html(html).into_response();
            res.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            res
        }
        Err(e) => {
            tracing::error!(error = %e, "couldn't render the page");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Couldn't render the page.",
            )
                .into_response()
        }
    }
}

fn asset(path: &str) -> Response {
    let Some(file) = Assets::get(path) else {
        return (StatusCode::NOT_FOUND, "Not found.\n").into_response();
    };
    let mime = match path.rsplit('.').next() {
        Some("webmanifest") => "application/manifest+json".to_string(),
        _ => file.metadata.mimetype().to_string(),
    };
    let cache = if path == "manifest.webmanifest" {
        "no-cache"
    } else {
        // Asset URLs carry ?v=<version>, so a day of caching is safe across upgrades.
        "public, max-age=86400"
    };
    (
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, cache.to_string()),
        ],
        file.data.into_owned(),
    )
        .into_response()
}

async fn static_file(Path(path): Path<String>) -> Response {
    asset(&path)
}

async fn security_headers(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; img-src 'self' data:; style-src 'self'; script-src 'self'; \
             connect-src 'self'; font-src 'self'; manifest-src 'self'; base-uri 'none'; \
             form-action 'self'; frame-ancestors 'none'",
        ),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    res
}

pub fn routes(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/static/{*path}", get(static_file))
        .route(
            "/manifest.webmanifest",
            get(|| async { asset("manifest.webmanifest") }),
        )
        .route(
            "/favicon.png",
            get(|| async { asset("icons/favicon-32.png") }),
        )
        .route(
            "/favicon.ico",
            get(|| async { asset("icons/favicon-32.png") }),
        )
        .route(
            "/apple-touch-icon.png",
            get(|| async { asset("icons/apple-touch-icon.png") }),
        )
        .with_state(state)
}

/// The whole HTTP app on the web port.
pub fn app(state: AppState) -> Router {
    api::routes(state.clone())
        .merge(routes(state))
        .fallback(|| async { (StatusCode::NOT_FOUND, "Not found.\n") })
        .layer(middleware::from_fn(api::same_origin_only))
        .layer(middleware::from_fn(security_headers))
}
