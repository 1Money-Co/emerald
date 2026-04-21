pub mod config;
pub mod error;
pub mod file;

#[cfg(feature = "aws-sm-kms")]
pub mod aws_sm_kms;

pub use config::KeyProviderConfig;
pub use error::KeyProviderError;
pub use file::FileKeyProvider;

#[cfg(feature = "aws-sm-kms")]
pub use aws_sm_kms::AwsSmKmsKeyProvider;

use async_trait::async_trait;
use zeroize::Zeroizing;

#[async_trait]
pub trait KeyProvider: Send + Sync {
    async fn load_private_key(&self) -> Result<Zeroizing<[u8; 32]>, KeyProviderError>;
}
