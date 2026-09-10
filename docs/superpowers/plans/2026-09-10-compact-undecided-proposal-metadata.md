# Compact Undecided Proposal Metadata Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove execution payload bytes from round-specific proposal rows while preserving full proposal reads,
atomic migration, and the approved N -> N-1 -> N compatibility window.

**Architecture:** Keep the existing `undecided_values` table and protobuf envelope, but encode only the eight-byte
`ValueId` in its nested value. A focused storage codec distinguishes compact and legacy-full records; store reads
hydrate compact metadata from the verified shared payload, startup rewrites legacy rows atomically, and re-upgrade
recovery repairs ID-only decided values created by N-1.

**Tech Stack:** Rust, redb 2.6.3, Prost protobuf, Bytes, Malachite, Tokio, Cargo Nextest

**Spec:** `docs/superpowers/specs/2026-09-10-compact-undecided-proposal-metadata-design.md`

## Global Constraints

- Keep `undecided_values` keyed by `(height, round, value_id)` and `undecided_block_data_v2` keyed by
  `(height, value_id)`.
- Do not change the network `ProtobufCodec` or weaken `Value::from_proto`; ID-only decoding is storage-specific.
- Preserve proposer, round, valid round, validity, and first-write-wins behavior exactly.
- Resolve v2 first and use only the exact-round legacy fallback during the compatibility release.
- Compare payload bytes as well as the 64-bit `ValueId`; mismatches fail closed without mutation.
- Keep the legacy block-data shadow for this release. Its removal and physical compaction remain tracked by
  1Money-Co/1money-interoperability-protocol#325.
- Do not log payload bytes or claim that deterministic tests are a mixed-binary release qualification run.
- Keep Markdown lines at or below 120 columns and do not modify `Cargo.lock`.

---

### Task 1: Storage-only compact proposal codec

**Files:**

- Create: `app/src/store/proposal_metadata.rs`
- Modify: `app/src/store.rs:1-25`
- Test: `app/src/store/proposal_metadata.rs`

**Interfaces:**

- Produces `StoredProposalMetadata::from_proposal(&ProposedValue<EmeraldContext>)`.
- Produces `StoredProposalMetadata::encode() -> Result<Bytes, ProtoError>`.
- Produces `decode_stored_proposal(Bytes) -> Result<DecodedStoredProposal, ProtoError>`.
- Produces `StoredProposalMetadata::hydrate(Bytes) -> ProposedValue<EmeraldContext>`.
- Produces `DecodedStoredProposal { metadata, embedded_payload }`, where `embedded_payload` is `None` for compact
  records and `Some` only for legacy-full records.

- [x] **Step 1: Add the module and write failing compact-codec tests**

Create `app/src/store/proposal_metadata.rs` with tests that express the desired API before defining it:

```rust
#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use malachitebft_app_channel::app::types::codec::Codec;
    use malachitebft_app_channel::app::types::core::{Round, Validity};
    use malachitebft_app_channel::app::types::ProposedValue;
    use malachitebft_eth_types::{Address, EmeraldContext, Height, Value};
    use prost::Message;

    use super::*;

    fn proposal(payload: Bytes) -> ProposedValue<EmeraldContext> {
        ProposedValue {
            height: Height::new(41),
            round: Round::new(3),
            valid_round: Round::new(1),
            proposer: Address::new([7; 20]),
            value: Value::new(payload),
            validity: Validity::Valid,
        }
    }

    #[test]
    fn compact_proposal_metadata_encoding_omits_payload_and_hydrates() {
        let payload = Bytes::from(vec![0xAB; 1024 * 1024]);
        let proposal = proposal(payload.clone());

        let encoded = StoredProposalMetadata::from_proposal(&proposal)
            .encode()
            .unwrap();
        let proto = malachitebft_eth_types::proto::ProposedValue::decode(encoded.clone()).unwrap();
        assert_eq!(proto.value.unwrap().value.unwrap().len(), u64::BITS as usize / 8);
        assert!(encoded.len() < 128);

        let decoded = decode_stored_proposal(encoded).unwrap();
        assert!(decoded.embedded_payload.is_none());
        assert_eq!(decoded.metadata.hydrate(payload), proposal);
    }

    #[test]
    fn compact_proposal_metadata_decodes_legacy_full_records() {
        let payload = Bytes::from_static(b"legacy-full-payload");
        let proposal = proposal(payload.clone());
        let encoded = malachitebft_eth_types::codec::proto::ProtobufCodec
            .encode(&proposal)
            .unwrap();

        let decoded = decode_stored_proposal(encoded).unwrap();
        assert_eq!(decoded.metadata, StoredProposalMetadata::from_proposal(&proposal));
        assert_eq!(decoded.embedded_payload, Some(payload));
    }
}
```

Add `mod proposal_metadata;` next to `mod keys;` in `app/src/store.rs`. Do not yet add production definitions.

- [x] **Step 2: Run the codec tests and verify red**

Run:

```bash
cargo test -p emerald compact_proposal_metadata_ -- --nocapture
```

Expected: compilation fails because `StoredProposalMetadata` and `decode_stored_proposal` do not exist.

- [x] **Step 3: Implement the compact and legacy-full decoders**

Define the production types and functions in `proposal_metadata.rs`:

```rust
use bytes::Bytes;
use malachitebft_app_channel::app::types::core::{Round, Validity};
use malachitebft_app_channel::app::types::ProposedValue;
use malachitebft_eth_types::{proto, Address, EmeraldContext, Height, Value, ValueId};
use malachitebft_proto::{Error as ProtoError, Protobuf};
use prost::Message;

const VALUE_ID_LEN: usize = u64::BITS as usize / 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct StoredProposalMetadata {
    pub height: Height,
    pub round: Round,
    pub valid_round: Round,
    pub proposer: Address,
    pub value_id: ValueId,
    pub validity: Validity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DecodedStoredProposal {
    pub metadata: StoredProposalMetadata,
    pub embedded_payload: Option<Bytes>,
}
```

Implement `from_proposal`, `hydrate`, and `encode` so `encode` builds `proto::ProposedValue` directly and stores
`value_id.as_u64().to_be_bytes()` as the entire nested `proto::Value.value`.

Implement `decode_stored_proposal` by decoding `proto::ProposedValue`, decoding the proposer, and inspecting the
nested value bytes:

```rust
let (value_id, embedded_payload) = if value_bytes.len() == VALUE_ID_LEN {
    let id = u64::from_be_bytes(
        value_bytes.as_ref().try_into().map_err(|_| {
            ProtoError::Other("Failed to decode stored proposal value ID".to_owned())
        })?,
    );
    (ValueId::new(id), None)
} else {
    let value = Value::from_proto(proto::Value {
        value: Some(value_bytes),
    })?;
    (value.id(), Some(value.extensions))
};
```

Return metadata constructed from the protobuf fields. Keep the strict `Value::from_proto` path for legacy-full rows.

- [x] **Step 4: Run the codec tests and verify green**

Run:

```bash
cargo test -p emerald compact_proposal_metadata_ -- --nocapture
```

Expected: both tests pass, the compact nested value is exactly eight bytes, and the encoded row is below 128 bytes.

- [x] **Step 5: Commit the codec increment**

```bash
git add app/src/store.rs app/src/store/proposal_metadata.rs
git commit -m "feat: encode compact proposal metadata"
```

### Task 2: Compact runtime writes and hydrated reads

**Files:**

- Modify: `app/src/store.rs:84-125`
- Modify: `app/src/store.rs:435-532`
- Test: `app/src/store.rs`

**Interfaces:**

- Consumes `StoredProposalMetadata`, `DecodedStoredProposal`, and `decode_stored_proposal` from Task 1.
- Produces `Db::hydrate_stored_proposal(tx, key, encoded) -> Result<ProposedValue<EmeraldContext>, StoreError>`.
- Preserves the existing `Db::{get_undecided_proposal,get_undecided_proposals,insert_undecided_proposal}` signatures.

- [x] **Step 1: Write failing runtime storage tests**

Add raw-table helpers and these tests to `app/src/store.rs`:

```rust
fn raw_undecided_proposal(db: &Db, key: (Height, Round, ValueId)) -> Vec<u8> {
    let tx = db.db.begin_read().unwrap();
    tx.open_table(UNDECIDED_PROPOSALS_TABLE)
        .unwrap()
        .get(&key)
        .unwrap()
        .unwrap()
        .value()
}

#[test]
fn compact_proposal_metadata_runtime_stores_one_primary_payload_copy_across_rounds() {
    let (db, _dir, _metrics) = create_test_db_with_metrics("compact_runtime_rounds");
    let payload = Bytes::from(vec![0xCD; 4 * 1024 * 1024]);
    let value = Value::new(payload.clone());

    for round in 0..4 {
        db.insert_undecided_block_data(
            Height::new(51),
            Round::new(round),
            value.id(),
            payload.clone(),
        )
        .unwrap();
        let proposal = ProposedValue {
            height: Height::new(51),
            round: Round::new(round),
            valid_round: if round == 0 { Round::Nil } else { Round::new(round - 1) },
            proposer: Address::new([round as u8; 20]),
            value: value.clone(),
            validity: Validity::Valid,
        };
        db.insert_undecided_proposal(proposal.clone()).unwrap();

        let raw = raw_undecided_proposal(&db, (proposal.height, proposal.round, value.id()));
        assert!(raw.len() < 128);
        assert!(!raw.windows(payload.len()).any(|window| window == payload.as_ref()));
        assert_eq!(
            db.get_undecided_proposal(proposal.height, proposal.round, value.id())
                .unwrap(),
            Some(proposal),
        );
    }

    assert_eq!(db.undecided_block_data_len().unwrap(), 1);
}

#[test]
fn compact_proposal_metadata_read_rejects_missing_shared_payload() {
    let (db, _dir) = create_test_db("compact_missing_payload");
    let proposal = make_proposed_value(52);
    db.insert_undecided_proposal(proposal.clone()).unwrap();

    let error = db
        .get_undecided_proposal(proposal.height, proposal.round, proposal.value.id())
        .unwrap_err();
    assert!(error.to_string().contains("missing shared payload"));
}
```

- [x] **Step 2: Run the runtime tests and verify red**

Run:

```bash
cargo test -p emerald compact_proposal_metadata_runtime_ -- --nocapture
cargo test -p emerald compact_proposal_metadata_read_ -- --nocapture
```

Expected: the first test fails because raw proposal rows still contain the full payload; the second fails because the
legacy full proposal currently decodes without consulting shared storage.

- [x] **Step 3: Add contextual integrity errors and hydration**

Add this `StoreError` variant:

```rust
#[error(
    "Irrecoverable undecided proposal at height {height}, round {round}, value {value_id}: {reason}"
)]
IrrecoverableUndecidedProposal {
    height: Height,
    round: Round,
    value_id: ValueId,
    reason: &'static str,
},
```

Implement a transaction-local payload resolver that opens `UNDECIDED_BLOCK_DATA_TABLE` first and uses
`LEGACY_UNDECIDED_BLOCK_DATA_TABLE` only when the v2 row is absent. Implement `hydrate_stored_proposal` to:

1. Decode the storage row.
2. Require metadata height, round, and value ID to equal the redb key.
3. Require a shared payload row.
4. Recompute `Value::new(Bytes::copy_from_slice(&payload))` and compare its ID.
5. If `embedded_payload` exists, compare it byte-for-byte with the shared payload.
6. Return `metadata.hydrate(payload)`.

Map every failure to `IrrecoverableUndecidedProposal` with one of these fixed reasons:

```text
stored proposal metadata cannot be decoded
stored proposal key does not match its metadata
missing shared payload
shared payload does not match the stored value ID
embedded proposal payload does not match shared storage
```

- [x] **Step 4: Switch runtime proposal writes and reads**

In `insert_undecided_proposal`, replace `ProtobufCodec.encode(&proposal)` with:

```rust
let value = StoredProposalMetadata::from_proposal(&proposal).encode()?;
```

In both read methods, replace direct `ProtobufCodec.decode` calls with `hydrate_stored_proposal` while the same read
transaction is alive. Retain first-write-wins and count only the compact row length for successful metadata writes.

Remove the function-local `use redb::TableHandle;` from the test helper because `TableHandle` is already imported at
module scope, addressing Frank's P3 comment.

- [x] **Step 5: Run runtime, state-flow, and metrics tests**

Run:

```bash
cargo test -p emerald compact_proposal_metadata_ -- --nocapture
cargo test -p emerald shared_undecided_block_data_ -- --nocapture
cargo test -p emerald restream_proposal_ -- --nocapture
cargo test -p emerald undecided_proposal_duplicate_does_not_increment_write_metrics -- --nocapture
```

Expected: all discovered tests pass; no exact filter reports zero tests.

- [x] **Step 6: Commit the runtime increment**

```bash
git add app/src/store.rs app/src/store/proposal_metadata.rs
git commit -m "fix: hydrate proposals from shared payloads"
```

### Task 3: Atomic migration of legacy-full proposal rows

**Files:**

- Modify: `app/src/store.rs:153-170`
- Modify: `app/src/store.rs:770-1040`
- Test: `app/src/store.rs`

**Interfaces:**

- Consumes Task 2's transaction-local hydration and compact encoder.
- Produces `Db::compact_undecided_proposals(&WriteTransaction, &mut MigrationStats)`.
- Extends migration statistics with proposal rows read, compacted rows, source bytes, and compact bytes.

- [ ] **Step 1: Write the successful migration and reopen test**

Create a raw pre-change database containing a legacy-full `ProtobufCodec` proposal row and matching legacy payload.
After `initialize_schema`, assert the raw row is compact and a normal read reconstructs the full value:

```rust
#[test]
fn proposal_metadata_migration_compacts_legacy_full_rows_and_reopens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proposal_metadata_migration.redb");
    let db = Db::new(&path, 1024 * 1024, DbMetrics::new()).unwrap();
    let payload = make_execution_payload_bytes(61);
    let proposal = ProposedValue {
        height: Height::new(61),
        round: Round::new(2),
        valid_round: Round::new(1),
        proposer: Address::new([6; 20]),
        value: Value::new(payload.clone()),
        validity: Validity::Valid,
    };
    insert_raw_legacy_full_proposal_and_payload(&db, &proposal, &payload);

    db.initialize_schema().unwrap();
    let raw = raw_undecided_proposal(
        &db,
        (proposal.height, proposal.round, proposal.value.id()),
    );
    assert!(raw.len() < 128);
    assert_eq!(
        db.get_undecided_proposal(proposal.height, proposal.round, proposal.value.id())
            .unwrap(),
        Some(proposal.clone()),
    );
    drop(db);

    let reopened = Db::new(path, 1024 * 1024, DbMetrics::new()).unwrap();
    reopened.initialize_schema().unwrap();
    assert_eq!(
        reopened
            .get_undecided_proposal(proposal.height, proposal.round, proposal.value.id())
            .unwrap(),
        Some(proposal),
    );
}
```

- [ ] **Step 2: Write table-driven migration rollback tests**

For each of these corrupt fixtures, snapshot the raw proposal and legacy tables, call `initialize_schema`, assert the
fixed error reason, and assert both tables remain byte-for-byte unchanged:

```rust
[
    ("missing_payload", "missing shared payload"),
    ("key_mismatch", "stored proposal key does not match its metadata"),
    ("value_id_mismatch", "stored proposal metadata cannot be decoded"),
    (
        "embedded_payload_conflict",
        "embedded proposal payload does not match shared storage",
    ),
    ("malformed_metadata", "stored proposal metadata cannot be decoded"),
]
```

Name the test `proposal_metadata_migration_rejects_corruption_without_mutation` and build every fixture through raw
redb writes so no production validation preconditions mask the migration behavior.

- [ ] **Step 3: Run migration tests and verify red**

Run:

```bash
cargo test -p emerald proposal_metadata_migration_ -- --nocapture
```

Expected: the successful case retains a full row and the corruption fixtures do not produce the new contextual
errors because startup compaction is not implemented.

- [ ] **Step 4: Implement validate-then-rewrite migration**

Extend `UndecidedBlockDataMigrationStats`:

```rust
proposal_rows: u64,
compacted_proposals: u64,
proposal_source_bytes: u64,
proposal_compact_bytes: u64,
repaired_compact_decided_values: u64,
repaired_compact_decided_value_bytes: u64,
```

Implement `compact_undecided_proposals` with this sequence:

1. Open proposals, v2 payloads, and the legacy fallback in the initialization transaction.
2. Iterate every proposal and decode/validate its key, metadata, shared payload, and optional embedded payload.
3. Encode compact bytes only for legacy-full rows and collect `(key, compact_bytes)` in a vector.
4. Return immediately on the first error; do not mutate the proposal table during validation.
5. Reopen the proposal table mutably and replace every queued row.
6. Accumulate counts and written compact bytes only for replaced rows.

Call it after `migrate_undecided_block_data` and before `backfill_legacy_undecided_block_data`. Include the new fields
in the successful migration log and post-commit write metrics:

```rust
self.metrics.add_writes(
    migration.inserted_payloads
        + migration.compatibility_rows
        + migration.recovered_decided_payloads
        + migration.compacted_proposals
        + migration.repaired_compact_decided_values,
    migration.inserted_bytes
        + migration.compatibility_bytes
        + migration.recovered_decided_bytes
        + migration.proposal_compact_bytes
        + migration.repaired_compact_decided_value_bytes,
);
```

- [ ] **Step 5: Run migration, compatibility, and reopen tests**

Run:

```bash
cargo test -p emerald proposal_metadata_migration_ -- --nocapture
cargo test -p emerald rollback_compatibility_ -- --nocapture
cargo test -p emerald legacy_undecided_block_data_migration_ -- --nocapture
```

Expected: every test passes and failed initialization leaves all pre-existing rows unchanged.

- [ ] **Step 6: Commit the migration increment**

```bash
git add app/src/store.rs
git commit -m "fix: migrate proposal rows to compact metadata"
```

### Task 4: N-1 rollback decoder and compact decided-value repair

**Files:**

- Modify: `app/src/store/proposal_metadata.rs`
- Modify: `app/src/store.rs:829-930`
- Test: `app/src/store.rs`
- Test: `app/src/store/proposal_metadata.rs`

**Interfaces:**

- Produces `decode_stored_value(Bytes) -> Result<DecodedStoredValue, ProtoError>`.
- Produces `DecodedStoredValue::{Full(Value), IdOnly(ValueId)}`.
- Extends `Db::recover_legacy_partial_commits` to repair ID-only decided rows in the same startup transaction.

- [ ] **Step 1: Pin N-1 decoding of compact proposal rows**

Add a test helper that reproduces the N-1 value decoder without calling current strict `Value::from_proto`:

```rust
fn decode_value_like_n_minus_one(value: proto::Value) -> Value {
    let bytes = value.value.unwrap();
    Value {
        value: u64::from_be_bytes(bytes[..8].try_into().unwrap()),
        extensions: bytes.slice(8..),
    }
}
```

Write `rollback_compact_proposal_is_readable_by_n_minus_one` to encode a compact proposal, decode its protobuf with
this helper, assert the ID is preserved and extensions are empty, then retrieve identical bytes from the exact legacy
`(height, round, value_id)` row.

- [ ] **Step 2: Write the failing N-1 commit re-upgrade test**

Simulate N-1 committing a compact proposal by raw-inserting:

- an ID-only `proto::Value` in `DECIDED_VALUES_TABLE`;
- the matching certificate and extracted header;
- the matching decided payload; and
- the compact proposal plus exact legacy undecided payload.

Then call `initialize_schema` and assert `get_decided_value` returns `Value::new(payload)`, the certificate is
unchanged, the repaired row survives reopen, and migration statistics count one repaired decided-value row. Name the
test `rollback_compact_proposal_n_minus_one_commit_is_repaired_on_reupgrade`.

- [ ] **Step 3: Run rollback tests and verify red**

Run:

```bash
cargo test -p emerald rollback_compact_proposal_ -- --nocapture
```

Expected: the proposal compatibility test passes only after Task 1, while re-upgrade fails because current recovery
routes the ID-only decided row through strict `Value::from_bytes`.

- [ ] **Step 4: Implement storage-only decided-value decoding and repair**

In `proposal_metadata.rs`, decode the outer `proto::Value`. Return `IdOnly` only when its nested bytes are exactly
eight bytes; otherwise call strict `Value::from_proto` and return `Full`.

Update `recover_legacy_partial_commits` to:

1. Decode each decided row with `decode_stored_value`.
2. Derive the expected ID from either the full value or the ID-only record.
3. Require certificate height and ID equality.
4. Resolve the existing decided payload first, then v2, then exact legacy payload.
5. Recompute `Value::new(payload)`, verify ID, decode SSZ, and verify the stored header.
6. For a full value, require its extensions to equal the payload.
7. For an ID-only value, replace `DECIDED_VALUES_TABLE[height]` with the full recomputed value only after all rows
   validate; collect repairs before mutating if table iteration otherwise holds guards.
8. Promote missing decided payloads as before and commit both repairs in the outer initialization transaction.

The ID-only path must remain private to store recovery; do not modify `types/src/value.rs`.

- [ ] **Step 5: Run rollback, recovery, and atomic decided-state tests**

Run:

```bash
cargo test -p emerald rollback_compact_proposal_ -- --nocapture
cargo test -p emerald legacy_partial_commit_ -- --nocapture
cargo test -p emerald decided_state_commit_ -- --nocapture
```

Expected: every test passes, including existing hybrid-state and failpoint regressions.

- [ ] **Step 6: Commit the rollback increment**

```bash
git add app/src/store.rs app/src/store/proposal_metadata.rs
git commit -m "fix: repair compact values after rollback"
```

### Task 5: Operator contract and follow-up cleanup boundary

**Files:**

- Modify: `.changelog/unreleased/breaking-changes/22-deduplicate-undecided-block-data.md`
- Modify: `docs/operational-docs/src/production-network/running-emerald.md:115-155`
- Modify: `docs/superpowers/specs/2026-09-09-undecided-block-data-deduplication-design.md`
- Modify: `docs/superpowers/specs/2026-09-10-undecided-payload-rollback-hardening-design.md`
- Modify: `docs/superpowers/specs/2026-09-10-compact-undecided-proposal-metadata-design.md`

**Interfaces:**

- Consumes the completed primary-layout and compatibility behavior from Tasks 1-4.
- Documents cleanup follow-up `1Money-Co/1money-interoperability-protocol#325` without implementing it here.

- [ ] **Step 1: Update the storage model documentation**

State explicitly in every relevant document:

- `undecided_values` contains only the proposal identity and round-specific fields after migration;
- runtime reads hydrate it from v2 and verify the payload-derived `ValueId`;
- legacy-full proposal rows are rewritten atomically during startup;
- logical row replacement lets redb reuse pages but does not guarantee that `store.db` shrinks;
- the temporary N-1 block-data shadow remains the only round-keyed payload duplication in this release; and
- issue #325 owns later activated table deletion and explicit offline redb compaction.

Do not promise automatic compaction or immediate filesystem-space reclamation.

- [ ] **Step 2: Run documentation and static storage checks**

Run:

```bash
awk 'length($0) > 120 { print FNR ":" length($0) ":" $0 }' \
  .changelog/unreleased/breaking-changes/22-deduplicate-undecided-block-data.md \
  docs/operational-docs/src/production-network/running-emerald.md \
  docs/superpowers/specs/2026-09-09-undecided-block-data-deduplication-design.md \
  docs/superpowers/specs/2026-09-10-undecided-payload-rollback-hardening-design.md \
  docs/superpowers/specs/2026-09-10-compact-undecided-proposal-metadata-design.md
rg -n 'ProtobufCodec\.encode\(&proposal\)|use redb::TableHandle' app/src/store.rs
```

Expected: the width command prints nothing. The search finds neither full runtime proposal encoding nor a
function-local `TableHandle` import.

- [ ] **Step 3: Commit the documentation increment**

```bash
git add .changelog/unreleased/breaking-changes/22-deduplicate-undecided-block-data.md \
  docs/operational-docs/src/production-network/running-emerald.md \
  docs/superpowers/specs/2026-09-09-undecided-block-data-deduplication-design.md \
  docs/superpowers/specs/2026-09-10-undecided-payload-rollback-hardening-design.md \
  docs/superpowers/specs/2026-09-10-compact-undecided-proposal-metadata-design.md
git commit -m "docs: explain compact proposal storage"
```

### Task 6: Full verification and Frank review follow-up

**Files:**

- Verify all modified files.
- Update Frank's original GitHub review threads after push.

**Interfaces:**

- Consumes Tasks 1-5.
- Produces a verified commit on `seb/deduplicate-undecided-block-data` and replies on review comments
  `3977079924` and `3977083106`.

- [ ] **Step 1: Run the Emerald library suite**

Run:

```bash
cargo test -p emerald --lib
```

Expected: every Emerald library test passes with no ignored failure.

- [ ] **Step 2: Run the practical workspace and MBT gates**

Run:

```bash
cargo nextest run --workspace --all-features --no-fail-fast --failure-output final \
  --filterset 'not package(emerald-mbt)'
cargo test -p emerald-mbt --no-run
```

Expected: all selected workspace tests pass and both MBT binaries compile.

- [ ] **Step 3: Run repository-required lint and format gates**

Run:

```bash
cargo clippy --tests -- -D warnings
cargo +nightly fmt --all --check
```

If untouched baseline files still fail, record the exact paths and diagnostics, then run the change-scoped gates:

```bash
cargo clippy -p emerald --tests --no-deps -- -D warnings -A clippy::manual-is-multiple-of
cargo +nightly fmt -p emerald -p emerald-mbt --check
```

Expected: change-scoped gates pass. Do not edit unrelated baseline files merely to make workspace-wide gates green.

- [ ] **Step 4: Run final static checks and inspect the diff**

Run:

```bash
git diff --check
rg -n 'ProtobufCodec\.encode\(&proposal\)|value\(\)\.to_vec\(\)|use redb::TableHandle' app/src/store.rs
git status --short
git diff --stat origin/seb/deduplicate-undecided-block-data...HEAD
```

Expected: no whitespace errors, no forbidden proposal encoding or redundant copies/imports, and only planned files
are modified or committed.

- [ ] **Step 5: Push the existing PR branch**

```bash
git push origin seb/deduplicate-undecided-block-data
gh pr view 22 --repo 1Money-Co/emerald --json url,headRefOid,state,statusCheckRollup
```

Expected: PR #22 is open and its head SHA matches local `HEAD`.

- [ ] **Step 6: Reply to Frank's P1 thread**

Reply to comment `3977079924` with this body:

```text
Addressed by the current PR head. undecided_values now stores only compact round-specific metadata with an eight-byte
ValueId reference. Reads load the shared v2 payload, recompute and verify its ID, and reconstruct the full
ProposedValue. Startup atomically validates and rewrites legacy-full rows. Multi-round large-payload tests inspect the
raw proposal rows and primary v2 table, cover reopen, decision, and pruning, and prove one payload copy in the primary
layout. The temporary N-1 block-data shadow remains for the approved compatibility release; its activated removal and
offline redb compaction are tracked by interop issue #325.
```

Use the inline reply endpoint:

```bash
gh api --method POST repos/1Money-Co/emerald/pulls/22/comments/3977079924/replies \
  --raw-field 'body=Addressed by the current PR head. undecided_values now stores only compact round-specific
metadata with an eight-byte ValueId reference. Reads load the shared v2 payload, recompute and verify its ID, and
reconstruct the full ProposedValue. Startup atomically validates and rewrites legacy-full rows. Multi-round
large-payload tests inspect the raw proposal rows and primary v2 table, cover reopen, decision, and pruning, and prove
one payload copy in the primary layout. The temporary N-1 block-data shadow remains for the approved compatibility
release; its activated removal and offline redb compaction are tracked by interop issue #325.'
```

- [ ] **Step 7: Reply to Frank's P3 thread**

Reply to comment `3977083106` with this body:

```text
Fixed by the current PR head. The redundant function-local TableHandle import is removed; the existing module-level
import is used instead.
```

Use the inline reply endpoint:

```bash
gh api --method POST repos/1Money-Co/emerald/pulls/22/comments/3977083106/replies \
  --raw-field 'body=Fixed by the current PR head. The redundant function-local TableHandle import is removed; the
existing module-level import is used instead.'
```
