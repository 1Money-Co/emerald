// Use jemalloc as the global allocator on non-MSVC targets. glibc's default
// malloc is not aggressive about returning freed memory to the OS, which keeps
// RSS elevated for long-running nodes; jemalloc reclaims more eagerly.
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use std::sync::{Arc, OnceLock};

use color_eyre::eyre::{eyre, Result};
use emerald::node::App;
use malachitebft_app_channel::app::node::Node;
use malachitebft_eth_cli::args::{Args, Commands};
use malachitebft_eth_cli::cmd::generate::GenerateCmd;
use malachitebft_eth_cli::cmd::init::InitCmd;
use malachitebft_eth_cli::cmd::start::StartCmd;
use malachitebft_eth_cli::cmd::testnet::TestnetCmd;
use malachitebft_eth_cli::{config, logging, runtime};
use malachitebft_eth_types::Height;
use tracing::{info, trace};

/// Main entry point for the application
///
/// This function:
/// - Parses command-line arguments
/// - Loads configuration from file
/// - Initializes logging system
/// - Sets up error handling
/// - Creates and runs the application node
fn main() -> Result<()> {
    color_eyre::install()?;

    // Load command-line arguments and possible configuration file.
    let args = Args::new();

    // Parse the input command.
    match &args.command {
        Commands::Start(cmd) => start(&args, cmd),
        Commands::Init(cmd) => {
            let logging = logging_from_args(config::LoggingConfig::default(), &args);
            let _guard = logging::init(&logging)?;
            trace!("Command-line parameters: {args:?}");
            init(&args, cmd, logging)
        }
        Commands::Testnet(cmd) => {
            let logging = logging_from_args(config::LoggingConfig::default(), &args);
            let _guard = logging::init(&logging)?;
            trace!("Command-line parameters: {args:?}");
            testnet(&args, cmd, logging)
        }
        Commands::ShowPubkey(cmd) => cmd.run(),
        Commands::DistributedTestnet(_) => unimplemented!(),
        // The `generate` subcommand supersedes the `testnet` subcommand for new
        // deployments: it supports both plaintext file keys (same behaviour as
        // `testnet`) and AWS SM+KMS-backed keys for deployments with higher
        // key-security requirements.  The `testnet` subcommand is kept unchanged
        // for backward compatibility with existing documentation and scripts; new
        // deployments should use `generate`.
        Commands::Generate(cmd) => {
            let logging = logging_from_args(config::LoggingConfig::default(), &args);
            let _guard = logging::init(&logging)?;
            trace!("Command-line parameters: {args:?}");
            generate(cmd, logging)
        }
    }
}

fn logging_from_args(mut logging: config::LoggingConfig, args: &Args) -> config::LoggingConfig {
    if let Some(log_level) = args.log_level {
        logging.log_level = log_level;
    }
    if let Some(log_format) = args.log_format {
        logging.log_format = log_format;
    }
    if let Some(log_file) = args.log_file.clone() {
        logging.file_path = Some(log_file);
    }
    if let Some(log_file_max_size_bytes) = args.log_file_max_size_bytes {
        logging.file_max_size_bytes = log_file_max_size_bytes;
    }
    if let Some(log_file_max_files) = args.log_file_max_files {
        logging.file_max_files = log_file_max_files;
    }
    logging
}

fn start(args: &Args, cmd: &StartCmd) -> Result<()> {
    // Load configuration file if it exists. Some commands do not require a configuration file.
    let config_file = args
        .get_config_file_path()
        .map_err(|error| eyre!("Failed to get configuration file path: {error}"))?;

    let mut config = config::load_config(&config_file, None)
        .map_err(|error| eyre!("Failed to load configuration file: {error}"))?;

    config.logging = logging_from_args(config.logging, args);

    // This is a drop guard responsible for flushing any remaining logs when the program terminates.
    // It must be assigned to a binding that is not _, as _ will result in the guard being dropped immediately.
    let _guard = logging::init(&config.logging)?;

    trace!("Command-line parameters: {args:?}");

    let rt = runtime::build_runtime(config.runtime)?;

    info!(
        file = %args.get_config_file_path().unwrap_or_default().display(),
        "Loaded configuration",
    );

    trace!(?config, "Configuration");

    // Setup the application
    let app = App {
        config,
        home_dir: args.get_home_dir()?,
        genesis_file: args.get_genesis_file_path()?,
        emerald_config_file: args.get_emerald_config_file()?,
        private_key_file: args.get_priv_validator_key_file_path()?,
        start_height: cmd.start_height.map(Height::new),
        resolved_private_key: Arc::new(OnceLock::new()),
    };

    // Start the node
    rt.block_on(app.run())
        .map_err(|error| eyre!("Failed to run the application node: {error}"))
}

fn init(args: &Args, cmd: &InitCmd, logging: config::LoggingConfig) -> Result<()> {
    // Setup the application
    let app = App {
        config: Default::default(), // There is not existing configuration yet
        home_dir: args.get_home_dir()?,
        genesis_file: args.get_genesis_file_path()?,
        emerald_config_file: args.get_emerald_config_file()?,
        private_key_file: args.get_priv_validator_key_file_path()?,
        start_height: Some(Height::new(1)), // We always start at height 1
        resolved_private_key: Arc::new(OnceLock::new()),
    };

    cmd.run(
        &app,
        &args.get_config_file_path()?,
        &args.get_genesis_file_path()?,
        &args.get_priv_validator_key_file_path()?,
        logging,
    )
    .map_err(|error| eyre!("Failed to run init command {error:?}"))
}

fn generate(cmd: &GenerateCmd, logging: config::LoggingConfig) -> Result<()> {
    let app = App {
        config: Default::default(),
        home_dir: Default::default(),
        genesis_file: Default::default(),
        emerald_config_file: Default::default(),
        private_key_file: Default::default(),
        start_height: Some(Height::new(1)),
        resolved_private_key: Arc::new(OnceLock::new()),
    };
    cmd.run(&app, logging)
        .map_err(|error| eyre!("Failed to run generate command: {error:?}"))
}

fn testnet(args: &Args, cmd: &TestnetCmd, logging: config::LoggingConfig) -> Result<()> {
    // Setup the application
    let app = App {
        config: Default::default(), // There is not existing configuration yet
        home_dir: args.get_home_dir()?,
        genesis_file: args.get_genesis_file_path()?,
        emerald_config_file: args.get_emerald_config_file()?,
        private_key_file: args.get_priv_validator_key_file_path()?,
        start_height: Some(Height::new(1)), // We always start at height 1
        resolved_private_key: Arc::new(OnceLock::new()),
    };

    cmd.run(&app, &args.get_home_dir()?, logging)
        .map_err(|error| eyre!("Failed to run testnet command {:?}", error))
}
