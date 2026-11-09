use serde::Deserialize;
use std::collections::HashMap;

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
    /// Largest share of the payload budget a kind may hold, in percent, e.g.
    /// `1059: 20`. Over its share, a kind loses its earliest-received events
    /// first. Kinds not listed share whatever is left.
    #[serde(default)]
    pub kind_quota: HashMap<String, f64>,
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

    /// Parsed `kind_quota` as fractions of the payload budget.
    pub fn kind_quota(&self) -> anyhow::Result<HashMap<u32, f64>> {
        let quota = self
            .kind_quota
            .iter()
            .map(|(kind, percent)| {
                let kind: u32 = kind
                    .trim()
                    .parse()
                    .map_err(|_| anyhow::anyhow!("kind_quota: bad kind {kind:?}"))?;
                if !(percent.is_finite() && *percent > 0.0 && *percent <= 100.0) {
                    anyhow::bail!(
                        "kind_quota: kind {kind} must be above 0 and at most 100, got {percent}"
                    );
                }
                Ok((kind, percent / 100.0))
            })
            .collect::<anyhow::Result<HashMap<_, _>>>()?;
        let total: f64 = quota.values().sum();
        if total > 1.0 + f64::EPSILON {
            anyhow::bail!(
                "kind_quota: shares add up to {:.1}%, more than 100%",
                total * 100.0
            );
        }
        Ok(quota)
    }
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
            kind_quota: HashMap::new(),
            log_dir: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quota(entries: &[(&str, f64)]) -> anyhow::Result<HashMap<u32, f64>> {
        let config = Config {
            kind_quota: entries.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            ..Config::default()
        };
        config.kind_quota()
    }

    #[test]
    fn kind_quota_parses_percentages() {
        assert_eq!(
            quota(&[("1059", 20.0), (" 30078 ", 12.5)]).unwrap(),
            HashMap::from([(1059, 0.2), (30078, 0.125)])
        );
        assert_eq!(quota(&[]).unwrap(), HashMap::new());
        assert!(quota(&[("1059", 60.0), ("30078", 40.0)]).is_ok());
    }

    #[test]
    fn kind_quota_rejects_bad_entries() {
        for bad in [
            vec![("gift", 10.0)],
            vec![("1059", 0.0)],
            vec![("1059", -5.0)],
            vec![("1059", 101.0)],
            vec![("1059", f64::NAN)],
            vec![("1059", 60.0), ("30078", 50.0)],
        ] {
            assert!(quota(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn kind_quota_loads_from_yaml() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, "kind_quota:\n  1059: 20\n  \"30078\": 7.5\n").unwrap();
        let config = Config::load(path.to_str().unwrap()).unwrap();
        assert_eq!(
            config.kind_quota().unwrap(),
            HashMap::from([(1059, 0.2), (30078, 0.075)])
        );
    }
}
