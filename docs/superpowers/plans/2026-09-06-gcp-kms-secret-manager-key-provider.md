# GCP Secret Manager + Cloud KMS Key Provider Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Enable Emerald to load its validator secp256k1 key at startup from the same GCP consensus/general envelope source used by l1client.

**Architecture:** Extend the existing `key-provider` crate with one stateless GCP provider. It validates the l1client source triplet, fetches raw ciphertext from an immutable Secret Manager version, decrypts it with Cloud KMS and exact AAD, returns `Zeroizing<[u8; 32]>`, and then relies on Emerald's unchanged startup code to build the in-memory consensus/libp2p key.

**Tech Stack:** Rust, async-trait, official Google Secret Manager/KMS Rust clients, CRC32C, serde, zeroize, Tokio.

**Spec:** `docs/superpowers/specs/2026-09-06-gcp-kms-secret-manager-key-provider-design.md`

## Global Constraints

- Only Emerald changes; do not modify l1client or interoperability repositories.
- Use only l1client's consensus/general source; never load or configure BLS.
- Support only application-layer envelope mode with raw KMS ciphertext and exact AAD.
- ADC is the only authentication mechanism.
- GCP is startup-only; recurring signing remains local.
- Preserve File and AWS behavior and public API.
- Default builds include `aws-sm-kms` and `gcp-sm-kms`; file-only builds remain possible.
- Use Google Secret Manager/KMS `=1.8.0` as the starting SDK generation and Rust 1.91.1 as the supported MSRV, matching the locked default AWS graph.
- Do not change config generators, ceremony scripts, consensus, storage, Reth or Solidity.
- Do not commit until the user explicitly requests a commit.

---

### Task 1: GCP configuration and canonical source validation

**Files:**
- Modify: `Cargo.toml`
- Modify: `crates/key-provider/Cargo.toml`
- Modify: `crates/key-provider/src/config.rs`
- Modify: `cli/src/config.rs`
- Generated: `Cargo.lock`

**Interfaces:**
- Produces: `KeyProviderConfig::GcpSmKms(GcpSmKmsConfig)`.
- Produces: crate-private `GcpSmKmsConfig::validate(&self) -> Result<(), &'static str>`.
- Serialized provider name: `gcp_sm_kms`.

- [x] **Step 1: Add a failing TOML configuration test**

Add a `cli/src/config.rs` test that parses this literal and asserts all three fields:

```toml
[key_provider]
type = "gcp_sm_kms"
secret_version = "projects/ceremony-test/secrets/validator-general/versions/7"
kms_crypto_key = "projects/ceremony-test/locations/global/keyRings/validators/cryptoKeys/envelope"
kms_aad = "1money:ceremony-test:validator:1:general:v1"
```

- [x] **Step 2: Run the test and observe the unknown-variant failure**

Run: `cargo test -p malachitebft-eth-cli emerald_config_parses_gcp_sm_kms_key_provider`

- [x] **Step 3: Add the feature, dependencies, config struct and enum variant**

Add `gcp-sm-kms`, make both cloud backends default, and add the approved three-field struct. Update workspace MSRV and the Docker toolchain to 1.91.1 and add exact Google service dependencies plus `crc32c`/`bytes` where directly used.

- [x] **Step 4: Run the focused TOML test and confirm it passes**

Run: `cargo test -p malachitebft-eth-cli emerald_config_parses_gcp_sm_kms_key_provider`

- [x] **Step 5: Add failing table-driven source validation tests**

Cover valid values plus rejection of `latest`, `0`, `+7`, `007`, overflow, whitespace, regional Secret Manager paths, malformed CryptoKey resources, and empty/padded AAD.

- [x] **Step 6: Run the validation tests and observe failures caused by missing validation**

Run: `cargo test -p key-provider gcp_sm_kms::tests::config --all-features -- --nocapture`

- [x] **Step 7: Implement minimal structural validation**

Use exact path segments and canonical decimal rules:

```rust
fn is_canonical_positive_version(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && !value.starts_with('0')
        && value.parse::<u64>().is_ok()
}
```

Preserve accepted strings verbatim.

- [x] **Step 8: Re-run config tests**

Run: `cargo test -p key-provider gcp_sm_kms::tests::config --all-features -- --nocapture`

### Task 2: Plaintext payload compatibility

**Files:**
- Create: `crates/key-provider/src/gcp_sm_kms.rs`
- Create: `crates/key-provider/src/gcp_sm_kms/tests.rs`
- Modify: `crates/key-provider/src/lib.rs`
- Modify: `crates/key-provider/src/error.rs`

**Interfaces:**
- Produces: private `parse_secret_material(&[u8]) -> Result<Zeroizing<[u8; 32]>, GcpSmKmsError>`.
- Produces: public `GcpSmKmsError`, transparently wrapped by `KeyProviderError`.

- [x] **Step 1: Add failing payload tests**

Use literal expected bytes for canonical lowercase hex, `0x`, `0X`, uppercase, and JSON aliases `private_key`, `privateKey`, and `key`. Add rejection cases for invalid UTF-8, malformed JSON, missing/non-string aliases, odd/invalid hex and decoded lengths other than 32.

- [x] **Step 2: Run payload tests and observe the unimplemented-path failures**

Run: `cargo test -p key-provider gcp_sm_kms::tests::payload --all-features -- --nocapture`

- [x] **Step 3: Implement the minimal l1client-compatible parser**

Parse bare hex first, fall back to JSON only when bare-hex parsing fails, preserve alias priority, strip only `0x`/`0X`, and do not trim plaintext. Recursively zeroize JSON strings owned by the new code and require exactly 32 decoded bytes.

- [x] **Step 4: Re-run payload tests**

Run: `cargo test -p key-provider gcp_sm_kms::tests::payload --all-features -- --nocapture`

### Task 3: One-shot Secret Manager and KMS envelope loader

**Files:**
- Modify: `crates/key-provider/src/gcp_sm_kms.rs`
- Modify: `crates/key-provider/src/gcp_sm_kms/tests.rs`
- Modify: `crates/key-provider/src/error.rs`
- Modify: `crates/key-provider/Cargo.toml`

**Interfaces:**
- Produces: `GcpSmKmsKeyProvider::new(GcpSmKmsConfig) -> Self`.
- Produces: `KeyProvider::load_private_key() -> Result<Zeroizing<[u8; 32]>, KeyProviderError>`.
- Produces: crate-private client-injected loader for deterministic SDK stub tests.

- [x] **Step 1: Add failing SDK-stub tests for Secret Manager**

Assert exact version resource, matching/absent checksum success, mismatch/missing payload failure before KMS, and preservation of permission-denied status.

- [x] **Step 2: Add failing SDK-stub tests for KMS**

Assert exact CryptoKey, raw ciphertext, exact AAD, both request CRC32Cs, matching/absent plaintext checksum success, mismatch failure, and preservation of KMS status.

- [x] **Step 3: Run the loader tests and observe missing loader failures**

Run: `cargo test -p key-provider gcp_sm_kms::tests::loader --all-features -- --nocapture`

- [x] **Step 4: Implement the one-shot loader**

Build official clients with ADC in production, call `AccessSecretVersion`, verify optional Secret Manager CRC32C, call `Decrypt` with raw bytes/AAD/request CRCs, verify optional plaintext CRC32C, parse exactly 32 bytes, and return `Zeroizing<[u8; 32]>`.

- [x] **Step 5: Re-run loader and full key-provider tests**

Run:

```bash
cargo test -p key-provider gcp_sm_kms::tests::loader --all-features -- --nocapture
cargo test -p key-provider --all-features
cargo test -p key-provider --no-default-features
```

### Task 4: Emerald startup routing and l1client handoff regression

**Files:**
- Modify: `app/src/node.rs`
- Modify: `cli/src/config.rs`
- Create: `crates/key-provider/src/gcp_sm_kms/fixtures/consensus_envelope.json`
- Modify: `crates/key-provider/src/gcp_sm_kms/tests.rs`

**Interfaces:**
- Consumes: `KeyProviderConfig::GcpSmKms` and `GcpSmKmsKeyProvider::new`.
- Preserves: existing `PrivateKey::from_slice`, `resolved_private_key`, `K256Provider`, and libp2p conversion.

- [x] **Step 1: Add failing provider-kind and handoff regression tests**

The fixture uses l1client's synthetic General source:

```json
{
  "secret_version": "projects/ceremony-test/secrets/validator-general/versions/7",
  "kms_crypto_key": "projects/ceremony-test/locations/global/keyRings/validators/cryptoKeys/envelope",
  "kms_aad": "1money:ceremony-test:validator:1:general:v1",
  "plaintext_hex": "0101010101010101010101010101010101010101010101010101010101010101",
  "expected_address": "0x1a642f0E3c3aF545E7AcBD38b07251B3990914F1",
  "expected_peer_id": "16Uiu2HAmEWQnHq2jLKJypwVnVoQeFCULuyop6atvq2eWjYSUjzNi"
}
```

Assert the stubbed pipeline loads the expected bytes, Emerald's existing key type derives the literal address/peer ID, and a signature verifies locally after the loader call has returned.

- [x] **Step 2: Run focused tests and observe missing routing/kind failures**

Run:

```bash
cargo test -p emerald key_provider_kind_names_gcp_provider
cargo test -p key-provider gcp_sm_kms::tests::ceremony --all-features -- --nocapture
```

- [x] **Step 3: Add only the existing provider-selection match arms**

Update `key_provider_kind` and `build_key_provider`. Generalize AWS-only comments to cloud providers. Do not change startup ordering or add another cache.

- [x] **Step 4: Re-run focused and affected tests**

Run:

```bash
cargo test -p emerald key_provider_kind_names_gcp_provider
cargo test -p key-provider --all-features
cargo test -p malachitebft-eth-cli emerald_config -- --nocapture
```

### Task 5: Final dependency and repository verification

**Files:**
- Verify all files listed above.
- Modify only code necessary to address failures caused by this feature.

- [x] **Step 1: Format generated Rust and validate TOML syntax**

Run:

```bash
cargo +nightly fmt --all
taplo fmt
```

- [x] **Step 2: Run focused feature combinations**

Run:

```bash
cargo test -p key-provider --no-default-features
cargo test -p key-provider --all-features
```

- [ ] **Step 3: Run workspace verification**

Run:

```bash
cargo check --workspace --all-features
cargo nextest run --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo +nightly fmt --all --check
taplo fmt --check
```

- [x] **Step 4: Verify release/default feature and source scope**

Run:

```bash
cargo build --release --locked -p emerald
cargo tree -p emerald -e features
git diff --check
git status --short
```

Confirm the default Emerald graph contains both cloud providers, no unexpected subsystem changed, and no secret material was added.

### Verification note

- `cargo check --workspace --all-features` passed.
- `cargo nextest run --workspace --all-features -E 'not package(emerald-mbt)'`
  passed 89/89 tests; the unfiltered run is blocked because the local environment
  has no `quint` executable, causing five existing model-based tests to fail before
  exercising Rust code.
- GCP production code passes focused clippy with warnings denied. Full workspace
  clippy stops on existing `std_instead_of_core` warnings in the AWS provider.
- Changed Rust files pass rustfmt with `skip_children=true`, TOML files pass
  `taplo lint`, and `git diff --check` passes. Whole-workspace fmt checks expose
  pre-existing formatting drift in unrelated files, which is intentionally not
  included in this feature change.
- `cargo build --release --locked -p emerald` passed, and the default feature tree
  contains both AWS and GCP service clients.
