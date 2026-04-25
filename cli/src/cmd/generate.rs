use std::{fs, path::PathBuf};

use clap::Parser;
use color_eyre::eyre::{eyre, Result, WrapErr};
use malachitebft_config::LoggingConfig;

use crate::cmd::shared::{KeyProviderType, P2pOptions};

pub(crate) enum KeyProviderSection<'a> {
    File,
    AwsSmKms {
        secret_id: &'a str,
        region: &'a str,
        kms_key_id: &'a str,
        kms_region: Option<&'a str>,
    },
}

pub(crate) fn make_emerald_toml_content(
    moniker: &str,
    fee_recipient: &str,
    key_provider: KeyProviderSection<'_>,
) -> String {
    let kp_section = match key_provider {
        KeyProviderSection::File => String::new(),
        KeyProviderSection::AwsSmKms {
            secret_id,
            region,
            kms_key_id,
            kms_region,
        } => {
            let kms_region_line = kms_region
                .map(|r| format!("kms_region = \"{r}\"\n"))
                .unwrap_or_default();
            format!(
                "\n[key_provider]\ntype       = \"aws_sm_kms\"\nsecret_id  = \"{secret_id}\"\nregion     = \"{region}\"\nkms_key_id = \"{kms_key_id}\"\n{kms_region_line}"
            )
        }
    };
    format!(
        "moniker       = \"{moniker}\"\nfee_recipient = \"{fee_recipient}\"\n\n[ethereum_config]\nexecution_authrpc_address = \"http://127.0.0.1:8551\"\nengine_authrpc_address    = \"http://127.0.0.1:8552\"\njwt_token_path            = \"./assets/jwt.hex\"\n{kp_section}"
    )
}

#[derive(Debug, Clone, Parser)]
pub struct GenerateCmd {
    /// Number of validator nodes to configure.
    #[clap(long)]
    pub nodes: usize,

    /// Output directory. Created if absent.
    #[clap(long, default_value = "./nodes")]
    pub home: PathBuf,

    /// Key-loading mechanism written into each node's emerald.toml.
    #[clap(long, value_enum, default_value = "file")]
    pub key_provider: KeyProviderType,

    /// One hex public key per line. Required when --key-provider aws-sm-kms.
    #[clap(long, required_if_eq("key_provider", "aws-sm-kms"))]
    pub public_keys_file: Option<PathBuf>,

    /// SM secret ID prefix. Node N gets "{prefix}/node-N/key".
    #[clap(long, default_value = "emerald/mainnet")]
    pub sm_secret_prefix: String,

    /// AWS region for Secrets Manager. Required when --key-provider aws-sm-kms.
    #[clap(long, required_if_eq("key_provider", "aws-sm-kms"))]
    pub sm_region: Option<String>,

    /// KMS key ID or alias. Required when --key-provider aws-sm-kms.
    #[clap(long, required_if_eq("key_provider", "aws-sm-kms"))]
    pub kms_key_id: Option<String>,

    /// KMS region if different from SM region.
    #[clap(long)]
    pub kms_region: Option<String>,

    /// Chain ID.
    #[clap(long, default_value_t = 12345)]
    pub chain_id: u64,

    #[command(flatten)]
    pub p2p: P2pOptions,
}

impl GenerateCmd {
    pub fn run<N>(&self, node: &N, logging: LoggingConfig) -> Result<()>
    where
        N: malachitebft_app::node::Node
            + malachitebft_app::node::CanGeneratePrivateKey
            + malachitebft_app::node::CanMakeGenesis
            + malachitebft_app::node::CanMakePrivateKeyFile,
        malachitebft_core_types::PrivateKey<N::Context>: serde::de::DeserializeOwned,
        malachitebft_core_types::PublicKey<N::Context>: serde::de::DeserializeOwned,
    {
        generate_all(self, node, logging)
    }
}

fn parse_public_key<N>(
    _node: &N,
    hex_str: &str,
) -> Result<malachitebft_core_types::PublicKey<N::Context>>
where
    N: malachitebft_app::node::Node,
    malachitebft_core_types::PublicKey<N::Context>: serde::de::DeserializeOwned,
{
    use malachitebft_eth_types::secp256k1::PublicKey as EmeraldPublicKey;

    let hex = hex_str.trim().strip_prefix("0x").unwrap_or(hex_str.trim());
    let bytes = hex::decode(hex).wrap_err("invalid hex in public key")?;

    if bytes.len() != 64 {
        return Err(eyre!(
            "expected 64-byte hex public key, got {} bytes",
            bytes.len()
        ));
    }

    let mut uncompressed = [0u8; 65];
    uncompressed[0] = 0x04;
    uncompressed[1..].copy_from_slice(&bytes);

    let eth_pub = EmeraldPublicKey::from_sec1_bytes(&uncompressed)
        .map_err(|_| eyre!("invalid secp256k1 public key material"))?;

    let json = serde_json::to_string(&eth_pub)?;
    serde_json::from_str(&json).map_err(Into::into)
}

fn generate_all<N>(cmd: &GenerateCmd, node: &N, logging: LoggingConfig) -> Result<()>
where
    N: malachitebft_app::node::Node
        + malachitebft_app::node::CanGeneratePrivateKey
        + malachitebft_app::node::CanMakeGenesis
        + malachitebft_app::node::CanMakePrivateKeyFile,
    malachitebft_core_types::PrivateKey<N::Context>: serde::de::DeserializeOwned,
    malachitebft_core_types::PublicKey<N::Context>: serde::de::DeserializeOwned,
{
    let assets_dir = cmd.home.join("assets");
    fs::create_dir_all(&assets_dir).wrap_err("creating assets dir")?;
    for i in 0..cmd.nodes {
        fs::create_dir_all(cmd.home.join(i.to_string()).join("config"))
            .wrap_err_with(|| format!("creating node {i} config dir"))?;
    }

    match cmd.key_provider {
        KeyProviderType::File => {
            let private_keys = crate::new::generate_private_keys(node, cmd.nodes, false);
            let public_keys = private_keys.iter().map(|pk| node.get_public_key(pk)).collect();
            let genesis = crate::new::generate_genesis(node, public_keys, false);
            crate::file::save_genesis(node, &assets_dir.join("emerald_genesis.json"), &genesis)?;

            for (i, private_key) in private_keys.iter().enumerate() {
                let config_dir = cmd.home.join(i.to_string()).join("config");
                let moniker = format!("node-{i}");

                crate::file::save_config(
                    &config_dir.join("config.toml"),
                    &crate::new::generate_config(
                        i,
                        cmd.nodes,
                        cmd.p2p.runtime.to_runtime_config(),
                        cmd.p2p.enable_discovery,
                        cmd.p2p.bootstrap_protocol,
                        cmd.p2p.selector,
                        cmd.p2p.num_outbound_peers,
                        cmd.p2p.num_inbound_peers,
                        cmd.p2p.ephemeral_connection_timeout_ms,
                        cmd.p2p.transport,
                        logging,
                        moniker.clone(),
                    ),
                )?;

                fs::write(
                    config_dir.join("emerald.toml"),
                    make_emerald_toml_content(
                        &moniker,
                        "0x0000000000000000000000000000000000000000",
                        KeyProviderSection::File,
                    ),
                )
                .wrap_err_with(|| format!("writing emerald.toml for node {i}"))?;

                let priv_key_file = node.make_private_key_file(private_key.clone());
                crate::file::save_priv_validator_key(
                    node,
                    &config_dir.join("priv_validator_key.json"),
                    &priv_key_file,
                )?;

                tracing::info!("generated config for node {i}");
            }
        }

        KeyProviderType::AwsSmKms => {
            let keys_file = cmd.public_keys_file.as_ref().unwrap(); // guaranteed by clap
            let keys_content =
                fs::read_to_string(keys_file).wrap_err("reading public-keys-file")?;
            let public_keys_hex: Vec<String> = keys_content
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect();

            if public_keys_hex.len() != cmd.nodes {
                return Err(eyre!(
                    "--nodes {} but {} keys in file",
                    cmd.nodes,
                    public_keys_hex.len()
                ));
            }

            let public_keys = public_keys_hex
                .iter()
                .enumerate()
                .map(|(i, hex)| {
                    parse_public_key(node, hex)
                        .wrap_err_with(|| format!("parsing public key at line {i}"))
                })
                .collect::<Result<Vec<_>>>()?;

            let genesis = crate::new::generate_genesis(node, public_keys, false);
            crate::file::save_genesis(node, &assets_dir.join("emerald_genesis.json"), &genesis)?;

            let sm_region = cmd.sm_region.as_deref().unwrap(); // guaranteed by clap
            let kms_key_id = cmd.kms_key_id.as_deref().unwrap(); // guaranteed by clap

            for i in 0..cmd.nodes {
                let config_dir = cmd.home.join(i.to_string()).join("config");
                let moniker = format!("node-{i}");
                let secret_id = format!("{}/node-{i}/key", cmd.sm_secret_prefix);

                crate::file::save_config(
                    &config_dir.join("config.toml"),
                    &crate::new::generate_config(
                        i,
                        cmd.nodes,
                        cmd.p2p.runtime.to_runtime_config(),
                        cmd.p2p.enable_discovery,
                        cmd.p2p.bootstrap_protocol,
                        cmd.p2p.selector,
                        cmd.p2p.num_outbound_peers,
                        cmd.p2p.num_inbound_peers,
                        cmd.p2p.ephemeral_connection_timeout_ms,
                        cmd.p2p.transport,
                        logging,
                        moniker.clone(),
                    ),
                )?;

                fs::write(
                    config_dir.join("emerald.toml"),
                    make_emerald_toml_content(
                        &moniker,
                        "0x0000000000000000000000000000000000000000",
                        KeyProviderSection::AwsSmKms {
                            secret_id: &secret_id,
                            region: sm_region,
                            kms_key_id,
                            kms_region: cmd.kms_region.as_deref(),
                        },
                    ),
                )
                .wrap_err_with(|| format!("writing emerald.toml for node {i}"))?;

                tracing::info!("generated config for node {i}");
            }
        }
    }

    println!("configs written to {}", cmd.home.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_provider_omits_key_provider_section() {
        let out = make_emerald_toml_content("node-0", "0x0000", KeyProviderSection::File);
        assert!(!out.contains("[key_provider]"));
        assert!(out.contains("node-0"));
    }

    #[test]
    fn aws_sm_kms_includes_key_provider_section() {
        let out = make_emerald_toml_content(
            "node-2",
            "0x0001",
            KeyProviderSection::AwsSmKms {
                secret_id: "emerald/mainnet/node-2/key",
                region: "ap-east-1",
                kms_key_id: "alias/emerald-validator-keys",
                kms_region: None,
            },
        );
        assert!(out.contains("[key_provider]"));
        assert!(out.contains("aws_sm_kms"));
        assert!(out.contains("emerald/mainnet/node-2/key"));
        assert!(!out.contains("kms_region"));
    }

    #[test]
    fn aws_sm_kms_includes_kms_region_when_set() {
        let out = make_emerald_toml_content(
            "node-0",
            "0x0001",
            KeyProviderSection::AwsSmKms {
                secret_id: "s",
                region: "ap-east-1",
                kms_key_id: "alias/key",
                kms_region: Some("us-east-1"),
            },
        );
        assert!(out.contains("kms_region = \"us-east-1\""));
    }
}
