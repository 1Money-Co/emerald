use core::error::Error as StdError;
use core::future::Future;
use core::time::Duration;
use core::{mem, str};

use async_trait::async_trait;
use bytes::Bytes;
use google_cloud_kms_v1::client::KeyManagementService;
use google_cloud_secretmanager_v1::client::SecretManagerService;
use serde_json::Value;
use tracing::{debug, info};
use zeroize::{Zeroize, Zeroizing};

use crate::config::GcpSmKmsConfig;
use crate::{KeyProvider, KeyProviderError};

type BoxError = Box<dyn StdError + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum GcpSmKmsError {
    #[error("invalid GCP key source configuration: {0}")]
    InvalidConfiguration(&'static str),
    #[error("failed to initialize GCP {service} client: {source}")]
    ClientInitialization {
        service: &'static str,
        #[source]
        source: BoxError,
    },
    #[error("failed to access GCP Secret Manager version {resource}: {source}")]
    SecretAccess {
        resource: String,
        #[source]
        source: google_cloud_secretmanager_v1::Error,
    },
    #[error("GCP Secret Manager version {resource} returned no payload")]
    MissingSecretPayload { resource: String },
    #[error(
        "GCP Secret Manager payload checksum mismatch for {resource}: expected {expected}, computed {computed}"
    )]
    SecretChecksumMismatch {
        resource: String,
        expected: i64,
        computed: i64,
    },
    #[error("failed to decrypt key material with Cloud KMS key {resource}: {source}")]
    KmsDecrypt {
        resource: String,
        #[source]
        source: google_cloud_kms_v1::Error,
    },
    #[error(
        "Cloud KMS plaintext checksum mismatch for {resource}: expected {expected}, computed {computed}"
    )]
    PlaintextChecksumMismatch {
        resource: String,
        expected: i64,
        computed: i64,
    },
    #[error("Cloud KMS plaintext is not valid UTF-8: {0}")]
    InvalidUtf8(#[from] str::Utf8Error),
    #[error("Cloud KMS plaintext is not valid hexadecimal key material: {reason}")]
    InvalidHex { reason: String },
    #[error(
        "Cloud KMS plaintext JSON does not contain a string private_key, privateKey, or key field"
    )]
    MissingKeyField,
    #[error("Cloud KMS plaintext must decode to exactly 32 bytes, got {actual}")]
    InvalidLength { actual: usize },
    #[error("timed out after {timeout:?} loading the private key from GCP")]
    Timeout { timeout: Duration },
}

/// Upper bound on the whole startup key load: Application Default Credentials
/// discovery, the Secret Manager access and the Cloud KMS decrypt. The Google
/// clients set neither a per-attempt timeout nor a retry policy by default, so
/// without this a stalled metadata server or control-plane incident would hang
/// node startup indefinitely instead of failing fast for the supervisor to retry.
const KEY_LOAD_TIMEOUT: Duration = Duration::from_secs(30);

async fn within_timeout<T>(
    timeout: Duration,
    future: impl Future<Output = Result<T, GcpSmKmsError>>,
) -> Result<T, GcpSmKmsError> {
    tokio::time::timeout(timeout, future)
        .await
        .unwrap_or_else(|_| Err(GcpSmKmsError::Timeout { timeout }))
}

/// `hex::FromHexError` renders the offending character, which here is decrypted
/// private-key material, so it must never reach an error message or a log. Keep
/// the position and a static reason and drop the character itself.
impl From<hex::FromHexError> for GcpSmKmsError {
    fn from(error: hex::FromHexError) -> Self {
        let reason = match error {
            hex::FromHexError::InvalidHexCharacter { index, .. } => {
                format!("non-hexadecimal character at position {index}")
            }
            hex::FromHexError::OddLength => "odd number of digits".to_owned(),
            hex::FromHexError::InvalidStringLength => "invalid string length".to_owned(),
        };
        Self::InvalidHex { reason }
    }
}

/// Loads an envelope-encrypted validator key from GCP at startup.
///
/// # Runtime requirements
///
/// Loading requires a Tokio runtime with I/O and time enabled, such as one built
/// with `tokio::runtime::Builder::enable_all()`. Without the time driver, the
/// startup timeout panics instead of returning a `KeyProviderError`.
pub struct GcpSmKmsKeyProvider {
    config: GcpSmKmsConfig,
}

impl GcpSmKmsKeyProvider {
    pub fn new(config: GcpSmKmsConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl KeyProvider for GcpSmKmsKeyProvider {
    async fn load_private_key(&self) -> Result<Zeroizing<[u8; 32]>, KeyProviderError> {
        self.config
            .validate()
            .map_err(GcpSmKmsError::InvalidConfiguration)?;

        info!(
            secret_version = %self.config.secret_version,
            timeout = ?KEY_LOAD_TIMEOUT,
            "Loading private key from GCP Secret Manager and Cloud KMS",
        );
        // The timeout wraps client construction too: Application Default
        // Credentials discovery talks to the metadata server and can stall there.
        let key = within_timeout(KEY_LOAD_TIMEOUT, async {
            let secret_manager =
                SecretManagerService::builder()
                    .build()
                    .await
                    .map_err(|source| GcpSmKmsError::ClientInitialization {
                        service: "Secret Manager",
                        source: Box::new(source),
                    })?;
            let kms = KeyManagementService::builder()
                .build()
                .await
                .map_err(|source| GcpSmKmsError::ClientInitialization {
                    service: "Cloud KMS",
                    source: Box::new(source),
                })?;
            load_private_key_with_clients(&self.config, &secret_manager, &kms).await
        })
        .await?;
        info!(
            secret_version = %self.config.secret_version,
            "Private key loaded successfully from GCP",
        );
        Ok(key)
    }
}

async fn load_private_key_with_clients(
    config: &GcpSmKmsConfig,
    secret_manager: &SecretManagerService,
    kms: &KeyManagementService,
) -> Result<Zeroizing<[u8; 32]>, GcpSmKmsError> {
    config
        .validate()
        .map_err(GcpSmKmsError::InvalidConfiguration)?;
    let ciphertext = access_secret(config, secret_manager).await?;
    let plaintext = decrypt(config, kms, ciphertext.as_slice()).await?;
    parse_secret_material(plaintext.as_slice())
}

async fn access_secret(
    config: &GcpSmKmsConfig,
    client: &SecretManagerService,
) -> Result<Zeroizing<Vec<u8>>, GcpSmKmsError> {
    debug!(
        secret_version = %config.secret_version,
        "Fetching ciphertext from GCP Secret Manager",
    );
    let response = client
        .access_secret_version()
        .set_name(&config.secret_version)
        .send()
        .await
        .map_err(|source| GcpSmKmsError::SecretAccess {
            resource: config.secret_version.clone(),
            source,
        })?;
    let payload = response
        .payload
        .ok_or_else(|| GcpSmKmsError::MissingSecretPayload {
            resource: config.secret_version.clone(),
        })?;
    verify_checksum(
        payload.data.as_ref(),
        payload.data_crc32c,
        |expected, computed| GcpSmKmsError::SecretChecksumMismatch {
            resource: config.secret_version.clone(),
            expected,
            computed,
        },
    )?;
    Ok(Zeroizing::new(payload.data.to_vec()))
}

async fn decrypt(
    config: &GcpSmKmsConfig,
    client: &KeyManagementService,
    ciphertext: &[u8],
) -> Result<Zeroizing<Vec<u8>>, GcpSmKmsError> {
    debug!(
        kms_crypto_key = %config.kms_crypto_key,
        "Decrypting key material with GCP Cloud KMS",
    );
    let aad = config.kms_aad.as_bytes();
    let response = client
        .decrypt()
        .set_name(&config.kms_crypto_key)
        .set_ciphertext(Bytes::copy_from_slice(ciphertext))
        .set_additional_authenticated_data(Bytes::copy_from_slice(aad))
        .set_ciphertext_crc32c(i64::from(crc32c::crc32c(ciphertext)))
        .set_additional_authenticated_data_crc32c(i64::from(crc32c::crc32c(aad)))
        .send()
        .await
        .map_err(|source| GcpSmKmsError::KmsDecrypt {
            resource: config.kms_crypto_key.clone(),
            source,
        })?;
    verify_checksum(
        response.plaintext.as_ref(),
        response.plaintext_crc32c,
        |expected, computed| GcpSmKmsError::PlaintextChecksumMismatch {
            resource: config.kms_crypto_key.clone(),
            expected,
            computed,
        },
    )?;
    Ok(Zeroizing::new(response.plaintext.to_vec()))
}

fn verify_checksum<E>(
    data: &[u8],
    expected: Option<i64>,
    mismatch: impl FnOnce(i64, i64) -> E,
) -> Result<(), E> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let computed = i64::from(crc32c::crc32c(data));
    if expected != computed {
        return Err(mismatch(expected, computed));
    }
    Ok(())
}

fn zeroize_json_value(value: &mut Value) {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
        Value::String(value) => value.zeroize(),
        Value::Array(values) => {
            for value in values {
                zeroize_json_value(value);
            }
        }
        Value::Object(_) => {
            let Value::Object(values) = mem::take(value) else {
                return;
            };
            for (mut key, mut value) in values {
                key.zeroize();
                zeroize_json_value(&mut value);
            }
        }
    }
}

struct ZeroizingJson(Value);

impl Drop for ZeroizingJson {
    fn drop(&mut self) {
        zeroize_json_value(&mut self.0);
    }
}

fn decode_hex_material(value: &str) -> Result<Zeroizing<Vec<u8>>, hex::FromHexError> {
    let value = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value);
    hex::decode(value).map(Zeroizing::new)
}

fn parse_secret_material(payload: &[u8]) -> Result<Zeroizing<[u8; 32]>, GcpSmKmsError> {
    // The accepted and rejected payload forms are pinned by
    // `gcp_sm_kms/fixtures/payload_conformance.json`, which also records where this
    // parser deliberately differs from l1client's `parse_secret_payload` over the
    // same ceremony secrets. Update that file alongside any change here.
    //
    // Surrounding whitespace is an artifact of how the secret was written (`echo`
    // appends a newline), not of the key material, so drop it before decoding.
    // This matches `AwsSmKmsKeyProvider`; l1client rejects such a payload.
    let payload = str::from_utf8(payload)?.trim();
    let bare_hex_error = match decode_hex_material(payload) {
        Ok(material) => return exact_key(material),
        Err(error) => error,
    };

    let mut json = match serde_json::from_str::<Value>(payload) {
        Ok(value) => ZeroizingJson(value),
        Err(_) => return Err(bare_hex_error.into()),
    };
    let object = json
        .0
        .as_object_mut()
        .ok_or(GcpSmKmsError::MissingKeyField)?;
    let field = ["private_key", "privateKey", "key"]
        .into_iter()
        .find(|field| object.contains_key(*field))
        .ok_or(GcpSmKmsError::MissingKeyField)?;
    let mut field_value =
        ZeroizingJson(object.remove(field).ok_or(GcpSmKmsError::MissingKeyField)?);
    let value = match &mut field_value.0 {
        Value::String(value) => Zeroizing::new(mem::take(value)),
        _ => return Err(GcpSmKmsError::MissingKeyField),
    };
    exact_key(decode_hex_material(value.as_str())?)
}

fn exact_key(material: Zeroizing<Vec<u8>>) -> Result<Zeroizing<[u8; 32]>, GcpSmKmsError> {
    let actual = material.len();
    let key = material
        .as_slice()
        .try_into()
        .map_err(|_| GcpSmKmsError::InvalidLength { actual })?;
    Ok(Zeroizing::new(key))
}

#[cfg(test)]
mod tests;
