use std::sync::{Arc, Mutex};

use bytes::Bytes;
use google_cloud_gax::error::rpc::{Code, Status};
use google_cloud_gax::error::Error;
use google_cloud_gax::options::RequestOptions;
use google_cloud_gax::response::Response;
use google_cloud_kms_v1::client::KeyManagementService as KmsClient;
use google_cloud_kms_v1::model::{DecryptRequest, DecryptResponse};
use google_cloud_kms_v1::stub::KeyManagementService;
use google_cloud_secretmanager_v1::client::SecretManagerService as SecretClient;
use google_cloud_secretmanager_v1::model::{
    AccessSecretVersionRequest, AccessSecretVersionResponse, SecretPayload,
};
use google_cloud_secretmanager_v1::stub::SecretManagerService;
use malachitebft_eth_types::secp256k1::{K256Provider, PrivateKey};
use malachitebft_eth_types::Address;
use serde::Deserialize;

use super::{load_private_key_with_clients, parse_secret_material, within_timeout, GcpSmKmsError};
use crate::config::GcpSmKmsConfig;

const SECRET_VERSION: &str = "projects/test-project/secrets/validator-general/versions/7";
const KMS_CRYPTO_KEY: &str =
    "projects/test-project/locations/us-central1/keyRings/validators/cryptoKeys/general";
const KMS_AAD: &str = "1money:testnet:validator:1:general:v1";
const CIPHERTEXT: &[u8] = b"raw-binary-ciphertext\x00\xff";
const KEY_HEX: &str = "0101010101010101010101010101010101010101010101010101010101010101";

#[derive(Deserialize)]
struct CeremonyFixture {
    secret_version: String,
    kms_crypto_key: String,
    kms_aad: String,
    plaintext_hex: String,
    expected_address: String,
    expected_peer_id: String,
}

#[derive(Clone, Debug)]
enum SecretReply {
    Success { data: Bytes, checksum: Option<i64> },
    MissingPayload,
    Error(Code),
}

#[derive(Clone, Debug)]
struct SecretStub {
    reply: SecretReply,
    requests: Arc<Mutex<Vec<AccessSecretVersionRequest>>>,
}

impl SecretStub {
    fn new(reply: SecretReply) -> Self {
        Self {
            reply,
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl SecretManagerService for SecretStub {
    async fn access_secret_version(
        &self,
        request: AccessSecretVersionRequest,
        _options: RequestOptions,
    ) -> google_cloud_secretmanager_v1::Result<Response<AccessSecretVersionResponse>> {
        self.requests.lock().unwrap().push(request);
        match &self.reply {
            SecretReply::Success { data, checksum } => {
                let payload = SecretPayload::new()
                    .set_data(data.clone())
                    .set_or_clear_data_crc32c(*checksum);
                Ok(Response::from(
                    AccessSecretVersionResponse::new().set_payload(payload),
                ))
            }
            SecretReply::MissingPayload => Ok(Response::from(AccessSecretVersionResponse::new())),
            SecretReply::Error(code) => Err(Error::service(
                Status::default()
                    .set_code(*code)
                    .set_message("secret access failed"),
            )),
        }
    }
}

#[derive(Clone, Debug)]
enum KmsReply {
    Success {
        plaintext: Bytes,
        checksum: Option<i64>,
    },
    Error(Code),
}

#[derive(Clone, Debug)]
struct KmsStub {
    reply: KmsReply,
    requests: Arc<Mutex<Vec<DecryptRequest>>>,
}

impl KmsStub {
    fn new(reply: KmsReply) -> Self {
        Self {
            reply,
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl KeyManagementService for KmsStub {
    async fn decrypt(
        &self,
        request: DecryptRequest,
        _options: RequestOptions,
    ) -> google_cloud_kms_v1::Result<Response<DecryptResponse>> {
        self.requests.lock().unwrap().push(request);
        match &self.reply {
            KmsReply::Success {
                plaintext,
                checksum,
            } => Ok(Response::from(
                DecryptResponse::new()
                    .set_plaintext(plaintext.clone())
                    .set_or_clear_plaintext_crc32c(*checksum),
            )),
            KmsReply::Error(code) => Err(Error::service(
                Status::default()
                    .set_code(*code)
                    .set_message("KMS decrypt failed"),
            )),
        }
    }
}

fn config() -> GcpSmKmsConfig {
    GcpSmKmsConfig {
        secret_version: SECRET_VERSION.to_string(),
        kms_crypto_key: KMS_CRYPTO_KEY.to_string(),
        kms_aad: KMS_AAD.to_string(),
    }
}

fn matching_secret() -> SecretStub {
    SecretStub::new(SecretReply::Success {
        data: Bytes::from_static(CIPHERTEXT),
        checksum: Some(i64::from(crc32c::crc32c(CIPHERTEXT))),
    })
}

fn matching_kms() -> KmsStub {
    KmsStub::new(KmsReply::Success {
        plaintext: Bytes::from_static(KEY_HEX.as_bytes()),
        checksum: Some(i64::from(crc32c::crc32c(KEY_HEX.as_bytes()))),
    })
}

#[derive(Deserialize)]
struct PayloadVectors {
    default_key_hex: String,
    cases: Vec<PayloadCase>,
}

#[derive(Deserialize)]
struct PayloadCase {
    name: String,
    #[serde(default)]
    payload: Option<String>,
    #[serde(default)]
    payload_hex: Option<String>,
    expect: String,
    #[serde(default)]
    key_hex: Option<String>,
}

/// Drives `fixtures/payload_conformance.json`, the shared description of what a
/// decrypted envelope payload may contain. l1client's `parse_secret_payload` reads
/// the same ceremony secrets, so the cases marked `contract: "shared"` in that file
/// must hold on both sides; the `contract: "emerald"` cases record where this parser
/// deliberately differs. Change the file, not just the code.
#[test]
fn payload_conformance_vectors() {
    let vectors: PayloadVectors =
        serde_json::from_str(include_str!("fixtures/payload_conformance.json")).unwrap();

    for case in &vectors.cases {
        let name = &case.name;
        let payload = match (&case.payload, &case.payload_hex) {
            (Some(text), None) => text.as_bytes().to_vec(),
            (None, Some(hex_bytes)) => hex::decode(hex_bytes).unwrap(),
            _ => panic!("case {name:?} needs exactly one of payload/payload_hex"),
        };
        let parsed = parse_secret_material(&payload);

        match case.expect.as_str() {
            "accept" => {
                let expected = case.key_hex.as_ref().unwrap_or(&vectors.default_key_hex);
                let expected = hex::decode(expected).unwrap();
                let parsed = parsed
                    .unwrap_or_else(|error| panic!("case {name:?} should parse, got {error}"));
                assert_eq!(parsed.as_slice(), expected.as_slice(), "case {name:?}");
            }
            "reject" => {
                assert!(parsed.is_err(), "case {name:?} should be rejected");
            }
            other => panic!("case {name:?} has unknown expect {other:?}"),
        }
    }
}

#[test]
fn rejects_invalid_utf8_plaintext() {
    assert!(matches!(
        parse_secret_material(&[0xff]),
        Err(GcpSmKmsError::InvalidUtf8(_))
    ));
}

#[test]
fn hex_errors_report_position_without_echoing_key_material() {
    // An even-length payload so hex reports the offending character rather than
    // an odd length. 'Z' stands in for a byte of decrypted key material.
    let payload = format!("{}ZZ", "01".repeat(31));
    let message = parse_secret_material(payload.as_bytes())
        .unwrap_err()
        .to_string();
    assert!(
        message.contains("position 62"),
        "lost the position: {message}"
    );
    assert!(!message.contains('Z'), "leaked key material: {message}");

    let payload =
        serde_json::json!({ "private_key": format!("{}ZZ", "01".repeat(31)) }).to_string();
    let message = parse_secret_material(payload.as_bytes())
        .unwrap_err()
        .to_string();
    assert!(!message.contains('Z'), "leaked key material: {message}");

    // Odd-length and wrong-length payloads carry no payload bytes either.
    let message = parse_secret_material("f".repeat(63).as_bytes())
        .unwrap_err()
        .to_string();
    assert_eq!(
        message,
        "Cloud KMS plaintext is not valid hexadecimal key material: odd number of digits"
    );
}

#[tokio::test]
async fn loader_sends_exact_resource_ciphertext_aad_and_checksums() {
    let secret = matching_secret();
    let secret_requests = Arc::clone(&secret.requests);
    let kms = matching_kms();
    let kms_requests = Arc::clone(&kms.requests);

    let key = load_private_key_with_clients(
        &config(),
        &SecretClient::from_stub(secret),
        &KmsClient::from_stub(kms),
    )
    .await
    .unwrap();

    assert_eq!(key.as_slice(), [1; 32]);
    let secret_requests = secret_requests.lock().unwrap();
    assert_eq!(secret_requests.len(), 1);
    assert_eq!(secret_requests[0].name, SECRET_VERSION);
    let kms_requests = kms_requests.lock().unwrap();
    assert_eq!(kms_requests.len(), 1);
    assert_eq!(kms_requests[0].name, KMS_CRYPTO_KEY);
    assert_eq!(kms_requests[0].ciphertext.as_ref(), CIPHERTEXT);
    assert_eq!(
        kms_requests[0].additional_authenticated_data.as_ref(),
        KMS_AAD.as_bytes()
    );
    assert_eq!(
        kms_requests[0].ciphertext_crc32c,
        Some(i64::from(crc32c::crc32c(CIPHERTEXT)))
    );
    assert_eq!(
        kms_requests[0].additional_authenticated_data_crc32c,
        Some(i64::from(crc32c::crc32c(KMS_AAD.as_bytes())))
    );
}

#[tokio::test]
async fn loader_fails_closed_on_missing_payload_or_checksum_mismatch() {
    let kms = matching_kms();
    let kms_requests = Arc::clone(&kms.requests);
    let missing = load_private_key_with_clients(
        &config(),
        &SecretClient::from_stub(SecretStub::new(SecretReply::MissingPayload)),
        &KmsClient::from_stub(kms),
    )
    .await;
    assert!(matches!(
        missing,
        Err(GcpSmKmsError::MissingSecretPayload { .. })
    ));
    assert!(kms_requests.lock().unwrap().is_empty());

    let kms = matching_kms();
    let kms_requests = Arc::clone(&kms.requests);
    let bad_checksum = load_private_key_with_clients(
        &config(),
        &SecretClient::from_stub(SecretStub::new(SecretReply::Success {
            data: Bytes::from_static(CIPHERTEXT),
            checksum: Some(0),
        })),
        &KmsClient::from_stub(kms),
    )
    .await;
    assert!(matches!(
        bad_checksum,
        Err(GcpSmKmsError::SecretChecksumMismatch { .. })
    ));
    assert!(kms_requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn loader_preserves_service_failures_and_does_not_fallback() {
    let secret = SecretStub::new(SecretReply::Error(Code::PermissionDenied));
    let secret_requests = Arc::clone(&secret.requests);
    let kms = matching_kms();
    let kms_requests = Arc::clone(&kms.requests);
    let result = load_private_key_with_clients(
        &config(),
        &SecretClient::from_stub(secret),
        &KmsClient::from_stub(kms),
    )
    .await;
    let status = match result.unwrap_err() {
        GcpSmKmsError::SecretAccess { source, .. } => source.status().map(|status| status.code),
        _ => None,
    };
    assert_eq!(status, Some(Code::PermissionDenied));
    assert_eq!(secret_requests.lock().unwrap().len(), 1);
    assert!(kms_requests.lock().unwrap().is_empty());

    let result = load_private_key_with_clients(
        &config(),
        &SecretClient::from_stub(matching_secret()),
        &KmsClient::from_stub(KmsStub::new(KmsReply::Error(Code::FailedPrecondition))),
    )
    .await;
    let status = match result.unwrap_err() {
        GcpSmKmsError::KmsDecrypt { source, .. } => source.status().map(|status| status.code),
        _ => None,
    };
    assert_eq!(status, Some(Code::FailedPrecondition));
}

#[tokio::test]
async fn load_fails_fast_instead_of_hanging() {
    // Stands in for a stalled metadata server or an unresponsive GCP endpoint:
    // without the bound the node would wait here forever at startup.
    let result = within_timeout(
        std::time::Duration::from_millis(10),
        std::future::pending::<Result<(), GcpSmKmsError>>(),
    )
    .await;
    assert!(matches!(result, Err(GcpSmKmsError::Timeout { .. })));

    // A future that completes in time is untouched.
    let ok = within_timeout(std::time::Duration::from_secs(30), async {
        Ok::<_, GcpSmKmsError>(7)
    })
    .await;
    assert_eq!(ok.unwrap(), 7);
}

#[tokio::test]
async fn loader_accepts_absent_response_checksums() {
    let result = load_private_key_with_clients(
        &config(),
        &SecretClient::from_stub(SecretStub::new(SecretReply::Success {
            data: Bytes::from_static(CIPHERTEXT),
            checksum: None,
        })),
        &KmsClient::from_stub(KmsStub::new(KmsReply::Success {
            plaintext: Bytes::from_static(KEY_HEX.as_bytes()),
            checksum: None,
        })),
    )
    .await
    .unwrap();
    assert_eq!(result.as_slice(), [1; 32]);
}

#[tokio::test]
async fn loader_rejects_plaintext_checksum_mismatch() {
    let result = load_private_key_with_clients(
        &config(),
        &SecretClient::from_stub(matching_secret()),
        &KmsClient::from_stub(KmsStub::new(KmsReply::Success {
            plaintext: Bytes::from_static(KEY_HEX.as_bytes()),
            checksum: Some(0),
        })),
    )
    .await;
    assert!(matches!(
        result,
        Err(GcpSmKmsError::PlaintextChecksumMismatch { .. })
    ));
}

#[tokio::test]
async fn ceremony_handoff_preserves_emerald_identity_and_local_signing() {
    let fixture: CeremonyFixture =
        serde_json::from_str(include_str!("fixtures/consensus_envelope.json")).unwrap();
    let config = GcpSmKmsConfig {
        secret_version: fixture.secret_version,
        kms_crypto_key: fixture.kms_crypto_key,
        kms_aad: fixture.kms_aad,
    };
    let secret = SecretStub::new(SecretReply::Success {
        data: Bytes::from_static(CIPHERTEXT),
        checksum: Some(i64::from(crc32c::crc32c(CIPHERTEXT))),
    });
    let kms = KmsStub::new(KmsReply::Success {
        plaintext: Bytes::copy_from_slice(fixture.plaintext_hex.as_bytes()),
        checksum: Some(i64::from(crc32c::crc32c(fixture.plaintext_hex.as_bytes()))),
    });

    let material = load_private_key_with_clients(
        &config,
        &SecretClient::from_stub(secret),
        &KmsClient::from_stub(kms),
    )
    .await
    .unwrap();
    let private_key = PrivateKey::from_slice(material.as_slice()).unwrap();
    let public_key = private_key.public_key();
    let address = Address::from_public_key(&public_key);
    assert_eq!(
        address.to_alloy_address().to_checksum(None),
        fixture.expected_address
    );

    let secret_bytes: [u8; 32] = private_key.inner().to_bytes().into();
    let network_secret =
        libp2p_identity::secp256k1::SecretKey::try_from_bytes(secret_bytes).unwrap();
    let network_keypair: libp2p_identity::Keypair =
        libp2p_identity::secp256k1::Keypair::from(network_secret).into();
    assert_eq!(
        network_keypair.public().to_peer_id().to_string(),
        fixture.expected_peer_id
    );

    let provider = K256Provider::new(private_key);
    let message = b"emerald-gcp-key-handoff-local-signing";
    let signature = provider.sign(message);
    assert!(provider.verify(message, &signature, &public_key));
}
