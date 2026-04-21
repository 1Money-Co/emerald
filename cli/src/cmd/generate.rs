use crate::cmd::shared::{KeyProviderType, P2pOptions};

#[allow(dead_code)]
pub(crate) enum KeyProviderSection<'a> {
    File,
    AwsSmKms {
        secret_id: &'a str,
        region: &'a str,
        kms_key_id: &'a str,
        kms_region: Option<&'a str>,
    },
}

#[allow(dead_code)]
pub(crate) fn make_emerald_toml_content(
    moniker: &str,
    fee_recipient: &str,
    key_provider: KeyProviderSection<'_>,
) -> String {
    todo!()
}

#[derive(Debug, clap::Parser)]
pub struct GenerateCmd {
    /// Number of validator nodes to configure.
    #[clap(long)]
    pub nodes: usize,

    /// Output directory. Created if absent.
    #[clap(long, default_value = "./nodes")]
    pub home: std::path::PathBuf,

    /// Key-loading mechanism written into each node's emerald.toml.
    #[clap(long, value_enum, default_value = "file")]
    pub key_provider: KeyProviderType,

    /// One hex public key per line. Required when --key-provider aws-sm-kms.
    #[clap(long, required_if_eq("key_provider", "aws-sm-kms"))]
    pub public_keys_file: Option<std::path::PathBuf>,

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
    pub fn run<N>(
        &self,
        _node: &N,
        _logging: malachitebft_config::LoggingConfig,
    ) -> color_eyre::eyre::Result<()>
    where
        N: malachitebft_app::node::Node
            + malachitebft_app::node::CanGeneratePrivateKey
            + malachitebft_app::node::CanMakeGenesis
            + malachitebft_app::node::CanMakePrivateKeyFile,
        malachitebft_core_types::PrivateKey<N::Context>: serde::de::DeserializeOwned,
    {
        todo!("implemented in Task 7 step 3")
    }
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
