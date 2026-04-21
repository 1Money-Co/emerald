---
adr: 0002
title: Mainnet Validator Key Management
status: Proposed
date: 2026-04-20
authors: []
supersedes: []
superseded_by: []
---

## Context

Each Emerald validator node requires a secp256k1 private key to sign consensus
messages (votes, proposals). Currently the key is stored as plaintext
base64-encoded JSON on disk:

```json
{ "type": "tendermint/PrivKeySecp256k1", "value": "<base64-encoded-32-bytes>" }
```

This is acceptable for testnet but creates unacceptable risk for mainnet:

- Any process with read access to the file can export the key.
- A root-level compromise of the host exposes the key in memory and on disk.
- There is no audit trail for key access.
- Key rotation has no tooling support.

A second related gap concerns genesis funding: the `emerald-utils genesis`
command has no mechanism to pre-fund arbitrary addresses in mainnet mode
(i.e. without `--devnet`). This means the PoA owner and other operational
accounts start with zero balance and cannot issue any transactions, including
validator management calls.

## Decision

### 1. Modular Key Provider

Introduce a `KeyProvider` trait and a new `emerald-key-provider` crate. The
provider is selected via `emerald.toml` and defaults to the existing file-based
behaviour so that all testnet workflows remain unchanged.

```
emerald-key-provider/
├── src/
│   ├── lib.rs          # pub trait KeyProvider; pub use …
│   ├── config.rs       # KeyProviderConfig enum (File | AwsSmKms)
│   ├── error.rs        # KeyProviderError
│   ├── file.rs         # FileKeyProvider  (existing behaviour)
│   └── aws_sm_kms.rs   # AwsSmKmsKeyProvider (new)
```

`app/src/node.rs` is the only application file that changes: one call site in
`build_runtime()` is replaced with a trait-dispatched async call.

**Configuration** (`emerald.toml`):

```toml
# Default — file provider, no change from current behaviour
# [key_provider]
# type = "file"

# AWS Secrets Manager + KMS
[key_provider]
type            = "aws_sm_kms"
secret_id       = "emerald/mainnet/node-0/key"
region          = "ap-east-1"
kms_key_id      = "alias/emerald-validator-keys"
# kms_region    = "ap-east-1"   # defaults to same as SM region
# kms_encryption_context = { "network" = "mainnet", "node" = "node-0" }
```

### 2. AWS SM + KMS Envelope Encryption

The private key material stored in AWS Secrets Manager is **never plaintext**.
The flow follows the same envelope-encryption pattern used by `om-keystore-sm`
in the L1 client:

```
Provisioning (offline, one-time per node):
  plaintext_hex = hex(private_key_bytes_32)
  ciphertext    = KMS.Encrypt(key_id, plaintext_hex)
  SM.CreateSecret(secret_id, base64(ciphertext))

Node startup (runtime):
  b64_ciphertext = SM.GetSecretValue(secret_id)
  plaintext_hex  = KMS.Decrypt(base64_decode(b64_ciphertext))
  private_key    = parse_hex(plaintext_hex)   # kept in memory only
```

The decrypted key is wrapped in `Zeroizing<T>` so it is zeroed on drop.
After startup the private key exists only in process memory; it is never
written back to disk.

### 3. Genesis Alloc for Mainnet Operational Accounts

`emerald-utils genesis` gains a repeatable `--alloc <ADDRESS:BALANCE_ETH>`
flag. This allows the PoA owner, relayer owner, and any other operational
accounts to be pre-funded at genesis without enabling `--devnet`.

```bash
emerald genesis \
  --public-keys-file validators.txt \
  --poa-owner-address  0xAAA... \
  --alloc 0xAAA...:100 \   # PoA owner
  --alloc 0xBBB...:50  \   # relayer owner
  --chain-id 12345
```

This change is already implemented (see `utils/src/genesis.rs`).

### 4. `emerald mainnet generate` Command

A new CLI subcommand generates per-node configuration files for a mainnet
deployment without embedding any private keys. It requires only public keys
and produces configs ready for SM-based key loading.

```bash
emerald mainnet generate \
  --public-keys-file    validator_public_keys.txt \
  --nodes               13 \
  --home                ./mainnet-nodes \
  --key-provider        aws-sm-kms \
  --sm-secret-prefix    "emerald/mainnet" \
  --sm-region           ap-east-1 \
  --kms-key-id          alias/emerald-validator-keys \
  --poa-owner-address   0xAAA... \
  --chain-id            12345
```

Output layout:

```
mainnet-nodes/
├── 0/config/
│   ├── emerald.toml        # [key_provider] secret_id = "emerald/mainnet/node-0/key"
│   └── config.toml         # Malachite consensus config
├── 1/config/
│   ├── emerald.toml
│   └── config.toml
├── …
└── assets/
    ├── genesis.json
    └── emerald_genesis.json
```

## Consequences

**Positive**

- Validator private keys never exist as plaintext on disk in mainnet deployments.
- AWS CloudTrail provides a full audit log of every key access.
- IAM instance roles scope each node to its own secret; no cross-node access.
- Backward compatible: the default `file` provider preserves all existing
  testnet and devnet workflows without any configuration change.
- Genesis funding of operational accounts is now a first-class CLI operation,
  eliminating the need to hand-edit `genesis.json`.

**Negative / Risks**

- Node startup depends on AWS SM and KMS availability. A misconfigured IAM
  role or a regional AWS outage will prevent startup.
- Provisioning keys to SM is a manual operational step; there is no automated
  ceremony. Runbooks must cover this.

**Follow-up**

- Long-term: evaluate `malachitebft-signing-remote` (custom socket-based
  signing delegation) to eliminate in-memory key exposure entirely. Requires
  upstream Malachite collaboration and is tracked separately.

## Alternatives Considered

- **File provider only with OS hardening**: Relies entirely on filesystem
  permissions and OS-level controls. Provides no audit trail, no key isolation
  from the validator process, and no path to HSM support. Rejected for mainnet.

- **tmkms (Tendermint Key Management System)**: Purpose-built for
  Tendermint-compatible validators. Supports YubiHSM2 backends and provides
  double-sign protection at the protocol level. Rejected for the immediate
  term because Emerald's consensus types (malachitebft-proto) are incompatible
  with tmkms's tendermint-proto type system; integration requires implementing
  a custom socket protocol from scratch (~4–6 weeks). Remains the recommended
  long-term target.

- **HashiCorp Vault**: Equivalent security properties to AWS SM + KMS.
  Rejected because the broader infrastructure is AWS-native and Vault would
  introduce an additional operational dependency without benefit.

## References

- `om-keystore-sm` crate in `l1client4/crates/om-keystore-sm` — reference
  implementation of the SM + KMS envelope encryption pattern used here.
- `utils/src/genesis.rs` — `--alloc` flag implementation.
- `app/src/node.rs` — current key loading entry point (`load_private_key_file`).
