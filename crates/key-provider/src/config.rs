#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum KeyProviderConfig {
    #[default]
    File,
    #[cfg(feature = "aws-sm-kms")]
    #[serde(rename = "aws_sm_kms")]
    AwsSmKms(AwsSmKmsConfig),
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
