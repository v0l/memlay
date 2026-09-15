use serde::Deserialize;

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
            log_dir: None,
        }
    }
}
