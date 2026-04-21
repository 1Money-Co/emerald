# Validator Key Management

This page covers how validator signing keys are managed in production, why
plaintext key files are not acceptable for mainnet, and the complete
implementation plan for secure key storage using AWS Secrets Manager and KMS.

For the architectural decision record see
[ADR-0002](../../../architecture/adr-0002-mainnet-validator-key-management.md).

---

## Background: Current Key Model

Every Emerald validator node holds a secp256k1 private key used to sign
consensus messages (prevote, precommit, proposal). Currently the key is stored
as a plaintext, base64-encoded JSON file:

```
{home}/config/priv_validator_key.json
{"type":"tendermint/PrivKeySecp256k1","value":"<base64-32-bytes>"}
```

This is fine for local testnets. For mainnet it creates several risks:

| Risk | Impact |
|---|---|
| Host compromise (root) | Key exported from disk or process memory |
| Misconfigured file permissions | Any local process can read the key |
| No audit trail | Impossible to know if/when the key was accessed |
| No rotation tooling | Replacing a key requires a validator set change |

---

## Design: Modular Key Provider

The solution introduces a `KeyProvider` trait with two implementations,
selected by a single config field. The default is the existing file provider,
so **all testnet and devnet workflows are entirely unchanged**.

```
┌─────────────────────────────────────────────┐
│  emerald-key-provider crate                  │
│                                              │
│  trait KeyProvider {                         │
│    async fn load_private_key() -> PrivateKey │
│  }                                           │
│                                              │
│  FileKeyProvider       ← default             │
│  AwsSmKmsKeyProvider   ← mainnet             │
└─────────────────────────────────────────────┘
         ↑ used by
┌─────────────────────────────────────────────┐
│  app/src/node.rs  (one call-site change)     │
│  build_runtime() → key_provider.load().await │
└─────────────────────────────────────────────┘
```

### Key Principle: Config Generation Never Touches Private Keys

Genesis and per-node config generation use **only public keys**. Private keys
are not needed and not accessed during any `emerald genesis`, `emerald testnet
generate`, or `emerald mainnet generate` operation. This means:

- Testnet key generation: unchanged.
- Mainnet key provisioning to SM: a separate, one-time operational step
  performed by each validator independently.

---

## AWS SM + KMS: How It Works

The private key is stored in AWS Secrets Manager as a KMS-encrypted blob.
The key material is never stored in plaintext anywhere.

### Storage Format

```
Stored in SM:
  base64( KMS.Encrypt( hex(private_key_32_bytes) ) )
```

### Runtime Load Sequence

```
1. SM.GetSecretValue(secret_id)      → base64_ciphertext
2. base64.decode(base64_ciphertext)  → kms_ciphertext_blob
3. KMS.Decrypt(kms_ciphertext_blob)  → plaintext_hex  (32 bytes)
4. parse_hex(plaintext_hex)          → PrivateKey      (in memory only)
5. Zeroizing wrapper ensures memory  → zeroed on drop
```

After startup the key exists only in process memory. It is never written back
to disk.

### IAM Permissions Required

Each validator node's EC2 instance role needs:

```json
{
  "Effect": "Allow",
  "Action": ["secretsmanager:GetSecretValue"],
  "Resource": "arn:aws:secretsmanager:<region>:<account>:secret:emerald/mainnet/node-N/*"
},
{
  "Effect": "Allow",
  "Action": ["kms:Decrypt"],
  "Resource": "arn:aws:kms:<region>:<account>:key/<key-id>",
  "Condition": {
    "StringEquals": {
      "kms:EncryptionContext:network": "mainnet",
      "kms:EncryptionContext:node": "node-N"
    }
  }
}
```

The KMS encryption context (`network`, `node`) binds each ciphertext to a
specific node — a stolen ciphertext from node-0 cannot be decrypted using
node-1's IAM role.

---

## Configuration

### `emerald.toml` — File Provider (default, testnet)

```toml
# No key_provider section needed. File provider is the default.
# The node reads from {home}/config/priv_validator_key.json as before.
```

### `emerald.toml` — AWS SM + KMS Provider (mainnet)

```toml
[key_provider]
type      = "aws_sm_kms"
secret_id = "emerald/mainnet/node-0/key"
region    = "ap-east-1"
kms_key_id = "alias/emerald-validator-keys"

# Optional: restrict decryption to specific context
# kms_encryption_context = { "network" = "mainnet", "node" = "node-0" }

# Optional: KMS in a different region from SM
# kms_region = "us-east-1"
```

---

## Genesis: Pre-Funding Operational Accounts

In mainnet mode (without `--devnet`), `emerald-utils genesis` previously
produced a genesis with zero-balance EOA accounts, making it impossible for
the PoA owner or relayer to issue any transaction.

The `--alloc` flag resolves this:

```bash
emerald genesis \
  --public-keys-file validator_public_keys.txt \
  --poa-owner-address  0xAAAA... \
  --alloc 0xAAAA...:100 \    # PoA owner  — 100 ETH
  --alloc 0xBBBB...:50  \    # Relayer owner — 50 ETH
  --chain-id 12345
```

`--alloc` is repeatable. Balances are in whole ETH units. Without `--alloc`
the genesis contains only the three system contracts
(`0x2000`, `0x2001`, `0x000F3df6…`); all EOA balances are zero.

---

## Mainnet 13-Node Deployment Workflow

### Overview

```
Coordinator                          Each Validator (×13)
──────────────────────────────────   ──────────────────────────────
                                     1. emerald init --home /home/N
                                        → generates priv_validator_key.json
                                     2. emerald show-pubkey …
                                        → sends pubkey to coordinator
3. Collect 13 public keys
4. emerald mainnet generate …
   → generates per-node configs
   → generates shared genesis
5. Distribute configs + genesis      6. Receive config + genesis
                                     7. KMS encrypt private key
                                     8. aws secretsmanager create-secret
                                     9. emerald start --home /home/N
```

### Step 1 — Each Validator Generates Their Key

Executed independently on each validator's own infrastructure:

```bash
emerald init --home /path/to/home
emerald show-pubkey /path/to/home/config/priv_validator_key.json
# Output: 0xd8620dd4…  ← send this to coordinator
```

The private key file never leaves the validator's machine.

### Step 2 — Coordinator Generates Configs and Genesis

After collecting all 13 public keys:

```bash
# One command generates everything
emerald mainnet generate \
  --public-keys-file    validator_public_keys.txt \
  --nodes               13 \
  --home                ./mainnet-nodes \
  --key-provider        aws-sm-kms \
  --sm-secret-prefix    "emerald/mainnet" \
  --sm-region           ap-east-1 \
  --kms-key-id          alias/emerald-validator-keys \
  --poa-owner-address   0xAAAA... \
  --alloc               0xAAAA...:100 \
  --alloc               0xBBBB...:50  \
  --chain-id            12345
```

Output:

```
mainnet-nodes/
├── 0/config/
│   ├── emerald.toml        # key_provider → "emerald/mainnet/node-0/key"
│   └── config.toml         # Malachite p2p / consensus config
├── 1/config/
│   ├── emerald.toml        # key_provider → "emerald/mainnet/node-1/key"
│   └── config.toml
├── …
└── assets/
    ├── genesis.json          # EVM genesis (shared by all nodes)
    └── emerald_genesis.json  # Consensus genesis (shared by all nodes)
```

Distribute `mainnet-nodes/N/config/` and `mainnet-nodes/assets/` to the
corresponding validator.

### Step 3 — Each Validator Provisions Their Key to SM

Executed independently on each validator's machine:

```bash
# Extract the raw 32-byte private key as hex
PRIVKEY_HEX=$(cat /path/to/home/config/priv_validator_key.json \
  | python3 -c "
import sys, json, base64, binascii
d = json.load(sys.stdin)
print(binascii.hexlify(base64.b64decode(d['value'])).decode())
")

# Encrypt with KMS
CIPHERTEXT=$(aws kms encrypt \
  --key-id alias/emerald-validator-keys \
  --region ap-east-1 \
  --plaintext "$PRIVKEY_HEX" \
  --encryption-context network=mainnet,node=node-N \
  --query CiphertextBlob \
  --output text)

# Store in Secrets Manager
aws secretsmanager create-secret \
  --name "emerald/mainnet/node-N/key" \
  --region ap-east-1 \
  --secret-string "$CIPHERTEXT"
```

After this step the local `priv_validator_key.json` can be kept as a
cold backup (stored offline) or deleted from the live machine.

### Step 4 — Start the Node

```bash
emerald start \
  --home        /path/to/home \
  --config      /path/to/mainnet-nodes/N/config/config.toml \
  --emerald-config /path/to/mainnet-nodes/N/config/emerald.toml
```

On startup the node:

1. Reads `emerald.toml` → finds `key_provider.type = "aws_sm_kms"`
2. Calls `SM.GetSecretValue("emerald/mainnet/node-N/key")`
3. Calls `KMS.Decrypt(ciphertext)` → plaintext hex
4. Parses hex → `PrivateKey` in memory
5. Proceeds with normal consensus startup

---

## Implementation Plan

### Crate: `emerald-key-provider` (new)

| File | Contents |
|---|---|
| `src/lib.rs` | `pub trait KeyProvider`; re-exports |
| `src/config.rs` | `KeyProviderConfig` enum (`File` \| `AwsSmKms`) |
| `src/error.rs` | `KeyProviderError` |
| `src/file.rs` | `FileKeyProvider` — wraps existing `fs::read_to_string` logic |
| `src/aws_sm_kms.rs` | `AwsSmKmsKeyProvider` — SM fetch + KMS decrypt + hex parse |

Models `om-keystore-sm` (`l1client4/crates/om-keystore-sm`): same
`Zeroizing<T>` pattern, same AWS SDK version (`aws-sdk-secretsmanager = "1"`),
same error structure.

### Changes to Existing Files

| File | Change |
|---|---|
| `cli/src/config.rs` | Add `key_provider: KeyProviderConfig` field to `EmeraldConfig` with `#[serde(default)]` |
| `app/src/node.rs` | Replace `self.load_private_key_file()` at line 71 with `self.key_provider().load_private_key().await?` |
| `app/Cargo.toml` | Add `emerald-key-provider = { workspace = true }` |
| `Cargo.toml` (workspace) | Register new crate; add `aws-sdk-secretsmanager`, `aws-config` |
| `cli/src/cmd/` | Add `mainnet/generate.rs` subcommand |

### Work Estimate

| Work Item | Estimate |
|---|---|
| `emerald-key-provider` crate (trait + File + AwsSmKms) | 2 days |
| `EmeraldConfig` extension + `node.rs` call-site change | 0.5 days |
| `emerald mainnet generate` CLI command | 2 days |
| Tests (unit + LocalStack integration) | 2 days |
| Cargo workspace wiring | 0.5 days |
| **Total** | **~7 days** |

---

## Relation to Testnet Workflows

No testnet workflow changes. The table below summarises the impact:

| Operation | Testnet | Mainnet |
|---|---|---|
| `emerald init` | Generates key to file | Generates key to file (same) |
| `emerald show-pubkey` | Reads file, prints pubkey | Reads file, prints pubkey (same) |
| `emerald testnet generate` | Writes per-node configs with file provider | Unchanged |
| `emerald-utils genesis` | `--devnet` pre-funds 10 addresses | `--alloc` pre-funds specified addresses |
| `emerald mainnet generate` | N/A | New: generates SM-backed configs |
| `emerald start` | Reads `priv_validator_key.json` | Reads from SM via `[key_provider]` |

---

## Future: Remote Signing (tmkms)

The AWS SM + KMS approach eliminates plaintext keys at rest but the decrypted
key still exists in process memory. The long-term target is remote signing via
a dedicated signer process (analogous to tmkms), where the key never enters
the validator node's address space.

This requires implementing a custom socket protocol adapting Malachite's
`SigningProvider` trait to a remote signer, since tmkms's built-in privval
protocol uses tendermint-proto types that are incompatible with
malachitebft-proto. Estimated effort: 4–6 weeks. Tracked as a separate
initiative.
