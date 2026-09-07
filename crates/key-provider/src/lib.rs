pub mod config;
pub mod error;
pub mod file;

#[cfg(feature = "aws-sm-kms")]
pub mod aws_sm_kms;

#[cfg(feature = "gcp-sm-kms")]
pub mod gcp_sm_kms;

use async_trait::async_trait;
#[cfg(feature = "aws-sm-kms")]
pub use aws_sm_kms::AwsSmKmsKeyProvider;
pub use config::KeyProviderConfig;
pub use error::KeyProviderError;
pub use file::FileKeyProvider;
#[cfg(feature = "gcp-sm-kms")]
pub use gcp_sm_kms::GcpSmKmsKeyProvider;
use zeroize::Zeroizing;

#[async_trait]
pub trait KeyProvider: Send + Sync {
    async fn load_private_key(&self) -> Result<Zeroizing<[u8; 32]>, KeyProviderError>;
}
