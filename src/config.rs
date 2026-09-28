//! Configuration from environment variables.
//!
//! `Config::from_map` takes a plain map so tests never touch the process
//! environment. Empty values count as unset, which is what Portainer and
//! `${VAR:-}` interpolation produce for fields left blank.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::time::Duration;

use crate::probe::Probe;
use crate::state::{BootSettings, Os};
use crate::wol::MacAddr;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("{0} is required but not set")]
    Missing(&'static str),
    #[error("{var}='{value}' is invalid: {reason}")]
    Invalid {
        var: &'static str,
        value: String,
        reason: String,
    },
}

/// Who may ask the GRUB port for a boot choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowList {
    Any,
    Only(Vec<IpAddr>),
}

impl AllowList {
    pub fn allows(&self, ip: IpAddr) -> bool {
        match self {
            AllowList::Any => true,
            AllowList::Only(list) => list.contains(&ip.to_canonical()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub pc_name: String,
    pub pc_mac: MacAddr,
    pub pc_ip: Option<IpAddr>,
    pub wol_broadcast: Ipv4Addr,
    pub wol_port: u16,
    pub bind_addr: IpAddr,
    pub web_port: u16,
    pub grub_port: u16,
    pub default_boot: Os,
    pub boot_choice_ttl: Option<Duration>,
    pub grub_repeat_window: Duration,
    pub grub_allowed: AllowList,
    pub probes: Vec<Probe>,
    pub probe_interval: Duration,
    pub wake_timeout: Duration,
    pub wake_token: Option<String>,
    pub data_dir: PathBuf,
}

pub const DEFAULT_PROBES: &str = "icmp,tcp:22,tcp:3389,tcp:445";

struct Env<'a>(&'a HashMap<String, String>);

impl Env<'_> {
    fn get(&self, var: &'static str) -> Option<&str> {
        self.0.get(var).map(|s| s.trim()).filter(|s| !s.is_empty())
    }

    fn invalid(var: &'static str, value: &str, reason: impl Into<String>) -> ConfigError {
        ConfigError::Invalid {
            var,
            value: value.to_string(),
            reason: reason.into(),
        }
    }

    fn parse<T>(&self, var: &'static str, default: T) -> Result<T, ConfigError>
    where
        T: std::str::FromStr,
        T::Err: std::fmt::Display,
    {
        match self.get(var) {
            None => Ok(default),
            Some(v) => v
                .parse()
                .map_err(|e: T::Err| Self::invalid(var, v, e.to_string())),
        }
    }

    fn port(&self, var: &'static str, default: u16) -> Result<u16, ConfigError> {
        let p: u16 = self.parse(var, default)?;
        if p == 0 {
            return Err(Self::invalid(var, "0", "port must be 1-65535"));
        }
        Ok(p)
    }

    /// `humantime` durations such as `90s`, `5m`, `6h`, `1h 30m`.
    fn duration(&self, var: &'static str, default: Duration) -> Result<Duration, ConfigError> {
        match self.get(var) {
            None => Ok(default),
            Some("0") => Ok(Duration::ZERO),
            Some(v) => humantime::parse_duration(v)
                .map_err(|e| Self::invalid(var, v, format!("{e} (try values like 90s, 5m or 6h)"))),
        }
    }
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_map(&std::env::vars().collect())
    }

    pub fn from_map(vars: &HashMap<String, String>) -> Result<Self, ConfigError> {
        let env = Env(vars);

        let pc_mac_raw = env.get("PC_MAC").ok_or(ConfigError::Missing("PC_MAC"))?;
        let pc_mac = pc_mac_raw
            .parse::<MacAddr>()
            .map_err(|e| Env::invalid("PC_MAC", pc_mac_raw, e.to_string()))?;

        let pc_ip = match env.get("PC_IP") {
            None => None,
            Some(v) => Some(
                v.parse::<IpAddr>()
                    .map_err(|e| Env::invalid("PC_IP", v, e.to_string()))?
                    .to_canonical(),
            ),
        };

        let default_boot: Os = env.parse("DEFAULT_BOOT", Os::Linux)?;

        let ttl = env.duration("BOOT_CHOICE_TTL", Duration::from_secs(6 * 3600))?;
        let repeat = env.duration("GRUB_REPEAT_WINDOW", Duration::from_secs(60))?;
        if repeat > Duration::from_secs(600) {
            return Err(Env::invalid(
                "GRUB_REPEAT_WINDOW",
                env.get("GRUB_REPEAT_WINDOW").unwrap_or_default(),
                "must be 10 minutes or less, or a real second boot could repeat the choice",
            ));
        }

        let grub_allowed = match env.get("GRUB_ALLOWED_IPS") {
            Some("*") | Some("any") => AllowList::Any,
            Some(list) => {
                let mut ips = Vec::new();
                for part in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    let ip = part
                        .parse::<IpAddr>()
                        .map_err(|e| Env::invalid("GRUB_ALLOWED_IPS", part, e.to_string()))?;
                    ips.push(ip.to_canonical());
                }
                AllowList::Only(ips)
            }
            None => match pc_ip {
                Some(ip) => AllowList::Only(vec![ip]),
                None => AllowList::Any,
            },
        };

        let probes =
            Probe::parse_list(env.get("PROBE").unwrap_or(DEFAULT_PROBES)).map_err(|reason| {
                Env::invalid("PROBE", env.get("PROBE").unwrap_or_default(), reason)
            })?;

        let probe_interval = env.duration("PROBE_INTERVAL", Duration::from_secs(5))?;
        if probe_interval < Duration::from_secs(1) {
            return Err(Env::invalid(
                "PROBE_INTERVAL",
                env.get("PROBE_INTERVAL").unwrap_or_default(),
                "must be at least 1s",
            ));
        }
        let wake_timeout = env.duration("WAKE_TIMEOUT", Duration::from_secs(180))?;
        if wake_timeout < Duration::from_secs(10) {
            return Err(Env::invalid(
                "WAKE_TIMEOUT",
                env.get("WAKE_TIMEOUT").unwrap_or_default(),
                "must be at least 10s",
            ));
        }

        let web_port = env.port("WEB_PORT", 8080)?;
        let grub_port = env.port("GRUB_PROTOCOL_PORT", 8081)?;
        if web_port == grub_port {
            return Err(Env::invalid(
                "GRUB_PROTOCOL_PORT",
                &grub_port.to_string(),
                "must differ from WEB_PORT",
            ));
        }

        Ok(Config {
            pc_name: env.get("PC_NAME").unwrap_or("My PC").to_string(),
            pc_mac,
            pc_ip,
            wol_broadcast: env.parse("WOL_BROADCAST", Ipv4Addr::BROADCAST)?,
            wol_port: env.port("WOL_PORT", 9)?,
            bind_addr: env.parse("BIND_ADDR", IpAddr::V4(Ipv4Addr::UNSPECIFIED))?,
            web_port,
            grub_port,
            default_boot,
            boot_choice_ttl: (!ttl.is_zero()).then_some(ttl),
            grub_repeat_window: repeat,
            grub_allowed,
            probes,
            probe_interval,
            wake_timeout,
            wake_token: env.get("WAKE_TOKEN").map(str::to_string),
            data_dir: PathBuf::from(env.get("DATA_DIR").unwrap_or("/data")),
        })
    }

    pub fn boot_settings(&self) -> BootSettings {
        BootSettings {
            default: self.default_boot,
            ttl: self.boot_choice_ttl,
            repeat_window: self.grub_repeat_window,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn invalid_var(r: Result<Config, ConfigError>) -> &'static str {
        match r {
            Err(ConfigError::Invalid { var, .. }) => var,
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn defaults() {
        let c = Config::from_map(&vars(&[("PC_MAC", "00:11:22:33:44:55")])).unwrap();
        assert_eq!(c.pc_name, "My PC");
        assert_eq!(c.pc_ip, None);
        assert_eq!(c.wol_broadcast, Ipv4Addr::BROADCAST);
        assert_eq!(c.wol_port, 9);
        assert_eq!(c.web_port, 8080);
        assert_eq!(c.grub_port, 8081);
        assert_eq!(c.default_boot, Os::Linux);
        assert_eq!(c.boot_choice_ttl, Some(Duration::from_secs(6 * 3600)));
        assert_eq!(c.grub_repeat_window, Duration::from_secs(60));
        assert_eq!(c.grub_allowed, AllowList::Any);
        assert_eq!(c.probes.len(), 4);
        assert_eq!(c.wake_token, None);
        assert_eq!(c.data_dir, PathBuf::from("/data"));
    }

    #[test]
    fn full_config() {
        let c = Config::from_map(&vars(&[
            ("PC_NAME", "Desk"),
            ("PC_MAC", "AA-BB-CC-DD-EE-FF"),
            ("PC_IP", "192.168.1.50"),
            ("WOL_BROADCAST", "192.168.1.255"),
            ("WOL_PORT", "7"),
            ("WEB_PORT", "9000"),
            ("GRUB_PROTOCOL_PORT", "9001"),
            ("DEFAULT_BOOT", "Windows"),
            ("BOOT_CHOICE_TTL", "0"),
            ("GRUB_REPEAT_WINDOW", "30s"),
            ("PROBE", "tcp:22"),
            ("WAKE_TOKEN", "hunter2"),
            ("DATA_DIR", "/tmp/wake"),
        ]))
        .unwrap();
        assert_eq!(c.pc_name, "Desk");
        assert_eq!(c.pc_ip, Some("192.168.1.50".parse().unwrap()));
        assert_eq!(
            c.grub_allowed,
            AllowList::Only(vec!["192.168.1.50".parse().unwrap()])
        );
        assert_eq!(c.default_boot, Os::Windows);
        assert_eq!(c.boot_choice_ttl, None);
        assert_eq!(c.grub_repeat_window, Duration::from_secs(30));
        assert_eq!(c.probes, vec![Probe::Tcp(22)]);
        assert_eq!(c.wake_token.as_deref(), Some("hunter2"));
    }

    #[test]
    fn blank_values_are_unset() {
        let c = Config::from_map(&vars(&[
            ("PC_MAC", "00:11:22:33:44:55"),
            ("PC_IP", ""),
            ("WAKE_TOKEN", "  "),
            ("WEB_PORT", ""),
        ]))
        .unwrap();
        assert_eq!(c.pc_ip, None);
        assert_eq!(c.wake_token, None);
        assert_eq!(c.web_port, 8080);
    }

    #[test]
    fn allowlist_override() {
        let c = Config::from_map(&vars(&[
            ("PC_MAC", "00:11:22:33:44:55"),
            ("PC_IP", "192.168.1.50"),
            ("GRUB_ALLOWED_IPS", "any"),
        ]))
        .unwrap();
        assert_eq!(c.grub_allowed, AllowList::Any);
        let c = Config::from_map(&vars(&[
            ("PC_MAC", "00:11:22:33:44:55"),
            ("GRUB_ALLOWED_IPS", "10.0.0.1, 10.0.0.2"),
        ]))
        .unwrap();
        assert!(c.grub_allowed.allows("10.0.0.2".parse().unwrap()));
        assert!(c.grub_allowed.allows("::ffff:10.0.0.1".parse().unwrap()));
        assert!(!c.grub_allowed.allows("10.0.0.3".parse().unwrap()));
    }

    #[test]
    fn errors_name_the_variable() {
        assert_eq!(
            Config::from_map(&vars(&[])).unwrap_err(),
            ConfigError::Missing("PC_MAC")
        );
        let base = [("PC_MAC", "00:11:22:33:44:55")];
        let with = |k: &'static str, v: &'static str| {
            let mut m = vars(&base);
            m.insert(k.into(), v.into());
            Config::from_map(&m)
        };
        assert_eq!(invalid_var(with("PC_MAC", "nope")), "PC_MAC");
        assert_eq!(invalid_var(with("PC_IP", "192.168.1")), "PC_IP");
        assert_eq!(invalid_var(with("WOL_BROADCAST", "::1")), "WOL_BROADCAST");
        assert_eq!(invalid_var(with("WOL_PORT", "70000")), "WOL_PORT");
        assert_eq!(invalid_var(with("WEB_PORT", "0")), "WEB_PORT");
        assert_eq!(
            invalid_var(with("GRUB_PROTOCOL_PORT", "8080")),
            "GRUB_PROTOCOL_PORT"
        );
        assert_eq!(invalid_var(with("DEFAULT_BOOT", "macos")), "DEFAULT_BOOT");
        assert_eq!(
            invalid_var(with("BOOT_CHOICE_TTL", "soon")),
            "BOOT_CHOICE_TTL"
        );
        assert_eq!(
            invalid_var(with("GRUB_REPEAT_WINDOW", "1h")),
            "GRUB_REPEAT_WINDOW"
        );
        assert_eq!(
            invalid_var(with("GRUB_ALLOWED_IPS", "10.0.0.1,pc")),
            "GRUB_ALLOWED_IPS"
        );
        assert_eq!(invalid_var(with("PROBE", "udp:53")), "PROBE");
        assert_eq!(
            invalid_var(with("PROBE_INTERVAL", "100ms")),
            "PROBE_INTERVAL"
        );
        assert_eq!(invalid_var(with("WAKE_TIMEOUT", "1s")), "WAKE_TIMEOUT");
    }
}
