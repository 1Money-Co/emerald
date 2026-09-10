# Undecided Payload Rollback Hardening Design

## Context

PR #22 introduces `undecided_block_data_v2`, keyed by `(height, value_id)`, and originally deleted the legacy
round-keyed table during startup migration. Review identified two upgrade hazards:

- an N-1 crash between decided-metadata persistence and decided-payload persistence leaves recoverable bytes only in
  the legacy undecided table, but N currently migrates those bytes without promoting them to decided storage;
- deleting the legacy table means an in-place N -> N-1 rollback cannot resolve proposal metadata written while N ran.

The v2 key remains `(height, value_id)`. `round` remains proposal metadata and is passed through temporary internal
store interfaces only to maintain exact N-1 compatibility.

## Goals

- Recover a valid N-1 partial decided commit atomically during schema initialization.
- Make new decided commits atomic across payload, value, certificate, and header.
- Preserve an in-place N -> N-1 -> N database rollback path for one release window.
- Keep same-ID/different-byte conflicts fail-closed without overwriting stored data.
- Avoid payload-sized allocations when checking duplicate redb rows.

## Non-goals

- Removing the legacy table in this release.
- Adding a storage schema version or network protocol activation height.
- Building a new multi-version, multi-node integration framework in this PR.
- Repairing corrupted local data by choosing among conflicting payloads.

## Compatibility-window storage

Initialization creates both payload tables:

| Table | Key | Purpose during this release |
| --- | --- | --- |
| `undecided_block_data_v2` | `(height, value_id)` | Primary N read path and final deduplicated layout |
| `undecided_block_data` | `(height, round, value_id)` | N-1 rollback shadow |

Initialization first copies legacy rows into v2, accepting identical cross-round bytes and rejecting conflicts. It
does not delete the legacy table. It then backfills missing legacy rows for v2-backed proposal metadata, covering
databases already opened by an earlier PR #22 build that deleted the legacy table.

Runtime writes receive the proposal round and update missing v2 and legacy rows in one redb transaction. Existing
rows are compared without the additional `.to_vec()` copy before mutation. Pinned redb 2.6.3 still decodes the
existing `Vec<u8>` table value into an owned vector; changing the table to a borrowed-slice value type would break the
recorded N-1 redb type. Any conflict aborts the transaction. Runtime reads prefer v2 and fall back to the exact legacy
key. Pruning removes expired rows from both tables in the same transaction.

The shadow table necessarily retains round duplicates during the compatibility window. A later activated release may
remove the dual-write and legacy fallback after operators no longer need N-1 rollback.

## Legacy partial-commit recovery

Schema initialization examines each decided value row. A complete metadata set consists of the decided value,
certificate, and stored execution header. If decided payload bytes are absent, initialization resolves the payload by
the certificate's `(height, round, value_id)`, preferring v2 after migration and falling back to legacy.

Before promotion, initialization verifies:

1. the certificate height matches the decided-value key;
2. the stored value ID matches the certificate value ID;
3. hashing the candidate payload produces that same value ID;
4. the candidate bytes equal the payload embedded in the stored value;
5. the payload decodes as `ExecutionPayloadV3`;
6. the extracted header bytes equal the stored decided header.

The migration, compatibility backfill, validation, and decided-payload promotion share one redb write transaction.
Missing metadata, missing payload, malformed payload, or conflicting bytes abort startup without mutation.

## Atomic decided commits

Replace the separate decided-payload and decided-metadata production methods with one store operation receiving the
certificate, value, execution header, and payload. One redb transaction inserts or verifies all four rows and commits
only after every row succeeds. Existing identical rows make retries idempotent; any conflicting row aborts without
overwriting data. Database metrics advance only after commit and count rows and bytes actually inserted.

Test-only failure injection returns an error after each table write. Reopening the database after each injected failure
must show that none of the four rows became visible.

## Verification

Deterministic store tests cover:

- an N-1 partial commit upgraded and reopened by N;
- missing and conflicting recovery data rolling back the entire startup transaction;
- N-1 schema -> N -> N-1-compatible access -> N reopen;
- new N writes being present in both v2 and the legacy rollback table;
- v2-first reads and exact legacy fallback;
- pruning both layouts;
- atomic decided commit failure injection at every write boundary;
- multi-megabyte duplicate inserts without adding physical writes or an explicit second full-payload copy.

These tests pin the database contract implemented by N-1. A real mixed-binary rolling-network exercise remains a
release qualification step because this repository has no versioned multi-node integration harness. Documentation
must not claim that exercise passed unless it is run separately.

## Operations

The upgrade remains rolling and wire-compatible. Operators stop and back up each node before upgrading. Unlike the
earlier one-way design, the same database remains readable by N-1 during the compatibility window. Operators must
still quiesce a node before changing binaries and must not run two Emerald processes against one database.

The migration log reports copied v2 payloads, compatibility backfills, recovered partial commits, and committed
bytes. The runbook explains the temporary storage overhead and the later legacy-removal requirement.
