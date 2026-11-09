use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_bind_addr")]
    pub bind_addr: String,
    /// Target RAM usage as percentage of available system memory (0 = unlimited)
    #[serde(default = "default_target_ram_percent")]
    pub target_ram_percent: u8,
    /// Maximum bytes for exact memory limit (0 = use target_ram_percent)
    #[serde(default)]
    pub max_bytes: usize,
    /// Maximum concurrent subscriptions per connection (advertised in NIP-11)
    #[serde(default = "default_max_subscriptions")]
    pub max_subscriptions: usize,
    /// Maximum value a client may request for `limit` in a filter (advertised in NIP-11)
    #[serde(default = "default_max_limit")]
    pub max_limit: usize,
    /// Seconds a connection may stay completely silent (no REQ, no EVENT) after
    /// connecting before it is closed. Reaps probe/half-open connection churn
    /// that would otherwise hold a slot and a task. 0 disables the timer.
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout: u64,
    /// Enable event persistence to disk (optional path)
    #[serde(default)]
    pub persistence_path: Option<String>,
    /// Background persistence interval in seconds (default: 60)
    #[serde(default = "default_persistence_interval")]
    pub persistence_interval: u64,
    /// CIDRs (or plain IPs) of reverse proxies whose `X-Forwarded-For` /
    /// `X-Real-IP` headers may be believed. Empty means the peer address of
    /// the TCP connection is always used, which is the safe default: a client
    /// can send any header it likes.
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
    /// Prometheus server URL to proxy metrics requests (optional)
    /// When set, /metrics endpoint will proxy to this server
    #[serde(default)]
    pub prometheus_url: Option<String>,
    /// Maximum distinct REQs, clients and IPs tracked by /analytics. Least recently seen are pruned.
    #[serde(default = "default_analytics_max_entries")]
    pub analytics_max_entries: usize,
    /// Bearer token required for /analytics. When unset the endpoint is open.
    #[serde(default)]
    pub analytics_token: Option<String>,
    /// How long events of a kind are kept after the relay receives them, e.g.
    /// `1059: 2d`. Kinds not listed are kept until memory eviction.
    #[serde(default)]
    pub kind_retention: HashMap<String, String>,
    /// Directory for the rotating log file. When unset, logs go to stdout only.
    #[serde(default)]
    pub log_dir: Option<String>,
}

impl Config {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let cfg = config::Config::builder()
            .add_source(config::File::with_name(path).format(config::FileFormat::Yaml))
            .add_source(config::Environment::default())
            .build()?;

        Ok(cfg.try_deserialize()?)
    }

    pub fn kind_retention(&self) -> anyhow::Result<HashMap<u32, Duration>> {
        self.kind_retention
            .iter()
            .map(|(kind, ttl)| {
                let kind: u32 = kind
                    .trim()
                    .parse()
                    .map_err(|_| anyhow::anyhow!("kind_retention: bad kind {kind:?}"))?;
                let ttl = parse_duration(ttl).ok_or_else(|| {
                    anyhow::anyhow!("kind_retention: bad duration {ttl:?} for kind {kind}")
                })?;
                Ok((kind, ttl))
            })
            .collect()
    }
}

fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (number, unit) = s.split_at(split);
    let number: u64 = number.parse().ok()?;
    let unit_secs = match unit.trim() {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        _ => return None,
    };
    let secs = number.checked_mul(unit_secs)?;
    (secs > 0).then(|| Duration::from_secs(secs))
}

fn default_bind_addr() -> String {
    "0.0.0.0:8080".to_string()
}

fn default_target_ram_percent() -> u8 {
    80 // 80% of available RAM by default
}

fn default_max_subscriptions() -> usize {
    300
}

fn default_max_limit() -> usize {
    5000
}

fn default_idle_timeout() -> u64 {
    10
}

fn default_analytics_max_entries() -> usize {
    10_000
}

fn default_persistence_interval() -> u64 {
    60
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind_addr: default_bind_addr(),
            target_ram_percent: default_target_ram_percent(),
            max_bytes: 0,
            max_subscriptions: default_max_subscriptions(),
            max_limit: default_max_limit(),
            idle_timeout: default_idle_timeout(),
            persistence_path: None,
            persistence_interval: default_persistence_interval(),
            trusted_proxies: Vec::new(),
            prometheus_url: None,
            analytics_max_entries: default_analytics_max_entries(),
            analytics_token: None,
            kind_retention: HashMap::new(),
            log_dir: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_retention_durations() {
        assert_eq!(parse_duration("90"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("45m"), Some(Duration::from_secs(2700)));
        assert_eq!(parse_duration("36h"), Some(Duration::from_secs(129_600)));
        assert_eq!(parse_duration(" 2d "), Some(Duration::from_secs(172_800)));
        assert_eq!(parse_duration("1w"), Some(Duration::from_secs(604_800)));
        for bad in ["", "0", "0h", "h", "2x", "-1d", "1.5d"] {
            assert_eq!(parse_duration(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn kind_retention_rejects_bad_entries() {
        let mut config = Config::default();
        config.kind_retention.insert("1059".into(), "2d".into());
        assert_eq!(
            config.kind_retention().unwrap(),
            HashMap::from([(1059, Duration::from_secs(172_800))])
        );

        config.kind_retention.insert("gift".into(), "2d".into());
        assert!(config.kind_retention().is_err());
        config.kind_retention.remove("gift");
        config.kind_retention.insert("30078".into(), "soon".into());
        assert!(config.kind_retention().is_err());
    }

    #[test]
    fn kind_retention_loads_from_yaml() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, "kind_retention:\n  1059: 2d\n  \"30078\": 12h\n").unwrap();
        let config = Config::load(path.to_str().unwrap()).unwrap();
        assert_eq!(
            config.kind_retention().unwrap(),
            HashMap::from([
                (1059, Duration::from_secs(172_800)),
                (30078, Duration::from_secs(43_200)),
            ])
        );
    }
}
