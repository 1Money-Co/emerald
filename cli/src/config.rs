use color_eyre::eyre;
use malachitebft_app::node::NodeConfig;
use serde::{Deserialize, Serialize};
use std::path::Path;

pub use malachitebft_config::{
    BootstrapProtocol, ConsensusConfig, DiscoveryConfig, LoggingConfig, MempoolConfig,
    MempoolLoadConfig, MetricsConfig, P2pConfig, PubSubProtocol, RuntimeConfig, ScoringStrategy,
    Selector, TestConfig, TimeoutConfig, TransportProtocol, ValuePayload, ValueSyncConfig,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MalakethConfig {
    /// A custom human-readable name for this node
    pub moniker: String,

    /// Maximum time to wait for execution client to sync before crashing
    #[serde(default = "default_sync_timeout")]
    pub sync_timeout_ms: u64,

    /// Initial retry delay for execution client sync validation
    #[serde(default = "default_sync_initial_delay")]
    pub sync_initial_delay_ms: u64,
}

impl Default for MalakethConfig {
    fn default() -> Self {
        Self {
            moniker: "malaketh-node".to_string(),
            sync_timeout_ms: default_sync_timeout(),
            sync_initial_delay_ms: default_sync_initial_delay(),
        }
    }
}

fn default_sync_timeout() -> u64 {
    30000 // 30 seconds
}

fn default_sync_initial_delay() -> u64 {
    100 // 100 ms
}

pub fn new_malaketh_config(
    moniker: String,
    sync_timeout_ms: u64,
    sync_initial_delay_ms: u64,
) -> MalakethConfig {
    MalakethConfig {
        moniker,
        sync_timeout_ms,
        sync_initial_delay_ms,
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// Consensus configuration options
    pub consensus: ConsensusConfig,

    /// Mempool configuration options
    pub mempool: MempoolConfig,

    /// ValueSync configuration options
    pub value_sync: ValueSyncConfig,

    /// Metrics configuration options
    pub metrics: MetricsConfig,

    /// Log configuration options
    pub logging: LoggingConfig,

    /// Runtime configuration options
    pub runtime: RuntimeConfig,

    /// Test configuration options
    pub test: TestConfig,

    /// Host application configuration
    pub malaketh: MalakethConfig,
}

impl NodeConfig for Config {
    fn moniker(&self) -> &str {
        &self.malaketh.moniker
    }

    fn consensus(&self) -> &ConsensusConfig {
        &self.consensus
    }

    fn value_sync(&self) -> &ValueSyncConfig {
        &self.value_sync
    }
}

pub fn load_config(path: impl AsRef<Path>, prefix: Option<&str>) -> eyre::Result<Config> {
    ::config::Config::builder()
        .add_source(::config::File::from(path.as_ref()))
        .add_source(
            ::config::Environment::with_prefix(prefix.unwrap_or("MALACHITE")).separator("__"),
        )
        .build()?
        .try_deserialize()
        .map_err(Into::into)
}
