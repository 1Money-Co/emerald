# Undecided Block Data Deduplication Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Store one undecided execution payload per height and value while preserving round-specific proposal metadata.

**Architecture:** Add a versioned redb table keyed by `(height, value_id)`, make writes compare existing bytes and fail
closed on conflicts, and split undecided and decided payload reads. Migrate the legacy round-keyed table atomically at
store startup, then update proposal, restream, decision, commit, sync, and pruning paths to use the shared payload.

**Tech Stack:** Rust, Tokio, redb 2.x, Prometheus client counters, Malachite channel types, SSZ execution payloads,
Cargo Nextest, Clippy, nightly rustfmt

**Spec:** `docs/superpowers/specs/2026-09-09-undecided-block-data-deduplication-design.md`

## Global Constraints

- Work on `seb/deduplicate-undecided-block-data`, derived from `origin/om-emerald` at `d73f21a`.
- Keep proposal metadata keyed by `(height, round, value_id)`; remove `round` only from undecided payload storage.
- Use the exact v2 table name `undecided_block_data_v2`; retain `undecided_block_data` only as the migration source.
- Treat same-key, different-byte payloads as `StoreError::ConflictingUndecidedBlockData`; never overwrite or log bytes.
- Preserve the payload-before-proposal-metadata write order in `State::store_undecided_value`.
- Keep migration atomic and one-way; delete the legacy table only in the successful migration transaction.
- Do not add automatic database compaction, dependencies, wire-format changes, or decided-retention changes.
- Keep existing database metrics as value-byte counters, not filesystem-page or write-amplification measurements.
- Do not modify or format `Cargo.lock`.
- Keep Markdown lines within 120 columns.
- Before pushing, run `cargo clippy --tests -- -D warnings` and `cargo +nightly fmt --all --check`.

## File Structure

- Modify `app/src/store/keys.rs`: define the round-independent redb key type for undecided payloads.
- Modify `app/src/store.rs`: own v2 schema initialization, migration, conflict-safe reads and writes, pruning, and store
  regression tests.
- Modify `app/src/metrics.rs`: allow migration to record multiple committed payload inserts and expose test snapshots.
- Modify `app/src/state.rs`: use the shared payload for proposal creation, restream, decision, commit, and state tests.
- Modify `app/src/app.rs`: use explicit undecided reads in `GetValue` and `Decided`, and cover synced-value recovery.
- Modify `tests/mbt/src/sut/get_value.rs`: keep the MBT adapter compiling against the round-independent state API.
- Modify `docs/operational-docs/src/production-network/running-emerald.md`: document backup, headroom, downgrade, and
  compaction behavior.

---

### Task 1: Add the conflict-safe v2 payload store

**Files:**

- Modify: `app/src/store/keys.rs:1-112`
- Modify: `app/src/store.rs:19-125`
- Modify: `app/src/store.rs:532-670`
- Modify: `app/src/store.rs:896-918`
- Modify: `app/src/store.rs:973-1020`
- Modify: `app/src/metrics.rs:150-170`

**Interfaces:**

- Consumes: existing `HeightKey`, `ValueIdKey`, `DbMetrics`, and redb `Vec<u8>` values.
- Produces:

```rust
pub type UndecidedBlockDataKey = (HeightKey, ValueIdKey);

StoreError::ConflictingUndecidedBlockData {
    height: Height,
    value_id: ValueId,
}

fn Db::get_undecided_block_data(
    &self,
    height: Height,
    value_id: ValueId,
) -> Result<Option<Bytes>, StoreError>;

fn Db::insert_undecided_block_data(
    &self,
    height: Height,
    value_id: ValueId,
    data: Bytes,
) -> Result<(), StoreError>;
```

- Produces `DbMetrics::add_writes(writes, bytes)` for Task 3 migration accounting.
- Keeps the legacy table definition and old async runtime methods temporarily. Rename the old private insertion to
  `insert_legacy_undecided_block_data` so it can coexist with the new v2 primitive. Task 2 removes all legacy runtime
  use, adds the v2 async wrappers, and confines the old table definition to migration.

- [ ] **Step 1: Write failing v2 storage tests**

In `app/src/store.rs`, add a metrics-preserving test helper and three focused tests. The tests deliberately pass a
manually chosen `ValueId` so the conflict case does not depend on finding a SipHash collision.

```rust
fn create_test_db_with_metrics(name: &str) -> (Db, tempfile::TempDir, DbMetrics) {
    let dir = tempfile::tempdir().unwrap();
    let metrics = DbMetrics::new();
    let db = Db::new(
        dir.path().join(format!("{name}.redb")),
        1024 * 1024,
        metrics.clone(),
    )
    .unwrap();
    db.create_tables().unwrap();
    (db, dir, metrics)
}

#[test]
fn undecided_block_data_deduplicates_identical_payloads() {
    let (db, dir, metrics) = create_test_db_with_metrics("deduplicate_payload");
    let height = Height::new(7);
    let value_id = ValueId::new(11);
    let payload = Bytes::from_static(b"one-payload");

    db.insert_undecided_block_data(height, value_id, payload.clone())
        .unwrap();
    db.insert_undecided_block_data(height, value_id, payload.clone())
        .unwrap();

    assert_eq!(db.undecided_block_data_len().unwrap(), 1);
    assert_eq!(metrics.write_count(), 1);
    assert_eq!(metrics.write_bytes(), payload.len() as u64);

    let path = dir.path().join("deduplicate_payload.redb");
    drop(db);
    let reopened = Db::new(path, 1024 * 1024, DbMetrics::new()).unwrap();
    reopened.create_tables().unwrap();
    assert_eq!(
        reopened.get_undecided_block_data(height, value_id).unwrap(),
        Some(payload)
    );
}

#[test]
fn undecided_block_data_keeps_distinct_values_at_one_height() {
    let (db, _dir, _metrics) = create_test_db_with_metrics("distinct_payloads");
    let height = Height::new(7);

    db.insert_undecided_block_data(
        height,
        ValueId::new(11),
        Bytes::from_static(b"first"),
    )
    .unwrap();
    db.insert_undecided_block_data(
        height,
        ValueId::new(12),
        Bytes::from_static(b"second"),
    )
    .unwrap();

    assert_eq!(db.undecided_block_data_len().unwrap(), 2);
}

#[test]
fn undecided_block_data_rejects_conflicting_bytes_without_overwrite() {
    let (db, _dir, metrics) = create_test_db_with_metrics("conflicting_payload");
    let height = Height::new(7);
    let value_id = ValueId::new(11);
    let original = Bytes::from_static(b"original");

    db.insert_undecided_block_data(height, value_id, original.clone())
        .unwrap();
    let error = db
        .insert_undecided_block_data(height, value_id, Bytes::from_static(b"conflict"))
        .unwrap_err();

    assert!(matches!(
        error,
        StoreError::ConflictingUndecidedBlockData {
            height: error_height,
            value_id: error_value_id,
        } if error_height == height && error_value_id == value_id
    ));
    assert_eq!(
        db.get_undecided_block_data(height, value_id).unwrap(),
        Some(original.clone())
    );
    assert_eq!(metrics.write_count(), 1);
    assert_eq!(metrics.write_bytes(), original.len() as u64);
}
```

- [ ] **Step 2: Run the focused tests and verify the red state**

Run:

```bash
cargo test -p emerald undecided_block_data_ -- --nocapture
```

Expected: compilation fails because the two-argument v2 database methods, row-count helper, metric snapshots, and
conflict variant do not exist. Confirm Cargo discovers the three named tests rather than reporting zero tests.

- [ ] **Step 3: Add the v2 key and table definitions**

In `app/src/store/keys.rs`, add the new alias without changing `UndecidedValueKey`:

```rust
pub type UndecidedValueKey = (HeightKey, RoundKey, ValueIdKey);
pub type UndecidedBlockDataKey = (HeightKey, ValueIdKey);
pub type PendingValueKey = (HeightKey, RoundKey, ValueIdKey);
```

In `app/src/store.rs`, import it, rename the current table definition, and add v2:

```rust
use keys::{HeightKey, UndecidedBlockDataKey, UndecidedValueKey};

const LEGACY_UNDECIDED_BLOCK_DATA_TABLE:
    redb::TableDefinition<'_, UndecidedValueKey, Vec<u8>> =
    redb::TableDefinition::new("undecided_block_data");

const UNDECIDED_BLOCK_DATA_TABLE:
    redb::TableDefinition<'_, UndecidedBlockDataKey, Vec<u8>> =
    redb::TableDefinition::new("undecided_block_data_v2");
```

Change `Db::create_tables` to open `UNDECIDED_BLOCK_DATA_TABLE`. Do not open the legacy table there; the existing
legacy async runtime method remains available only until Task 2 and opens the legacy table on demand. Update the old
combined getter to open `LEGACY_UNDECIDED_BLOCK_DATA_TABLE`, and rename the old private insertion method plus its
async wrapper call like this:

```rust
fn insert_legacy_undecided_block_data(
    &self,
    height: Height,
    round: Round,
    value_id: ValueId,
    data: Bytes,
) -> Result<(), StoreError>;

// Inside the temporary round-taking Store::store_undecided_block_data wrapper:
db.insert_legacy_undecided_block_data(height, round, value_id, data)
```

- [ ] **Step 4: Add the fail-closed conflict error**

Add this non-sensitive variant to `StoreError`:

```rust
#[error("Conflicting undecided block data at height {height}, value {value_id}")]
ConflictingUndecidedBlockData {
    height: Height,
    value_id: ValueId,
},
```

Do not include either payload, a URL, or database path in the error.

- [ ] **Step 5: Make database write accounting accept committed batches**

In `app/src/metrics.rs`, keep existing callers working and add test-only snapshots:

```rust
pub fn add_write_bytes(&self, bytes: u64) {
    self.add_writes(1, bytes);
}

pub fn add_writes(&self, writes: u64, bytes: u64) {
    self.db_write_bytes.inc_by(bytes);
    self.db_write_count.inc_by(writes);
}

#[cfg(test)]
pub(crate) fn write_count(&self) -> u64 {
    self.db_write_count.get()
}

#[cfg(test)]
pub(crate) fn write_bytes(&self) -> u64 {
    self.db_write_bytes.get()
}
```

Task 1 calls `add_write_bytes` only after a unique payload commit. Task 3 uses `add_writes` after the whole migration
transaction commits.

- [ ] **Step 6: Implement v2 lookup and conflict-safe insertion**

Replace the undecided half of the combined lookup with this focused database method:

```rust
fn get_undecided_block_data(
    &self,
    height: Height,
    value_id: ValueId,
) -> Result<Option<Bytes>, StoreError> {
    let start = Instant::now();
    let tx = self.db.begin_read()?;
    let table = tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)?;
    let value = table
        .get(&(height, value_id))?
        .map(|data| Bytes::copy_from_slice(data.value()));

    self.metrics.observe_read_time(start.elapsed());
    self.metrics
        .add_read_bytes(value.as_ref().map_or(0, |bytes| bytes.len() as u64));
    self.metrics.add_key_read_bytes(
        (size_of::<Height>() + size_of::<ValueId>()) as u64,
    );
    Ok(value)
}
```

Implement insertion so a duplicate or conflict drops the uncommitted transaction:

```rust
fn insert_undecided_block_data(
    &self,
    height: Height,
    value_id: ValueId,
    data: Bytes,
) -> Result<(), StoreError> {
    let start = Instant::now();
    let tx = self.db.begin_write()?;
    let inserted = {
        let mut table = tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)?;
        let key = (height, value_id);
        let existing = table.get(&key)?.map(|value| value.value().to_vec());
        match existing {
            Some(existing) if existing.as_slice() == data.as_ref() => false,
            Some(_) => {
                return Err(StoreError::ConflictingUndecidedBlockData { height, value_id });
            }
            None => {
                table.insert(key, data.to_vec())?;
                true
            }
        }
    };

    if !inserted {
        return Ok(());
    }

    tx.commit()?;
    self.metrics.observe_write_time(start.elapsed());
    self.metrics.add_write_bytes(data.len() as u64);
    Ok(())
}
```

- [ ] **Step 7: Add a test-only v2 row-count helper**

Keep table-shape assertions out of production interfaces:

```rust
#[cfg(test)]
fn undecided_block_data_len(&self) -> Result<u64, StoreError> {
    let tx = self.db.begin_read()?;
    Ok(tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)?.len()?)
}
```

- [ ] **Step 8: Run the focused store tests and format the touched Rust files**

Run:

```bash
cargo test -p emerald undecided_block_data_ -- --nocapture
cargo +nightly fmt -p emerald
cargo test -p emerald undecided_block_data_ -- --nocapture
```

Expected: all three focused tests pass after formatting; the metrics assertions remain `1` write and exactly the
original payload length.

- [ ] **Step 9: Commit the v2 storage primitive**

```bash
git add app/src/store/keys.rs app/src/store.rs app/src/metrics.rs
git commit -m "feat(store): add deduplicated undecided payload storage"
```

---

### Task 2: Move runtime flows to explicit shared-payload reads

**Files:**

- Modify: `app/src/store.rs:418-485`
- Modify: `app/src/store.rs:607-690`
- Modify: `app/src/store.rs:896-927`
- Modify: `app/src/state.rs:260-280`
- Modify: `app/src/state.rs:503-610`
- Modify: `app/src/state.rs:658-705`
- Modify: `app/src/state.rs:1033-1205`
- Modify: `app/src/app.rs:210-230`
- Modify: `app/src/app.rs:564-600`
- Modify: `app/src/app.rs:732-790`
- Modify: `tests/mbt/src/sut/get_value.rs:34-48`

**Interfaces:**

- Consumes: Task 1's `Db::{get_undecided_block_data,insert_undecided_block_data}`.
- Produces:

```rust
fn Db::get_decided_block_data(
    &self,
    height: Height,
) -> Result<Option<Bytes>, StoreError>;

pub async fn Store::get_decided_block_data(
    &self,
    height: Height,
) -> Result<Option<Bytes>, StoreError>;

pub async fn Store::get_undecided_block_data(
    &self,
    height: Height,
    value_id: ValueId,
) -> Result<Option<Bytes>, StoreError>;

pub async fn Store::store_undecided_block_data(
    &self,
    height: Height,
    value_id: ValueId,
    data: Bytes,
) -> Result<(), StoreError>;

pub async fn State::get_undecided_block_data(
    &self,
    height: Height,
    value_id: ValueId,
) -> Option<Bytes>;
```

- Removes the combined `get_block_data(height, round, value_id)` and the legacy runtime insertion method.
- Leaves `LEGACY_UNDECIDED_BLOCK_DATA_TABLE` defined for Task 3 migration only.

- [ ] **Step 1: Strengthen the restream test to cover multiple rounds and one payload row**

Rename `restream_proposal_stores_reproposal_at_current_round` to
`shared_undecided_block_data_survives_restream_rounds`. After the existing round-1 handler invocation and assertions,
invoke `prepare_restream_proposal` for round 2 and assert all metadata plus one payload row:

```rust
let round_two = Round::new(2);
let (round_two_value, round_two_bytes) = state
    .prepare_restream_proposal(height, proposal_round, round_two, value.id())
    .await
    .unwrap()
    .expect("round-two restream must reuse the stored value");

assert_eq!(round_two_value.round, round_two);
assert_eq!(round_two_bytes, bytes);
for round in [proposal_round, current_round, round_two] {
    assert!(state
        .store
        .get_undecided_proposal(height, round, value.id())
        .await
        .unwrap()
        .is_some());
}
assert_eq!(state.store.undecided_block_data_len().await.unwrap(), 1);
```

- [ ] **Step 2: Add failing decision and synced-recovery tests**

In the `app/src/state.rs` test module, import `ExecutionPayloadV1` and `ExecutionPayloadV2` and add a minimal valid SSZ
payload helper:

```rust
use alloy_rpc_types_engine::{ExecutionPayloadV1, ExecutionPayloadV2};

fn make_execution_payload_bytes() -> Bytes {
    let payload = ExecutionPayloadV3 {
        payload_inner: ExecutionPayloadV2 {
            payload_inner: ExecutionPayloadV1 {
                parent_hash: Default::default(),
                fee_recipient: Default::default(),
                state_root: Default::default(),
                receipts_root: Default::default(),
                logs_bloom: Default::default(),
                prev_randao: Default::default(),
                block_number: 1,
                gas_limit: 30_000_000,
                gas_used: 0,
                timestamp: 1,
                extra_data: Bytes::new(),
                base_fee_per_gas: Default::default(),
                block_hash: Default::default(),
                transactions: Vec::new(),
            },
            withdrawals: Vec::new(),
        },
        blob_gas_used: 0,
        excess_blob_gas: 0,
    };
    Bytes::from(payload.as_ssz_bytes())
}

#[tokio::test]
async fn shared_undecided_block_data_commits_from_later_round() {
    let (mut state, _dir) = make_test_state().await;
    let height = Height::new(1426);
    let source_round = Round::new(0);
    let decided_round = Round::new(2);
    let bytes = make_execution_payload_bytes();
    let value = Value::new(bytes.clone());
    let proposal = ProposedValue {
        height,
        round: source_round,
        valid_round: Round::Nil,
        proposer: state.address,
        value: value.clone(),
        validity: Validity::Valid,
    };
    state.store_undecided_value(&proposal, bytes.clone()).await.unwrap();
    state
        .prepare_restream_proposal(height, source_round, decided_round, value.id())
        .await
        .unwrap()
        .unwrap();

    state
        .commit(CommitCertificate {
            height,
            round: decided_round,
            value_id: value.id(),
            commit_signatures: Vec::new(),
        })
        .await
        .unwrap();

    assert_eq!(
        state.store.get_decided_block_data(height).await.unwrap(),
        Some(bytes)
    );
}
```

In the same module, import `on_process_synced_value` and test the recovery ingestion boundary:

```rust
use crate::app::{on_process_synced_value, on_restream_proposal};

#[tokio::test]
async fn shared_undecided_block_data_stores_synced_value() {
    let (mut state, _dir) = make_test_state().await;
    let height = Height::new(1426);
    let round = Round::new(3);
    let bytes = make_execution_payload_bytes();
    let value = Value::new(bytes.clone());
    let value_bytes = ProtobufCodec.encode(&value).unwrap();
    let (reply, receiver) = tokio::sync::oneshot::channel();

    on_process_synced_value(
        AppMsg::ProcessSyncedValue {
            height,
            round,
            proposer: state.address,
            value_bytes,
            reply,
        },
        &mut state,
    )
    .await
    .unwrap();

    assert_eq!(receiver.await.unwrap().unwrap().value, value);
    assert_eq!(
        state
            .store
            .get_undecided_block_data(height, value.id())
            .await
            .unwrap(),
        Some(bytes)
    );
}
```

- [ ] **Step 3: Run the state-flow tests and verify the red state**

Run:

```bash
cargo test -p emerald shared_undecided_block_data_ -- --nocapture
```

Expected: at least the restream row-count assertion fails because production still writes the legacy table, and the
new decided getter does not compile. Confirm all three named tests are discovered.

- [ ] **Step 4: Split decided and undecided reads in `Store`**

Extract the decided half of the old combined method into this method and matching async wrapper:

```rust
fn get_decided_block_data(&self, height: Height) -> Result<Option<Bytes>, StoreError> {
    let start = Instant::now();
    let tx = self.db.begin_read()?;
    let table = tx.open_table(DECIDED_BLOCK_DATA_TABLE)?;
    let value = table
        .get(&height)?
        .map(|data| Bytes::copy_from_slice(data.value()));

    self.metrics.observe_read_time(start.elapsed());
    self.metrics
        .add_read_bytes(value.as_ref().map_or(0, |bytes| bytes.len() as u64));
    self.metrics.add_key_read_bytes(size_of::<Height>() as u64);
    Ok(value)
}

pub async fn get_decided_block_data(
    &self,
    height: Height,
) -> Result<Option<Bytes>, StoreError> {
    let db = Arc::clone(&self.db);
    tokio::task::spawn_blocking(move || db.get_decided_block_data(height)).await?
}
```

Delete `Db::get_block_data`, `Store::get_block_data`, and the old round-taking
`Store::store_undecided_block_data`, together with `Db::insert_legacy_undecided_block_data`. Add async wrappers around
Task 1's v2 getter and insertion:

```rust
pub async fn get_undecided_block_data(
    &self,
    height: Height,
    value_id: ValueId,
) -> Result<Option<Bytes>, StoreError> {
    let db = Arc::clone(&self.db);
    tokio::task::spawn_blocking(move || db.get_undecided_block_data(height, value_id)).await?
}

pub async fn store_undecided_block_data(
    &self,
    height: Height,
    value_id: ValueId,
    data: Bytes,
) -> Result<(), StoreError> {
    let db = Arc::clone(&self.db);
    tokio::task::spawn_blocking(move || db.insert_undecided_block_data(height, value_id, data)).await?
}
```

Keep `LEGACY_UNDECIDED_BLOCK_DATA_TABLE` unopened until Task 3.

- [ ] **Step 5: Update every runtime call site**

Make these exact substitutions:

```rust
// State::get_latest_block_candidate
let raw_block_data = self
    .store
    .get_decided_block_data(certificate.height)
    .await
    .ok()
    .flatten()
    .expect("state: certificate should have associated block data");

// State wrapper used by on_decided
pub async fn get_undecided_block_data(
    &self,
    height: Height,
    value_id: ValueId,
) -> Option<Bytes> {
    self.store
        .get_undecided_block_data(height, value_id)
        .await
        .ok()
        .flatten()
}

// State::store_undecided_value
self.store
    .store_undecided_block_data(value.height, value.value.id(), data)
    .await?;

// State::commit
self.store
    .get_undecided_block_data(certificate.height, certificate.value_id)
    .await?;

// State::prepare_restream_proposal
self.store
    .get_undecided_block_data(height, value_id)
    .await?;
```

In `app/src/app.rs`, update `on_get_value` to call
`state.store.get_undecided_block_data(height, proposal.value.id())` and `on_decided_inner` to call
`state.get_undecided_block_data(height, value_id)`. In `tests/mbt/src/sut/get_value.rs`, call
`state.get_undecided_block_data(height, value_id)` and remove the now-unused local `round` binding.

- [ ] **Step 6: Move pruning and existing store tests to the v2 methods**

In `Db::prune`, retain the v2 table by height:

```rust
let mut undecided_block_data = tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)?;
undecided_block_data.retain(|k, _| k.0 >= block_data_retain_height)?;
```

In `test_prune`, insert undecided data with `(height, value_id)` and use explicit getters:

```rust
db.insert_undecided_block_data(
    Height::new(h),
    ValueId::new(h),
    Bytes::from(vec![h as u8; 40]),
)
.unwrap();

assert!(db.get_decided_block_data(Height::new(3)).unwrap().is_some());
assert!(db
    .get_undecided_block_data(Height::new(3), ValueId::new(3))
    .unwrap()
    .is_some());
```

Before pruning, insert a second payload at surviving height 3 under `ValueId::new(33)`. After pruning, assert both
height-3 value IDs remain independently retrievable. Retain the existing height-2 and height-1 absence assertions
using their corresponding explicit getter.

For the state-flow row-count assertion from Step 1, add this test-only async wrapper next to the v2 store methods:

```rust
#[cfg(test)]
pub async fn undecided_block_data_len(&self) -> Result<u64, StoreError> {
    let db = Arc::clone(&self.db);
    tokio::task::spawn_blocking(move || db.undecided_block_data_len()).await?
}
```

- [ ] **Step 7: Run state, pruning, package, and MBT compile checks**

Run:

```bash
cargo test -p emerald shared_undecided_block_data_ -- --nocapture
cargo test -p emerald test_prune -- --nocapture
cargo test -p emerald --lib
cargo test -p emerald-mbt --no-run
rg -n "get_block_data\(|store_undecided_block_data\([^\n]*round" app tests/mbt
```

Expected: the three shared-payload tests, pruning tests, and Emerald library tests pass; the MBT crate compiles; `rg`
finds no combined lookup or round-taking payload write. A zero-test Cargo result is not acceptable evidence.

- [ ] **Step 8: Format and commit the runtime conversion**

```bash
cargo +nightly fmt -p emerald
cargo +nightly fmt -p emerald-mbt
git add app/src/store.rs app/src/state.rs app/src/app.rs tests/mbt/src/sut/get_value.rs
git commit -m "refactor(store): share undecided payloads across rounds"
```

---

### Task 3: Migrate legacy round-keyed payloads atomically

**Files:**

- Modify: `app/src/store.rs:107-125`
- Modify: `app/src/store.rs:532-548`
- Modify: `app/src/store.rs:732-748`
- Modify: `app/src/store.rs:973-1289`

**Interfaces:**

- Consumes: `LEGACY_UNDECIDED_BLOCK_DATA_TABLE`, `UNDECIDED_BLOCK_DATA_TABLE`, Task 1's conflict error, and
  `DbMetrics::add_writes`.
- Produces:

```rust
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct UndecidedBlockDataMigrationStats {
    legacy_rows: u64,
    inserted_payloads: u64,
    duplicate_payloads: u64,
    inserted_bytes: u64,
}

fn Db::initialize_schema(&self) -> Result<(), StoreError>;

fn Db::migrate_undecided_block_data(
    tx: &redb::WriteTransaction,
) -> Result<UndecidedBlockDataMigrationStats, StoreError>;
```

- `Store::open` calls `initialize_schema` instead of `create_tables`.

- [ ] **Step 1: Add helpers that construct and inspect a real legacy database**

In the `store.rs` test module, write legacy rows through the old typed table definition without calling schema
initialization:

```rust
fn create_legacy_test_db(
    name: &str,
    rows: &[(Height, Round, ValueId, Bytes)],
) -> (Db, tempfile::TempDir, DbMetrics) {
    let dir = tempfile::tempdir().unwrap();
    let metrics = DbMetrics::new();
    let db = Db::new(
        dir.path().join(format!("{name}.redb")),
        1024 * 1024,
        metrics.clone(),
    )
    .unwrap();
    let tx = db.db.begin_write().unwrap();
    {
        let mut table = tx.open_table(LEGACY_UNDECIDED_BLOCK_DATA_TABLE).unwrap();
        for (height, round, value_id, bytes) in rows {
            table
                .insert((*height, *round, *value_id), bytes.to_vec())
                .unwrap();
        }
    }
    tx.commit().unwrap();
    (db, dir, metrics)
}

fn has_table(db: &Db, name: &str) -> bool {
    use redb::TableHandle;

    let tx = db.db.begin_read().unwrap();
    tx.list_tables()
        .unwrap()
        .any(|table| table.name() == name)
}
```

- [ ] **Step 2: Write the successful migration and reopen tests**

```rust
#[test]
fn legacy_undecided_block_data_migration_deduplicates_and_reopens() {
    let height = Height::new(9);
    let first_id = ValueId::new(21);
    let second_id = ValueId::new(22);
    let first = Bytes::from_static(b"first-payload");
    let second = Bytes::from_static(b"second-payload");
    let (db, dir, metrics) = create_legacy_test_db(
        "legacy_success",
        &[
            (height, Round::new(0), first_id, first.clone()),
            (height, Round::new(1), first_id, first.clone()),
            (height, Round::new(2), second_id, second.clone()),
        ],
    );

    db.initialize_schema().unwrap();
    assert!(!has_table(&db, "undecided_block_data"));
    assert!(has_table(&db, "undecided_block_data_v2"));
    assert_eq!(db.undecided_block_data_len().unwrap(), 2);
    assert_eq!(metrics.write_count(), 2);
    assert_eq!(metrics.write_bytes(), (first.len() + second.len()) as u64);

    let path = dir.path().join("legacy_success.redb");
    drop(db);
    let reopened = Db::new(path, 1024 * 1024, DbMetrics::new()).unwrap();
    reopened.initialize_schema().unwrap();
    assert_eq!(
        reopened.get_undecided_block_data(height, first_id).unwrap(),
        Some(first)
    );
    assert_eq!(
        reopened.get_undecided_block_data(height, second_id).unwrap(),
        Some(second)
    );
}

#[test]
fn legacy_undecided_block_data_migration_new_database_never_creates_legacy_table() {
    let (db, _dir, _metrics) = create_test_db_with_metrics("new_v2_only");
    assert!(!has_table(&db, "undecided_block_data"));
    assert!(has_table(&db, "undecided_block_data_v2"));
}
```

- [ ] **Step 3: Write the conflicting migration rollback test**

```rust
#[test]
fn legacy_undecided_block_data_migration_conflict_rolls_back() {
    let height = Height::new(9);
    let value_id = ValueId::new(21);
    let (db, _dir, metrics) = create_legacy_test_db(
        "legacy_conflict",
        &[
            (height, Round::new(0), value_id, Bytes::from_static(b"first")),
            (height, Round::new(1), value_id, Bytes::from_static(b"different")),
        ],
    );

    let error = db.initialize_schema().unwrap_err();
    assert!(matches!(
        error,
        StoreError::ConflictingUndecidedBlockData {
            height: error_height,
            value_id: error_value_id,
        } if error_height == height && error_value_id == value_id
    ));
    assert!(has_table(&db, "undecided_block_data"));
    assert!(!has_table(&db, "undecided_block_data_v2"));
    assert_eq!(metrics.write_count(), 0);
    assert_eq!(metrics.write_bytes(), 0);
}
```

- [ ] **Step 4: Run migration tests and verify the red state**

Run:

```bash
cargo test -p emerald legacy_undecided_block_data_migration_ -- --nocapture
```

Expected: compilation fails because `initialize_schema` and migration statistics do not exist. Confirm Cargo
discovers all three named migration tests.

- [ ] **Step 5: Implement atomic schema initialization and migration**

Import `redb::TableHandle`, add `UndecidedBlockDataMigrationStats`, and replace `create_tables` with
`initialize_schema`. Detect the legacy table before opening any table so a new database does not create it:

```rust
fn initialize_schema(&self) -> Result<(), StoreError> {
    let start = Instant::now();
    let tx = self.db.begin_write()?;
    let legacy_exists = tx
        .list_tables()?
        .any(|table| table.name() == LEGACY_UNDECIDED_BLOCK_DATA_TABLE.name());

    {
        let _ = tx.open_table(DECIDED_VALUES_TABLE)?;
        let _ = tx.open_table(CERTIFICATES_TABLE)?;
        let _ = tx.open_table(UNDECIDED_PROPOSALS_TABLE)?;
        let _ = tx.open_table(DECIDED_BLOCK_DATA_TABLE)?;
        let _ = tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)?;
        let _ = tx.open_table(DECIDED_BLOCK_HEADERS_TABLE)?;
        let _ = tx.open_table(PERSISTENT_METRICS_TABLE)?;
        let _ = tx.open_table(PENDING_PROPOSAL_PARTS_TABLE)?;
    }

    let migration = legacy_exists
        .then(|| Self::migrate_undecided_block_data(&tx))
        .transpose()?;
    tx.commit()?;

    if let Some(stats) = migration {
        self.metrics.observe_write_time(start.elapsed());
        self.metrics
            .add_writes(stats.inserted_payloads, stats.inserted_bytes);
        tracing::info!(
            event = "undecided_block_data_migration",
            legacy_rows = stats.legacy_rows,
            inserted_payloads = stats.inserted_payloads,
            duplicate_payloads = stats.duplicate_payloads,
            duration_seconds = start.elapsed().as_secs_f64(),
            "Migrated legacy undecided block data"
        );
    }

    Ok(())
}
```

Implement the transaction-local migration. Copy each legacy value before querying or mutating the target table so no
redb access guard spans a mutable insertion:

```rust
fn migrate_undecided_block_data(
    tx: &redb::WriteTransaction,
) -> Result<UndecidedBlockDataMigrationStats, StoreError> {
    let mut stats = UndecidedBlockDataMigrationStats::default();
    {
        let legacy = tx.open_table(LEGACY_UNDECIDED_BLOCK_DATA_TABLE)?;
        let mut target = tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)?;
        for entry in legacy.iter()? {
            let (legacy_key, legacy_value) = entry?;
            let (height, _round, value_id) = legacy_key.value();
            let payload = legacy_value.value().to_vec();
            let key = (height, value_id);
            let existing = target.get(&key)?.map(|value| value.value().to_vec());

            stats.legacy_rows += 1;
            match existing {
                Some(existing) if existing == payload => stats.duplicate_payloads += 1,
                Some(_) => {
                    tracing::error!(
                        event = "undecided_block_data_migration_conflict",
                        %height,
                        value = %value_id,
                        "Conflicting legacy undecided block data"
                    );
                    return Err(StoreError::ConflictingUndecidedBlockData { height, value_id });
                }
                None => {
                    stats.inserted_payloads += 1;
                    stats.inserted_bytes += payload.len() as u64;
                    target.insert(key, payload)?;
                }
            }
        }
    }
    tx.delete_table(LEGACY_UNDECIDED_BLOCK_DATA_TABLE)?;
    Ok(stats)
}
```

Change `Store::open`, both test helpers, and the new-database reopen assertion from Task 1 to call
`initialize_schema`. Delete `create_tables`; `rg -n "create_tables" app/src/store.rs` must return no matches after
this step.

- [ ] **Step 6: Test defensive merge with an existing v2 row**

Add `legacy_undecided_block_data_migration_merges_existing_v2_data`. Pre-create v2 and legacy tables in the same raw
database, insert the same payload into v2 and two legacy rounds, run initialization, and assert:

```rust
assert_eq!(db.undecided_block_data_len().unwrap(), 1);
assert_eq!(metrics.write_count(), 0);
assert_eq!(metrics.write_bytes(), 0);
assert!(!has_table(&db, "undecided_block_data"));
```

This pins idempotent merge behavior without weakening the one-transaction migration.

- [ ] **Step 7: Run migration, reopen, and package tests**

Run:

```bash
cargo test -p emerald legacy_undecided_block_data_migration_ -- --nocapture
cargo test -p emerald undecided_block_data_ -- --nocapture
cargo test -p emerald shared_undecided_block_data_ -- --nocapture
cargo test -p emerald --lib
```

Expected: four migration tests and all runtime/store tests pass. The conflict test leaves the legacy table intact and
reports zero successful write metrics.

- [ ] **Step 8: Format and commit the migration**

```bash
cargo +nightly fmt -p emerald
git add app/src/store.rs
git commit -m "feat(store): migrate undecided payloads to v2"
```

---

### Task 4: Document the one-way storage upgrade

**Files:**

- Modify: `docs/operational-docs/src/production-network/running-emerald.md:33-42`

**Interfaces:**

- Consumes: Task 3's startup migration behavior and structured `undecided_block_data_migration` event.
- Produces: operator instructions for backup, temporary disk headroom, conflict recovery, downgrade, and space
  reclamation expectations.

- [ ] **Step 1: Locate the upgrade guidance boundary**

Open `docs/operational-docs/src/production-network/running-emerald.md` and place the new section after `## Start` and
before `## Monitoring`. Do not move or rewrite the surrounding operational instructions.

- [ ] **Step 2: Add the migration runbook**

Add this exact section:

```markdown
## Undecided Payload Storage Upgrade

The first startup with the round-independent payload schema migrates the local redb
`undecided_block_data` table to `undecided_block_data_v2`. The migration keeps one payload per
`(height, value_id)`, removes identical round duplicates, and deletes the legacy table in the same transaction.

Before upgrading each node:

1. Stop Emerald cleanly.
2. Back up `store.redb`.
3. Reserve temporary free space for approximately one deduplicated undecided payload set plus redb overhead.
4. Start the new binary and wait for the `undecided_block_data_migration` event before upgrading another validator.

If legacy rows use the same `(height, value_id)` with different bytes, startup fails without changing the legacy
table. Investigate or restore the backup; the node must not choose either payload automatically.

The migration is one-way. To run an older Emerald binary, restore the database backup taken before the upgrade.
Deleting the legacy table frees redb pages for reuse but might not reduce the file size immediately. Startup does not
run full database compaction; reclaiming filesystem space is a separate offline maintenance operation.
```

- [ ] **Step 3: Verify scope and Markdown line length**

Run:

```bash
awk 'length($0) > 120 { print FNR ":" length($0) ":" $0 }' \
  docs/operational-docs/src/production-network/running-emerald.md
rg -n "store\.redb|undecided_block_data_v2|one-way|compaction" \
  docs/operational-docs/src/production-network/running-emerald.md
git diff --check -- docs/operational-docs/src/production-network/running-emerald.md
```

Expected: the line-length command prints nothing, all four operational topics are present, and `git diff --check`
reports no whitespace errors.

- [ ] **Step 4: Commit the runbook**

```bash
git add docs/operational-docs/src/production-network/running-emerald.md
git commit -m "docs: explain undecided payload storage migration"
```

---

### Task 5: Run the complete verification gate

**Files:**

- Verify: `app/src/store/keys.rs`
- Verify: `app/src/store.rs`
- Verify: `app/src/metrics.rs`
- Verify: `app/src/state.rs`
- Verify: `app/src/app.rs`
- Verify: `tests/mbt/src/sut/get_value.rs`
- Verify: `docs/operational-docs/src/production-network/running-emerald.md`

**Interfaces:**

- Consumes: Tasks 1-4 and the repository's Rust and documentation quality gates.
- Produces: fresh focused-test, package-test, workspace-test, compile, lint, formatting, and diff evidence.

- [ ] **Step 1: Re-run the deterministic regression tests**

Run each filter separately so a failure names the affected contract:

```bash
cargo test -p emerald undecided_block_data_ -- --nocapture
cargo test -p emerald legacy_undecided_block_data_migration_ -- --nocapture
cargo test -p emerald shared_undecided_block_data_ -- --nocapture
cargo test -p emerald test_prune -- --nocapture
```

Expected: Cargo discovers non-zero tests for every filter and all discovered tests pass.

- [ ] **Step 2: Run package and MBT gates**

Run:

```bash
cargo nextest run --all-targets --all-features -p emerald
cargo test -p emerald-mbt --no-run
```

Expected: the Emerald package suite passes and the model-based test crate compiles against the new state interface.

- [ ] **Step 3: Run the workspace test gate**

Run:

```bash
cargo test --workspace --exclude emerald-mbt
```

Expected: all runnable workspace tests pass. If a failure is unrelated to this branch, preserve its exact command and
error output, prove the changed Emerald package remains green with Step 2, and report the baseline separately.

- [ ] **Step 4: Run required lint and formatting gates**

Run the repository-required commands exactly:

```bash
cargo clippy --tests -- -D warnings
cargo +nightly fmt --all --check
```

Expected: both commands succeed. If workspace-wide Clippy fails outside the changed packages, capture the exact
failure and additionally run this non-substitute diagnostic:

```bash
cargo clippy -p emerald --tests --no-deps -- -D warnings
```

Do not edit or format `Cargo.lock` while addressing findings.

- [ ] **Step 5: Audit schema names, round coupling, and sensitive logging**

Run:

```bash
rg -n 'undecided_block_data(_v2)?' app docs/operational-docs
rg -n 'get_block_data\(' app tests/mbt
rg -n 'store_undecided_block_data' app tests/mbt
rg -n 'ConflictingUndecidedBlockData|undecided_block_data_migration' app/src
```

Verify manually from the results:

- `undecided_block_data` appears only as the legacy migration table name and in operator documentation.
- `undecided_block_data_v2` is the only runtime table for undecided payloads.
- no combined `get_block_data` method remains.
- every runtime payload write uses `(height, value_id, data)` without `round`.
- conflict errors and migration logs contain identifiers and counts, never payload bytes or database paths.

- [ ] **Step 6: Review the final diff and repository state**

Run:

```bash
git diff --check origin/om-emerald...
git diff --stat origin/om-emerald...
git status --short --branch
```

Read the complete diff. Confirm it contains only the planned storage, runtime, tests, metrics, and runbook changes;
does not alter proposal wire types or decided retention; and does not modify `Cargo.lock`.

- [ ] **Step 7: Commit only verification-driven corrections**

If Steps 1-6 required code or documentation fixes, stage only those files, inspect the staged diff, and commit:

```bash
git diff --cached --check
git commit -m "fix: address undecided payload verification findings"
```

If verification required no changes, do not create an empty commit. Record every command and result in the handoff.
