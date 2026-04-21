# Mainnet Key Management Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add modular key provider support so validator nodes can load their signing key from AWS Secrets Manager + KMS instead of a plaintext file, without changing any testnet workflows.

**Architecture:** Introduce a new `key-provider` crate under `crates/` with a `KeyProvider` async trait and two implementations (`FileKeyProvider` for existing behaviour, `AwsSmKmsKeyProvider` for mainnet). A new `key_provider` field in `EmeraldConfig` selects the provider at runtime; the single call-site change is in `app/src/node.rs`. A new `emerald generate` CLI subcommand with `--key-provider file|aws-sm-kms` generates per-node configs; the `aws-sm-kms` path writes configs referencing SM secrets without touching private keys.

**Tech Stack:** Rust, `async-trait`, `zeroize`, `aws-config 1.x`, `aws-sdk-secretsmanager 1.x`, `aws-sdk-kms 1.x`, `serde`, `base64 0.22`, `hex`, LocalStack for integration tests.

---

## File Map

### New files
| File | Purpose |
|---|---|
| `crates/key-provider/Cargo.toml` | New crate manifest |
| `crates/key-provider/src/lib.rs` | `KeyProvider` trait + re-exports |
| `crates/key-provider/src/error.rs` | `KeyProviderError` |
| `crates/key-provider/src/config.rs` | `KeyProviderConfig` enum + `AwsSmKmsConfig` |
| `crates/key-provider/src/file.rs` | `FileKeyProvider` |
| `crates/key-provider/src/aws_sm_kms.rs` | `AwsSmKmsKeyProvider` |
| `cli/src/cmd/shared.rs` | `RuntimeFlavour`, `KeyProviderType`, `P2pOptions` — shared by testnet and new generate commands |
| `cli/src/cmd/generate.rs` | New top-level `emerald generate` command |

### Modified files
| File | Change |
|---|---|
| `Cargo.toml` (workspace root) | Add `crates/key-provider` member + workspace deps for AWS SDKs + zeroize |
| `cli/Cargo.toml` | Add `key-provider` dependency |
| `app/Cargo.toml` | Add `key-provider` dependency |
| `cli/src/config.rs` | Add `key_provider: KeyProviderConfig` field to `EmeraldConfig` |
| `app/src/node.rs` | Replace `load_private_key_file()` at line 71 with async key provider call |
| `cli/src/args.rs` | Add `Generate(GenerateCmd)` variant to top-level `Commands` enum |
| `cli/src/cmd/mod.rs` | Declare `pub mod generate` and `pub mod shared` |

`cli/src/cmd/testnet/generate.rs` is **not modified** — fully preserved for backward compatibility.

---

## Task 1: Workspace scaffold — add crate and dependencies

**Files:**
- Modify: `Cargo.toml` (workspace root)
- Create: `crates/key-provider/Cargo.toml`
- Create: `crates/key-provider/src/lib.rs`

- [ ] **Step 1: Add the new crate to the workspace members list**

In `/Users/nsh/workspace/1money/sidechain/emerald/Cargo.toml`, find the `[workspace]` `members` array and add `"crates/key-provider"`:

```toml
members = [
    "app",
    "cli",
    "contracts",
    "crates/core",
    "crates/key-provider",    # ← add this line
    "engine",
    "utils",
    "types",
    "tests/mbt",
]
```

- [ ] **Step 2: Add workspace-level dependency entries for AWS SDKs and zeroize**

In the same `Cargo.toml`, find the `[workspace.dependencies]` section and append:

```toml
aws-config               = { version = "1" }
aws-sdk-kms              = { version = "1" }
aws-sdk-secretsmanager   = { version = "1" }
zeroize                  = { version = "1", features = ["derive"] }
```

- [ ] **Step 3: Create the crate directory and Cargo.toml**

```bash
mkdir -p crates/key-provider/src
```

Create `crates/key-provider/Cargo.toml`:

```toml
[package]
name             = "key-provider"
version.workspace = true
edition.workspace = true
repository.workspace = true
license.workspace = true
rust-version.workspace = true

[dependencies]
async-trait.workspace = true
base64 = "0.22"
hex.workspace = true
serde = { workspace = true, features = ["derive"] }
thiserror.workspace = true
tracing.workspace = true
zeroize.workspace = true

# AWS — gated on feature flag so non-mainnet builds stay lean
aws-config             = { workspace = true, optional = true }
aws-sdk-kms            = { workspace = true, optional = true }
aws-sdk-secretsmanager = { workspace = true, optional = true }

[features]
default   = ["aws-sm-kms"]
aws-sm-kms = ["dep:aws-config", "dep:aws-sdk-kms", "dep:aws-sdk-secretsmanager"]

[dev-dependencies]
tokio    = { workspace = true, features = ["rt", "macros"] }
tempfile = "3"
```

- [ ] **Step 4: Create a minimal lib.rs so the workspace compiles**

Create `crates/key-provider/src/lib.rs`:

```rust
pub mod config;
pub mod error;
pub mod file;

#[cfg(feature = "aws-sm-kms")]
pub mod aws_sm_kms;

pub use config::KeyProviderConfig;
pub use error::KeyProviderError;
pub use file::FileKeyProvider;

#[cfg(feature = "aws-sm-kms")]
pub use aws_sm_kms::AwsSmKmsKeyProvider;

use async_trait::async_trait;
use zeroize::Zeroizing;

#[async_trait]
pub trait KeyProvider: Send + Sync {
    async fn load_private_key(&self) -> Result<Zeroizing<[u8; 32]>, KeyProviderError>;
}
```

Create placeholder files so it compiles:

`crates/key-provider/src/error.rs`:
```rust
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
```

`crates/key-provider/src/config.rs`:
```rust
#[derive(Debug, Clone, serde::Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum KeyProviderConfig {
    #[default]
    File,
    #[cfg(feature = "aws-sm-kms")]
    #[serde(rename = "aws_sm_kms")]
    AwsSmKms(AwsSmKmsConfig),
}

#[cfg(feature = "aws-sm-kms")]
#[derive(Debug, Clone, serde::Deserialize)]
pub struct AwsSmKmsConfig {
    pub secret_id: String,
    pub region:    String,
    pub kms_key_id: String,
    #[serde(default)]
    pub kms_region: Option<String>,
    #[serde(default)]
    pub kms_encryption_context:
        Option<std::collections::BTreeMap<String, String>>,
}
```

`crates/key-provider/src/file.rs`:
```rust
use std::path::PathBuf;
use async_trait::async_trait;
use zeroize::Zeroizing;
use crate::{KeyProvider, KeyProviderError};

pub struct FileKeyProvider {
    pub path: PathBuf,
}

impl FileKeyProvider {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

#[async_trait]
impl KeyProvider for FileKeyProvider {
    async fn load_private_key(&self) -> Result<Zeroizing<[u8; 32]>, KeyProviderError> {
        todo!("implemented in Task 2")
    }
}
```

`crates/key-provider/src/aws_sm_kms.rs`:
```rust
use async_trait::async_trait;
use zeroize::Zeroizing;
use crate::{KeyProvider, KeyProviderError, config::AwsSmKmsConfig};

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
        todo!("implemented in Task 3")
    }
}
```

- [ ] **Step 5: Verify the workspace compiles**

```bash
cargo build -p key-provider
```

Expected: Compiles successfully (todo!() panics are never reached at compile time).

- [ ] **Step 6: Commit scaffold**

```bash
git add crates/key-provider/ Cargo.toml Cargo.lock
git commit -m "feat: scaffold crates/key-provider with KeyProvider trait"
```

---

## Task 2: Implement FileKeyProvider (TDD)

**Files:**
- Modify: `crates/key-provider/src/file.rs`

- [ ] **Step 1: Write failing tests**

Replace the entire `crates/key-provider/src/file.rs` with:

```rust
use std::path::PathBuf;
use async_trait::async_trait;
use base64::Engine as _;
use zeroize::Zeroizing;
use crate::{KeyProvider, KeyProviderError};

pub struct FileKeyProvider {
    pub path: PathBuf,
}

impl FileKeyProvider {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

#[derive(serde::Deserialize)]
struct PrivValidatorKeyFile {
    value: String,
}

#[async_trait]
impl KeyProvider for FileKeyProvider {
    async fn load_private_key(&self) -> Result<Zeroizing<[u8; 32]>, KeyProviderError> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_key_json(key_bytes: &[u8; 32]) -> String {
        let encoded = base64::engine::general_purpose::STANDARD.encode(key_bytes);
        format!(
            r#"{{"type":"tendermint/PrivKeySecp256k1","value":"{}"}}"#,
            encoded
        )
    }

    #[tokio::test]
    async fn loads_32_byte_key_from_valid_file() {
        let key_bytes: [u8; 32] = core::array::from_fn(|i| i as u8);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("priv_validator_key.json");
        std::fs::write(&path, make_key_json(&key_bytes)).unwrap();

        let provider = FileKeyProvider::new(&path);
        let result = provider.load_private_key().await.unwrap();
        assert_eq!(*result, key_bytes);
    }

    #[tokio::test]
    async fn returns_io_error_for_missing_file() {
        let provider = FileKeyProvider::new("/nonexistent/priv_validator_key.json");
        let err = provider.load_private_key().await.unwrap_err();
        assert!(matches!(err, KeyProviderError::Io(_)));
    }

    #[tokio::test]
    async fn returns_parse_error_for_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, b"not-json").unwrap();

        let provider = FileKeyProvider::new(&path);
        let err = provider.load_private_key().await.unwrap_err();
        assert!(matches!(err, KeyProviderError::ParseKey(_)));
    }

    #[tokio::test]
    async fn returns_parse_error_when_base64_is_not_32_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("short.json");
        // only 16 bytes
        let short = base64::engine::general_purpose::STANDARD.encode([0u8; 16]);
        let json = format!(
            r#"{{"type":"tendermint/PrivKeySecp256k1","value":"{}"}}"#,
            short
        );
        std::fs::write(&path, json).unwrap();

        let provider = FileKeyProvider::new(&path);
        let err = provider.load_private_key().await.unwrap_err();
        assert!(matches!(err, KeyProviderError::ParseKey(_)));
    }
}
```

- [ ] **Step 2: Run tests — verify they all fail**

```bash
cargo test -p key-provider file::tests
```

Expected output: 4 test failures including "not yet implemented" panics.

- [ ] **Step 3: Implement load_private_key**

Replace the `todo!()` in `load_private_key`:

```rust
async fn load_private_key(&self) -> Result<Zeroizing<[u8; 32]>, KeyProviderError> {
    let contents = std::fs::read_to_string(&self.path)?;
    let parsed: PrivValidatorKeyFile = serde_json::from_str(&contents)
        .map_err(|e| KeyProviderError::ParseKey(e.to_string()))?;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(&parsed.value)
        .map_err(|e| KeyProviderError::ParseKey(e.to_string()))?;
    let arr: [u8; 32] = raw
        .try_into()
        .map_err(|_| KeyProviderError::ParseKey("key must be exactly 32 bytes".into()))?;
    Ok(Zeroizing::new(arr))
}
```

You'll need `serde_json` in the crate. Add it to `crates/key-provider/Cargo.toml` under `[dependencies]`:

```toml
serde_json = { workspace = true }
```

- [ ] **Step 4: Run tests — verify they all pass**

```bash
cargo test -p key-provider file::tests
```

Expected: 4 tests pass, 0 fail.

- [ ] **Step 5: Commit**

```bash
git add crates/key-provider/
git commit -m "feat: implement FileKeyProvider with base64 JSON key loading"
```

---

## Task 3: Implement AwsSmKmsKeyProvider (TDD)

**Files:**
- Modify: `crates/key-provider/src/aws_sm_kms.rs`

The AWS network calls cannot be unit-tested without LocalStack. This task covers the synchronous parsing/decoding logic with unit tests, and structures the async AWS calls correctly. Full end-to-end testing is in Task 8.

- [ ] **Step 1: Write unit tests for the hex-decode + byte-extraction logic**

Add a test module at the bottom of `crates/key-provider/src/aws_sm_kms.rs`:

```rust
#[cfg(test)]
mod tests {
    use base64::Engine as _;

    fn encode_ciphertext_for_sm(fake_ciphertext: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(fake_ciphertext)
    }

    fn plaintext_hex_from_key(key: &[u8; 32]) -> Vec<u8> {
        hex::encode(key).into_bytes()
    }

    #[test]
    fn parse_hex_plaintext_to_key_bytes() {
        let key: [u8; 32] = core::array::from_fn(|i| i as u8);
        let hex_bytes = plaintext_hex_from_key(&key);
        let hex_str = std::str::from_utf8(&hex_bytes).unwrap();
        let decoded = hex::decode(hex_str.trim()).unwrap();
        let arr: [u8; 32] = decoded.try_into().unwrap();
        assert_eq!(arr, key);
    }

    #[test]
    fn base64_decode_ciphertext_roundtrip() {
        let fake_ct = b"some-kms-ciphertext-blob";
        let b64 = encode_ciphertext_for_sm(fake_ct);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&b64)
            .unwrap();
        assert_eq!(decoded, fake_ct);
    }
}
```

- [ ] **Step 2: Run tests — verify they pass immediately (pure logic)**

```bash
cargo test -p key-provider aws_sm_kms::tests
```

Expected: 2 tests pass.

- [ ] **Step 3: Implement AwsSmKmsKeyProvider**

Replace `crates/key-provider/src/aws_sm_kms.rs` with the full implementation:

```rust
use async_trait::async_trait;
use base64::Engine as _;
use aws_config::Region;
use aws_sdk_kms::primitives::Blob;
use zeroize::Zeroizing;

use crate::{KeyProvider, KeyProviderError, config::AwsSmKmsConfig};

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
        // ── Step 1: fetch ciphertext blob from Secrets Manager ──────────────
        let sm_cfg = aws_config::from_env()
            .region(Region::new(self.config.region.clone()))
            .load()
            .await;
        let sm = aws_sdk_secretsmanager::Client::new(&sm_cfg);

        let resp = sm
            .get_secret_value()
            .secret_id(&self.config.secret_id)
            .send()
            .await
            .map_err(|e| KeyProviderError::SecretsManager(e.to_string()))?;

        let b64_ciphertext = resp.secret_string().ok_or_else(|| {
            KeyProviderError::SecretsManager(
                "GetSecretValue returned no secret string".into(),
            )
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
        let kms_cfg = aws_config::from_env()
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

        let decrypt_resp = req
            .send()
            .await
            .map_err(|e| KeyProviderError::Kms(e.to_string()))?;

        let plaintext_blob = decrypt_resp.plaintext().ok_or_else(|| {
            KeyProviderError::Kms("KMS Decrypt returned no plaintext".into())
        })?;

        // ── Step 4: plaintext is hex(32-byte-key) → parse to [u8; 32] ───────
        let hex_str = std::str::from_utf8(plaintext_blob.as_ref()).map_err(|_| {
            KeyProviderError::ParseKey("KMS plaintext is not valid UTF-8".into())
        })?;

        let key_bytes = hex::decode(hex_str.trim())
            .map_err(|e| KeyProviderError::ParseKey(format!("KMS plaintext hex decode: {e}")))?;

        let arr: [u8; 32] = key_bytes.try_into().map_err(|_| {
            KeyProviderError::ParseKey("KMS plaintext must decode to exactly 32 bytes".into())
        })?;

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
```

- [ ] **Step 4: Run all unit tests**

```bash
cargo test -p key-provider
```

Expected: All tests pass. No AWS calls are made.

- [ ] **Step 5: Commit**

```bash
git add crates/key-provider/src/aws_sm_kms.rs
git commit -m "feat: implement AwsSmKmsKeyProvider using SM + KMS envelope decryption"
```

---

## Task 4: Add `key_provider` to `EmeraldConfig` (TDD)

**Files:**
- Modify: `cli/Cargo.toml`
- Modify: `cli/src/config.rs`

- [ ] **Step 1: Write failing deserialization test**

Open `cli/src/config.rs`. Find the bottom of the file and add:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emerald_config_defaults_to_file_key_provider() {
        let toml = r#"
moniker = "node-0"
fee_recipient = "0x0000000000000000000000000000000000000000"

[ethereum_config]
execution_authrpc_address = "http://127.0.0.1:8551"
engine_authrpc_address    = "http://127.0.0.1:8552"
jwt_token_path            = "./assets/jwt.hex"
"#;
        let cfg: EmeraldConfig = toml::from_str(toml).unwrap();
        assert!(matches!(
            cfg.key_provider,
            key_provider::KeyProviderConfig::File
        ));
    }

    #[test]
    fn emerald_config_parses_aws_sm_kms_key_provider() {
        let toml = r#"
moniker = "node-0"
fee_recipient = "0x0000000000000000000000000000000000000000"

[ethereum_config]
execution_authrpc_address = "http://127.0.0.1:8551"
engine_authrpc_address    = "http://127.0.0.1:8552"
jwt_token_path            = "./assets/jwt.hex"

[key_provider]
type       = "aws_sm_kms"
secret_id  = "emerald/mainnet/node-0/key"
region     = "ap-east-1"
kms_key_id = "alias/emerald-validator-keys"
"#;
        let cfg: EmeraldConfig = toml::from_str(toml).unwrap();
        match &cfg.key_provider {
            key_provider::KeyProviderConfig::AwsSmKms(c) => {
                assert_eq!(c.secret_id,  "emerald/mainnet/node-0/key");
                assert_eq!(c.region,     "ap-east-1");
                assert_eq!(c.kms_key_id, "alias/emerald-validator-keys");
                assert!(c.kms_region.is_none());
            }
            other => panic!("expected AwsSmKms, got {:?}", other),
        }
    }
}
```

- [ ] **Step 2: Run tests — verify they fail**

```bash
cargo test -p malachitebft-eth-cli config::tests
```

Expected: Compile error — `EmeraldConfig` has no field `key_provider`.

- [ ] **Step 3: Add `key-provider` dependency to cli crate**

In `cli/Cargo.toml`, add under `[dependencies]`:

```toml
key-provider = { path = "../crates/key-provider" }
```

- [ ] **Step 4: Add the `key_provider` field to `EmeraldConfig`**

In `cli/src/config.rs`, add this import near the top (with the other use statements):

```rust
use key_provider::KeyProviderConfig;
```

In the `EmeraldConfig` struct, add the field (position does not matter, but alongside other `#[serde(default)]` fields is clearest):

```rust
#[serde(default)]
pub key_provider: KeyProviderConfig,
```

- [ ] **Step 5: Run tests — verify they pass**

```bash
cargo test -p malachitebft-eth-cli config::tests
```

Expected: Both tests pass.

- [ ] **Step 6: Commit**

```bash
git add cli/Cargo.toml cli/src/config.rs
git commit -m "feat: add key_provider field to EmeraldConfig with File default"
```

---

## Task 5: Replace key loading in `app/src/node.rs` (TDD)

**Files:**
- Modify: `app/Cargo.toml`
- Modify: `app/src/node.rs`

Before writing any code, verify the exact `PrivateKey` constructor API:

- [ ] **Step 1: Find how PrivateKey is constructed from raw bytes**

```bash
grep -rn "fn new\|from_bytes\|from_slice\|impl PrivateKey" types/src/
```

Note the exact method signature. The k256-backed type is typically constructed via one of:
- `PrivateKey::from_bytes(&bytes)?`  
- `k256::SecretKey::from_bytes(GenericArray::from_slice(&bytes))?` then wrapped

Use the actual API you find for the implementation below.

- [ ] **Step 2: Verify whether `build_runtime()` is already async**

```bash
grep -n "async fn build_runtime\|fn build_runtime" app/src/node.rs
```

If it is already `async fn build_runtime`, no signature change is needed. If it is `fn build_runtime`, you must change it to `async fn build_runtime` — check all call sites first:

```bash
grep -rn "build_runtime" app/src/ cli/src/ engine/src/
```

Update any call sites to `.await` the result.

- [ ] **Step 3: Add `key-provider` dependency to app crate**

In `app/Cargo.toml`, under `[dependencies]`:

```toml
key-provider = { path = "../crates/key-provider" }
```

- [ ] **Step 4: Replace the key loading call at line 71 of `app/src/node.rs`**

Current code (line 71):
```rust
let private_key_file = self.load_private_key_file()?;
```

Replace with the following. First add the imports to the top of `node.rs` if not present:

```rust
use key_provider::{KeyProvider, FileKeyProvider, KeyProviderConfig};
#[cfg(feature = "aws-sm-kms")]
use key_provider::AwsSmKmsKeyProvider;
use zeroize::Zeroizing;
```

Then replace line 71:

```rust
let emerald_config = self.load_emerald_config()?;

let key_bytes: Zeroizing<[u8; 32]> = {
    let provider: Box<dyn KeyProvider> = match &emerald_config.key_provider {
        KeyProviderConfig::File => {
            Box::new(FileKeyProvider::new(&self.private_key_file))
        }
        #[cfg(feature = "aws-sm-kms")]
        KeyProviderConfig::AwsSmKms(cfg) => {
            Box::new(AwsSmKmsKeyProvider::new(cfg.clone()))
        }
    };
    provider.load_private_key().await
        .map_err(|e| eyre::eyre!("key provider error: {e}"))?
};

// Replace this with the actual PrivateKey constructor found in Step 1.
// Likely one of:
//   PrivateKey::from_bytes(&*key_bytes)?
//   PrivateKey::try_from(key_bytes.as_slice())?
let private_key = PrivateKey::from_bytes(&*key_bytes)
    .map_err(|e| eyre::eyre!("invalid private key bytes: {e}"))?;
```

> **Note:** If `build_runtime()` previously called `self.load_emerald_config()` separately later in the function, remove that duplicate call and reuse the `emerald_config` value bound above.

- [ ] **Step 5: Remove (or keep) the old `load_private_key_file` method**

The method at lines 245–248 is now dead code. Either delete it or leave it with `#[allow(dead_code)]` temporarily. The compiler will warn if it stays. Delete it:

```bash
# Verify nothing else calls it
grep -n "load_private_key_file" app/src/node.rs
```

If only the definition remains (no callers), delete those 4 lines.

- [ ] **Step 6: Build the app crate to confirm no compile errors**

```bash
cargo build -p emerald
```

Expected: Compiles successfully.

- [ ] **Step 7: Run existing app tests**

```bash
cargo test -p emerald
```

Expected: All existing tests pass (the file key loading path is unchanged for testnet).

- [ ] **Step 8: Commit**

```bash
git add app/Cargo.toml app/src/node.rs
git commit -m "feat: replace load_private_key_file with async KeyProvider in node build_runtime"
```

---

## Task 6: Create `cli/src/cmd/shared.rs` — `RuntimeFlavour`, `KeyProviderType`, `P2pOptions`

**Context:** The new `emerald generate` command needs P2P and runtime options. We put shared types in a new module. `cli/src/cmd/testnet/generate.rs` is **not touched**.

**Files:**
- Create: `cli/src/cmd/shared.rs`
- Modify: `cli/src/cmd/mod.rs`

- [ ] **Step 1: Create `cli/src/cmd/shared.rs` with tests and stub implementations**

```rust
// cli/src/cmd/shared.rs
use core::str::FromStr;
use clap::Parser;
use malachitebft_config::{
    BootstrapProtocol, RuntimeConfig, Selector, TransportProtocol,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeFlavour {
    SingleThreaded,
    MultiThreaded(usize),
}

impl FromStr for RuntimeFlavour {
    type Err = String;
    fn from_str(_s: &str) -> Result<Self, Self::Err> {
        todo!()
    }
}

impl RuntimeFlavour {
    pub fn to_runtime_config(self) -> RuntimeConfig {
        todo!()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum KeyProviderType {
    File,
    AwsSmKms,
}

#[derive(Parser, Debug, Clone, PartialEq)]
pub struct P2pOptions {
    #[clap(short, long, default_value = "single-threaded", verbatim_doc_comment)]
    pub runtime: RuntimeFlavour,
    #[clap(long, default_value = "false")]
    pub enable_discovery: bool,
    #[clap(long, default_value = "full")]
    pub bootstrap_protocol: BootstrapProtocol,
    #[clap(long, default_value = "random")]
    pub selector: Selector,
    #[clap(long, default_value = "20")]
    pub num_outbound_peers: usize,
    #[clap(long, default_value = "20")]
    pub num_inbound_peers: usize,
    #[clap(long, default_value = "5000")]
    pub ephemeral_connection_timeout_ms: u64,
    #[clap(short, long, default_value = "tcp")]
    pub transport: TransportProtocol,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multi_threaded_with_count() {
        let f: RuntimeFlavour = "multi-threaded:4".parse().unwrap();
        assert_eq!(f, RuntimeFlavour::MultiThreaded(4));
    }

    #[test]
    fn parses_single_threaded() {
        let f: RuntimeFlavour = "single-threaded".parse().unwrap();
        assert_eq!(f, RuntimeFlavour::SingleThreaded);
    }

    #[test]
    fn rejects_invalid_flavour() {
        assert!("nonsense".parse::<RuntimeFlavour>().is_err());
    }
}
```

- [ ] **Step 2: Register the module in `cli/src/cmd/mod.rs`**

Add:
```rust
pub mod shared;
```

- [ ] **Step 3: Run — verify tests fail at `todo!()`**

```bash
cargo test -p malachitebft-eth-cli cmd::shared::tests
```

Expected: 3 failures — panics at `todo!()`.

- [ ] **Step 4: Implement `FromStr` and `to_runtime_config`**

Replace the two `todo!()` bodies:

```rust
fn from_str(s: &str) -> Result<Self, Self::Err> {
    if let Some(("multi-threaded", n)) = s.split_once(':') {
        return Ok(Self::MultiThreaded(
            n.parse().map_err(|_| format!("invalid thread count: {n}"))?,
        ));
    }
    match s {
        "single-threaded" => Ok(Self::SingleThreaded),
        "multi-threaded"  => Ok(Self::MultiThreaded(0)),
        _                 => Err(format!("unknown runtime flavour: {s}")),
    }
}
```

```rust
pub fn to_runtime_config(self) -> RuntimeConfig {
    match self {
        Self::SingleThreaded   => RuntimeConfig::SingleThreaded,
        Self::MultiThreaded(n) => RuntimeConfig::MultiThreaded { worker_threads: n },
    }
}
```

- [ ] **Step 5: Run — verify tests pass**

```bash
cargo test -p malachitebft-eth-cli cmd::shared::tests
cargo build -p malachitebft-eth-cli
```

Expected: 3 tests pass, full CLI crate builds.

- [ ] **Step 6: Commit**

```bash
git add cli/src/cmd/shared.rs cli/src/cmd/mod.rs
git commit -m "feat: add shared P2pOptions, RuntimeFlavour, KeyProviderType for generate command"
```

---

## Task 7: Add `emerald generate` subcommand

**Context:** Unified `emerald generate --key-provider file|aws-sm-kms`. The `file` path generates fresh private keys and writes `priv_validator_key.json`; the `aws-sm-kms` path reads pre-existing public keys from a file and writes SM-backed `emerald.toml`. Both paths write `config.toml`, `emerald.toml`, and the consensus genesis (`emerald_genesis.json`). EVM genesis (`genesis.json` reth format) is **not** generated here — that remains a separate `emerald-utils genesis` step. `testnet generate` is **not touched**.

**Files:**
- Create: `cli/src/cmd/generate.rs`
- Modify: `cli/src/cmd/mod.rs`
- Modify: `cli/src/args.rs`

- [ ] **Step 1: Create `cli/src/cmd/generate.rs` with test stub**

```rust
// cli/src/cmd/generate.rs — test-first stub

pub(crate) enum KeyProviderSection<'a> {
    File,
    AwsSmKms {
        secret_id:  &'a str,
        region:     &'a str,
        kms_key_id: &'a str,
        kms_region: Option<&'a str>,
    },
}

pub(crate) fn make_emerald_toml_content(
    moniker: &str,
    fee_recipient: &str,
    key_provider: KeyProviderSection<'_>,
) -> String {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_provider_omits_key_provider_section() {
        let out = make_emerald_toml_content("node-0", "0x0000", KeyProviderSection::File);
        assert!(!out.contains("[key_provider]"));
        assert!(out.contains("node-0"));
    }

    #[test]
    fn aws_sm_kms_includes_key_provider_section() {
        let out = make_emerald_toml_content(
            "node-2", "0x0001",
            KeyProviderSection::AwsSmKms {
                secret_id:  "emerald/mainnet/node-2/key",
                region:     "ap-east-1",
                kms_key_id: "alias/emerald-validator-keys",
                kms_region: None,
            },
        );
        assert!(out.contains("[key_provider]"));
        assert!(out.contains("aws_sm_kms"));
        assert!(out.contains("emerald/mainnet/node-2/key"));
        assert!(!out.contains("kms_region"));
    }

    #[test]
    fn aws_sm_kms_includes_kms_region_when_set() {
        let out = make_emerald_toml_content(
            "node-0", "0x0001",
            KeyProviderSection::AwsSmKms {
                secret_id: "s", region: "ap-east-1",
                kms_key_id: "alias/key",
                kms_region: Some("us-east-1"),
            },
        );
        assert!(out.contains("kms_region = \"us-east-1\""));
    }
}
```

- [ ] **Step 2: Register module and run — verify tests fail**

Add to `cli/src/cmd/mod.rs`:
```rust
pub mod generate;
```

```bash
cargo test -p malachitebft-eth-cli cmd::generate::tests
```

Expected: 3 failures at `todo!()`.

- [ ] **Step 3: Implement `make_emerald_toml_content` and `GenerateCmd`**

Replace the stub with the full file:

```rust
use std::{fs, path::PathBuf};
use clap::Parser;
use color_eyre::eyre::{eyre, Result, WrapErr};
use malachitebft_config::LoggingConfig;

use crate::cmd::shared::{KeyProviderType, P2pOptions};

pub(crate) enum KeyProviderSection<'a> {
    File,
    AwsSmKms {
        secret_id:  &'a str,
        region:     &'a str,
        kms_key_id: &'a str,
        kms_region: Option<&'a str>,
    },
}

pub(crate) fn make_emerald_toml_content(
    moniker: &str,
    fee_recipient: &str,
    key_provider: KeyProviderSection<'_>,
) -> String {
    let kp_section = match key_provider {
        KeyProviderSection::File => String::new(),
        KeyProviderSection::AwsSmKms { secret_id, region, kms_key_id, kms_region } => {
            let kms_region_line = kms_region
                .map(|r| format!("kms_region = \"{r}\"\n"))
                .unwrap_or_default();
            format!(
                "\n[key_provider]\ntype       = \"aws_sm_kms\"\nsecret_id  = \"{secret_id}\"\nregion     = \"{region}\"\nkms_key_id = \"{kms_key_id}\"\n{kms_region_line}"
            )
        }
    };
    format!(
        "moniker       = \"{moniker}\"\nfee_recipient = \"{fee_recipient}\"\n\n[ethereum_config]\nexecution_authrpc_address = \"http://127.0.0.1:8551\"\nengine_authrpc_address    = \"http://127.0.0.1:8552\"\njwt_token_path            = \"./assets/jwt.hex\"\n{kp_section}"
    )
}

#[derive(Debug, Parser)]
pub struct GenerateCmd {
    /// Number of validator nodes to configure.
    #[clap(long)]
    pub nodes: usize,

    /// Output directory. Created if absent.
    #[clap(long, default_value = "./nodes")]
    pub home: PathBuf,

    /// Key-loading mechanism written into each node's emerald.toml.
    #[clap(long, value_enum, default_value = "file")]
    pub key_provider: KeyProviderType,

    /// One hex public key per line. Required when --key-provider aws-sm-kms.
    #[clap(long, required_if_eq("key_provider", "aws-sm-kms"))]
    pub public_keys_file: Option<PathBuf>,

    /// SM secret ID prefix. Node N gets "{prefix}/node-N/key".
    #[clap(long, default_value = "emerald/mainnet")]
    pub sm_secret_prefix: String,

    /// AWS region for Secrets Manager. Required when --key-provider aws-sm-kms.
    #[clap(long, required_if_eq("key_provider", "aws-sm-kms"))]
    pub sm_region: Option<String>,

    /// KMS key ID or alias. Required when --key-provider aws-sm-kms.
    #[clap(long, required_if_eq("key_provider", "aws-sm-kms"))]
    pub kms_key_id: Option<String>,

    /// KMS region if different from SM region.
    #[clap(long)]
    pub kms_region: Option<String>,

    /// Chain ID.
    #[clap(long, default_value_t = 12345)]
    pub chain_id: u64,

    #[command(flatten)]
    pub p2p: P2pOptions,
}

impl GenerateCmd {
    pub fn run<N>(&self, node: &N, logging: LoggingConfig) -> Result<()>
    where
        N: crate::new::CanGeneratePrivateKey
            + crate::new::CanMakeGenesis
            + crate::new::CanMakePrivateKeyFile,
        malachitebft_app::node::PrivateKey<N::Context>: serde::de::DeserializeOwned,
    {
        generate_all(self, node, logging)
    }
}

fn generate_all<N>(cmd: &GenerateCmd, node: &N, logging: LoggingConfig) -> Result<()>
where
    N: crate::new::CanGeneratePrivateKey
        + crate::new::CanMakeGenesis
        + crate::new::CanMakePrivateKeyFile,
    malachitebft_app::node::PrivateKey<N::Context>: serde::de::DeserializeOwned,
{
    let assets_dir = cmd.home.join("assets");
    fs::create_dir_all(&assets_dir).wrap_err("creating assets dir")?;
    for i in 0..cmd.nodes {
        fs::create_dir_all(cmd.home.join(i.to_string()).join("config"))
            .wrap_err_with(|| format!("creating node {i} config dir"))?;
    }

    match cmd.key_provider {
        KeyProviderType::File => {
            let private_keys = crate::new::generate_private_keys(node, cmd.nodes, false);
            let public_keys  = private_keys.iter().map(|pk| node.get_public_key(pk)).collect();
            let genesis       = crate::new::generate_genesis(node, public_keys, false);
            crate::file::save_genesis(node, &assets_dir.join("emerald_genesis.json"), &genesis)?;

            for (i, private_key) in private_keys.iter().enumerate() {
                let config_dir = cmd.home.join(i.to_string()).join("config");
                let moniker     = format!("node-{i}");

                crate::file::save_config(
                    &config_dir.join("config.toml"),
                    &crate::new::generate_config(
                        i, cmd.nodes,
                        cmd.p2p.runtime.to_runtime_config(),
                        cmd.p2p.enable_discovery,
                        cmd.p2p.bootstrap_protocol,
                        cmd.p2p.selector,
                        cmd.p2p.num_outbound_peers,
                        cmd.p2p.num_inbound_peers,
                        cmd.p2p.ephemeral_connection_timeout_ms,
                        cmd.p2p.transport,
                        logging,
                        moniker.clone(),
                    ),
                )?;

                fs::write(
                    config_dir.join("emerald.toml"),
                    make_emerald_toml_content(
                        &moniker,
                        "0x0000000000000000000000000000000000000000",
                        KeyProviderSection::File,
                    ),
                )
                .wrap_err_with(|| format!("writing emerald.toml for node {i}"))?;

                let priv_key_file = node.make_private_key_file(private_key.clone());
                crate::file::save_priv_validator_key(
                    node,
                    &config_dir.join("priv_validator_key.json"),
                    &priv_key_file,
                )?;

                tracing::info!("generated config for node {i}");
            }
        }

        KeyProviderType::AwsSmKms => {
            let keys_file    = cmd.public_keys_file.as_ref().unwrap(); // guaranteed by clap
            let keys_content = fs::read_to_string(keys_file).wrap_err("reading public-keys-file")?;
            let public_keys_hex: Vec<String> = keys_content
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect();

            if public_keys_hex.len() != cmd.nodes {
                return Err(eyre!(
                    "--nodes {} but {} keys in file",
                    cmd.nodes,
                    public_keys_hex.len()
                ));
            }

            // Parse hex public keys into the node's PublicKey type.
            // Find the exact method with:
            //   grep -rn "public_key.*hex\|parse_public_key\|from_hex" cli/src/ types/src/
            let public_keys = public_keys_hex
                .iter()
                .enumerate()
                .map(|(i, hex)| {
                    node.public_key_from_hex(hex)
                        .wrap_err_with(|| format!("parsing public key at line {i}"))
                })
                .collect::<Result<Vec<_>>>()?;

            let genesis = crate::new::generate_genesis(node, public_keys, false);
            crate::file::save_genesis(node, &assets_dir.join("emerald_genesis.json"), &genesis)?;

            let sm_region  = cmd.sm_region.as_deref().unwrap();   // guaranteed by clap
            let kms_key_id = cmd.kms_key_id.as_deref().unwrap();  // guaranteed by clap

            for i in 0..cmd.nodes {
                let config_dir = cmd.home.join(i.to_string()).join("config");
                let moniker     = format!("node-{i}");
                let secret_id   = format!("{}/node-{i}/key", cmd.sm_secret_prefix);

                crate::file::save_config(
                    &config_dir.join("config.toml"),
                    &crate::new::generate_config(
                        i, cmd.nodes,
                        cmd.p2p.runtime.to_runtime_config(),
                        cmd.p2p.enable_discovery,
                        cmd.p2p.bootstrap_protocol,
                        cmd.p2p.selector,
                        cmd.p2p.num_outbound_peers,
                        cmd.p2p.num_inbound_peers,
                        cmd.p2p.ephemeral_connection_timeout_ms,
                        cmd.p2p.transport,
                        logging,
                        moniker.clone(),
                    ),
                )?;

                fs::write(
                    config_dir.join("emerald.toml"),
                    make_emerald_toml_content(
                        &moniker,
                        "0x0000000000000000000000000000000000000000",
                        KeyProviderSection::AwsSmKms {
                            secret_id:  &secret_id,
                            region:     sm_region,
                            kms_key_id,
                            kms_region: cmd.kms_region.as_deref(),
                        },
                    ),
                )
                .wrap_err_with(|| format!("writing emerald.toml for node {i}"))?;

                tracing::info!("generated config for node {i}");
            }
        }
    }

    println!("configs written to {}", cmd.home.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_provider_omits_key_provider_section() {
        let out = make_emerald_toml_content("node-0", "0x0000", KeyProviderSection::File);
        assert!(!out.contains("[key_provider]"));
        assert!(out.contains("node-0"));
    }

    #[test]
    fn aws_sm_kms_includes_key_provider_section() {
        let out = make_emerald_toml_content(
            "node-2", "0x0001",
            KeyProviderSection::AwsSmKms {
                secret_id:  "emerald/mainnet/node-2/key",
                region:     "ap-east-1",
                kms_key_id: "alias/emerald-validator-keys",
                kms_region: None,
            },
        );
        assert!(out.contains("[key_provider]"));
        assert!(out.contains("aws_sm_kms"));
        assert!(out.contains("emerald/mainnet/node-2/key"));
        assert!(!out.contains("kms_region"));
    }

    #[test]
    fn aws_sm_kms_includes_kms_region_when_set() {
        let out = make_emerald_toml_content(
            "node-0", "0x0001",
            KeyProviderSection::AwsSmKms {
                secret_id: "s", region: "ap-east-1",
                kms_key_id: "alias/key",
                kms_region: Some("us-east-1"),
            },
        );
        assert!(out.contains("kms_region = \"us-east-1\""));
    }
}
```

- [ ] **Step 4: Run — verify tests pass**

```bash
cargo test -p malachitebft-eth-cli cmd::generate::tests
cargo build -p malachitebft-eth-cli
```

Expected: 3 tests pass, full CLI builds.

- [ ] **Step 5: Wire `GenerateCmd` into the top-level CLI**

Open `cli/src/args.rs`. Add to the `Commands` enum:

```rust
/// Generate per-node configs (file or AWS SM+KMS key provider).
#[command(arg_required_else_help = true)]
Generate(crate::cmd::generate::GenerateCmd),
```

Find the `match &self.command` block and add:

```rust
Commands::Generate(cmd) => cmd.run(node, logging),
```

> Verify existing arm patterns: `grep -n "Commands::" cli/src/args.rs`

> **`public_key_from_hex`:** If no such method exists on `Node`, find the actual API:
> ```bash
> grep -rn "public_key.*hex\|parse_public_key\|PublicKey.*from" cli/src/ types/src/
> ```

> **Trait bounds on `GenerateCmd::run`:** Verify exact trait names:
> ```bash
> grep -rn "pub trait Can" cli/src/new.rs app/src/
> ```

- [ ] **Step 6: Build and smoke-test help**

```bash
cargo build -p malachitebft-eth-cli
cargo run   -p malachitebft-eth-cli -- generate --help
```

Expected: Shows `--nodes`, `--key-provider`, `--public-keys-file`, `--sm-region`, `--kms-key-id`, and all P2P flags.

- [ ] **Step 7: Commit**

```bash
git add cli/src/cmd/generate.rs cli/src/cmd/mod.rs cli/src/args.rs
git commit -m "feat: add 'emerald generate' subcommand with --key-provider file|aws-sm-kms"
```

---

## Task 8: LocalStack integration test for `AwsSmKmsKeyProvider`

**Context:** These tests require LocalStack running locally. They validate the full SM→KMS→key roundtrip against a local mock of AWS APIs.

**Files:**
- Create: `crates/key-provider/tests/localstack_integration.rs`

- [ ] **Step 1: Start LocalStack (run once before the test)**

```bash
docker run -d --name localstack -p 4566:4566 localstack/localstack
```

- [ ] **Step 2: Create the integration test file**

```rust
// crates/key-provider/tests/localstack_integration.rs
// Run with:
//   AWS_DEFAULT_REGION=ap-east-1 \
//   AWS_ENDPOINT_URL=http://localhost:4566 \
//   AWS_ACCESS_KEY_ID=test \
//   AWS_SECRET_ACCESS_KEY=test \
//   cargo test -p key-provider --test localstack_integration -- --ignored

use base64::Engine as _;
use key_provider::{AwsSmKmsKeyProvider, KeyProvider, config::AwsSmKmsConfig};

const REGION: &str = "ap-east-1";

async fn localstack_client_config() -> aws_config::SdkConfig {
    aws_config::from_env()
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
    let sm  = aws_sdk_secretsmanager::Client::new(cfg);

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
```

- [ ] **Step 3: Add dev-dependencies to `crates/key-provider/Cargo.toml`**

```toml
[dev-dependencies]
tokio    = { workspace = true, features = ["rt-multi-thread", "macros"] }
tempfile = "3"
aws-config             = { workspace = true }
aws-sdk-kms            = { workspace = true }
aws-sdk-secretsmanager = { workspace = true }
```

- [ ] **Step 4: Run with LocalStack active**

```bash
AWS_DEFAULT_REGION=ap-east-1 \
AWS_ENDPOINT_URL=http://localhost:4566 \
AWS_ACCESS_KEY_ID=test \
AWS_SECRET_ACCESS_KEY=test \
cargo test -p key-provider --test localstack_integration -- --ignored
```

Expected: `loads_private_key_from_localstack_sm_and_kms` passes.

- [ ] **Step 5: Commit**

```bash
git add crates/key-provider/tests/ crates/key-provider/Cargo.toml
git commit -m "test: add LocalStack integration test for AwsSmKmsKeyProvider"
```

---

## Self-Review

### Spec coverage

| ADR requirement | Task |
|---|---|
| `KeyProvider` trait with `File` and `AwsSmKms` implementations | Tasks 1–3 |
| `KeyProviderConfig` enum selectable via `emerald.toml` | Task 4 |
| Single call-site change in `app/src/node.rs` | Task 5 |
| Testnet workflows unchanged (`File` is `#[serde(default)]`) | Tasks 4–5 |
| `cli/src/cmd/testnet/generate.rs` not modified | Task 6 (creates `shared.rs` only) |
| `emerald generate` with `--key-provider file\|aws-sm-kms` | Task 7 |
| Both paths write `config.toml`, `emerald.toml`, `emerald_genesis.json` | Task 7 `generate_all` |
| `file` path generates fresh keys + `priv_validator_key.json` | Task 7 `KeyProviderType::File` arm |
| `aws-sm-kms` path reads public keys from file, no private key gen | Task 7 `KeyProviderType::AwsSmKms` arm |
| EVM genesis NOT generated by `generate` command | Task 7 (separate `emerald-utils genesis` step) |
| `Zeroizing<T>` for in-memory key material | Tasks 2–3 |
| LocalStack integration test | Task 8 |

### Notes for implementor

- **`PrivateKey` constructor (Task 5):** Verify the exact method to build `PrivateKey` from `[u8; 32]` by reading `types/src/secp256k1.rs`. The plan uses `PrivateKey::from_bytes(&*key_bytes)` as the most likely API; adjust if it differs.
- **`build_runtime()` async (Task 5):** If not already async, check all call sites before changing the signature: `grep -rn "build_runtime" app/src/ cli/src/ engine/src/`
- **`generate_config` signature (Task 7):** Confirmed 12-parameter function in `cli/src/new.rs`. Verify with `grep -n "pub fn generate_config" cli/src/new.rs` before calling.
- **`public_key_from_hex` (Task 7 aws-sm-kms arm):** Method name is speculative — find the actual API: `grep -rn "public_key.*hex\|parse_public_key\|from_hex" cli/src/ types/src/`
- **Trait bounds on `GenerateCmd::run` (Task 7):** Confirm exact names with `grep -rn "pub trait Can" cli/src/new.rs app/src/`
