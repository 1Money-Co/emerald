#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum KeyProviderConfig {
    #[default]
    File,
    #[cfg(feature = "aws-sm-kms")]
    #[serde(rename = "aws_sm_kms")]
    AwsSmKms(AwsSmKmsConfig),
    #[cfg(feature = "gcp-sm-kms")]
    #[serde(rename = "gcp_sm_kms")]
    GcpSmKms(GcpSmKmsConfig),
}

#[cfg(feature = "aws-sm-kms")]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct AwsSmKmsConfig {
    pub secret_id: String,
    pub region: String,
    pub kms_key_id: String,
    #[serde(default)]
    pub kms_region: Option<String>,
    #[serde(default)]
    pub kms_encryption_context: Option<std::collections::BTreeMap<String, String>>,
}

#[cfg(feature = "gcp-sm-kms")]
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct GcpSmKmsConfig {
    pub secret_version: String,
    pub kms_crypto_key: String,
    pub kms_aad: String,
}

#[cfg(feature = "gcp-sm-kms")]
impl GcpSmKmsConfig {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if !is_numeric_secret_version(&self.secret_version) {
            return Err(
                "secret_version must be a global Secret Manager version resource ending in a canonical positive numeric version",
            );
        }
        if !is_crypto_key(&self.kms_crypto_key) {
            return Err("kms_crypto_key must be a complete Cloud KMS CryptoKey resource");
        }
        if self.kms_aad.is_empty() || self.kms_aad.trim() != self.kms_aad {
            return Err(
                "kms_aad must be nonempty and must not contain leading or trailing whitespace",
            );
        }
        Ok(())
    }
}

#[cfg(feature = "gcp-sm-kms")]
#[derive(serde::Deserialize)]
struct UnvalidatedGcpSmKmsConfig {
    secret_version: String,
    kms_crypto_key: String,
    kms_aad: String,
}

#[cfg(feature = "gcp-sm-kms")]
impl<'de> serde::Deserialize<'de> for GcpSmKmsConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = UnvalidatedGcpSmKmsConfig::deserialize(deserializer)?;
        let config = Self {
            secret_version: raw.secret_version,
            kms_crypto_key: raw.kms_crypto_key,
            kms_aad: raw.kms_aad,
        };
        config.validate().map_err(serde::de::Error::custom)?;
        Ok(config)
    }
}

#[cfg(feature = "gcp-sm-kms")]
fn is_numeric_secret_version(resource: &str) -> bool {
    let parts: Vec<_> = resource.split('/').collect();
    parts.len() == 6
        && parts[0] == "projects"
        && is_resource_segment(parts[1])
        && parts[2] == "secrets"
        && is_resource_segment(parts[3])
        && parts[4] == "versions"
        && is_canonical_positive_version(parts[5])
}

#[cfg(feature = "gcp-sm-kms")]
fn is_canonical_positive_version(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && !value.starts_with('0')
        && value.parse::<u64>().is_ok()
}

#[cfg(feature = "gcp-sm-kms")]
fn is_crypto_key(resource: &str) -> bool {
    let parts: Vec<_> = resource.split('/').collect();
    parts.len() == 8
        && parts[0] == "projects"
        && is_resource_segment(parts[1])
        && parts[2] == "locations"
        && is_resource_segment(parts[3])
        && parts[4] == "keyRings"
        && is_resource_segment(parts[5])
        && parts[6] == "cryptoKeys"
        && is_resource_segment(parts[7])
}

#[cfg(feature = "gcp-sm-kms")]
fn is_resource_segment(segment: &str) -> bool {
    !segment.is_empty() && !segment.chars().any(char::is_whitespace)
}

#[cfg(all(test, feature = "gcp-sm-kms"))]
mod tests {
    use super::GcpSmKmsConfig;

    const SECRET_VERSION: &str = "projects/ceremony-test/secrets/validator-general/versions/7";
    const KMS_CRYPTO_KEY: &str =
        "projects/ceremony-test/locations/global/keyRings/validators/cryptoKeys/envelope";
    const KMS_AAD: &str = "1money:ceremony-test:validator:1:general:v1";

    fn parse(
        secret_version: &str,
        kms_crypto_key: &str,
        kms_aad: &str,
    ) -> Result<GcpSmKmsConfig, serde_json::Error> {
        serde_json::from_value(serde_json::json!({
            "secret_version": secret_version,
            "kms_crypto_key": kms_crypto_key,
            "kms_aad": kms_aad,
        }))
    }

    #[test]
    fn valid_gcp_source_preserves_all_values_verbatim() {
        let config = parse(SECRET_VERSION, KMS_CRYPTO_KEY, KMS_AAD).unwrap();

        assert_eq!(config.secret_version, SECRET_VERSION);
        assert_eq!(config.kms_crypto_key, KMS_CRYPTO_KEY);
        assert_eq!(config.kms_aad, KMS_AAD);
    }

    #[test]
    fn rejects_noncanonical_secret_manager_versions() {
        for invalid in [
            "",
            "projects//secrets/key/versions/1",
            "projects/project/secrets//versions/1",
            "projects/project/secrets/key/versions/latest",
            "projects/project/secrets/key/versions/0",
            "projects/project/secrets/key/versions/+7",
            "projects/project/secrets/key/versions/007",
            "projects/project/secrets/key/versions/-1",
            "projects/project/secrets/key/versions/18446744073709551616",
            "projects/project/locations/us/secrets/key/versions/1",
            "projects/project/secrets/key/versions/1/extra",
            " projects/project/secrets/key/versions/1",
            "projects/project/secrets/key/versions/1 ",
        ] {
            assert!(
                parse(invalid, KMS_CRYPTO_KEY, KMS_AAD).is_err(),
                "accepted invalid secret version: {invalid}"
            );
        }
    }

    #[test]
    fn rejects_malformed_kms_crypto_key_resources() {
        for invalid in [
            "",
            "projects/project/locations/us/keyRings/ring/cryptoKeys/",
            "projects/project/locations/us/keyRings//cryptoKeys/key",
            "projects//locations/us/keyRings/ring/cryptoKeys/key",
            "projects/project/locations//keyRings/ring/cryptoKeys/key",
            "projects/project/locations/us/keyRings/ring/cryptoKeys/key/versions/1",
            " projects/project/locations/us/keyRings/ring/cryptoKeys/key",
            "projects/project/locations/us/keyRings/ring/cryptoKeys/key ",
        ] {
            assert!(
                parse(SECRET_VERSION, invalid, KMS_AAD).is_err(),
                "accepted invalid CryptoKey: {invalid}"
            );
        }
    }

    #[test]
    fn rejects_empty_or_padded_kms_aad() {
        for invalid in ["", " ", "\n", " aad", "aad "] {
            assert!(
                parse(SECRET_VERSION, KMS_CRYPTO_KEY, invalid).is_err(),
                "accepted invalid AAD: {invalid:?}"
            );
        }
    }
}
