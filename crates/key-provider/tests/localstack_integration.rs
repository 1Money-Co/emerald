// Run with:
//   AWS_DEFAULT_REGION=ap-east-1 \
//   AWS_ENDPOINT_URL=http://localhost:4566 \
//   AWS_ACCESS_KEY_ID=test \
//   AWS_SECRET_ACCESS_KEY=test \
//   cargo test -p key-provider --test localstack_integration -- --ignored

use base64::Engine as _;
use key_provider::{config::AwsSmKmsConfig, AwsSmKmsKeyProvider, KeyProvider};

const REGION: &str = "ap-east-1";

async fn localstack_client_config() -> aws_config::SdkConfig {
    aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(REGION))
        .endpoint_url("http://localhost:4566")
        .load()
        .await
}

async fn create_test_kms_key(cfg: &aws_config::SdkConfig) -> String {
    let kms = aws_sdk_kms::Client::new(cfg);
    let resp = kms.create_key().send().await.unwrap();
    resp.key_metadata().unwrap().key_id().to_string()
}

async fn provision_key_to_sm(
    cfg: &aws_config::SdkConfig,
    key_id: &str,
    secret_name: &str,
    private_key: &[u8; 32],
) {
    let kms = aws_sdk_kms::Client::new(cfg);
    let sm = aws_sdk_secretsmanager::Client::new(cfg);

    let hex_key = hex::encode(private_key);
    let encrypt_resp = kms
        .encrypt()
        .key_id(key_id)
        .plaintext(aws_sdk_kms::primitives::Blob::new(hex_key.as_bytes().to_vec()))
        .send()
        .await
        .unwrap();

    let ciphertext = encrypt_resp.ciphertext_blob().unwrap().as_ref().to_vec();
    let b64 = base64::engine::general_purpose::STANDARD.encode(&ciphertext);

    sm.create_secret()
        .name(secret_name)
        .secret_string(b64)
        .send()
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires LocalStack at http://localhost:4566"]
async fn loads_private_key_from_localstack_sm_and_kms() {
    let cfg = localstack_client_config().await;
    let key_id = create_test_kms_key(&cfg).await;
    let secret_name = "emerald/test/node-0/key";
    let expected_key: [u8; 32] = core::array::from_fn(|i| i as u8);

    provision_key_to_sm(&cfg, &key_id, secret_name, &expected_key).await;

    let provider = AwsSmKmsKeyProvider::new(AwsSmKmsConfig {
        secret_id: secret_name.into(),
        region: REGION.into(),
        kms_key_id: key_id,
        kms_region: None,
        kms_encryption_context: None,
    });

    let loaded = provider.load_private_key().await.unwrap();
    assert_eq!(*loaded, expected_key);
}
