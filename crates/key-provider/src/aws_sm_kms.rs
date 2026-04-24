use async_trait::async_trait;
use aws_config::{BehaviorVersion, Region};
use aws_sdk_kms::primitives::Blob;
use base64::Engine as _;
use tracing::{debug, info};
use zeroize::Zeroizing;

use crate::{config::AwsSmKmsConfig, KeyProvider, KeyProviderError};

// Walk the std::error::Error source chain so callers see the root cause,
// not just the outermost "dispatch failure" wrapper.
fn full_error_chain(e: &dyn std::error::Error) -> String {
    let mut msg = e.to_string();
    let mut src = e.source();
    while let Some(cause) = src {
        msg.push_str(&format!(": {cause}"));
        src = cause.source();
    }
    msg
}

pub struct AwsSmKmsKeyProvider {
    config: AwsSmKmsConfig,
}

impl AwsSmKmsKeyProvider {
    pub fn new(config: AwsSmKmsConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl KeyProvider for AwsSmKmsKeyProvider {
    async fn load_private_key(&self) -> Result<Zeroizing<[u8; 32]>, KeyProviderError> {
        info!(
            secret_id = %self.config.secret_id,
            region    = %self.config.region,
            "Loading private key from AWS Secrets Manager and KMS",
        );

        // ── Step 1: fetch ciphertext blob from Secrets Manager ──────────────
        debug!(
            secret_id = %self.config.secret_id,
            region    = %self.config.region,
            "Fetching ciphertext from Secrets Manager",
        );
        let sm_cfg = aws_config::defaults(BehaviorVersion::latest())
            .region(Region::new(self.config.region.clone()))
            .load()
            .await;
        let sm = aws_sdk_secretsmanager::Client::new(&sm_cfg);

        let resp = sm
            .get_secret_value()
            .secret_id(&self.config.secret_id)
            .send()
            .await
            .map_err(|e| {
                KeyProviderError::SecretsManager(format!(
                    "GetSecretValue failed (secret_id={}, region={}): {}",
                    self.config.secret_id,
                    self.config.region,
                    full_error_chain(&e),
                ))
            })?;
        debug!("Ciphertext retrieved from Secrets Manager");

        let b64_ciphertext = resp.secret_string().ok_or_else(|| {
            KeyProviderError::SecretsManager("GetSecretValue returned no secret string".into())
        })?;

        // ── Step 2: base64-decode → raw KMS ciphertext blob ─────────────────
        let ciphertext = base64::engine::general_purpose::STANDARD
            .decode(b64_ciphertext)
            .map_err(|e| {
                KeyProviderError::ParseKey(format!("SM value is not valid base64: {e}"))
            })?;

        // ── Step 3: KMS decrypt ──────────────────────────────────────────────
        let kms_region = self
            .config
            .kms_region
            .as_deref()
            .unwrap_or(&self.config.region);
        debug!(
            kms_key_id = %self.config.kms_key_id,
            kms_region = %kms_region,
            "Decrypting key material via KMS",
        );
        let kms_cfg = aws_config::defaults(BehaviorVersion::latest())
            .region(Region::new(kms_region.to_string()))
            .load()
            .await;
        let kms = aws_sdk_kms::Client::new(&kms_cfg);

        let mut req = kms
            .decrypt()
            .key_id(&self.config.kms_key_id)
            .ciphertext_blob(Blob::new(ciphertext));

        if let Some(ctx) = &self.config.kms_encryption_context {
            for (k, v) in ctx {
                req = req.encryption_context(k, v);
            }
        }

        let decrypt_resp = req.send().await.map_err(|e| {
            KeyProviderError::Kms(format!(
                "Decrypt failed (kms_key_id={}, region={}): {}",
                self.config.kms_key_id,
                kms_region,
                full_error_chain(&e),
            ))
        })?;
        debug!("Key material decrypted by KMS");

        let plaintext_blob = decrypt_resp
            .plaintext()
            .ok_or_else(|| KeyProviderError::Kms("KMS Decrypt returned no plaintext".into()))?;

        // ── Step 4: plaintext is hex(32-byte-key) → parse to [u8; 32] ───────
        let hex_str = std::str::from_utf8(plaintext_blob.as_ref())
            .map_err(|_| KeyProviderError::ParseKey("KMS plaintext is not valid UTF-8".into()))?;

        let key_bytes = hex::decode(hex_str.trim())
            .map_err(|e| KeyProviderError::ParseKey(format!("KMS plaintext hex decode: {e}")))?;

        let arr: [u8; 32] = key_bytes.try_into().map_err(|_| {
            KeyProviderError::ParseKey("KMS plaintext must decode to exactly 32 bytes".into())
        })?;

        info!(
            secret_id = %self.config.secret_id,
            "Private key loaded successfully from AWS",
        );
        Ok(Zeroizing::new(arr))
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;

    #[test]
    fn parse_hex_plaintext_to_key_bytes() {
        let key: [u8; 32] = core::array::from_fn(|i| i as u8);
        let hex_bytes = hex::encode(key).into_bytes();
        let hex_str = std::str::from_utf8(&hex_bytes).unwrap();
        let decoded = hex::decode(hex_str.trim()).unwrap();
        let arr: [u8; 32] = decoded.try_into().unwrap();
        assert_eq!(arr, key);
    }

    #[test]
    fn base64_decode_ciphertext_roundtrip() {
        let fake_ct = b"some-kms-ciphertext-blob";
        let b64 = base64::engine::general_purpose::STANDARD.encode(fake_ct);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&b64)
            .unwrap();
        assert_eq!(decoded.as_slice(), fake_ct);
    }
}
