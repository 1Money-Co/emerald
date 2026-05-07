//! The Application (or Node) definition. The Node trait implements the Consensus context and the
//! cryptographic library used for signing.

use core::str::FromStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use color_eyre::eyre;
use libp2p_identity::Keypair;
use malachitebft_app_channel::app::events::{RxEvent, TxEvent};
use malachitebft_app_channel::app::metrics::SharedRegistry;
use malachitebft_app_channel::app::node::{
    CanGeneratePrivateKey, CanMakeGenesis, CanMakePrivateKeyFile, EngineHandle, Node, NodeHandle,
};
use malachitebft_app_channel::app::types::core::VotingPower;
use malachitebft_app_channel::Channels;
use malachitebft_eth_cli::config::{Config, EmeraldConfig};
use malachitebft_eth_cli::metrics;
use malachitebft_eth_engine::engine::Engine;
use malachitebft_eth_engine::engine_rpc::EngineRPC;
use malachitebft_eth_engine::ethereum_rpc::EthereumRPC;
use malachitebft_eth_types::codec::proto::ProtobufCodec;
use malachitebft_eth_types::secp256k1::{K256Provider, PrivateKey, PublicKey};
use malachitebft_eth_types::{Address, EmeraldContext, Genesis, Height, Validator, ValidatorSet};
use rand::{CryptoRng, RngCore};
use tokio::task::JoinHandle;
use tracing::info;
use url::Url;

// Use the same types used for integration tests.
// A real application would use its own types and context instead.
use crate::metrics::Metrics;
use crate::state::{State, StateMetrics};
use crate::store::Store;

/// Main application struct implementing the consensus node functionality
#[derive(Clone)]
pub struct App {
    pub config: Config,
    pub home_dir: PathBuf,
    pub genesis_file: PathBuf,
    pub emerald_config_file: PathBuf,
    pub private_key_file: PathBuf,
    pub start_height: Option<Height>,
    /// Private key resolved through `emerald_config.key_provider` at the top of
    /// `build_runtime`. The sync `Node::load_private_key_file` callback (invoked
    /// inside `start_engine`) reads from here so that network keypair, consensus
    /// signing provider, and `State` all share the same key — including in the
    /// AWS SM+KMS provider mode where the file on disk is not the source of truth.
    /// Stays empty for non-engine subcommands (init/testnet/generate).
    pub resolved_private_key: Arc<OnceLock<PrivateKey>>,
}

/// Components needed to run the application
pub struct AppRuntime {
    pub state: State,
    pub channels: Channels<EmeraldContext>,
    pub engine: Engine,
    pub emerald_config: EmeraldConfig,
    pub engine_handle: EngineHandle,
    pub tx_event: TxEvent<EmeraldContext>,
}

impl App {
    /// Build the application state and all necessary components.
    ///
    /// This function performs all the initialization and setup required to run
    /// the application, including loading configuration, initializing the
    /// consensus engine, and creating the state.
    ///
    /// Returns a [AppRuntime] struct containing the state and all components
    /// needed to run the app.
    pub async fn build_runtime(&self) -> eyre::Result<AppRuntime> {
        let config = self.load_config()?;
        let span = tracing::error_span!("node", moniker = %config.moniker);
        let _enter = span.enter();

        let emerald_config = self.load_emerald_config()?;
        log_emerald_config(&self.emerald_config_file, &emerald_config);

        // Resolve the private key once, via the configured provider. Caching it
        // in `resolved_private_key` makes the sync `Node::load_private_key_file`
        // callback invoked inside `start_engine` see the same key — without this,
        // the AWS SM+KMS provider mode would still fall back to the on-disk file
        // for the libp2p keypair and consensus signing provider.
        let key_bytes = build_key_provider(&emerald_config, &self.private_key_file)
            .load_private_key()
            .await
            .map_err(|e| eyre::eyre!("key provider error: {e}"))?;
        let private_key = PrivateKey::from_slice(key_bytes.as_ref())
            .map_err(|e| eyre::eyre!("invalid private key bytes: {e}"))?;
        self.resolved_private_key
            .set(private_key.clone())
            .map_err(|_| eyre::eyre!("private key already resolved"))?;

        let public_key = self.get_public_key(&private_key);
        let address = self.get_address(&public_key);
        let public_key_hex = public_key_hex(&public_key);
        info!(
            public_key = %public_key_hex,
            ?address,
            "loaded node public key and address"
        );
        let signing_provider = self.get_signing_provider(private_key);
        let ctx = EmeraldContext::new();

        let genesis = self.load_genesis()?;
        let initial_validator_set = genesis.validator_set.clone();

        let codec = ProtobufCodec;

        let (channels, engine_handle) = malachitebft_app_channel::start_engine(
            ctx,
            self.clone(),
            config.clone(),
            codec, // WAL codec
            codec, // Network codec
            self.start_height,
            initial_validator_set,
        )
        .await?;

        let tx_event = channels.events.clone();

        let registry = SharedRegistry::global().with_moniker(&config.moniker);
        let metrics = Metrics::register(&registry);

        if config.metrics.enabled {
            tokio::spawn(metrics::serve(config.metrics.listen_addr));
        }

        let store = Store::open(self.get_home_dir().join("store.db"), metrics.db.clone()).await?;
        let start_height = self.start_height.unwrap_or_default();

        // Load cumulative metrics from database for crash recovery
        let (txs_count, chain_bytes, elapsed_seconds) =
            store.load_cumulative_metrics().await?.unwrap_or_else(|| {
                tracing::info!("📊 No metrics found in database, starting with default values");
                (0, 0, 0)
            });

        let state_metrics = StateMetrics {
            txs_count,
            chain_bytes,
            elapsed_seconds,
            metrics,
        };

        let engine: Engine = {
            let engine_url = Url::parse(&emerald_config.ethereum_config.engine_authrpc_address)?;
            let jwt_path = PathBuf::from_str(&emerald_config.ethereum_config.jwt_token_path)?;
            let eth_url = Url::parse(&emerald_config.ethereum_config.execution_authrpc_address)?;
            Engine::new(
                EngineRPC::new(engine_url, jwt_path.as_path())?,
                EthereumRPC::new(eth_url)?,
            )
        };

        // Check the validity of the configuration parameters
        let num_certificates_to_retain = emerald_config.num_certificates_to_retain;
        let num_temp_blocks_retained = emerald_config.num_temp_blocks_retained;

        if num_certificates_to_retain < num_temp_blocks_retained {
            return Err(eyre::eyre!(
                "num_certificates_to_retain has to be >= than num_temp_blocks_retained."
            ));
        }

        let prune_at_block_interval = emerald_config.prune_at_block_interval;

        assert!(
            prune_at_block_interval != 0,
            "prune block interval cannot be 0"
        );

        let state = State::new(
            genesis,
            ctx,
            signing_provider,
            address,
            start_height,
            store,
            state_metrics,
            emerald_config.clone(),
        );

        Ok(AppRuntime {
            state,
            channels,
            engine,
            emerald_config,
            engine_handle,
            tx_event,
        })
    }

    fn load_emerald_config(&self) -> eyre::Result<EmeraldConfig> {
        let emerald_config_content =
            fs::read_to_string(&self.emerald_config_file).map_err(|e| {
                eyre::eyre!(
                    "Failed to read emerald config file `{}`: {e}",
                    self.emerald_config_file.display()
                )
            })?;
        let emerald_config = toml::from_str::<EmeraldConfig>(&emerald_config_content)
            .map_err(|e| eyre::eyre!("Failed to parse emerald config file: {e}"))?;
        Ok(emerald_config)
    }
}

pub struct Handle {
    pub app: JoinHandle<()>,
    pub engine: EngineHandle,
    pub tx_event: TxEvent<EmeraldContext>,
}

#[async_trait]
impl NodeHandle<EmeraldContext> for Handle {
    fn subscribe(&self) -> RxEvent<EmeraldContext> {
        self.tx_event.subscribe()
    }

    async fn kill(&self, _reason: Option<String>) -> eyre::Result<()> {
        self.engine.actor.kill_and_wait(None).await?;
        self.app.abort();
        self.engine.handle.abort();
        Ok(())
    }
}

#[async_trait]
impl Node for App {
    type Context = EmeraldContext;
    type Config = Config;
    type Genesis = Genesis;
    type PrivateKeyFile = PrivateKey;
    type SigningProvider = K256Provider;
    type NodeHandle = Handle;

    fn get_home_dir(&self) -> PathBuf {
        self.home_dir.to_owned()
    }

    fn load_config(&self) -> eyre::Result<Self::Config> {
        Ok(self.config.clone())
    }

    fn get_signing_provider(&self, private_key: PrivateKey) -> Self::SigningProvider {
        K256Provider::new(private_key)
    }

    fn get_address(&self, pk: &PublicKey) -> Address {
        Address::from_public_key(pk)
    }

    fn get_public_key(&self, pk: &PrivateKey) -> PublicKey {
        pk.public_key()
    }

    fn get_keypair(&self, pk: PrivateKey) -> Keypair {
        use libp2p_identity::secp256k1::{Keypair as Secp256k1Keypair, SecretKey};

        let secret_bytes: [u8; 32] = pk.inner().to_bytes().into();
        let secret_key =
            SecretKey::try_from_bytes(secret_bytes).expect("failed to decode secp256k1 secret key");
        Secp256k1Keypair::from(secret_key).into()
    }

    fn load_private_key(&self, file: Self::PrivateKeyFile) -> PrivateKey {
        file
    }

    fn load_private_key_file(&self) -> eyre::Result<Self::PrivateKeyFile> {
        // Engine path (Commands::Start): `build_runtime` has already resolved
        // the key through the configured provider — return the cached copy so
        // the libp2p keypair, consensus signing provider, and `State` all share
        // it. Non-engine subcommands (init/testnet/generate) leave the cache
        // empty and fall through to the on-disk file.
        if let Some(pk) = self.resolved_private_key.get() {
            info!("Using cached private key pre-loaded in build_runtime");
            return Ok(pk.clone());
        }

        let private_key = std::fs::read_to_string(&self.private_key_file)?;
        info!(
            "Loading private key from file: {}",
            self.private_key_file.display()
        );
        serde_json::from_str(&private_key).map_err(Into::into)
    }

    fn load_genesis(&self) -> eyre::Result<Self::Genesis> {
        let genesis = std::fs::read_to_string(&self.genesis_file)?;
        serde_json::from_str(&genesis).map_err(Into::into)
    }

    async fn start(&self) -> eyre::Result<Handle> {
        let AppRuntime {
            mut state,
            mut channels,
            engine,
            emerald_config,
            engine_handle,
            tx_event,
        } = self.build_runtime().await?;

        let app_handle = tokio::spawn(async move {
            if let Err(e) = crate::app::run(&mut state, &mut channels, engine, emerald_config).await
            {
                tracing::error!(%e, "Application error");
            }
        });

        Ok(Handle {
            app: app_handle,
            engine: engine_handle,
            tx_event,
        })
    }

    async fn run(self) -> eyre::Result<()> {
        self.log_startup_fields();
        let handles = self.start().await?;
        handles.app.await.map_err(Into::into)
    }
}

impl App {
    fn log_startup_fields(&self) {
        tracing::info!(
            home_dir = %self.home_dir.display(),
            genesis_file = %self.genesis_file.display(),
            emerald_config_file = %self.emerald_config_file.display(),
            private_key_file = %self.private_key_file.display(),
            start_height = ?self.start_height,
            "Starting Emerald node",
        );
    }
}

fn log_emerald_config(path: &Path, config: &EmeraldConfig) {
    tracing::info!(
        config_file = %path.display(),
        moniker = %config.moniker,
        execution_authrpc_address = %config.ethereum_config.execution_authrpc_address,
        engine_authrpc_address = %config.ethereum_config.engine_authrpc_address,
        jwt_token_path = %config.ethereum_config.jwt_token_path,
        eth_genesis_path = %config.ethereum_config.eth_genesis_path,
        key_provider = key_provider_kind(&config.key_provider),
        min_block_time = ?config.min_block_time,
        fee_recipient = ?config.fee_recipient,
        el_node_type = ?config.el_node_type,
        retry_config = ?config.retry_config,
        num_certificates_to_retain = config.num_certificates_to_retain,
        prune_at_block_interval = config.prune_at_block_interval,
        num_temp_blocks_retained = config.num_temp_blocks_retained,
        emerald_config = ?config,
        "Loaded Emerald configuration",
    );
}

fn key_provider_kind(config: &key_provider::KeyProviderConfig) -> &'static str {
    match config {
        key_provider::KeyProviderConfig::File => "file",
        key_provider::KeyProviderConfig::AwsSmKms(_) => "aws_sm_kms",
    }
}

/// Build the configured `KeyProvider`. The `File` variant takes its path from
/// the CLI (`priv_validator_key.json`) since `EmeraldConfig::File` doesn't
/// carry a path of its own.
fn build_key_provider(
    config: &EmeraldConfig,
    file_fallback: &Path,
) -> Box<dyn key_provider::KeyProvider> {
    match &config.key_provider {
        key_provider::KeyProviderConfig::File => {
            Box::new(key_provider::FileKeyProvider::new(file_fallback))
        }
        key_provider::KeyProviderConfig::AwsSmKms(cfg) => {
            Box::new(key_provider::AwsSmKmsKeyProvider::new(cfg.clone()))
        }
    }
}

fn public_key_hex(public_key: &PublicKey) -> String {
    let uncompressed = public_key.inner().to_encoded_point(false);
    let bytes = uncompressed.as_bytes();

    debug_assert_eq!(bytes.len(), 65);
    debug_assert_eq!(bytes[0], 0x04);

    format!("0x{}", hex::encode(&bytes[1..]))
}

#[cfg(test)]
mod tests {
    use super::{key_provider_kind, public_key_hex};
    use malachitebft_eth_types::secp256k1::PrivateKey;

    #[test]
    fn key_provider_kind_names_file_provider() {
        assert_eq!(
            key_provider_kind(&key_provider::KeyProviderConfig::File),
            "file"
        );
    }

    #[test]
    fn public_key_hex_uses_show_pubkey_format() {
        let private_key = PrivateKey::from_slice(&[7_u8; 32]).unwrap();
        let public_key = private_key.public_key();
        let encoded = public_key_hex(&public_key);

        assert!(encoded.starts_with("0x"));
        let encoded_bytes = hex::decode(encoded.strip_prefix("0x").unwrap()).unwrap();
        let uncompressed = public_key.inner().to_encoded_point(false);

        assert_eq!(encoded_bytes.len(), 64);
        assert_eq!(encoded_bytes, &uncompressed.as_bytes()[1..]);
    }
}

impl CanMakeGenesis for App {
    fn make_genesis(&self, validators: Vec<(PublicKey, VotingPower)>) -> Self::Genesis {
        let validators = validators
            .into_iter()
            .map(|(pk, vp)| Validator::new(pk, vp));

        let validator_set = ValidatorSet::new(validators);

        Genesis { validator_set }
    }
}

impl CanGeneratePrivateKey for App {
    fn generate_private_key<R>(&self, rng: R) -> PrivateKey
    where
        R: RngCore + CryptoRng,
    {
        PrivateKey::generate(rng)
    }
}

impl CanMakePrivateKeyFile for App {
    fn make_private_key_file(&self, private_key: PrivateKey) -> Self::PrivateKeyFile {
        private_key
    }
}
