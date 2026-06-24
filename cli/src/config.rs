use std::path::Path;

use color_eyre::eyre;
use malachitebft_app::node::NodeConfig;
pub use malachitebft_config::{
    BootstrapProtocol, ConsensusConfig, DiscoveryConfig, LoggingConfig, MempoolConfig,
    MempoolLoadConfig, MetricsConfig, P2pConfig, PubSubProtocol, RuntimeConfig, ScoringStrategy,
    Selector, TestConfig, TimeoutConfig, TransportProtocol, ValuePayload, ValueSyncConfig,
};
use malachitebft_eth_types::{Address, RetryConfig};
use serde::{Deserialize, Serialize};
use tokio::time::Duration;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum ElNodeType {
    /// No pruning - keeps all historical data
    #[default]
    Archive,
    /// Standard pruning - keeps recent data based on distance
    Full,
    /// Custom pruning configuration
    Custom,
}

// `fee_recipient` below is deprecated; `#[allow(deprecated)]` keeps the derived
// impls from tripping the `deprecated` lint (denied in CI) while still warning
// on hand-written accesses elsewhere.
#[allow(deprecated)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EmeraldConfig {
    /// A custom human-readable name for this node
    pub moniker: String,

    /// Execution layer config
    pub ethereum_config: EthereumConfig,

    /// Retry configuration for execution client sync operations
    #[serde(default)]
    pub retry_config: RetryConfig,

    /// Type of execution layer node (archive, full, or custom)
    #[serde(default)]
    pub el_node_type: ElNodeType,

    /// Number of certificates to retain.
    /// Default is retain all (u64::MAX).
    /// Once the certificates are deleted those blocks
    /// cannot be validated on this node.
    /// Has to be >= num_temp_blocks_retained (whose default is 10)
    #[serde(default = "default_num_certificates_to_retain")]
    pub num_certificates_to_retain: u64,

    /// Number of blocks to wait before attempting pruning
    /// Note that this applies only to pruning certificates.
    /// Certificates are pruned based on num_certificates_to_retain.
    /// This value cannot be 0.
    /// Default: 10.
    #[serde(default = "prune_at_interval_default")]
    pub prune_at_block_interval: u64,

    /// Key provider configuration (file-based or AWS SM+KMS).
    /// Defaults to file-based for backward compatibility.
    #[serde(default)]
    pub key_provider: key_provider::KeyProviderConfig,

    // Application set min_block_time forcing the app to sleep
    // before moving onto the next height.
    // Malachite does not have a notion of min_block_time, thus
    // this has to be handled by the application.
    // Default: 500ms
    #[serde(with = "humantime_serde", default = "default_min_block_time")]
    pub min_block_time: Duration,

    /// Deprecated: no longer has any effect. Block rewards are sent to the
    /// proposing validator's own address, not to a configured recipient.
    /// Retained (optional) only for backward compatibility with existing configs.
    #[deprecated(
        note = "fee_recipient has no effect; block rewards go to the proposing validator's address"
    )]
    #[serde(default)]
    pub fee_recipient: Option<Address>,

    /// Emerald will store up to num_temp_blocks_retained
    /// blocks locally and then delete them. This data
    /// is stored and managed by the execution layer
    /// thus no need to store it twice.
    /// WARN: For Reth, this parameter has to be equal or greater than
    /// the value of `engine.persistence-threshold` passed
    /// to Reth on startup. If it is lower, on a crash,
    /// the node will NOT be able to restart
    /// Default: 10
    #[serde(default = "default_num_temp_blocks_retained")]
    pub num_temp_blocks_retained: u64,
}

fn default_min_block_time() -> Duration {
    Duration::from_millis(500)
}

fn default_num_certificates_to_retain() -> u64 {
    u64::MAX
}
fn prune_at_interval_default() -> u64 {
    10
}

fn default_num_temp_blocks_retained() -> u64 {
    10
}

fn default_eth_gensesis_path() -> String {
    "./assets/genesis.json".to_string()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EthereumConfig {
    /// RPC endpoint of Ethereum execution client
    pub execution_authrpc_address: String,

    /// RPC endpoint of Ethereum Engine API
    pub engine_authrpc_address: String,

    /// Path of the JWT token file
    pub jwt_token_path: String,

    /// Path of the EVM genesis file
    #[serde(default = "default_eth_gensesis_path")]
    pub eth_genesis_path: String,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// A custom human-readable name for this node
    pub moniker: String,

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
}

impl NodeConfig for Config {
    fn moniker(&self) -> &str {
        &self.moniker
    }

    fn consensus(&self) -> &ConsensusConfig {
        &self.consensus
    }

    fn consensus_mut(&mut self) -> &mut ConsensusConfig {
        &mut self.consensus
    }

    fn value_sync(&self) -> &ValueSyncConfig {
        &self.value_sync
    }

    fn value_sync_mut(&mut self) -> &mut ValueSyncConfig {
        &mut self.value_sync
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emerald_config_defaults_to_file_key_provider() {
        let toml = r#"
moniker = "node-0"
fee_recipient = "0x0000000000000000000000000000000000000000"

[ethereum_config]
execution_authrpc_address = "http://127.0.0.1:8551"
engine_authrpc_address    = "http://127.0.0.1:8552"
jwt_token_path            = "./assets/jwt.hex"
"#;
        let cfg: EmeraldConfig = toml::from_str(toml).unwrap();
        assert!(matches!(
            cfg.key_provider,
            key_provider::KeyProviderConfig::File
        ));
    }

    #[test]
    fn emerald_config_parses_aws_sm_kms_key_provider() {
        let toml = r#"
moniker = "node-0"
fee_recipient = "0x0000000000000000000000000000000000000000"

[ethereum_config]
execution_authrpc_address = "http://127.0.0.1:8551"
engine_authrpc_address    = "http://127.0.0.1:8552"
jwt_token_path            = "./assets/jwt.hex"

[key_provider]
type       = "aws_sm_kms"
secret_id  = "emerald/mainnet/node-0/key"
region     = "ap-east-1"
kms_key_id = "alias/emerald-validator-keys"
"#;
        let cfg: EmeraldConfig = toml::from_str(toml).unwrap();
        match &cfg.key_provider {
            key_provider::KeyProviderConfig::AwsSmKms(c) => {
                assert_eq!(c.secret_id, "emerald/mainnet/node-0/key");
                assert_eq!(c.region, "ap-east-1");
                assert_eq!(c.kms_key_id, "alias/emerald-validator-keys");
                assert!(c.kms_region.is_none());
            }
            other => panic!("expected AwsSmKms, got {:?}", other),
        }
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
