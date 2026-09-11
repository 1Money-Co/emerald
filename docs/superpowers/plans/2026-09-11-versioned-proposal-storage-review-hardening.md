# Versioned Proposal Storage Review Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Preserve wire-complete N-1 proposal behavior while keeping compact proposal metadata as Emerald's primary
layout and making steady-state startup skip completed migration scans.

**Architecture:** Keep `undecided_values` as a full N-1 compatibility table and add `undecided_values_v2` for compact
metadata. A versioned marker atomically gates one-time reconciliation; later startups skip payload/proposal scans and
only recover decided rows whose payload is actually missing.

**Tech Stack:** Rust, redb 2.6.3, Prost protobuf, Malachite application types, SSZ `ExecutionPayloadV3`, Tokio tests,
Bash testnet tooling, GitHub CLI.

**Spec:** `docs/superpowers/specs/2026-09-11-versioned-proposal-storage-review-hardening-design.md`

## Global Constraints

- Preserve `undecided_values` with its existing `(height, round, value_id)` redb type for N-1.
- Store full wire-complete `ProposedValue` bytes in `undecided_values` throughout the compatibility release.
- Store compact payload-free metadata in new `undecided_values_v2` rows with the same key type.
- Keep `undecided_block_data_v2` keyed by `(height, value_id)` and the exact-round legacy payload fallback.
- Compare payload bytes as well as the 64-bit `ValueId`; conflicts fail without overwrite or partial metrics.
- Keep transport `Value::from_proto` strict; storage-only ID decoding must not enter network paths.
- Write reconciliation version `1` only in the transaction that successfully completes every migration phase.
- A version-1 restart must visit zero legacy payload rows and zero proposal rows during full reconciliation.
- Later startup recovery may validate payload bytes only for a decided row whose decided payload is missing.
- Do not promise automatic redb file shrinkage or run full compaction during startup.
- Treat real mixed-binary execution as opt-in release qualification, not as a deterministic CI claim.
- Keep Markdown lines at or below 120 columns and do not format `Cargo.lock`.

---

### Task 1: Versioned proposal and schema metadata tables

**Files:**

- Modify: `app/src/store.rs:138-185`
- Modify: `app/src/store/proposal_metadata.rs`
- Test: `app/src/store.rs:1680-1860`

**Interfaces:**

- Produces `LEGACY_UNDECIDED_PROPOSALS_TABLE` named `undecided_values`.
- Produces `UNDECIDED_PROPOSALS_TABLE` named `undecided_values_v2`.
- Produces `SCHEMA_METADATA_TABLE`, `UNDECIDED_STORAGE_RECONCILIATION_KEY`, and
  `UNDECIDED_STORAGE_RECONCILIATION_VERSION`.
- Preserves `StoredProposalMetadata::{from_proposal,encode,hydrate}` and `decode_stored_proposal`.

- [x] **Step 1: Add failing schema-name and encoding tests**

Add raw-table helpers and this test in `app/src/store.rs`:

```rust
fn raw_legacy_proposal(db: &Db, key: (Height, Round, ValueId)) -> Option<Vec<u8>> {
    let tx = db.db.begin_read().unwrap();
    tx.open_table(LEGACY_UNDECIDED_PROPOSALS_TABLE)
        .unwrap()
        .get(&key)
        .unwrap()
        .map(|value| value.value())
}

fn raw_compact_proposal(db: &Db, key: (Height, Round, ValueId)) -> Option<Vec<u8>> {
    let tx = db.db.begin_read().unwrap();
    tx.open_table(UNDECIDED_PROPOSALS_TABLE)
        .unwrap()
        .get(&key)
        .unwrap()
        .map(|value| value.value())
}

#[test]
fn schema_creates_separate_legacy_compact_and_metadata_tables() {
    let (db, _dir, _metrics) = create_test_db_with_metrics("versioned-proposal-schema");

    assert!(has_table(&db, "undecided_values"));
    assert!(has_table(&db, "undecided_values_v2"));
    assert!(has_table(&db, "storage_schema_metadata"));
}
```

Keep the existing codec tests in `proposal_metadata.rs`; rename
`compact_proposal_metadata_decodes_legacy_full_records` to
`proposal_metadata_decoder_distinguishes_compact_and_full_records` and assert the full record retains
`embedded_payload == Some(payload)`.

- [x] **Step 2: Run the schema tests and verify red**

Run:

```bash
cargo test -p emerald schema_creates_separate_legacy_compact_and_metadata_tables -- --nocapture
cargo test -p emerald proposal_metadata_decoder_distinguishes_compact_and_full_records -- --nocapture
```

Expected: the schema test fails because `undecided_values_v2` and `storage_schema_metadata` do not exist. The codec
test passes and pins both record forms before table routing changes.

- [x] **Step 3: Define the versioned table constants**

Replace the current proposal table constant and add the schema metadata constants in `app/src/store.rs`:

```rust
const LEGACY_UNDECIDED_PROPOSALS_TABLE:
    redb::TableDefinition<'_, UndecidedValueKey, Vec<u8>> =
    redb::TableDefinition::new("undecided_values");

const UNDECIDED_PROPOSALS_TABLE: redb::TableDefinition<'_, UndecidedValueKey, Vec<u8>> =
    redb::TableDefinition::new("undecided_values_v2");

const SCHEMA_METADATA_TABLE: redb::TableDefinition<'_, &str, u64> =
    redb::TableDefinition::new("storage_schema_metadata");

const UNDECIDED_STORAGE_RECONCILIATION_KEY: &str =
    "undecided_storage_reconciliation_version";
const UNDECIDED_STORAGE_RECONCILIATION_VERSION: u64 = 1;
```

Open all three tables in `Db::initialize_schema`. Do not move existing data yet; Task 3 owns reconciliation.

- [x] **Step 4: Run the schema and codec tests**

Run:

```bash
cargo test -p emerald schema_creates_separate_legacy_compact_and_metadata_tables -- --nocapture
cargo test -p emerald store::proposal_metadata::tests -- --nocapture
```

Expected: the schema test and all proposal metadata codec tests pass.

- [x] **Step 5: Commit the schema boundary**

```bash
git add app/src/store.rs app/src/store/proposal_metadata.rs
git commit -m "feat: add versioned proposal storage tables"
```

### Task 2: Atomic dual writes, v2-first reads, and paired pruning

**Files:**

- Modify: `app/src/store.rs:455-590`
- Modify: `app/src/store.rs:722-770`
- Test: `app/src/store.rs:2060-2200`
- Test: `app/src/store.rs:2940-3250`

**Interfaces:**

- Consumes `LEGACY_UNDECIDED_PROPOSALS_TABLE` and `UNDECIDED_PROPOSALS_TABLE` from Task 1.
- Produces `Db::decode_full_stored_proposal(key, encoded) -> Result<ProposedValue<EmeraldContext>, StoreError>`.
- Keeps existing public `Store::{store_undecided_proposal,get_undecided_proposal,get_undecided_proposals}` APIs.
- Makes `Db::insert_undecided_proposal` write missing full and compact representations in one transaction.

- [ ] **Step 1: Write failing runtime dual-write and fallback tests**

Replace the unsafe `rollback_compact_proposal_is_readable_by_n_minus_one` expectation with:

```rust
#[test]
fn proposal_runtime_dual_writes_wire_complete_v1_and_compact_v2() {
    let (db, _dir) = create_test_db("proposal-dual-write");
    let payload = make_execution_payload_bytes(81);
    let proposal = ProposedValue {
        height: Height::new(81),
        round: Round::new(4),
        valid_round: Round::new(2),
        proposer: Address::new([8; 20]),
        value: Value::new(payload.clone()),
        validity: Validity::Valid,
    };
    let key = (proposal.height, proposal.round, proposal.value.id());

    db.insert_undecided_block_data(key.0, key.1, key.2, payload.clone())
        .unwrap();
    db.insert_undecided_proposal(proposal.clone()).unwrap();

    let legacy = raw_legacy_proposal(&db, key).unwrap();
    let legacy_proto = proto::ProposedValue::decode(legacy.as_slice()).unwrap();
    let legacy_value = decode_value_like_n_minus_one(legacy_proto.value.unwrap());
    assert_eq!(legacy_value.extensions, payload);

    let compact = raw_compact_proposal(&db, key).unwrap();
    assert!(compact.len() < 128);
    assert!(decode_stored_proposal(Bytes::from(compact))
        .unwrap()
        .embedded_payload
        .is_none());
    assert_eq!(db.get_undecided_proposal(key.0, key.1, key.2).unwrap(), Some(proposal));
}
```

Add `proposal_read_falls_back_to_full_v1_written_after_reconciliation`. Initialize an empty database so the version
marker exists, raw-insert only a full v1 proposal and exact legacy payload, reopen, call `initialize_schema`, assert
the compact table remains empty, and assert `get_undecided_proposal` returns the full proposal.

- [ ] **Step 2: Run the runtime tests and verify red**

Run:

```bash
cargo test -p emerald proposal_runtime_dual_writes_wire_complete_v1_and_compact_v2 -- --nocapture
cargo test -p emerald proposal_read_falls_back_to_full_v1_written_after_reconciliation -- --nocapture
```

Expected: dual-write fails because the current table contains compact bytes under the legacy name. Fallback fails
because reads consult only compact storage.

- [ ] **Step 3: Implement full-v1 decoding and v2-first reads**

Add a strict helper that validates a full row without accepting ID-only data:

```rust
fn decode_full_stored_proposal(
    key: (Height, Round, ValueId),
    encoded: Bytes,
) -> Result<ProposedValue<EmeraldContext>, StoreError> {
    let proposal: ProposedValue<EmeraldContext> = ProtobufCodec.decode(&encoded).map_err(|_| {
        Self::irrecoverable_undecided_proposal(key, "full proposal cannot be decoded")
    })?;
    if (proposal.height, proposal.round, proposal.value.id()) != key {
        return Err(Self::irrecoverable_undecided_proposal(
            key,
            "full proposal key does not match its metadata",
        ));
    }
    Ok(proposal)
}
```

Refactor `get_undecided_proposal` to open the compact table first. Hydrate and verify a compact hit through the shared
payload. On a miss, decode the full legacy row and require the resolved v2 or exact legacy payload to have both the
same derived ID and the same bytes as `proposal.value.extensions`.

Refactor `get_undecided_proposals` to collect the union of matching compact and legacy keys. Prefer compact when both
exist, and return each key once. Preserve read-byte and key-byte metrics for rows actually read.

- [ ] **Step 4: Implement atomic proposal dual writes**

Encode both representations before opening the transaction:

```rust
let key = (proposal.height, proposal.round, proposal.value.id());
let legacy = ProtobufCodec.encode(&proposal)?;
let compact = StoredProposalMetadata::from_proposal(&proposal).encode()?;
```

Within one write transaction, insert missing rows in both tables. If one representation already exists, decode it as
the authoritative proposal and derive the missing representation from that proposal rather than overwriting its
proposer or validity with the retry argument. Commit only when at least one row is inserted; metrics count the exact
bytes physically inserted after commit.

- [ ] **Step 5: Add paired proposal pruning coverage**

Extend `test_prune` to assert that retained keys exist in both proposal tables before pruning, keys below the retain
height are absent from both afterward, and surviving reads still return one logical proposal rather than duplicates.

- [ ] **Step 6: Run runtime, restream, metrics, and pruning tests**

Run:

```bash
cargo test -p emerald proposal_runtime_ -- --nocapture
cargo test -p emerald proposal_read_falls_back_ -- --nocapture
cargo test -p emerald restream_proposal_ -- --nocapture
cargo test -p emerald undecided_proposal_duplicate_ -- --nocapture
cargo test -p emerald store::tests::test_prune -- --exact --nocapture
```

Expected: all tests pass; existing restream callers receive fully hydrated proposals without API changes.

- [ ] **Step 7: Commit runtime compatibility**

```bash
git add app/src/store.rs
git commit -m "fix: preserve full rollback proposal values"
```

### Task 3: Atomic one-time reconciliation and durable phase marker

**Files:**

- Modify: `app/src/store.rs:838-1215`
- Test: `app/src/store.rs:1950-2310`

**Interfaces:**

- Consumes both proposal tables and schema metadata from Tasks 1-2.
- Produces `SchemaInitializationStats` and
  `Db::initialize_schema() -> Result<SchemaInitializationStats, StoreError>`.
- Produces `Db::reconcile_undecided_storage(tx, migration, stats) -> Result<(), StoreError>`.
- Writes reconciliation version `1` only after all validation and queued mutations succeed.

- [ ] **Step 1: Add initialization statistics and raw marker helpers for tests**

Define:

```rust
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SchemaInitializationStats {
    reconciliation_ran: bool,
    legacy_payload_rows_visited: u64,
    proposal_rows_visited: u64,
    targeted_decided_rows_visited: u64,
    expensive_decided_rows_validated: u64,
}
```

Add test helpers `reconciliation_version(&Db) -> Option<u64>` and
`insert_raw_legacy_proposal(Db, key, bytes)` that explicitly target the full v1 table. Rename the existing raw compact
helper to target v2 so tests never rely on an ambiguous table name.

- [ ] **Step 2: Write failing migration-state matrix tests**

Add these cases:

- `reconciliation_preserves_full_n_minus_one_rows_and_populates_compact_v2` inserts a full v1 proposal plus legacy
  payload, initializes, and asserts v1 remains byte-for-byte full, v2 is compact, and marker `1` exists.
- `reconciliation_restores_current_pr_compact_rows_to_full_v1` inserts the current PR's compact bytes in v1 plus v2
  and legacy payloads, initializes, and asserts full v1 plus compact v2 after reopen.
- `reconciliation_failure_leaves_marker_and_all_tables_unchanged` covers missing payload, key mismatch, derived-ID
  mismatch, embedded-byte conflict, and malformed metadata.

For every failure case, snapshot raw v1, compact v2, legacy payload, shared payload, decided, and marker values before
initialization and assert exact equality afterward.

- [ ] **Step 3: Run migration-state tests and verify red**

Run:

```bash
cargo test -p emerald reconciliation_preserves_full_ -- --nocapture
cargo test -p emerald reconciliation_restores_current_pr_ -- --nocapture
cargo test -p emerald reconciliation_failure_ -- --nocapture
```

Expected: the full-row case rewrites the legacy table compactly, the current-PR case does not restore full bytes, and
no completion marker exists.

- [ ] **Step 4: Refactor full reconciliation around both proposal representations**

Change `compact_undecided_proposals` into `reconcile_undecided_proposals`. For every v1 row:

1. Decode `proto::ProposedValue` and classify nested value bytes as full or exactly-eight-byte compact.
2. Resolve v2 payload first, then exact legacy payload.
3. Validate key fields, payload-derived `ValueId`, and embedded full bytes.
4. Queue compact v2 insertion or identical verification.
5. Queue full v1 restoration only for a compact input row.

Apply queued v1 restorations and v2 insertions only after every proposal validates. Keep legacy payload migration,
rollback-shadow backfill, and decided-state reconciliation in the same outer write transaction.

- [ ] **Step 5: Gate reconciliation with the durable marker**

Structure `initialize_schema` as:

```rust
let mut stats = SchemaInitializationStats::default();
let version = tx
    .open_table(SCHEMA_METADATA_TABLE)?
    .get(UNDECIDED_STORAGE_RECONCILIATION_KEY)?
    .map(|value| value.value())
    .unwrap_or_default();

if version < UNDECIDED_STORAGE_RECONCILIATION_VERSION {
    stats.reconciliation_ran = true;
    Self::reconcile_undecided_storage(&tx, &mut migration, &mut stats)?;
    tx.open_table(SCHEMA_METADATA_TABLE)?.insert(
        UNDECIDED_STORAGE_RECONCILIATION_KEY,
        UNDECIDED_STORAGE_RECONCILIATION_VERSION,
    )?;
} else {
    Self::recover_missing_decided_payloads(&tx, &mut migration, &mut stats)?;
}
tx.commit()?;
```

Do not use retained-table existence as a completion signal. Emit the full migration event only when
`stats.reconciliation_ran` is true and the transaction commits.

- [ ] **Step 6: Run migration, rollback, reopen, and conflict tests**

Run:

```bash
cargo test -p emerald reconciliation_ -- --nocapture
cargo test -p emerald legacy_undecided_block_data_migration_ -- --nocapture
cargo test -p emerald rollback_compatibility_ -- --nocapture
cargo test -p emerald proposal_metadata_migration_ -- --nocapture
```

Expected: all tests pass. Update old test names and assertions to distinguish full v1 bytes from compact v2 bytes.

- [ ] **Step 7: Commit one-time reconciliation**

```bash
git add app/src/store.rs
git commit -m "fix: version-gate storage reconciliation"
```

### Task 4: Cheap steady-state startup and targeted incomplete recovery

**Files:**

- Modify: `app/src/store.rs:912-1050`
- Test: `app/src/store.rs:2350-2660`
- Test: `app/src/store.rs` ignored benchmark module

**Interfaces:**

- Consumes `SchemaInitializationStats` and marker gating from Task 3.
- Produces `Db::recover_missing_decided_payloads(tx, migration, stats) -> Result<(), StoreError>`.
- Keeps the full `DecodedStoredValue::IdOnly` repair only in one-time reconciliation.

- [ ] **Step 1: Write the failing second-startup no-rescan test**

Add `versioned_second_startup_skips_reconciliation_and_expensive_validation`. Build a database with at least two
legacy payload/proposal rows and one complete decided row, run initialization once, reopen, and capture the returned
statistics from the second initialization:

```rust
assert_eq!(stats.reconciliation_ran, false);
assert_eq!(stats.legacy_payload_rows_visited, 0);
assert_eq!(stats.proposal_rows_visited, 0);
assert_eq!(stats.expensive_decided_rows_validated, 0);
assert_eq!(reconciliation_version(&reopened), Some(1));
```

- [ ] **Step 2: Write targeted recovery tests**

Add two tests:

- `versioned_restart_recovers_only_missing_decided_payload` creates marker `1`, one complete decided row, and one
  N-1 partial row. Assert two decided keys visited, exactly one expensive validation, one promotion, and full reopen.
- `versioned_restart_recovery_conflict_aborts_without_mutation` creates marker `1` and a partial row with a wrong
  legacy payload. Assert the marker remains `1`, no decided payload is inserted, and existing rows are unchanged.

- [ ] **Step 3: Run steady-state and targeted tests and verify red**

Run:

```bash
cargo test -p emerald versioned_second_startup_ -- --nocapture
cargo test -p emerald versioned_restart_ -- --nocapture
```

Expected: the current code reports proposal/legacy scans on the second startup and hashes or SSZ-validates the
complete decided row.

- [ ] **Step 4: Split initial decided reconciliation from later recovery**

Retain the current full decoder and ID-only repair in `reconcile_existing_decided_state`, called only before writing
marker `1`.

Implement `recover_missing_decided_payloads` so its loop performs this order:

```rust
for entry in values.iter()? {
    let (height, encoded_value) = entry?;
    stats.targeted_decided_rows_visited += 1;
    if decided_payloads.get(&height.value())?.is_some() {
        continue;
    }
    stats.expensive_decided_rows_validated += 1;
    // Only now decode value/certificate/header, resolve v2 or exact legacy bytes,
    // verify ID + bytes + SSZ + header, and queue promotion.
}
```

The targeted path requires a full stored value. An ID-only value after marker `1` is an integrity error because
reconciliation repaired every pre-existing ID-only row and safe N/N-1 proposal storage cannot create another.

- [ ] **Step 5: Add an ignored large-dataset restart benchmark**

Add:

```rust
#[test]
#[ignore = "manual large-dataset restart benchmark"]
fn versioned_restart_large_dataset_benchmark() {
    const PROPOSALS: u64 = 256;
    const PAYLOAD_BYTES: usize = 256 * 1024;
    // Insert full v1 proposals and legacy payloads, time first initialization,
    // reopen and time second initialization, then print both durations and stats.
    assert_eq!(second.legacy_payload_rows_visited, 0);
    assert_eq!(second.proposal_rows_visited, 0);
    assert_eq!(second.expensive_decided_rows_validated, 0);
}
```

Use deterministic visit-count assertions only. Print elapsed durations with `eprintln!` for manual evidence; do not
assert a timing ratio.

- [ ] **Step 6: Run recovery regressions and compile the ignored benchmark**

Run:

```bash
cargo test -p emerald versioned_ -- --nocapture
cargo test -p emerald legacy_partial_commit_ -- --nocapture
cargo test -p emerald rollback_compact_proposal_ -- --nocapture
cargo test -p emerald versioned_restart_large_dataset_benchmark -- --ignored --nocapture
```

Expected: targeted and legacy recovery tests pass. The manual benchmark prints first/second durations and proves zero
heavy phase visits on the second startup.

- [ ] **Step 7: Commit restart gating**

```bash
git add app/src/store.rs
git commit -m "perf: skip completed storage migration scans"
```

### Task 5: N-1 commit and sync contract while rollback is active

**Files:**

- Modify: `app/src/store.rs:2310-2445`
- Test: `app/src/store.rs`

**Interfaces:**

- Consumes full v1 dual writes and v1 fallback from Tasks 2-4.
- Uses `decode_value_like_n_minus_one(proto::Value) -> Value` as the pinned N-1 decoder.
- Produces deterministic coverage for N proposal -> N-1 commit -> N-1/current sync -> N re-upgrade.

- [ ] **Step 1: Replace the ID-only rollback test with a wire-complete lifecycle test**

Add `rollback_wire_complete_proposal_commits_and_syncs_while_n_minus_one_active`:

1. Initialize N, store a valid SSZ payload and proposal, and read raw v1 bytes.
2. Decode the v1 nested value with `decode_value_like_n_minus_one`; assert extensions equal the SSZ payload.
3. Simulate N-1's separate metadata and decided-payload transactions using the decoded full value.
4. Before calling N initialization again, encode the stored decided value as N-1's `get_raw_decided_value` does.
5. Decode those bytes with current strict `Value::from_bytes` and with the N-1 decoder.
6. Assert both results equal `Value::new(payload)` and both extensions decode as `ExecutionPayloadV3`.
7. Reopen with N, initialize, and assert the full decided value and certificate remain unchanged.

Use a test name and assertion message that explicitly mention the while-N-1-active sync boundary Simon identified.

- [ ] **Step 2: Add post-marker N-1 write and N re-upgrade coverage**

Add `rollback_full_v1_proposal_written_after_marker_is_readable_on_reupgrade`. After creating marker `1`, raw-insert
only a full v1 proposal and exact legacy payload as N-1 would. Reopen with N and assert:

```rust
assert_eq!(stats.reconciliation_ran, false);
assert!(raw_compact_proposal(&n, key).is_none());
assert_eq!(n.get_undecided_proposal(key.0, key.1, key.2).unwrap(), Some(proposal));
```

This proves the durable marker does not strand state created during a rollback interval.

- [ ] **Step 3: Run lifecycle tests and verify red before implementation adjustments**

Run:

```bash
cargo test -p emerald rollback_wire_complete_ -- --nocapture
cargo test -p emerald rollback_full_v1_ -- --nocapture
```

Expected: tests fail against the current same-table compact encoding. Do not weaken current strict wire decoding to
make them pass.

- [ ] **Step 4: Complete only the minimal lifecycle fixes exposed by the tests**

Route every raw N-1 compatibility lookup to `LEGACY_UNDECIDED_PROPOSALS_TABLE`, keep v1 encoding full, and ensure
marker-present startup leaves post-marker v1-only proposals for the runtime fallback. Remove the obsolete expectation
that an ID-only proposal is a supported N-1 commit input.

- [ ] **Step 5: Run sync, state, and rollback suites**

Run:

```bash
cargo test -p emerald rollback_ -- --nocapture
cargo test -p emerald synced_value_ -- --nocapture
cargo test -p emerald shared_undecided_block_data_commits_from_later_round -- --nocapture
cargo test -p emerald decided_state_commit_ -- --nocapture
```

Expected: all tests pass and no network decoder accepts an unattached ID-only value.

- [ ] **Step 6: Commit the N-1 lifecycle coverage**

```bash
git add app/src/store.rs
git commit -m "test: pin rollback commit and sync compatibility"
```

### Task 6: Opt-in binary qualification and operator contract

**Files:**

- Create: `scripts/tests/rollback_binary_qualification.sh`
- Create: `scripts/tests/rollback_binary_qualification_test.sh`
- Modify: `.changelog/unreleased/breaking-changes/22-deduplicate-undecided-block-data.md`
- Modify: `docs/operational-docs/src/production-network/running-emerald.md:115-175`
- Modify: `docs/superpowers/specs/2026-09-10-compact-undecided-proposal-metadata-design.md`
- Modify: `docs/superpowers/specs/2026-09-11-versioned-proposal-storage-review-hardening-design.md`
- Update: `1Money-Co/1money-interoperability-protocol#325`

**Interfaces:**

- Consumes the completed storage and compatibility behavior from Tasks 1-5.
- Produces an opt-in script requiring `--current-emerald-bin`, `--n-minus-one-emerald-bin`, and
  `--custom-reth-bin`.
- Documents that v1 proposal and block-data shadows both remain until activated cleanup.

- [ ] **Step 1: Add a failing shell contract test for the qualification script**

Create `scripts/tests/rollback_binary_qualification_test.sh` using the repository's existing shell-test style. It
must assert that `--help` documents all three binary inputs and that missing or identical Emerald binaries fail before
starting processes. Run:

```bash
bash scripts/tests/rollback_binary_qualification_test.sh
```

Expected: FAIL because `rollback_binary_qualification.sh` does not exist.

- [ ] **Step 2: Implement the opt-in real-binary script**

Create `scripts/tests/rollback_binary_qualification.sh` with `set -euo pipefail`, absolute path normalization, a
temporary home created by `mktemp -d`, and a trap that invokes testnet stop before preserving logs on failure.

The script must:

1. Parse `--current-emerald-bin`, `--n-minus-one-emerald-bin`, and `--custom-reth-bin`.
2. Require all paths to be executable and require distinct Emerald `--version` output.
3. Record binary versions and every command in the qualification log.
4. Start an isolated four-node current-binary testnet with the supplied Reth binary.
5. Wait for a minimum height through the testnet status/RPC helper; never use a fixed sleep as the success oracle.
6. Stop one node, restart it with N-1, and require it to catch up while the remaining nodes stay live.
7. Stop a second node long enough to lag, restart it with N-1, and require catch-up while another N-1 node is active.
8. Fail on panic, `Failed to decode synced value`, empty execution-payload decoding, or process exit in N-1 logs.
9. Stop both N-1 nodes, restart them with the current binary, and require all four nodes to converge.
10. Print the artifact directory and preserve logs for both success and failure.

The script is release evidence for real process/version transitions. It must state in `--help` that deterministic
database tests, not this network run, pin the exact N-proposal/N-1-commit interleaving.

- [ ] **Step 3: Run the shell contract test**

Run:

```bash
bash scripts/tests/rollback_binary_qualification_test.sh
bash -n scripts/tests/rollback_binary_qualification.sh
```

Expected: both pass. Do not claim the real-binary qualification passed unless all three external binaries are supplied
and the full script is executed.

- [ ] **Step 4: Update changelog, runbook, and design history**

Document these exact operator facts:

- `undecided_values` remains full and round-keyed for N-1; `undecided_values_v2` is compact and primary for N.
- Runtime dual-writes both proposal tables and both payload tables during the compatibility release.
- Successful reconciliation writes version `1`; ordinary later starts skip legacy payload/proposal scans.
- A missing decided payload still triggers certificate-bound targeted recovery.
- Compatibility duplicates can reuse freed redb pages later but do not imply immediate `store.db` shrinkage.
- The opt-in script is a release qualification tool and deterministic tests do not claim a mixed-binary run passed.

Mark the same-table compact encoding in the 2026-09-10 design as superseded rather than deleting its history.

- [ ] **Step 5: Extend cleanup issue #325**

Edit the issue so its objective and acceptance criteria cover both compatibility tables. Preserve its existing payload
cleanup and offline-compaction requirements, and add:

```text
- Reconcile any full v1 proposals created during an N-1 interval into `undecided_values_v2` before activation.
- Stop dual-writing and delete both `undecided_values` and `undecided_block_data` atomically after validation.
- Prove compact proposal and shared payload reads no longer use either compatibility table.
```

First run `gh issue view 325 --repo 1Money-Co/1money-interoperability-protocol --json body` and verify that the issue
still describes the PR #22 compatibility release. Create `/private/tmp/emerald-issue-325-body.md` with `apply_patch`
using the complete current body, then make only these semantic edits before passing that exact path to:

```bash
gh issue edit 325 --repo 1Money-Co/1money-interoperability-protocol \
  --body-file /private/tmp/emerald-issue-325-body.md
```

Required edits to the copied body are fully bounded:

1. In Context, replace the claim that `undecided_values` is compact with the v1/v2 proposal-table model.
2. In Objective, say both proposal and payload compatibility shadows are removed after the rollback window.
3. Before deletion, require reconciliation of N-1-created full proposals into `undecided_values_v2`.
4. Require atomic deletion of both `undecided_values` and `undecided_block_data` after validation.
5. Require reads, writes, and pruning to stop depending on either compatibility table.
6. Preserve every existing compaction, backup, logging, metrics, conflict rollback, and qualification criterion.

- [ ] **Step 6: Run documentation width and shell checks**

Run:

```bash
awk 'length($0) > 120 { print FNR ":" length($0) ":" $0 }' \
  .changelog/unreleased/breaking-changes/22-deduplicate-undecided-block-data.md \
  docs/operational-docs/src/production-network/running-emerald.md \
  docs/superpowers/specs/2026-09-10-compact-undecided-proposal-metadata-design.md \
  docs/superpowers/specs/2026-09-11-versioned-proposal-storage-review-hardening-design.md
bash scripts/tests/rollback_binary_qualification_test.sh
```

Expected: the width command prints nothing and the shell contract test prints `ok`.

- [ ] **Step 7: Commit qualification and documentation**

```bash
git add scripts/tests/rollback_binary_qualification.sh \
  scripts/tests/rollback_binary_qualification_test.sh \
  .changelog/unreleased/breaking-changes/22-deduplicate-undecided-block-data.md \
  docs/operational-docs/src/production-network/running-emerald.md \
  docs/superpowers/specs/2026-09-10-compact-undecided-proposal-metadata-design.md \
  docs/superpowers/specs/2026-09-11-versioned-proposal-storage-review-hardening-design.md
git commit -m "docs: define rollback qualification boundary"
```

### Task 7: Full verification, push, and Simon thread follow-up

**Files:**

- Verify all modified files.
- Update Simon's original GitHub review threads after push.

**Interfaces:**

- Consumes Tasks 1-6.
- Produces a verified PR head on `seb/deduplicate-undecided-block-data`.
- Replies to review comments `3987393054` and `3987393069` in their original threads.

- [ ] **Step 1: Run the Emerald library suite**

```bash
cargo test -p emerald --lib
```

Expected: every Emerald library test passes with no ignored failure.

- [ ] **Step 2: Run practical workspace and MBT gates**

```bash
cargo nextest run --workspace --all-features --no-fail-fast --failure-output final \
  --filterset 'not package(emerald-mbt)'
cargo test -p emerald-mbt --no-run
```

Expected: all selected workspace tests pass and both MBT binaries compile.

- [ ] **Step 3: Run repository-required lint and format gates**

```bash
cargo clippy --tests -- -D warnings
cargo +nightly fmt --all --check
```

If untouched baseline files still fail, record exact paths and diagnostics, then require:

```bash
cargo clippy -p emerald --tests --no-deps -- -D warnings -A clippy::manual-is-multiple-of
cargo +nightly fmt -p emerald -p emerald-mbt --check
```

Expected: scoped gates pass. Do not edit unrelated CLI/contracts/key-provider/utils baselines.

- [ ] **Step 4: Run final static and migration checks**

```bash
git diff --check
rg -n 'TableDefinition::new\("undecided_values"\)|TableDefinition::new\("undecided_values_v2"\)' \
  app/src/store.rs
rg -n 'UNDECIDED_STORAGE_RECONCILIATION_(KEY|VERSION)' app/src/store.rs
rg -n 'Value::from_proto|ProtobufCodec' app/src/store.rs app/src/store/proposal_metadata.rs
git status --short
git diff --stat origin/seb/deduplicate-undecided-block-data...HEAD
```

Inspect each match. Require full encoding only for v1 storage and wire paths, compact encoding only for v2 storage,
and no ID-only exception in network decoding.

- [ ] **Step 5: Push and verify the existing PR head**

```bash
git push origin seb/deduplicate-undecided-block-data
gh pr view 22 --repo 1Money-Co/emerald --json url,headRefOid,state,statusCheckRollup
```

Expected: PR #22 is open and `headRefOid` equals local `git rev-parse HEAD`.

- [ ] **Step 6: Reply to Simon's rollback-wire thread**

Reply to comment `3987393054` in place:

```text
Addressed at the current head. `undecided_values` is again a full wire-complete N-1 compatibility table, while new
`undecided_values_v2` holds compact metadata for N. Writes and pruning update both, N reads v2 first with a verified
v1 fallback, and one-time reconciliation restores compact rows written by the earlier PR revision. The regression
pins N proposal -> N-1 commit -> full N-1/current sync bytes while N-1 is active -> N re-upgrade. An opt-in
real-binary qualification script covers process-level downgrade, sync, and re-upgrade; the exact stored-proposal
interleaving remains deterministically pinned at the database/wire boundary.
```

Use:

```bash
gh api --method POST repos/1Money-Co/emerald/pulls/22/comments/3987393054/replies \
  --raw-field 'body=Addressed at the current head. `undecided_values` is again a full wire-complete N-1 compatibility
table, while new `undecided_values_v2` holds compact metadata for N. Writes and pruning update both, N reads v2 first
with a verified v1 fallback, and one-time reconciliation restores compact rows written by the earlier PR revision.
The regression pins N proposal -> N-1 commit -> full N-1/current sync bytes while N-1 is active -> N re-upgrade. An
opt-in real-binary qualification script covers process-level downgrade, sync, and re-upgrade; the exact
stored-proposal interleaving remains deterministically pinned at the database/wire boundary.'
```

- [ ] **Step 7: Reply to Simon's restart-scan thread**

Reply to comment `3987393069` in place:

```text
Addressed at the current head. Reconciliation now writes an atomic version-1 completion marker. Marker-present
restarts skip all legacy payload and proposal scans and only inspect decided keys for missing decided payload rows;
complete rows are not decoded, hashed, SSZ-decoded, or header-validated again. Tests prove zero migration visits on a
second startup, preserve targeted N-1 partial-commit recovery, and include an ignored representative restart
benchmark without unstable timing assertions.
```

Use:

```bash
gh api --method POST repos/1Money-Co/emerald/pulls/22/comments/3987393069/replies \
  --raw-field 'body=Addressed at the current head. Reconciliation now writes an atomic version-1 completion marker.
Marker-present restarts skip all legacy payload and proposal scans and only inspect decided keys for missing decided
payload rows; complete rows are not decoded, hashed, SSZ-decoded, or header-validated again. Tests prove zero migration
visits on a second startup, preserve targeted N-1 partial-commit recovery, and include an ignored representative
restart benchmark without unstable timing assertions.'
```

- [ ] **Step 8: Re-read GitHub state and report qualification honestly**

```bash
gh api repos/1Money-Co/emerald/pulls/comments/3987393054
gh api repos/1Money-Co/emerald/pulls/comments/3987393069
gh pr view 22 --repo 1Money-Co/emerald --json headRefOid,statusCheckRollup,reviews,url
```

Report deterministic tests, scoped lint/format results, unchanged workspace baselines, whether the real-binary script
was actually run, review-thread reply links, and the current remote check/approval state separately.
