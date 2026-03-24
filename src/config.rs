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
    /// Enable event persistence to disk (optional path)
    #[serde(default)]
    pub persistence_path: Option<String>,
    /// Background persistence interval in seconds (default: 60)
    #[serde(default = "default_persistence_interval")]
    pub persistence_interval: u64,
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
            persistence_path: None,
            persistence_interval: default_persistence_interval(),
        }
    }
}
