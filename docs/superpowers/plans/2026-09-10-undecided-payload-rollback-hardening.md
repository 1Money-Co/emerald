# Undecided Payload Rollback Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make PR #22 crash-recoverable and temporarily downgrade-compatible while preserving the round-independent
v2 payload key.

**Architecture:** Retain and atomically dual-write the N-1 table for one release window while using v2 as the primary
read path. Reconcile valid N-1 partial commits during startup and replace split decided writes with one transaction.

**Tech Stack:** Rust, redb, Tokio, SSZ `ExecutionPayloadV3`, Cargo Nextest

**Spec:** `docs/superpowers/specs/2026-09-10-undecided-payload-rollback-hardening-design.md`

## Global Constraints

- Keep the v2 payload key exactly `(height, value_id)`; `round` remains round-specific proposal metadata.
- Identical duplicate bytes are idempotent; same-key different bytes fail closed without overwrite.
- Keep local conflict diagnostics free of payload contents.
- Do not claim a real rolling-network test passed without a versioned multi-node run.
- Keep Markdown lines at or below 120 columns.

---

### Task 1: Compatibility-window schema and runtime access

**Files:**

- Modify: `app/src/store.rs`
- Modify: `app/src/state.rs`
- Modify: `app/src/app.rs`
- Modify: `tests/mbt/src/sut/get_value.rs`

**Interfaces:**

- Produces `Db::insert_undecided_block_data(height, round, value_id, data)`.
- Produces `Db::get_undecided_block_data(height, round, value_id)` with v2-first fallback.
- Produces matching asynchronous `Store` methods.

- [x] **Step 1: Write failing compatibility tests**

Add store tests that retain the legacy table after migration, dual-write new rows, fall back to legacy, prune both
tables, and exercise N-1 schema -> N -> N-1-compatible read -> N reopen.

- [x] **Step 2: Run the focused tests and verify red**

Run:

```bash
cargo test -p emerald rollback_compatibility_ -- --nocapture
cargo test -p emerald legacy_undecided_block_data_migration_ -- --nocapture
```

Expected: assertions fail because initialization deletes the legacy table and runtime writes only v2.

- [x] **Step 3: Implement retained migration, dual-write, fallback, and pruning**

Open both schemas during initialization, migrate legacy rows into v2 without deletion, and backfill the legacy table
from v2-backed proposal keys. Change internal runtime methods to accept `round`, avoid an explicit second `to_vec`
copy after redb's owned `Vec<u8>` decode, insert missing rows in one transaction, and prune both tables.

- [x] **Step 4: Update all callers**

Pass the proposal, certificate, or consensus round from `state.rs`, `app.rs`, and the MBT SUT. Do not add `round` to
`UndecidedBlockDataKey`.

- [x] **Step 5: Run compatibility and state-flow tests**

Run:

```bash
cargo test -p emerald rollback_compatibility_ -- --nocapture
cargo test -p emerald legacy_undecided_block_data_migration_ -- --nocapture
cargo test -p emerald shared_undecided_block_data_ -- --nocapture
cargo test -p emerald test_prune -- --nocapture
```

Expected: all discovered tests pass and no filter reports zero tests.

### Task 2: Legacy partial-commit recovery

**Files:**

- Modify: `app/src/store.rs`
- Test: `app/src/store.rs`

**Interfaces:**

- Produces `Db::recover_legacy_partial_commits(&WriteTransaction)`.
- Produces a height-scoped `StoreError` for irrecoverable decided state.

- [x] **Step 1: Write failing recovery tests**

Construct N-1 raw tables with value, certificate, header, and legacy undecided payload but no decided payload. Assert
that initialization promotes valid data and reopens successfully. Add absence, malformed payload, wrong ID, wrong
header, and existing-conflict cases that assert the whole initialization transaction is unchanged.

- [x] **Step 2: Run recovery tests and verify red**

Run:

```bash
cargo test -p emerald legacy_partial_commit_ -- --nocapture
```

Expected: the valid case has no decided payload because recovery is not implemented.

- [x] **Step 3: Implement transaction-local validation and promotion**

Decode metadata with errors rather than silently dropping it. Resolve the certificate-bound payload, recompute its
`ValueId`, compare it with the stored `Value`, decode SSZ, compare the extracted header, and insert decided bytes only
after every check succeeds.

- [x] **Step 4: Run recovery and migration tests**

Run:

```bash
cargo test -p emerald legacy_partial_commit_ -- --nocapture
cargo test -p emerald legacy_undecided_block_data_migration_ -- --nocapture
```

Expected: every case passes; failure cases preserve their pre-initialization tables.

### Task 3: Atomic decided persistence

**Files:**

- Modify: `app/src/store.rs`
- Modify: `app/src/state.rs`
- Test: `app/src/store.rs`

**Interfaces:**

- Produces `Db::insert_decided_state(decided_value, block_header_bytes, block_data)`.
- Produces `Store::store_decided_state(certificate, value, block_header_bytes, block_data)`.

- [x] **Step 1: Write failing idempotency, conflict, and failpoint tests**

Test one successful four-row commit, an identical retry, conflicts in each component, and injected failure after each
table write. Reopen after every failure and assert no partial decided state is visible.

- [x] **Step 2: Run atomic commit tests and verify red**

Run:

```bash
cargo test -p emerald decided_state_commit_ -- --nocapture
```

Expected: compilation or assertions fail because the four-row operation does not exist.

- [x] **Step 3: Implement the single transaction**

Insert or verify payload, value, certificate, and header in one redb write transaction. Drop borrowed guards before
mutation, update metrics only after commit, and keep the failpoint parameter test-only.

- [x] **Step 4: Replace the production split write**

Change `State::commit` to call only `Store::store_decided_state`. Remove production-facing methods that can publish a
partial decided state and update test setup to use the atomic method or raw N-1 fixtures.

- [x] **Step 5: Run atomic and state commit tests**

Run:

```bash
cargo test -p emerald decided_state_commit_ -- --nocapture
cargo test -p emerald shared_undecided_block_data_commits_from_later_round -- --nocapture
```

Expected: all discovered tests pass.

### Task 4: Allocation-safe comparisons and operator contract

**Files:**

- Modify: `app/src/store.rs`
- Modify: `docs/operational-docs/src/production-network/running-emerald.md`
- Modify: `.changelog/unreleased/breaking-changes/22-deduplicate-undecided-block-data.md`
- Modify: `docs/superpowers/specs/2026-09-09-undecided-block-data-deduplication-design.md`

**Interfaces:**

- Consumes the compatibility-window and atomic persistence behavior from Tasks 1-3.

- [x] **Step 1: Add a large duplicate regression**

Insert the same multi-megabyte payload twice, assert one v2 row and an idempotent result, and keep the implementation
free of `value().to_vec()` on duplicate-comparison paths.

- [x] **Step 2: Update documentation**

Replace one-way rollback instructions with the compatibility window, temporary storage overhead, quiesced binary
switch, later legacy removal, and honest release-qualification boundary.

- [x] **Step 3: Run static and focused checks**

Run:

```bash
rg -n 'value\(\)\.to_vec\(\)' app/src/store.rs
cargo test -p emerald undecided_block_data_ -- --nocapture
```

Expected: no duplicate-comparison allocation remains and all discovered storage tests pass.

### Task 5: Full verification and GitHub follow-up

**Files:**

- Verify all modified files.
- Update the existing Simon review threads through GitHub after push.

**Interfaces:**

- Consumes all preceding tasks.

- [x] **Step 1: Run the practical test gates**

Run:

```bash
cargo nextest run --workspace --all-features --no-fail-fast --failure-output final \
  --filterset 'not package(emerald-mbt)'
cargo test -p emerald-mbt --no-run
cargo clippy --tests -- -D warnings
cargo +nightly fmt --all --check
git diff --check
```

- [x] **Step 2: Inspect the final diff**

Confirm the v2 key remains round-independent, all four decided rows share one production transaction, the legacy
table is retained, and documentation does not claim unexecuted rolling-network evidence.

- [ ] **Step 3: Commit and push**

Stage only review-follow-up files, commit with a plain descriptive message, and push
`seb/deduplicate-undecided-block-data`.

- [ ] **Step 4: Reply to Simon's review threads**

Summarize the validated fix and tests on the two mandatory threads. On the optional allocation thread, identify the
borrowed-guard comparison change and large-payload regression. State separately that a real rolling-network exercise
is a release qualification step, not an in-repo test result.
