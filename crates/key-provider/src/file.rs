use std::path::PathBuf;
use async_trait::async_trait;
use zeroize::Zeroizing;
use crate::{KeyProvider, KeyProviderError};

pub struct FileKeyProvider {
    pub path: PathBuf,
}

impl FileKeyProvider {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

#[async_trait]
impl KeyProvider for FileKeyProvider {
    async fn load_private_key(&self) -> Result<Zeroizing<[u8; 32]>, KeyProviderError> {
        todo!("implemented in Task 2")
    }
}
