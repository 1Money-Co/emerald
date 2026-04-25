use crate::{KeyProvider, KeyProviderError};
use async_trait::async_trait;
use base64::Engine as _;
use std::path::PathBuf;
use tracing::info;
use zeroize::Zeroizing;

pub struct FileKeyProvider {
    pub path: PathBuf,
}

impl FileKeyProvider {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

#[derive(serde::Deserialize)]
struct PrivValidatorKeyFile {
    value: String,
}

#[async_trait]
impl KeyProvider for FileKeyProvider {
    async fn load_private_key(&self) -> Result<Zeroizing<[u8; 32]>, KeyProviderError> {
        let key_path = &self.path;
        let contents = std::fs::read_to_string(key_path)?;
        let parsed: PrivValidatorKeyFile = serde_json::from_str(&contents)
            .map_err(|e| KeyProviderError::ParseKey(e.to_string()))?;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(&parsed.value)
            .map_err(|e| KeyProviderError::ParseKey(e.to_string()))?;
        let arr: [u8; 32] = raw
            .try_into()
            .map_err(|_| KeyProviderError::ParseKey("key must be exactly 32 bytes".into()))?;
        info!(?key_path, "Private key loaded successfully from local file");
        Ok(Zeroizing::new(arr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_key_json(key_bytes: &[u8; 32]) -> String {
        let encoded = base64::engine::general_purpose::STANDARD.encode(key_bytes);
        format!(
            r#"{{"type":"tendermint/PrivKeySecp256k1","value":"{}"}}"#,
            encoded
        )
    }

    #[tokio::test]
    async fn loads_32_byte_key_from_valid_file() {
        let key_bytes: [u8; 32] = core::array::from_fn(|i| i as u8);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("priv_validator_key.json");
        std::fs::write(&path, make_key_json(&key_bytes)).unwrap();

        let provider = FileKeyProvider::new(&path);
        let result = provider.load_private_key().await.unwrap();
        assert_eq!(*result, key_bytes);
    }

    #[tokio::test]
    async fn returns_io_error_for_missing_file() {
        let provider = FileKeyProvider::new("/nonexistent/priv_validator_key.json");
        let err = provider.load_private_key().await.unwrap_err();
        assert!(matches!(err, KeyProviderError::Io(_)));
    }

    #[tokio::test]
    async fn returns_parse_error_for_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, b"not-json").unwrap();

        let provider = FileKeyProvider::new(&path);
        let err = provider.load_private_key().await.unwrap_err();
        assert!(matches!(err, KeyProviderError::ParseKey(_)));
    }

    #[tokio::test]
    async fn returns_parse_error_when_base64_is_not_32_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("short.json");
        let short = base64::engine::general_purpose::STANDARD.encode([0u8; 16]);
        let json = format!(
            r#"{{"type":"tendermint/PrivKeySecp256k1","value":"{}"}}"#,
            short
        );
        std::fs::write(&path, json).unwrap();

        let provider = FileKeyProvider::new(&path);
        let err = provider.load_private_key().await.unwrap_err();
        assert!(matches!(err, KeyProviderError::ParseKey(_)));
    }
}
