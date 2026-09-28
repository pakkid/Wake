//! Is the PC up? Reachability probes and the status the UI shows.
//!
//! Linux and Windows expose different things (Windows drops ping by default,
//! Linux may not run sshd), so the probe list is configurable and any single
//! success counts. A TCP connection that is *refused* also counts: a RST means
//! the host's network stack answered.

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::Notify;

use crate::state::{BootService, EventKind};
use crate::{Millis, Revision, bump, now_ms};

const PROBE_TIMEOUT: Duration = Duration::from_millis(1200);
const FAST_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    Icmp,
    Tcp(u16),
}

impl Probe {
    /// Parses `icmp,tcp:22,tcp:3389`.
    pub fn parse_list(s: &str) -> Result<Vec<Probe>, String> {
        let mut out = Vec::new();
        for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let probe = match part.to_ascii_lowercase().as_str() {
                "icmp" | "ping" => Probe::Icmp,
                other => match other.strip_prefix("tcp:") {
                    Some(port) => match port.parse::<u16>() {
                        Ok(p) if p > 0 => Probe::Tcp(p),
                        _ => return Err(format!("'{part}' has an invalid port")),
                    },
                    None => return Err(format!("'{part}' isn't a probe; use icmp or tcp:<port>")),
                },
            };
            if !out.contains(&probe) {
                out.push(probe);
            }
        }
        if out.is_empty() {
            return Err("at least one probe is needed".into());
        }
        Ok(out)
    }
}

impl fmt::Display for Probe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Probe::Icmp => f.write_str("icmp"),
            Probe::Tcp(p) => write!(f, "tcp:{p}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PcStatus {
    Unknown,
    Offline,
    Waking,
    Booting,
    Online,
}

impl PcStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PcStatus::Unknown => "unknown",
            PcStatus::Offline => "offline",
            PcStatus::Waking => "waking",
            PcStatus::Booting => "booting",
            PcStatus::Online => "online",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            PcStatus::Unknown => "Unknown",
            PcStatus::Offline => "Offline",
            PcStatus::Waking => "Waking",
            PcStatus::Booting => "Booting",
            PcStatus::Online => "Online",
        }
    }

    fn in_progress(self) -> bool {
        matches!(self, PcStatus::Waking | PcStatus::Booting)
    }
}

/// Pure status rule, kept separate so it can be tested without a network.
///
/// `reachable` is `None` when there's nothing to probe (no `PC_IP`).
pub fn derive_status(
    reachable: Option<bool>,
    last_wol: Option<Millis>,
    last_grub: Option<Millis>,
    now: Millis,
    wake_timeout: Duration,
) -> PcStatus {
    if reachable == Some(true) {
        return PcStatus::Online;
    }
    let window = wake_timeout.as_millis() as Millis;
    let recent = |t: Option<Millis>| t.filter(|&t| now >= t && now - t < window);
    match (recent(last_wol), recent(last_grub)) {
        (_, Some(grub)) if last_wol.is_none_or(|w| grub >= w) => PcStatus::Booting,
        (Some(_), _) => PcStatus::Waking,
        _ if reachable.is_none() => PcStatus::Unknown,
        _ => PcStatus::Offline,
    }
}

pub struct Prober {
    ip: IpAddr,
    probes: Vec<Probe>,
    icmp: Option<surge_ping::Client>,
    ident: u16,
    seq: AtomicU16,
}

impl Prober {
    pub fn new(ip: IpAddr, probes: Vec<Probe>) -> Self {
        let icmp = if probes.contains(&Probe::Icmp) {
            let kind = if ip.is_ipv4() {
                surge_ping::ICMP::V4
            } else {
                surge_ping::ICMP::V6
            };
            match surge_ping::Client::new(&surge_ping::Config::builder().kind(kind).build()) {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "can't open an ICMP socket (needs CAP_NET_RAW or ping_group_range); \
                         skipping the icmp probe"
                    );
                    None
                }
            }
        } else {
            None
        };
        Self {
            ip,
            probes,
            icmp,
            ident: (now_ms() as u16) ^ (std::process::id() as u16),
            seq: AtomicU16::new(0),
        }
    }

    pub fn probes(&self) -> &[Probe] {
        &self.probes
    }

    /// True if any probe gets an answer.
    pub async fn check(&self) -> bool {
        let checks = self.probes.iter().map(|&p| Box::pin(self.one(p)));
        // Stop at the first success rather than waiting for slow timeouts.
        let mut pending: Vec<_> = checks.collect();
        while !pending.is_empty() {
            let (ok, _, rest) = futures_util::future::select_all(pending).await;
            if ok {
                return true;
            }
            pending = rest;
        }
        false
    }

    async fn one(&self, probe: Probe) -> bool {
        match probe {
            Probe::Tcp(port) => tcp_alive(SocketAddr::new(self.ip, port)).await,
            Probe::Icmp => {
                let Some(client) = &self.icmp else {
                    return false;
                };
                let mut pinger = client
                    .pinger(self.ip, surge_ping::PingIdentifier(self.ident))
                    .await;
                pinger.timeout(PROBE_TIMEOUT);
                let seq = self.seq.fetch_add(1, Ordering::Relaxed);
                pinger
                    .ping(surge_ping::PingSequence(seq), &[0u8; 16])
                    .await
                    .is_ok()
            }
        }
    }
}

async fn tcp_alive(addr: SocketAddr) -> bool {
    match tokio::time::timeout(PROBE_TIMEOUT, tokio::net::TcpStream::connect(addr)).await {
        Ok(Ok(_)) => true,
        Ok(Err(e)) => e.kind() == std::io::ErrorKind::ConnectionRefused,
        Err(_) => false,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PcSnapshot {
    pub status: PcStatus,
    pub label: &'static str,
    /// Whether Wake can probe at all (false without `PC_IP`).
    pub probing: bool,
    pub last_seen: Option<Millis>,
}

#[derive(Debug)]
struct Observed {
    reachable: Option<bool>,
    failures: u32,
    last_seen: Option<Millis>,
    status: Option<PcStatus>,
}

pub struct Monitor {
    prober: Option<Prober>,
    boot: Arc<BootService>,
    revision: Revision,
    interval: Duration,
    wake_timeout: Duration,
    pc_name: String,
    observed: std::sync::Mutex<Observed>,
    kick: Notify,
}

impl Monitor {
    pub fn new(
        prober: Option<Prober>,
        boot: Arc<BootService>,
        revision: Revision,
        interval: Duration,
        wake_timeout: Duration,
        pc_name: String,
    ) -> Self {
        let reachable = prober.as_ref().map(|_| false);
        Self {
            prober,
            boot,
            revision,
            interval,
            wake_timeout,
            pc_name,
            observed: std::sync::Mutex::new(Observed {
                reachable,
                failures: 0,
                last_seen: None,
                status: None,
            }),
            kick: Notify::new(),
        }
    }

    pub fn snapshot(&self) -> PcSnapshot {
        let o = self.observed.lock().expect("monitor lock");
        let status = o.status.unwrap_or(if self.prober.is_some() {
            PcStatus::Offline
        } else {
            PcStatus::Unknown
        });
        PcSnapshot {
            status,
            label: status.label(),
            probing: self.prober.is_some(),
            last_seen: o.last_seen,
        }
    }

    pub fn is_online(&self) -> bool {
        self.observed.lock().expect("monitor lock").reachable == Some(true)
    }

    /// Re-derive status now and probe sooner (called after a wake or GRUB request).
    pub async fn nudge(&self) {
        self.update(now_ms()).await;
        self.kick.notify_one();
    }

    /// One probe-and-derive cycle.
    pub async fn tick(&self, now: Millis) {
        if let Some(prober) = &self.prober {
            let up = prober.check().await;
            let mut o = self.observed.lock().expect("monitor lock");
            if up {
                o.failures = 0;
                o.reachable = Some(true);
                o.last_seen = Some(now);
            } else {
                o.failures += 1;
                // One lost probe shouldn't flip an online PC to offline.
                if o.reachable != Some(true) || o.failures >= 2 {
                    o.reachable = Some(false);
                }
            }
        }
        self.boot.sweep_at(now).await;
        self.update(now).await;
    }

    async fn update(&self, now: Millis) {
        let last_wol = self.boot.last_wol_at().await;
        let last_grub = self.boot.last_grub_at().await;
        let (prev, next) = {
            let mut o = self.observed.lock().expect("monitor lock");
            let next = derive_status(o.reachable, last_wol, last_grub, now, self.wake_timeout);
            let prev = o.status.replace(next);
            (prev, next)
        };
        let Some(prev) = prev else {
            bump(&self.revision);
            return;
        };
        if prev == next {
            return;
        }
        tracing::info!(
            from = prev.as_str(),
            to = next.as_str(),
            "PC status changed"
        );
        let name = &self.pc_name;
        let timeout = humantime::format_duration(self.wake_timeout);
        let event = match (prev, next) {
            (PcStatus::Waking | PcStatus::Booting, PcStatus::Online) => {
                Some((EventKind::Success, format!("{name} is awake.")))
            }
            (_, PcStatus::Online) => Some((EventKind::Info, format!("{name} came online."))),
            (PcStatus::Online, PcStatus::Offline) => {
                Some((EventKind::Info, format!("{name} went offline.")))
            }
            (PcStatus::Waking, PcStatus::Offline) => Some((
                EventKind::Warning,
                format!("{name} didn't answer within {timeout} of the magic packet."),
            )),
            (PcStatus::Booting, PcStatus::Offline) => Some((
                EventKind::Warning,
                format!("GRUB asked, but {name} didn't answer within {timeout}."),
            )),
            _ => None,
        };
        match event {
            Some((kind, message)) => self.boot.log_at(kind, message, now).await,
            None => bump(&self.revision),
        }
    }

    pub async fn run(self: Arc<Self>) {
        loop {
            self.tick(now_ms()).await;
            let wait = if self.snapshot().status.in_progress() {
                FAST_INTERVAL.min(self.interval)
            } else {
                self.interval
            };
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = self.kick.notified() => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::BootSettings;

    const T: Millis = 1_700_000_000_000;
    const TO: Duration = Duration::from_secs(180);

    #[test]
    fn parses_probe_lists() {
        assert_eq!(
            Probe::parse_list("icmp, tcp:22,TCP:3389,tcp:22").unwrap(),
            vec![Probe::Icmp, Probe::Tcp(22), Probe::Tcp(3389)]
        );
        assert!(Probe::parse_list("").is_err());
        assert!(Probe::parse_list("tcp:0").is_err());
        assert!(Probe::parse_list("tcp:http").is_err());
        assert!(Probe::parse_list("udp:9").is_err());
    }

    #[test]
    fn status_rules() {
        use PcStatus::*;
        assert_eq!(derive_status(Some(true), None, None, T, TO), Online);
        assert_eq!(derive_status(Some(true), Some(T), Some(T), T, TO), Online);
        assert_eq!(derive_status(Some(false), None, None, T, TO), Offline);
        assert_eq!(derive_status(None, None, None, T, TO), Unknown);
        assert_eq!(
            derive_status(Some(false), Some(T - 1_000), None, T, TO),
            Waking
        );
        assert_eq!(derive_status(None, Some(T - 1_000), None, T, TO), Waking);
        assert_eq!(
            derive_status(Some(false), Some(T - 9_000), Some(T - 1_000), T, TO),
            Booting
        );
        // A GRUB request from an earlier boot doesn't make a new wake "booting".
        assert_eq!(
            derive_status(Some(false), Some(T - 1_000), Some(T - 9_000), T, TO),
            Waking
        );
        // Timeouts.
        assert_eq!(
            derive_status(Some(false), Some(T - 180_000), None, T, TO),
            Offline
        );
        assert_eq!(derive_status(None, Some(T - 180_000), None, T, TO), Unknown);
        // A boot without a wake (pressed the power button) still shows booting.
        assert_eq!(
            derive_status(Some(false), None, Some(T - 1_000), T, TO),
            Booting
        );
    }

    #[tokio::test]
    async fn tcp_refused_counts_as_alive() {
        // Bind then drop to find a port with nothing listening.
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        assert!(tcp_alive(addr).await);
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        assert!(tcp_alive(l.local_addr().unwrap()).await);
    }

    #[tokio::test]
    async fn monitor_logs_wake_timeout() {
        let rev = crate::new_revision();
        let boot = Arc::new(BootService::in_memory(BootSettings::default(), rev.clone()));
        let m = Monitor::new(
            None,
            boot.clone(),
            rev,
            Duration::from_secs(5),
            TO,
            "My PC".into(),
        );
        m.tick(T).await;
        assert_eq!(m.snapshot().status, PcStatus::Unknown);
        boot.record_wol_at(crate::state::Os::Windows, Ok(()), T)
            .await;
        m.tick(T + 1_000).await;
        assert_eq!(m.snapshot().status, PcStatus::Waking);
        boot.serve_grub_at("192.168.1.50".parse().unwrap(), T + 20_000)
            .await;
        m.tick(T + 21_000).await;
        assert_eq!(m.snapshot().status, PcStatus::Booting);
        m.tick(T + 400_000).await;
        assert_eq!(m.snapshot().status, PcStatus::Unknown);
    }
}
