use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_bind_addr")]
    pub bind_addr: String,
    #[serde(default = "default_max_events")]
    pub max_events: usize,
    #[serde(default)]
    pub max_bytes: usize,
    /// Maximum concurrent subscriptions per connection (advertised in NIP-11)
    #[serde(default = "default_max_subscriptions")]
    pub max_subscriptions: usize,
    /// Maximum value a client may request for `limit` in a filter (advertised in NIP-11)
    #[serde(default = "default_max_limit")]
    pub max_limit: usize,
}

fn default_bind_addr() -> String {
    "0.0.0.0:8080".to_string()
}

fn default_max_events() -> usize {
    1_000_000
}

fn default_max_subscriptions() -> usize {
    300
}

fn default_max_limit() -> usize {
    5000
}

impl Config {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let cfg = config::Config::builder()
            .add_source(config::File::with_name(path))
            .add_source(config::Environment::default())
            .build()?;

        Ok(cfg.try_deserialize()?)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind_addr: default_bind_addr(),
            max_events: default_max_events(),
            max_bytes: 0,
            max_subscriptions: default_max_subscriptions(),
            max_limit: default_max_limit(),
        }
    }
}
