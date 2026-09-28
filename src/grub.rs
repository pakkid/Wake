//! The GRUB-facing protocol.
//!
//! GRUB 2 can only reach the network over TFTP or HTTP (`tftp.mod`,
//! `http.mod`), and it has no command that reads a raw response body into a
//! variable. What it *does* have is `load_env`, which imports named variables
//! from a GRUB environment block. So GRUB runs
//!
//! ```text
//! load_env --skip-sig --file (http,SERVER:PORT)/grub/boot.env wake_boot
//! ```
//!
//! and this module answers with a 1024-byte environment block containing
//! `wake_boot=0` (Linux) or `wake_boot=1` (Windows).
//!
//! This is a deliberately tiny hand-written HTTP/1.1 responder rather than
//! hyper: GRUB's `http.c` matches `HTTP/1.1 ` and `Content-Length: ` with a
//! case-sensitive `memcmp`, and hyper writes header names in lowercase. Writing
//! the bytes ourselves guarantees exactly what GRUB expects, and keeps this port
//! down to two read-only paths.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

use crate::config::AllowList;
use crate::now_ms;
use crate::probe::Monitor;
use crate::state::{AnswerSource, BootService, Os};

pub const ENV_BLOCK_LEN: usize = 1024;
pub const ENV_SIGNATURE: &str = "# GRUB Environment Block\n";
pub const BOOT_PATH: &str = "/grub/boot.env";
pub const PREVIEW_PATH: &str = "/grub/preview.env";

const MAX_REQUEST: usize = 4096;
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONNECTIONS: usize = 32;

/// A GRUB environment block in the same layout `grub-editenv` writes:
/// signature, `key=value` lines, then `#` padding to exactly 1024 bytes.
pub fn env_block(os: Os) -> Vec<u8> {
    let mut block = format!("{ENV_SIGNATURE}wake_boot={}\n", os.grub_value()).into_bytes();
    block.resize(ENV_BLOCK_LEN, b'#');
    block
}

#[derive(Clone)]
pub struct GrubContext {
    pub boot: Arc<BootService>,
    pub allowed: AllowList,
    pub monitor: Option<Arc<Monitor>>,
}

#[derive(Debug, PartialEq, Eq)]
enum Parsed<'a> {
    Get {
        path: &'a str,
        range_start: Option<usize>,
    },
    WrongMethod,
    Bad,
}

fn parse_request(raw: &[u8]) -> Parsed<'_> {
    let Ok(text) = std::str::from_utf8(raw) else {
        return Parsed::Bad;
    };
    let mut lines = text.split("\r\n");
    let Some(request_line) = lines.next() else {
        return Parsed::Bad;
    };
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Parsed::Bad;
    };
    if !version.starts_with("HTTP/1.") || !target.starts_with('/') {
        return Parsed::Bad;
    }
    if method != "GET" {
        return Parsed::WrongMethod;
    }
    let path = target.split('?').next().unwrap_or(target);
    let mut range_start = None;
    for line in lines.take_while(|l| !l.is_empty()) {
        let Some((name, value)) = line.split_once(':') else {
            return Parsed::Bad;
        };
        if name.trim().eq_ignore_ascii_case("range") {
            // GRUB only ever asks for `bytes=N-` when it re-opens after a seek.
            let spec = value.trim().strip_prefix("bytes=").unwrap_or("");
            let start = spec.split('-').next().unwrap_or("");
            match start.parse::<usize>() {
                Ok(n) => range_start = Some(n),
                Err(_) => return Parsed::Bad,
            }
        }
    }
    Parsed::Get { path, range_start }
}

fn response(status: &str, extra_headers: &[(&str, String)], body: &[u8]) -> Vec<u8> {
    let mut head = format!("HTTP/1.1 {status}\r\n");
    head.push_str("Content-Type: text/plain\r\n");
    for (name, value) in extra_headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    head.push_str("Cache-Control: no-store\r\n");
    head.push_str("Connection: close\r\n\r\n");
    let mut out = head.into_bytes();
    out.extend_from_slice(body);
    out
}

fn text(status: &str, msg: &str) -> Vec<u8> {
    response(status, &[], format!("{msg}\n").as_bytes())
}

fn env_response(os: Os, range_start: Option<usize>) -> Vec<u8> {
    let block = env_block(os);
    match range_start {
        None | Some(0) => response("200 OK", &[], &block),
        Some(start) if start < block.len() => response(
            "206 Partial Content",
            &[(
                "Content-Range",
                format!("bytes {start}-{}/{}", block.len() - 1, block.len()),
            )],
            &block[start..],
        ),
        Some(_) => response(
            "416 Range Not Satisfiable",
            &[("Content-Range", format!("bytes */{}", block.len()))],
            b"",
        ),
    }
}

/// Handles one complete request and returns the bytes to send back.
pub async fn handle(raw: &[u8], peer: IpAddr, ctx: &GrubContext) -> Vec<u8> {
    let peer = peer.to_canonical();
    let (path, range_start) = match parse_request(raw) {
        Parsed::Get { path, range_start } => (path, range_start),
        Parsed::WrongMethod => {
            return response(
                "405 Method Not Allowed",
                &[("Allow", "GET".into())],
                b"Only GET is supported.\n",
            );
        }
        Parsed::Bad => return text("400 Bad Request", "Malformed request."),
    };
    if !ctx.allowed.allows(peer) {
        tracing::warn!(%peer, path, "GRUB request from an address not in GRUB_ALLOWED_IPS");
        return text(
            "403 Forbidden",
            "This address may not ask for a boot choice.",
        );
    }
    match path {
        BOOT_PATH => {
            let answer = ctx.boot.serve_grub_at(peer, now_ms()).await;
            match answer.source {
                AnswerSource::Repeat => {
                    tracing::debug!(%peer, os = %answer.os, ?range_start, "GRUB repeat request")
                }
                _ => {
                    tracing::info!(%peer, os = %answer.os, source = ?answer.source, "answered GRUB")
                }
            }
            if let Some(m) = &ctx.monitor {
                m.nudge().await;
            }
            env_response(answer.os, range_start)
        }
        PREVIEW_PATH => {
            let os = ctx.boot.peek_at(now_ms()).await;
            tracing::info!(%peer, %os, "GRUB preview request (nothing consumed)");
            env_response(os, range_start)
        }
        _ => text(
            "404 Not Found",
            "Nothing here. GRUB asks for /grub/boot.env.",
        ),
    }
}

async fn read_request(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 512];
    loop {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return if buf.is_empty() { None } else { Some(buf) };
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            buf.truncate(end + 4);
            return Some(buf);
        }
        if buf.len() >= MAX_REQUEST {
            return Some(buf);
        }
    }
}

async fn serve_connection(mut stream: TcpStream, peer: IpAddr, ctx: GrubContext) {
    let reply = match tokio::time::timeout(READ_TIMEOUT, read_request(&mut stream)).await {
        Ok(Some(raw)) if raw.len() >= MAX_REQUEST && !raw.ends_with(b"\r\n\r\n") => {
            text("431 Request Header Fields Too Large", "Request too large.")
        }
        Ok(Some(raw)) if !raw.ends_with(b"\r\n\r\n") => {
            text("400 Bad Request", "Incomplete request.")
        }
        Ok(Some(raw)) => handle(&raw, peer, &ctx).await,
        Ok(None) => return,
        Err(_) => {
            tracing::debug!(%peer, "GRUB connection timed out before sending a request");
            return;
        }
    };
    if let Err(e) = stream.write_all(&reply).await {
        tracing::warn!(%peer, error = %e, "couldn't send the GRUB reply");
        return;
    }
    let _ = stream.shutdown().await;
}

/// Accepts connections forever.
pub async fn serve(listener: TcpListener, ctx: GrubContext) {
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "GRUB listener accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            tracing::warn!(peer = %addr, "too many GRUB connections; dropping one");
            continue;
        };
        let _ = stream.set_nodelay(true);
        let ctx = ctx.clone();
        tokio::spawn(async move {
            serve_connection(stream, addr.ip(), ctx).await;
            drop(permit);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_block_matches_grub_editenv_layout() {
        for (os, line) in [(Os::Linux, "wake_boot=0\n"), (Os::Windows, "wake_boot=1\n")] {
            let b = env_block(os);
            assert_eq!(b.len(), ENV_BLOCK_LEN);
            assert!(b.starts_with(ENV_SIGNATURE.as_bytes()));
            let body = &b[ENV_SIGNATURE.len()..];
            assert!(body.starts_with(line.as_bytes()));
            assert!(body[line.len()..].iter().all(|&c| c == b'#'));
        }
    }

    #[test]
    fn parses_grub_request() {
        let raw = b"GET /grub/boot.env HTTP/1.1\r\nHost: 192.168.1.10:8081\r\nUser-Agent: GRUB 2.14\r\n\r\n";
        assert_eq!(
            parse_request(raw),
            Parsed::Get {
                path: BOOT_PATH,
                range_start: None
            }
        );
        let raw = b"GET /grub/boot.env HTTP/1.1\r\nRange: bytes=10-\r\n\r\n";
        assert_eq!(
            parse_request(raw),
            Parsed::Get {
                path: BOOT_PATH,
                range_start: Some(10)
            }
        );
        assert_eq!(
            parse_request(b"POST / HTTP/1.1\r\n\r\n"),
            Parsed::WrongMethod
        );
        assert_eq!(parse_request(b"GET\r\n\r\n"), Parsed::Bad);
        assert_eq!(parse_request(b"GET / SPDY/3\r\n\r\n"), Parsed::Bad);
        assert_eq!(
            parse_request(b"GET / HTTP/1.1\r\nbroken header\r\n\r\n"),
            Parsed::Bad
        );
        assert_eq!(
            parse_request(b"GET / HTTP/1.1\r\nRange: bytes=x-\r\n\r\n"),
            Parsed::Bad
        );
        assert_eq!(parse_request(&[0xff, 0xfe, b'\r', b'\n']), Parsed::Bad);
    }

    #[test]
    fn range_responses() {
        let full = env_response(Os::Windows, None);
        assert!(full.starts_with(b"HTTP/1.1 200 OK\r\n"));
        let part = env_response(Os::Windows, Some(1000));
        assert!(part.starts_with(b"HTTP/1.1 206 Partial Content\r\n"));
        assert!(
            part.windows(29)
                .any(|w| w == b"Content-Range: bytes 1000-102")
        );
        assert!(part.ends_with(&[b'#'; 24]));
        let past = env_response(Os::Windows, Some(1024));
        assert!(past.starts_with(b"HTTP/1.1 416 "));
    }
}
