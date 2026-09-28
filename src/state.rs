//! Boot-choice state and the one-shot consumption rules.
//!
//! Everything that decides what GRUB is told lives behind one async mutex, and
//! every change is persisted before the lock is released, so concurrent or
//! repeated requests always see a single, ordered history.
//!
//! GRUB can legitimately fetch the answer more than once during a single boot:
//! its network file layer reopens the connection whenever a reader seeks
//! backwards (the decompression sniff in `grub_file_open` does exactly that).
//! So the first request consumes the choice, and further requests from the same
//! IP inside `repeat_window` get the same answer instead of the default, unless
//! a new choice was made since, which is always meant for the next boot.

use std::collections::VecDeque;
use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::store::{Loaded, Store};
use crate::{Millis, Revision, bump, now_ms};

pub const STATE_VERSION: u32 = 1;
const MAX_EVENTS: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Os {
    Linux,
    Windows,
}

impl Os {
    /// The value handed to GRUB: `0` boots Linux, `1` boots Windows.
    pub fn grub_value(self) -> u8 {
        match self {
            Os::Linux => 0,
            Os::Windows => 1,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Os::Linux => "linux",
            Os::Windows => "windows",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Os::Linux => "Linux",
            Os::Windows => "Windows",
        }
    }
}

impl fmt::Display for Os {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("'{0}' isn't an OS Wake knows. Use linux or windows.")]
pub struct InvalidOs(pub String);

impl FromStr for Os {
    type Err = InvalidOs;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "linux" | "0" => Ok(Os::Linux),
            "windows" | "1" => Ok(Os::Windows),
            _ => Err(InvalidOs(s.to_string())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    pub os: Os,
    pub set_at: Millis,
    pub expires_at: Option<Millis>,
}

impl Selection {
    fn expired(&self, now: Millis) -> bool {
        self.expires_at.is_some_and(|t| now >= t)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnswerSource {
    /// A one-shot choice was waiting and has now been used up.
    Choice,
    /// A repeat request inside the repeat window: same answer as last time.
    Repeat,
    /// Nothing was waiting; the configured default.
    Default,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Served {
    pub os: Os,
    pub at: Millis,
    pub ip: IpAddr,
    pub from_choice: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WolRecord {
    pub at: Millis,
    pub next_boot: Os,
    pub ok: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub at: Millis,
    pub kind: EventKind,
    pub message: String,
}

/// Everything written to `state.json`. Unknown or missing fields fall back to
/// defaults so older and newer files both load.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Persisted {
    pub version: u32,
    pub next_boot: Option<Selection>,
    pub last_requested: Option<Os>,
    pub last_served: Option<Served>,
    pub last_wol: Option<WolRecord>,
    pub events: VecDeque<Event>,
}

impl Persisted {
    fn push_event(&mut self, at: Millis, kind: EventKind, message: impl Into<String>) {
        let message = message.into();
        match kind {
            EventKind::Error => tracing::error!(target: "wake::event", "{message}"),
            EventKind::Warning => tracing::warn!(target: "wake::event", "{message}"),
            _ => tracing::info!(target: "wake::event", "{message}"),
        }
        self.events.push_back(Event { at, kind, message });
        while self.events.len() > MAX_EVENTS {
            self.events.pop_front();
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct BootSettings {
    pub default: Os,
    /// How long an unused choice stays valid. `None` keeps it forever.
    pub ttl: Option<Duration>,
    pub repeat_window: Duration,
}

impl Default for BootSettings {
    fn default() -> Self {
        Self {
            default: Os::Linux,
            ttl: Some(Duration::from_secs(6 * 3600)),
            repeat_window: Duration::from_secs(15),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrubAnswer {
    pub os: Os,
    pub source: AnswerSource,
}

#[derive(Debug, Clone, Serialize)]
pub struct NextBoot {
    pub os: Os,
    /// False when nothing was picked and this is just the default.
    pub explicit: bool,
    pub set_at: Option<Millis>,
    pub expires_at: Option<Millis>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BootSnapshot {
    pub next_boot: NextBoot,
    pub default_boot: Os,
    pub last_boot: Option<Served>,
    pub last_wol: Option<WolRecord>,
    pub last_requested: Option<Os>,
    /// Newest first.
    pub events: Vec<Event>,
    pub storage_ok: bool,
}

pub struct BootService {
    state: Mutex<Persisted>,
    store: Option<Store>,
    settings: BootSettings,
    revision: Revision,
    storage_ok: AtomicBool,
    /// The last refused GRUB request we logged, so a retrying GRUB doesn't
    /// flood the event list.
    last_refused: std::sync::Mutex<Option<(IpAddr, Millis)>>,
}

fn millis(d: Duration) -> Millis {
    d.as_millis() as Millis
}

impl BootService {
    /// State held only in memory. Used by tests.
    pub fn in_memory(settings: BootSettings, revision: Revision) -> Self {
        Self {
            state: Mutex::new(Persisted {
                version: STATE_VERSION,
                ..Default::default()
            }),
            store: None,
            settings,
            revision,
            storage_ok: AtomicBool::new(true),
            last_refused: std::sync::Mutex::new(None),
        }
    }

    /// Loads persisted state, recovering from a corrupt or unreadable file
    /// rather than refusing to start.
    pub async fn load(store: Store, settings: BootSettings, revision: Revision) -> Self {
        let now = now_ms();
        let mut storage_ok = true;
        let mut state = match store.load::<Persisted>().await {
            Ok(Loaded::Loaded(p)) => p,
            Ok(Loaded::Fresh) => {
                let mut p = Persisted::default();
                p.push_event(
                    now,
                    EventKind::Info,
                    "Wake started with a fresh state file.",
                );
                p
            }
            Ok(Loaded::Recovered { backup, reason }) => {
                let mut p = Persisted::default();
                p.push_event(
                    now,
                    EventKind::Warning,
                    format!(
                        "The saved state couldn't be read ({reason}), so Wake started fresh. \
                         The old file was kept as {}.",
                        backup.display()
                    ),
                );
                p
            }
            Err(e) => {
                storage_ok = false;
                let mut p = Persisted::default();
                p.push_event(
                    now,
                    EventKind::Error,
                    format!("Couldn't read saved state: {e}. Starting fresh in memory."),
                );
                p
            }
        };
        state.version = STATE_VERSION;
        let svc = Self {
            state: Mutex::new(state),
            store: Some(store),
            settings,
            revision,
            storage_ok: AtomicBool::new(storage_ok),
            last_refused: std::sync::Mutex::new(None),
        };
        {
            let guard = svc.state.lock().await;
            svc.persist(&guard).await;
        }
        svc
    }

    pub fn settings(&self) -> BootSettings {
        self.settings
    }

    pub fn storage_ok(&self) -> bool {
        self.storage_ok.load(Ordering::Relaxed)
    }

    async fn persist(&self, state: &Persisted) {
        let Some(store) = &self.store else { return };
        match store.save(state).await {
            Ok(()) => {
                if !self.storage_ok.swap(true, Ordering::Relaxed) {
                    tracing::info!(path = %store.path().display(), "state file is writable again");
                }
            }
            Err(e) => {
                if self.storage_ok.swap(false, Ordering::Relaxed) {
                    tracing::error!(error = %e, "couldn't save state; changes will be lost on restart");
                }
            }
        }
    }

    async fn commit(&self, state: &Persisted) {
        self.persist(state).await;
        bump(&self.revision);
    }

    pub async fn select(&self, os: Os) -> Selection {
        self.select_at(os, now_ms()).await
    }

    pub async fn select_at(&self, os: Os, now: Millis) -> Selection {
        let mut s = self.state.lock().await;
        let selection = Selection {
            os,
            set_at: now,
            expires_at: self.settings.ttl.map(|ttl| now + millis(ttl)),
        };
        s.next_boot = Some(selection);
        s.last_requested = Some(os);
        s.push_event(now, EventKind::Info, format!("Next boot set to {os}."));
        self.commit(&s).await;
        selection
    }

    pub async fn clear(&self) {
        self.clear_at(now_ms()).await
    }

    pub async fn clear_at(&self, now: Millis) {
        let mut s = self.state.lock().await;
        s.next_boot = None;
        let default = self.settings.default;
        s.push_event(
            now,
            EventKind::Info,
            format!("Next boot reset to the default ({default})."),
        );
        self.commit(&s).await;
    }

    /// The answer for a GRUB request from `ip`. Consumes the one-shot choice.
    pub async fn serve_grub(&self, ip: IpAddr) -> GrubAnswer {
        self.serve_grub_at(ip, now_ms()).await
    }

    pub async fn serve_grub_at(&self, ip: IpAddr, now: Millis) -> GrubAnswer {
        let mut s = self.state.lock().await;

        let chosen_since = |at: Millis| s.next_boot.is_some_and(|sel| sel.set_at > at);
        if let Some(last) = s.last_served
            && last.ip == ip
            && now >= last.at
            && now - last.at < millis(self.settings.repeat_window)
            && !chosen_since(last.at)
        {
            tracing::debug!(%ip, os = %last.os, "repeat GRUB request inside the repeat window");
            return GrubAnswer {
                os: last.os,
                source: AnswerSource::Repeat,
            };
        }

        let choice = s.next_boot.take();
        let (os, source) = match choice {
            Some(sel) if !sel.expired(now) => (sel.os, AnswerSource::Choice),
            Some(sel) => {
                s.push_event(
                    now,
                    EventKind::Info,
                    format!("The {} choice had expired unused.", sel.os),
                );
                (self.settings.default, AnswerSource::Default)
            }
            None => (self.settings.default, AnswerSource::Default),
        };

        s.last_served = Some(Served {
            os,
            at: now,
            ip,
            from_choice: source == AnswerSource::Choice,
        });
        let message = match source {
            AnswerSource::Choice => {
                format!("GRUB asked from {ip}. Told it {os}; the one-shot choice is used up.")
            }
            _ => format!("GRUB asked from {ip}. Nothing was waiting, so {os} (the default)."),
        };
        s.push_event(now, EventKind::Success, message);
        // Persist before answering so a crash can't hand out the same choice twice.
        self.commit(&s).await;
        GrubAnswer { os, source }
    }

    /// What the next GRUB request would get, without consuming anything.
    pub async fn peek_at(&self, now: Millis) -> Os {
        let s = self.state.lock().await;
        match s.next_boot {
            Some(sel) if !sel.expired(now) => sel.os,
            _ => self.settings.default,
        }
    }

    pub async fn record_wol_at(&self, next_boot: Os, result: Result<(), String>, now: Millis) {
        let mut s = self.state.lock().await;
        match &result {
            Ok(()) => s.push_event(
                now,
                EventKind::Info,
                format!("Magic packet sent. Next boot: {next_boot}."),
            ),
            Err(e) => s.push_event(
                now,
                EventKind::Error,
                format!("Couldn't send the magic packet: {e}"),
            ),
        }
        s.last_wol = Some(WolRecord {
            at: now,
            next_boot,
            ok: result.is_ok(),
            error: result.err(),
        });
        self.commit(&s).await;
    }

    /// Records a GRUB request refused by the allowlist, at most once a minute
    /// per address. Returns true if it was logged.
    pub async fn note_refused_at(&self, ip: IpAddr, now: Millis) -> bool {
        {
            let mut last = self.last_refused.lock().expect("refused lock");
            if let Some((prev, at)) = *last
                && prev == ip
                && now >= at
                && now - at < 60_000
            {
                return false;
            }
            *last = Some((ip, now));
        }
        self.log_at(
            EventKind::Warning,
            format!(
                "GRUB asked from {ip}, which isn't allowed, so it booted its default. \
                 Add {ip} to GRUB_ALLOWED_IPS, or give GRUB the PC's usual address \
                 (WAKE_NET=static)."
            ),
            now,
        )
        .await;
        true
    }

    pub async fn log_at(&self, kind: EventKind, message: impl Into<String>, now: Millis) {
        let mut s = self.state.lock().await;
        s.push_event(now, kind, message);
        self.commit(&s).await;
    }

    /// Drops an expired choice so the UI stops showing it. Returns true if it did.
    pub async fn sweep_at(&self, now: Millis) -> bool {
        let mut s = self.state.lock().await;
        match s.next_boot {
            Some(sel) if sel.expired(now) => {
                s.next_boot = None;
                let default = self.settings.default;
                s.push_event(
                    now,
                    EventKind::Info,
                    format!(
                        "The {} choice expired unused. Next boot is back to {default}.",
                        sel.os
                    ),
                );
                self.commit(&s).await;
                true
            }
            _ => false,
        }
    }

    pub async fn snapshot_at(&self, now: Millis) -> BootSnapshot {
        let s = self.state.lock().await;
        let next_boot = match s.next_boot {
            Some(sel) if !sel.expired(now) => NextBoot {
                os: sel.os,
                explicit: true,
                set_at: Some(sel.set_at),
                expires_at: sel.expires_at,
            },
            _ => NextBoot {
                os: self.settings.default,
                explicit: false,
                set_at: None,
                expires_at: None,
            },
        };
        BootSnapshot {
            next_boot,
            default_boot: self.settings.default,
            last_boot: s.last_served,
            last_wol: s.last_wol.clone(),
            last_requested: s.last_requested,
            events: s.events.iter().rev().cloned().collect(),
            storage_ok: self.storage_ok(),
        }
    }

    pub async fn last_wol_at(&self) -> Option<Millis> {
        self.state.lock().await.last_wol.as_ref().map(|w| w.at)
    }

    pub async fn last_grub_at(&self) -> Option<Millis> {
        self.state.lock().await.last_served.map(|s| s.at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::sync::Arc;

    const PC: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50));
    const OTHER: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 99));
    const T0: Millis = 1_700_000_000_000;

    fn svc() -> BootService {
        BootService::in_memory(BootSettings::default(), crate::new_revision())
    }

    #[test]
    fn os_parsing_and_values() {
        assert_eq!("Windows".parse::<Os>(), Ok(Os::Windows));
        assert_eq!(" linux ".parse::<Os>(), Ok(Os::Linux));
        assert_eq!("1".parse::<Os>(), Ok(Os::Windows));
        assert!("".parse::<Os>().is_err());
        assert!("macos".parse::<Os>().is_err());
        assert_eq!(Os::Linux.grub_value(), 0);
        assert_eq!(Os::Windows.grub_value(), 1);
    }

    #[tokio::test]
    async fn default_when_nothing_selected() {
        let s = svc();
        let a = s.serve_grub_at(PC, T0).await;
        assert_eq!(
            a,
            GrubAnswer {
                os: Os::Linux,
                source: AnswerSource::Default
            }
        );
    }

    #[tokio::test]
    async fn choice_is_consumed_once() {
        let s = svc();
        s.select_at(Os::Windows, T0).await;
        assert_eq!(s.peek_at(T0).await, Os::Windows);
        let first = s.serve_grub_at(PC, T0 + 1_000).await;
        assert_eq!(
            first,
            GrubAnswer {
                os: Os::Windows,
                source: AnswerSource::Choice
            }
        );
        // Next real boot, well after the repeat window: back to the default.
        let later = s.serve_grub_at(PC, T0 + 10 * 60_000).await;
        assert_eq!(
            later,
            GrubAnswer {
                os: Os::Linux,
                source: AnswerSource::Default
            }
        );
        assert!(!s.snapshot_at(T0 + 10 * 60_000).await.next_boot.explicit);
    }

    #[tokio::test]
    async fn repeats_inside_window_get_same_answer() {
        let s = svc();
        s.select_at(Os::Windows, T0).await;
        s.serve_grub_at(PC, T0).await;
        for dt in [0, 1, 500, 10_000, 14_999] {
            let a = s.serve_grub_at(PC, T0 + dt).await;
            assert_eq!(
                a,
                GrubAnswer {
                    os: Os::Windows,
                    source: AnswerSource::Repeat
                },
                "dt={dt}"
            );
        }
        let after = s.serve_grub_at(PC, T0 + 15_000).await;
        assert_eq!(after.os, Os::Linux);
    }

    #[tokio::test]
    async fn repeat_window_is_per_client() {
        let s = svc();
        s.select_at(Os::Windows, T0).await;
        assert_eq!(s.serve_grub_at(PC, T0).await.os, Os::Windows);
        assert_eq!(s.serve_grub_at(OTHER, T0 + 10).await.os, Os::Linux);
    }

    #[tokio::test]
    async fn a_choice_made_after_grub_asked_is_for_the_next_boot() {
        // A quick reboot inside the repeat window must not replay the old
        // answer once something new has been picked.
        let s = BootService::in_memory(
            BootSettings {
                default: Os::Windows,
                ..Default::default()
            },
            crate::new_revision(),
        );
        assert_eq!(s.serve_grub_at(PC, T0).await.os, Os::Windows);
        s.select_at(Os::Linux, T0 + 3_000).await;
        let a = s.serve_grub_at(PC, T0 + 4_000).await;
        assert_eq!(
            a,
            GrubAnswer {
                os: Os::Linux,
                source: AnswerSource::Choice
            }
        );
        assert!(!s.snapshot_at(T0 + 4_000).await.next_boot.explicit);
        // And that boot's own repeats agree with it.
        assert_eq!(
            s.serve_grub_at(PC, T0 + 4_100).await.source,
            AnswerSource::Repeat
        );
    }

    #[tokio::test]
    async fn repeats_without_a_new_choice_keep_the_answer() {
        let s = svc();
        s.select_at(Os::Windows, T0).await;
        s.serve_grub_at(PC, T0).await;
        // Nothing new picked: a re-fetch in the same boot still says Windows.
        assert_eq!(s.serve_grub_at(PC, T0 + 6_000).await.os, Os::Windows);
    }

    #[tokio::test]
    async fn expired_choice_falls_back_to_default() {
        let s = BootService::in_memory(
            BootSettings {
                ttl: Some(Duration::from_secs(60)),
                ..Default::default()
            },
            crate::new_revision(),
        );
        s.select_at(Os::Windows, T0).await;
        assert_eq!(s.peek_at(T0 + 59_000).await, Os::Windows);
        assert_eq!(s.peek_at(T0 + 60_000).await, Os::Linux);
        let a = s.serve_grub_at(PC, T0 + 61_000).await;
        assert_eq!(
            a,
            GrubAnswer {
                os: Os::Linux,
                source: AnswerSource::Default
            }
        );
    }

    #[tokio::test]
    async fn sweep_drops_expired_choice() {
        let s = BootService::in_memory(
            BootSettings {
                ttl: Some(Duration::from_secs(1)),
                ..Default::default()
            },
            crate::new_revision(),
        );
        s.select_at(Os::Windows, T0).await;
        assert!(!s.sweep_at(T0 + 500).await);
        assert!(s.sweep_at(T0 + 1_000).await);
        assert!(!s.snapshot_at(T0 + 1_000).await.next_boot.explicit);
    }

    #[tokio::test]
    async fn no_ttl_keeps_choice() {
        let s = BootService::in_memory(
            BootSettings {
                ttl: None,
                ..Default::default()
            },
            crate::new_revision(),
        );
        s.select_at(Os::Windows, T0).await;
        assert_eq!(s.peek_at(T0 + 365 * 24 * 3600 * 1000).await, Os::Windows);
    }

    #[tokio::test]
    async fn windows_default_is_respected() {
        let s = BootService::in_memory(
            BootSettings {
                default: Os::Windows,
                ..Default::default()
            },
            crate::new_revision(),
        );
        assert_eq!(s.serve_grub_at(PC, T0).await.os, Os::Windows);
    }

    #[tokio::test]
    async fn concurrent_requests_consume_exactly_once() {
        let s = Arc::new(svc());
        s.select_at(Os::Windows, T0).await;
        let mut tasks = Vec::new();
        for i in 0..100u8 {
            let s = s.clone();
            tasks.push(tokio::spawn(async move {
                let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, i));
                s.serve_grub_at(ip, T0 + 1).await
            }));
        }
        let mut windows = 0;
        for t in tasks {
            if t.await.unwrap().os == Os::Windows {
                windows += 1;
            }
        }
        assert_eq!(windows, 1);
    }

    #[tokio::test]
    async fn concurrent_requests_from_the_pc_all_agree() {
        let s = Arc::new(svc());
        s.select_at(Os::Windows, T0).await;
        let tasks: Vec<_> = (0..50)
            .map(|_| {
                let s = s.clone();
                tokio::spawn(async move { s.serve_grub_at(PC, T0 + 1).await.os })
            })
            .collect();
        for t in tasks {
            assert_eq!(t.await.unwrap(), Os::Windows);
        }
    }

    #[tokio::test]
    async fn state_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let settings = BootSettings {
            ttl: None,
            ..Default::default()
        };
        {
            let s =
                BootService::load(Store::new(dir.path()), settings, crate::new_revision()).await;
            s.select_at(Os::Windows, T0).await;
        }
        let s = BootService::load(Store::new(dir.path()), settings, crate::new_revision()).await;
        assert_eq!(s.peek_at(T0).await, Os::Windows);
        assert_eq!(s.serve_grub_at(PC, T0 + 1).await.os, Os::Windows);
        drop(s);
        // Consumption (and the repeat window) survive a restart too.
        let s = BootService::load(Store::new(dir.path()), settings, crate::new_revision()).await;
        assert_eq!(
            s.serve_grub_at(PC, T0 + 2).await.source,
            AnswerSource::Repeat
        );
        assert_eq!(s.serve_grub_at(PC, T0 + 120_000).await.os, Os::Linux);
    }

    #[tokio::test]
    async fn corrupt_state_starts_fresh_with_warning() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("state.json"), b"\0\0garbage").unwrap();
        let s = BootService::load(
            Store::new(dir.path()),
            BootSettings::default(),
            crate::new_revision(),
        )
        .await;
        let snap = s.snapshot_at(T0).await;
        assert_eq!(snap.next_boot.os, Os::Linux);
        assert_eq!(snap.events[0].kind, EventKind::Warning);
        assert!(snap.storage_ok);
    }

    #[tokio::test]
    async fn events_are_capped() {
        let s = svc();
        for i in 0..(MAX_EVENTS as u64 + 10) {
            s.log_at(EventKind::Info, format!("e{i}"), T0 + i).await;
        }
        let snap = s.snapshot_at(T0).await;
        assert_eq!(snap.events.len(), MAX_EVENTS);
        assert_eq!(snap.events[0].message, format!("e{}", MAX_EVENTS + 9));
    }
}
