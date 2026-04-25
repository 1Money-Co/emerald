#[derive(Debug, thiserror::Error)]
pub enum KeyProviderError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("key parse error: {0}")]
    ParseKey(String),
    #[cfg(feature = "aws-sm-kms")]
    #[error("Secrets Manager error: {0}")]
    SecretsManager(String),
    #[cfg(feature = "aws-sm-kms")]
    #[error("KMS error: {0}")]
    Kms(String),
}
