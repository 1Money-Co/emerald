# GCP Secret Manager + Cloud KMS Key Provider Design

**Status:** Approved for implementation

**Created:** 2026-09-06

## 1. Summary

Add a GCP key-provider backend to Emerald alongside the existing file and AWS
Secrets Manager + KMS backends. At node startup, Emerald reads the configured
Secret Manager version as application-layer envelope ciphertext, decrypts it once
with Cloud KMS and converts the recovered 32-byte secp256k1 private key into the
existing Emerald runtime key types.

Emerald and the corresponding l1client validator will reuse the same GCP key-source
configuration for the l1client consensus/general key:

- the same numeric Secret Manager version resource;
- the same Cloud KMS CryptoKey resource; and
- the same exact AAD bytes.

Emerald does not read the l1client BLS key. It has one validator private key, which
already supplies consensus signing and the libp2p identity through Emerald's
existing startup path.

After startup, signing and verification remain local. Secret Manager and Cloud KMS
are not called from consensus or networking hot paths:

```text
GCP Secret Manager -> Cloud KMS decrypt
    -> parse 32-byte secp256k1 material
    -> existing PrivateKey / K256Provider / libp2p Keypair
    -> local in-memory signing for the process lifetime
```

This changes no consensus messages, signatures, addresses, storage, genesis format
or wire protocol. Nodes using file, AWS and GCP providers remain interoperable
because every provider produces the same private-key bytes before the protocol
services start.

## 2. Goals

1. Allow Emerald to start from the l1client validator's existing GCP
   consensus/general key source without writing plaintext private-key material to
   disk.
2. Use the same application-layer envelope contract as l1client: raw KMS ciphertext
   in Secret Manager, exact configured AAD and CRC32C integrity checks.
3. Load the key only during startup and retain the existing in-memory signing path,
   avoiding cloud latency and availability dependencies during consensus.
4. Use Google Application Default Credentials (ADC), including attached service
   accounts and Workload Identity.
5. Preserve the current file and AWS configurations, public APIs and behavior.
6. Fail closed on invalid configuration, cloud access, checksum, decryption, payload
   or secp256k1 construction errors.
7. Keep the production change narrow enough to review as one cohesive pull request.

## 3. Non-goals

- GCP key generation, ceremony, provisioning, backup, restore, rotation or deletion.
- Creating or updating Secret Manager secrets or Cloud KMS keys from Emerald.
- Reading or configuring the l1client BLS secret.
- A separate Emerald key or AAD for the same validator.
- Remote signing through Cloud KMS.
- Direct plaintext Secret Manager loading.
- Secret Manager CMEK-only loading.
- Hot reload or periodic refresh of private keys.
- Falling back to another secret version, file provider or AWS provider after a GCP
  failure.
- Adding service-account JSON, access tokens or credential paths to Emerald's TOML.
- Changing `emerald generate`, `scripts/generate_testnet_config.sh` or adding a GCP
  ceremony script.
- Extracting a cross-repository crate from l1client or adding a generic multi-cloud
  provider framework.
- Refactoring the existing AWS provider or broadening its accepted payload formats.
- Guaranteeing that temporary allocations inside Google SDK and transport internals
  are zeroized or memory-locked.

## 4. Existing Emerald Boundary

Emerald already has the correct provider boundary:

- `crates/key-provider/src/lib.rs` defines the asynchronous `KeyProvider` trait;
- `FileKeyProvider` and `AwsSmKmsKeyProvider` return `Zeroizing<[u8; 32]>`;
- `EmeraldConfig.key_provider` selects the provider;
- `App::build_runtime` resolves the provider exactly once before protocol services
  start;
- the bytes are converted through `PrivateKey::from_slice`;
- `resolved_private_key: OnceLock<PrivateKey>` makes consensus, libp2p and application
  state use the same resolved key; and
- all subsequent signing uses `K256Provider` and the existing libp2p keypair in
  process memory.

The GCP implementation extends this boundary. It does not introduce a second cache,
key handle, signing abstraction or lifecycle state machine.

Emerald differs from l1client in one important respect. l1client loads two physical
keys: one General ECDSA key for consensus and libp2p, plus one BLS key. Emerald only
needs the former. Copying l1client's BLS routing or multi-key configuration would add
dead functionality and is explicitly excluded.

## 5. Chosen Architecture

### 5.1 Provider placement

Add a focused GCP implementation inside the existing `key-provider` crate. The
public integration surface is conceptually:

```rust
pub struct GcpSmKmsKeyProvider { /* config only */ }

#[async_trait]
impl KeyProvider for GcpSmKmsKeyProvider {
    async fn load_private_key(
        &self,
    ) -> Result<Zeroizing<[u8; 32]>, KeyProviderError>;
}
```

The module owns:

- GCP source validation;
- ADC client creation;
- Secret Manager access;
- CRC32C verification;
- Cloud KMS decryption with AAD;
- plaintext payload parsing; and
- conversion to exactly 32 bytes.

Algorithm-level secp256k1 scalar validation remains in the existing
`PrivateKey::from_slice` call in `App::build_runtime`. The provider must not duplicate
k256 validity rules.

No new workspace crate is needed. The shared behavior with l1client is a deployment
and wire contract, not a Rust dependency between repositories.

### 5.2 Startup data flow

```text
EmeraldConfig::key_provider = GcpSmKms
  -> build_key_provider
  -> GcpSmKmsKeyProvider::load_private_key
      -> validate exact resource names and AAD
      -> initialize Secret Manager and KMS clients with ADC
      -> AccessSecretVersion(exact numeric resource)
      -> verify Secret Manager payload CRC32C when present
      -> KMS Decrypt(raw payload, exact AAD, request CRC32Cs)
      -> verify returned plaintext CRC32C when present
      -> parse l1client-compatible plaintext payload
      -> require exactly 32 decoded bytes
  -> PrivateKey::from_slice
  -> existing public key, address, K256Provider and libp2p keypair
  -> existing local signing paths
```

Any error aborts node startup before the consensus engine is usable. The application
does not add an outer retry loop or fallback. Bounded retries provided by the Google
SDK remain in effect.

Cloud availability is therefore a startup dependency only. Once `build_runtime`
finishes, losing access to GCP does not affect consensus signing or verification.

### 5.3 Client test seam

Production loading builds the official Google clients internally. A crate-private
helper may accept SDK clients or stubs so tests can inspect deterministic requests
and responses. Do not add a public cloud-client trait solely for tests.

## 6. Configuration

### 6.1 TOML shape

Add a feature-gated `GcpSmKms` variant to `KeyProviderConfig` with the serialized
provider name `gcp_sm_kms`:

```toml
[key_provider]
type = "gcp_sm_kms"
secret_version = "projects/PROJECT/secrets/SECRET/versions/7"
kms_crypto_key = "projects/PROJECT/locations/LOCATION/keyRings/RING/cryptoKeys/KEY"
kms_aad = "1money:mainnet:validator:1:general:v1"
```

The Rust configuration contains exactly these three required strings:

```rust
pub struct GcpSmKmsConfig {
    pub secret_version: String,
    pub kms_crypto_key: String,
    pub kms_aad: String,
}
```

There is no separate project, location, credentials, mode, plaintext, BLS or retry
field. Complete resource names avoid reconstruction rules and can be copied directly
from the l1client key-source handoff.

### 6.2 Shared l1client source contract

For a validator running both l1client and Emerald, copy the three values from the
l1client consensus/general source without modification. In particular:

- do not choose the BLS record;
- do not replace the numeric version with `latest`;
- do not derive an Emerald-specific AAD;
- do not change `general` to `emerald` or `consensus` inside an existing AAD; and
- do not trim, normalize, lowercase or otherwise rewrite the AAD.

The AAD is public binding metadata rather than a credential, but it is
cryptographically authenticated. A one-byte difference makes KMS decryption fail.

### 6.3 Source validation

Before creating clients or making a request, validate the same source invariants as
the l1client GCP loader:

- `secret_version` has the global form
  `projects/{project}/secrets/{secret}/versions/{version}`;
- `{version}` is a canonical positive decimal integer;
- reject `latest`, zero, signs, leading zeros, whitespace and extra path segments;
- `kms_crypto_key` has the complete form
  `projects/{project}/locations/{location}/keyRings/{ring}/cryptoKeys/{key}`;
- every resource segment is nonempty and contains no whitespace; and
- `kms_aad` is nonempty and has no leading or trailing whitespace.

Local validation remains intentionally structural. GCP is authoritative for project,
resource existence, key state and authorization.

### 6.4 Authentication and IAM

Authentication uses ADC only. This naturally supports:

- attached Compute Engine or GKE service accounts;
- Workload Identity;
- local developer ADC;
- `GOOGLE_APPLICATION_CREDENTIALS` when supplied outside Emerald; and
- impersonated ADC configurations already supported by the selected SDK.

The validator runtime identity needs only:

- permission to access the configured Secret Manager version, normally through
  `roles/secretmanager.secretAccessor`; and
- permission to decrypt with the configured CryptoKey, normally through
  `roles/cloudkms.cryptoKeyDecrypter`.

Emerald does not require secret creation, update or KMS encryption permission.

## 7. Envelope and Plaintext Contract

### 7.1 Secret Manager payload

The Secret Manager payload is the raw binary ciphertext returned by the l1client GCP
key ceremony's Cloud KMS encryption operation. It is not:

- plaintext private-key material;
- base64 text;
- JSON wrapping the ciphertext; or
- a reference to another resource.

The loader forwards these bytes unchanged as the KMS decrypt ciphertext.

### 7.2 AAD and request integrity

Send `kms_aad.as_bytes()` as `additional_authenticated_data`. Populate KMS request
CRC32C fields for both ciphertext and AAD where supported by the selected API.

For Secret Manager:

- verify `data_crc32c` when present;
- reject a mismatch before calling KMS; and
- accept absence because the API declares the checksum optional.

For Cloud KMS:

- verify `plaintext_crc32c` when present;
- reject a mismatch before parsing; and
- accept absence according to the API response contract.

Do not synthesize missing checksums and do not continue after a mismatch.

### 7.3 Decrypted payload

The canonical ceremony plaintext is lowercase ASCII hex containing the 32-byte
private key, without `0x` and without a trailing newline.

For compatibility with the l1client GCP loader, Emerald also accepts:

- bare hex with optional `0x` or `0X` prefix;
- uppercase hex digits; and
- a JSON object containing a string under `private_key`, `privateKey` or `key`.

This parsing is private to the GCP provider. The existing AWS parser and behavior are
not changed as part of this work. The selected payload must decode to exactly 32
bytes, after which the existing Emerald secp256k1 constructor decides whether it is a
valid private scalar.

Buffers controlled by the new code are wrapped in `Zeroizing` where practical. The
design does not claim full zeroization of buffers allocated inside Google SDK,
protobuf, HTTP or TLS internals, nor does it change Emerald's long-lived runtime key
representations.

## 8. Features and Dependencies

### 8.1 Feature wiring

Add `gcp-sm-kms` to `crates/key-provider` and keep GCP SDK dependencies optional.
The crate's default feature set becomes:

```toml
default = ["aws-sm-kms", "gcp-sm-kms"]
```

Ordinary `cargo build`, release and Docker builds therefore deserialize and run all
three existing provider choices: file, AWS and GCP. No release-command-specific
`--features` flag is required.

The GCP configuration variant, provider module, error surface and app match arms are
gated consistently. `key-provider --no-default-features` must continue to compile
with the file provider only.

### 8.2 Google SDK selection and MSRV

Use the official Google SDK generation already proven by l1client as the starting
dependency set:

- `google-cloud-secretmanager-v1 = =1.8.0`;
- `google-cloud-kms-v1 = =1.8.0`;
- the compatible GAX/Auth versions selected and locked by Cargo; and
- an explicit CRC32C dependency used directly by production code.

The service crates require Rust 1.86. Emerald currently declares Rust 1.83 while its
Docker build already uses Rust 1.90 and CI uses the current stable toolchain. Update
the workspace `rust-version` to 1.86 so package metadata states the real minimum.

Before merging, verify the final lockfile rather than assuming l1client's complete
dependency graph can be copied. Emerald already contains AWS SDK, rustls 0.21/0.23,
Tokio 1.49 and mio 1.1, so the combined graph must pass build, tests and a real
startup boundary with both cloud features enabled.

Do not upgrade unrelated runtime dependencies merely to use a newer Google SDK.

## 9. Runtime Integration

Extend only the existing provider selection points:

1. add `GcpSmKms(GcpSmKmsConfig)` to `KeyProviderConfig`;
2. expose `GcpSmKmsKeyProvider` from the `key-provider` crate;
3. return `"gcp_sm_kms"` from `key_provider_kind`;
4. construct the provider in `build_key_provider`; and
5. update configuration tests and generic comments that currently say only
   file/AWS.

The rest of `App::build_runtime` remains unchanged. In particular, GCP key bytes flow
through the same `PrivateKey::from_slice`, public-key/address derivation,
`resolved_private_key`, `K256Provider` and libp2p conversion used by the other
providers.

Do not add GCP branches to `emerald generate` or the shell configuration generator.
Deployment already obtains the source triplet from the l1client key ceremony and can
write the small TOML section directly.

## 10. Error Handling and Logging

Errors remain actionable while excluding secret material. The provider distinguishes
at least:

- invalid local GCP source configuration;
- Google client initialization failure;
- Secret Manager access failure;
- missing Secret Manager payload;
- Secret Manager checksum mismatch;
- KMS decrypt failure;
- plaintext checksum mismatch;
- invalid decrypted payload; and
- wrong decoded length.

Preserve underlying Google SDK errors as error sources where practical so callers can
identify permission denied, not found, disabled, destroyed and unavailable states.
Do not create an enum variant for every GCP status code.

Logs may include operation names and configured resource identifiers. They must not
include:

- plaintext or decoded private-key bytes;
- Secret Manager payload bytes;
- KMS ciphertext;
- ADC credentials or tokens; or
- SDK request/response debug dumps.

AAD is non-confidential and may remain visible through the existing configuration
`Debug` log, matching the l1client contract. Normal provider events should log that
AAD was supplied rather than echoing its value.

Success logs identify the provider and Secret Manager version but never the private
key. The existing derived public key and address startup log remains the operator's
identity check.

## 11. Testing

### 11.1 Configuration tests

Cover:

- TOML deserialization and serialization of `gcp_sm_kms`;
- the existing absent-section default to `File`;
- unchanged AWS parsing;
- exact preservation of the three GCP strings;
- valid global numeric Secret Manager versions;
- rejection of `latest`, `0`, `+7`, `007`, negative, whitespace, regional and
  malformed resources;
- valid complete CryptoKey resources and malformed-resource rejection; and
- empty or whitespace-padded AAD rejection.

### 11.2 Provider tests

Use official SDK stubs or a crate-private injected client seam. Tests must not depend
on ambient ADC or live GCP resources.

Cover:

- the exact Secret Manager version sent to `AccessSecretVersion`;
- present/matching, present/mismatching and absent Secret Manager CRC32C;
- no KMS call after Secret Manager failure or checksum mismatch;
- raw binary ciphertext forwarded unchanged;
- exact AAD forwarded unchanged;
- ciphertext and AAD request CRC32Cs;
- present/matching, present/mismatching and absent plaintext CRC32C;
- Secret Manager and KMS status errors retained as sources;
- no application-level retry or fallback;
- canonical lowercase ceremony plaintext;
- l1client-compatible prefix, case and JSON payload forms;
- invalid UTF-8, malformed JSON, missing aliases, invalid hex and wrong length; and
- controlled temporary plaintext buffers use zeroizing wrappers.

### 11.3 Emerald integration regression

Add a synthetic fixture representing the l1client consensus/general handoff. It
contains:

- one numeric Secret Manager version resource;
- one complete CryptoKey resource;
- the exact versioned General-key AAD;
- raw synthetic ciphertext;
- a valid synthetic secp256k1 plaintext; and
- the expected public key and Emerald address.

Load the fixture through the GCP pipeline and existing Emerald key constructor, then
assert:

1. the resulting public key and address match the fixture;
2. consensus signing succeeds locally;
3. the libp2p identity is derived from the same key; and
4. no cloud client is needed after initialization.

The fixture contains no production resource identifiers, ciphertext or private key.

### 11.4 Feature and repository verification

Run the narrow checks first, followed by the repository checks:

```bash
cargo test -p key-provider --no-default-features
cargo test -p key-provider --all-features
cargo check --workspace --all-features
cargo nextest run --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo +nightly fmt --all --check
taplo fmt --check
```

Also verify:

- a default release build includes both AWS and GCP provider variants;
- a real Emerald process reaches consensus/libp2p initialization with the combined
  TLS and crypto-provider graph; and
- `git diff --check` passes.

An ignored live-GCP smoke test may be retained for manual validation with disposable
resources, but it is not a CI dependency and must never reference production keys.

## 12. Expected File Scope

The implementation should remain concentrated in:

- root `Cargo.toml` and generated `Cargo.lock`;
- `crates/key-provider/Cargo.toml`;
- `crates/key-provider/src/config.rs`;
- `crates/key-provider/src/error.rs`;
- `crates/key-provider/src/lib.rs`;
- one focused GCP provider module and its tests;
- `app/src/node.rs`; and
- the existing Emerald configuration tests in `cli/src/config.rs`.

No protocol, engine, storage, type, Solidity, Reth, key-ceremony or deployment-script
file is expected to change.

The Google dependency lockfile diff may be mechanically large. Keep hand-written
production code and tests in the narrow files above so reviewers can separate
dependency resolution from behavior.

## 13. Delivery

Implement this as one cohesive production pull request. Unlike l1client, Emerald has
one key type, one provider trait and one startup routing point; splitting the loader
from its only consumer would create an unusable intermediate state without materially
reducing review risk.

The implementation order inside the PR is:

1. add and verify optional Google dependencies, feature combinations and MSRV;
2. add source validation and focused tests;
3. add the envelope loader and deterministic SDK-stub tests;
4. add the configuration variant and startup routing;
5. add the l1client handoff compatibility regression; and
6. run the full verification matrix.

If dependency resolution reveals a required unrelated Tokio, mio, rustls or AWS SDK
upgrade, stop and split that prerequisite into a separate reviewed change. Do not
hide a broad runtime upgrade inside the key-provider PR.

## 14. Rollout

1. Use the l1client key ceremony output for the validator's consensus/general key.
2. Grant the Emerald runtime service account access to the exact Secret Manager
   version and decrypt permission on the exact CryptoKey.
3. Copy `secret_version`, `kms_crypto_key` and `kms_aad` byte-for-byte into Emerald's
   `gcp_sm_kms` TOML section.
4. Start one Emerald validator and compare its logged public key/address with the
   expected validator identity before wider rollout.
5. Confirm the node continues signing after startup without further GCP calls.
6. Roll out normally. No coordinated protocol upgrade or genesis migration is
   required.

On restart, GCP becomes available as a startup dependency again. If the configured
version is unavailable or cannot be decrypted, the node remains stopped rather than
using a different identity.

## 15. Alternatives Rejected

### Separate GCP/material workspace crates

l1client benefits from separate crates because it has several configuration routes,
two algorithms and an AWS parser compatibility boundary. Emerald has one 32-byte key
and an existing provider crate. Extra crates would add manifests, public APIs and
dependency wiring without isolating another real responsibility.

### Depend on l1client crates

Direct cross-repository reuse would couple Emerald releases and lockfiles to an
unrelated node repository. The stable shared boundary is the GCP resource/AAD/payload
contract, which is small enough to implement locally and test against a common-shaped
fixture.

### Generic cloud-provider abstraction

The existing `KeyProvider` trait already abstracts the only operation Emerald needs.
Adding a second abstraction for cloud clients, versions, AAD and credentials would
duplicate that boundary and force AWS/GCP into a least-common-denominator model.

### Modify AWS to share the plaintext parser

That could reduce a small amount of code but would expand the change into AWS
behavior compatibility. The GCP feature does not require it, so the existing AWS
surface stays untouched.

### Direct Secret Manager or CMEK-only mode

Those modes do not match the l1client ceremony handoff and introduce different
payload, IAM and threat models. They can be designed later if a concrete deployment
requires them; this provider must never silently fall back to plaintext.

### Remote KMS signing

Remote signing would put cloud latency and availability on high-frequency consensus
paths and would not match the current Emerald key ownership model. This design
restores the real key once and keeps the existing local signer.

## 16. Success Criteria

The feature is complete when:

1. Emerald starts using the exact GCP consensus/general source triplet already used
   by its corresponding l1client validator.
2. Emerald never reads or requires the l1client BLS secret.
3. Secret Manager contains raw application-envelope ciphertext and Cloud KMS receives
   the configured AAD unchanged.
4. Supported Secret Manager and KMS CRC32C values are verified, and mismatches fail
   closed.
5. The recovered material becomes the existing Emerald secp256k1 runtime key without
   being written to disk.
6. Consensus and libp2p use that same key, and all post-startup signing is local.
7. GCP outages after initialization do not interrupt signing; GCP failures during
   startup prevent the node from starting.
8. File and AWS configurations, behavior and tests remain compatible.
9. Default release and Docker builds support file, AWS and GCP providers without
   extra feature flags.
10. The declared MSRV, dependency graph, TLS initialization and full repository
    checks pass with AWS and GCP enabled together.
