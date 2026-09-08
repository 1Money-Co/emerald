# Validator Key Management

Emerald holds exactly one validator private key: a 32-byte secp256k1 secret that supplies both the
consensus signing key and the libp2p network identity. The key is resolved **once at node startup**,
before any consensus or networking service runs, and is then held in process memory for the lifetime
of the process. Nothing on the consensus or networking hot path reaches out to a key store.

Where that key comes from is selected by the `[key_provider]` section of `emerald.toml`:

| `type`        | Key source                                              |
|---------------|---------------------------------------------------------|
| `file`        | `priv_validator_key.json` on local disk (default)        |
| `aws_sm_kms`  | AWS Secrets Manager ciphertext, decrypted by AWS KMS      |
| `gcp_sm_kms`  | GCP Secret Manager ciphertext, decrypted by Cloud KMS     |

All three producers yield the same private-key bytes before protocol services start, so nodes using
different providers are fully interoperable on the same network. Every provider **fails closed**:
if the key cannot be resolved, the node exits rather than falling back to another source.

> [!IMPORTANT]
> The provider only *reads* an existing key. None of them generate, rotate, back up or write key
> material. Key generation and the ceremony that populates a cloud secret are out of scope for
> Emerald.

## `file` (default)

```toml
[key_provider]
type = "file"
```

This is the default, and is what you get if `[key_provider]` is omitted entirely. The path is not
configurable in `emerald.toml` — it is always `<home>/config/priv_validator_key.json`, derived from
the `--home` flag.

The file is JSON with a single `value` field holding the base64 encoding of the raw 32 key bytes:

```json
{ "value": "<base64 of 32 bytes>" }
```

Restrict it to the node user (`chmod 600`) and keep it off shared storage and out of backups that
leave the host.

## `aws_sm_kms`

```toml
[key_provider]
type       = "aws_sm_kms"
secret_id  = "emerald/validator-0/consensus-key"
region     = "us-east-1"
kms_key_id = "alias/emerald-validator-keys"
# Optional: KMS lives in a different region than the secret.
# kms_region = "us-west-2"
# Optional: must match the encryption context used at encrypt time, exactly.
# [key_provider.kms_encryption_context]
# validator = "0"
```

| Field                    | Required | Meaning                                                           |
|--------------------------|----------|-------------------------------------------------------------------|
| `secret_id`              | yes      | Secrets Manager secret name or ARN                                 |
| `region`                 | yes      | Region of the secret                                               |
| `kms_key_id`             | yes      | KMS key id, ARN or alias used to decrypt                           |
| `kms_region`             | no       | Region of the KMS key; defaults to `region`                        |
| `kms_encryption_context` | no       | Encryption context key/value pairs, matched exactly at decrypt time |

The secret's `SecretString` must be the **base64 encoding of the raw KMS ciphertext blob**, and the
decrypted plaintext must be hex for the 32 key bytes. Surrounding whitespace in the plaintext is
tolerated.

Credentials come from the standard AWS provider chain (instance role, environment, profile). The
node identity needs `secretsmanager:GetSecretValue` on the secret and `kms:Decrypt` on the key.

## `gcp_sm_kms`

```toml
[key_provider]
type           = "gcp_sm_kms"
secret_version = "projects/PROJECT/secrets/SECRET/versions/7"
kms_crypto_key = "projects/PROJECT/locations/LOCATION/keyRings/RING/cryptoKeys/KEY"
kms_aad        = "1money:mainnet:validator:1:general:v1"
```

Three required strings, and nothing else — there is no project, location, credentials, mode or
retry field.

> [!IMPORTANT]
> For a validator that also runs a 1Money L1 node, copy all three values **verbatim** from that
> node's consensus/general GCP key source. In particular: do not use the BLS record, do not replace
> the numeric version with `latest`, and do not invent an Emerald-specific AAD or edit an existing
> one. The AAD is not secret, but it is cryptographically authenticated — a single byte of
> difference makes Cloud KMS refuse to decrypt.

### Authentication and IAM

Authentication is **Application Default Credentials only**. That covers attached Compute Engine and
GKE service accounts, Workload Identity, local developer ADC, and `GOOGLE_APPLICATION_CREDENTIALS`
exported into the node's environment. There is no way to point at a service-account key file from
`emerald.toml`, by design.

The runtime identity needs exactly two permissions:

- `roles/secretmanager.secretAccessor` on the configured secret
- `roles/cloudkms.cryptoKeyDecrypter` on the configured CryptoKey

Secret creation, secret update and KMS *encrypt* permissions are not required and should not be
granted.

Secret versions inherit the secret's IAM policy. The numeric `secret_version` selects what Emerald
reads; it does not restrict the identity's IAM permissions to that version. See
[Google's access guidance](https://docs.cloud.google.com/secret-manager/docs/access-secret-version).

### What the secret must contain

The Secret Manager payload is the **raw binary ciphertext** produced by encrypting the key material
with the configured Cloud KMS key and AAD. It is not base64, not JSON wrapping the ciphertext, and
not plaintext key material.

The canonical decrypted plaintext is lowercase hex for the 32 key bytes, with no `0x` prefix. For
compatibility with the 1Money L1 loader, a `0x`/`0X` prefix, uppercase hex digits, and a JSON object
with a string `private_key`, `privateKey` or `key` field are also accepted. Bare hex must not contain
leading or trailing whitespace. Whitespace inside a JSON key string is also rejected; ordinary
JSON formatting whitespace is accepted, matching the 1Money L1 loader.

> [!NOTE]
> Write ceremony key material without a trailing newline:
> `printf '%s' "$hex" | gcloud kms encrypt ...`. Neither GCP loader trims bare hex.

Emerald verifies the CRC32C checksums both services return, and sends request checksums for the
ciphertext and the AAD. A checksum mismatch aborts startup before the material is used.

The whole load — ADC discovery, the Secret Manager access and the Cloud KMS decrypt — is bounded at
**30 seconds**. The Google clients apply no timeout of their own, so without this bound a stalled
metadata server or a GCP control-plane incident would hang node startup indefinitely. The budget is
fixed, not configurable: on expiry the node exits with a timeout error, and restarting it (via
systemd or your supervisor) is the retry.

### Configuration validation

The three values are checked structurally before any network call, so a typo fails immediately
rather than as a permission error minutes later:

- `secret_version` must be exactly `projects/{project}/secrets/{secret}/versions/{n}`, where `{n}`
  is a canonical positive decimal integer. `latest`, `0`, `007`, `+7`, `-1`, regional paths
  (`.../locations/...`) and extra path segments are all rejected.
- `kms_crypto_key` must be exactly
  `projects/{project}/locations/{location}/keyRings/{ring}/cryptoKeys/{key}`. A CryptoKeyVersion
  suffix is rejected — KMS selects the version from the ciphertext.
- `kms_aad` must be non-empty with no leading or trailing whitespace.
- No segment may be empty or contain whitespace.

GCP remains authoritative for whether the project, resources, key state and permissions are actually
valid.

## Verifying the node picked the right key

Regardless of provider, two startup log lines tell you what happened:

- `Loaded Emerald configuration` carries a `key_provider` field naming the selected provider
  (`file`, `aws_sm_kms` or `gcp_sm_kms`).
- `loaded node public key and address` carries the public key and address **derived from the key
  that was actually resolved**.

That address is the operator's identity check — compare it against the address recorded for this
validator during the key ceremony. The private key itself is never logged.

## Troubleshooting

| Startup error contains                                   | Cause                                                                        |
|----------------------------------------------------------|------------------------------------------------------------------------------|
| `invalid GCP key source configuration`                    | A value failed the structural checks above; the message names which rule      |
| `failed to initialize GCP ... client`                     | ADC could not be resolved — no attached service account, no exported credentials |
| `failed to access GCP Secret Manager version`             | Wrong resource name, or the identity lacks `secretAccessor`                   |
| `returned no payload`                                     | The secret version exists but is disabled or destroyed                        |
| `payload checksum mismatch`                               | The secret payload was corrupted in transit or at rest                        |
| `failed to decrypt key material with Cloud KMS key`       | Wrong CryptoKey, wrong AAD, missing `cryptoKeyDecrypter`, or a disabled key   |
| `plaintext checksum mismatch`                             | The KMS response was corrupted in transit                                     |
| `not valid hexadecimal key material`                      | The plaintext is not hex; the message gives the position of the offending byte |
| `must decode to exactly 32 bytes`                         | The secret holds something other than a 32-byte secp256k1 key (a BLS key?)    |
| `timed out after 30s`                                     | ADC discovery or a GCP endpoint stalled; the node exits so it can be restarted |

> [!NOTE]
> A wrong `kms_aad` and a wrong CryptoKey are indistinguishable from the outside: both surface as a
> Cloud KMS decrypt failure. Re-check the AAD byte-for-byte against the ceremony handoff before
> assuming a permissions problem.
