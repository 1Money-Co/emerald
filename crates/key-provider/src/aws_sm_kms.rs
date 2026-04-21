use async_trait::async_trait;
use zeroize::Zeroizing;
use crate::{KeyProvider, KeyProviderError, config::AwsSmKmsConfig};

pub struct AwsSmKmsKeyProvider {
    pub config: AwsSmKmsConfig,
}

impl AwsSmKmsKeyProvider {
    pub fn new(config: AwsSmKmsConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl KeyProvider for AwsSmKmsKeyProvider {
    async fn load_private_key(&self) -> Result<Zeroizing<[u8; 32]>, KeyProviderError> {
        todo!("implemented in Task 3")
    }
}
