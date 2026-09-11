# Versioned Proposal Storage Review Hardening Design

## Status

Approved in design discussion on 2026-09-11. This specification supersedes the same-table compact proposal encoding
selected in `2026-09-10-compact-undecided-proposal-metadata-design.md` for the N-1 compatibility release. The earlier
document remains as decision history.

## Context

Emerald PR #22 deduplicates the primary undecided execution payload layout by moving payloads from the round-keyed
`undecided_block_data` table to `undecided_block_data_v2`, keyed by `(height, value_id)`. The first review-hardening
round retained the legacy block-data table as an N-1 rollback shadow. A later change compacted the existing
`undecided_values` table in place by storing only an eight-byte `ValueId` in the nested value.

Simon's second review identified two remaining rollout defects.

First, the in-place compact encoding is readable but not operationally safe in N-1. N-1 decodes it as a `Value` with
the correct ID and empty extensions. If N-1 commits that proposal, its decided-value table contains the ID-only value.
Its value-sync path serializes the stored value rather than retrieving the separately stored decided payload. An N
peer rejects the ID/empty-payload mismatch; an N-1 peer accepts it and may later try to decode empty bytes as an
`ExecutionPayloadV3`. Repairing the row only when N returns is too late for peers syncing while N-1 is active.

Second, startup currently uses the retained legacy table's existence as the migration trigger. Because that table is
deliberately retained, every restart rescans all legacy payloads and proposals and revalidates retained decided
payloads, including hashing and SSZ decoding. This makes ordinary validator restart cost proportional to retained
payload volume after migration has already succeeded.

## Goals

- Keep proposal values wire-complete for N-1 commit and sync throughout the compatibility release.
- Preserve compact, payload-free proposal metadata as N's primary proposal layout.
- Upgrade databases created by N-1, earlier PR #22 revisions, or partially completed attempts atomically.
- Mark successful one-time reconciliation durably and skip heavy scans on ordinary later starts.
- Continue recovering a real N-1 partial decided commit after the one-time migration.
- Keep missing data and same-ID/different-byte conflicts fail-closed without partial mutation.
- Distinguish deterministic database and wire-contract tests from opt-in real-binary release qualification.
- Extend phase-two cleanup to remove both rollback compatibility shadows.

## Non-goals

- Removing N-1 rollback support in this release.
- Eliminating all physical round-keyed payload duplication while N-1 rollback remains supported.
- Changing the public Malachite `ProposedValue`, `Value`, certificate, or sync wire formats.
- Adding a permanent general-purpose mixed-version network framework.
- Running redb file compaction during node startup.
- Using wall-clock thresholds as correctness assertions in normal tests.

## Considered Approaches

### Separate full v1 and compact v2 proposal tables

Keep `undecided_values` wire-complete for N-1 and add `undecided_values_v2` for compact N metadata. N dual-writes both
and reads v2 first with a verified v1 fallback. This is selected because it preserves the existing rollback contract
without weakening wire-value validation.

The cost is explicit temporary duplication: full proposal rows and the legacy block-data shadow both remain
round-keyed during the compatibility release. The active v2 layout is deduplicated, but full physical reclamation is
deferred to the activated cleanup release.

### Keep compact rows in the shared table and repair later

This is rejected. Re-upgrade repair cannot protect N-1 peers that serve or consume an ID-only decided value while N-1
is still active.

### Require a pre-downgrade rehydration command

N could rewrite compact rows to full proposals before an operator starts N-1. This avoids a second proposal table but
makes rollback depend on a new procedural step being completed successfully during an incident. It is rejected in
favor of continuously backward-readable storage.

### Defer all proposal compaction until compatibility ends

This is safe but leaves no compact primary proposal layout in the current release and discards the implementation
already developed for issue #318. A versioned compatibility shadow gives the same rollback safety with a clear
cleanup path.

## Storage Model

| Table | Key | Contents | Compatibility role |
| --- | --- | --- | --- |
| `undecided_values_v2` | `(height, round, value_id)` | Proposal identity and round fields; no payload | N primary |
| `undecided_values` | `(height, round, value_id)` | Full wire-complete `ProposedValue` | N-1 proposal shadow |
| `undecided_block_data_v2` | `(height, value_id)` | One shared execution payload | N primary |
| `undecided_block_data` | `(height, round, value_id)` | Full execution payload | N-1 payload shadow |
| `storage_schema_metadata` | string | Unsigned phase version | N-only migration state |

The proposal v2 row uses the storage-only protobuf envelope already implemented by PR #22: the nested
`proto::Value.value` contains exactly the eight-byte big-endian `ValueId`. It is never passed through the network
decoder. The v1 proposal row uses the normal `ProtobufCodec`, including `Value.extensions`, and remains readable and
safe to commit or sync with N-1.

The metadata table contains an `undecided_storage_reconciliation_version` entry. Version `1` means the complete
payload, proposal, rollback-shadow, and existing decided-state reconciliation described below committed atomically.
Table existence alone is never a migration-completion signal.

## Runtime Write Contract

`State::store_undecided_value` retains payload-before-proposal ordering:

1. One payload transaction inserts or verifies v2 `(height, value_id)` and legacy
   `(height, round, value_id)` rows.
2. One proposal transaction inserts full v1 and compact v2 rows for `(height, round, value_id)`.

Both proposal rows are first-write-wins for the same key. A retry does not replace authoritative proposer metadata.
Different bytes under the same payload identity remain a storage-integrity error and abort the payload transaction.

Writing the two proposal representations in one transaction prevents N from publishing only one representation.
Payload storage still commits first, so a crash can leave an unused payload but cannot deliberately publish proposal
metadata whose referenced bytes were never stored.

Pruning removes expired rows from both proposal tables and both payload tables in the same transaction.

## Runtime Read Contract

N reads proposals as follows:

1. Look up compact metadata in `undecided_values_v2`.
2. If present, load the payload from v2 with exact-round legacy fallback, recompute its `ValueId`, verify the key and
   metadata, and hydrate a full `ProposedValue`.
3. If compact metadata is absent, look up the full v1 row. Decode it through strict `Value::from_proto`, require the
   row fields to match the key, resolve the shared or exact legacy payload, and require both ID and bytes to match.
4. Return absence only when neither proposal representation exists. Existing metadata with missing or inconsistent
   payload data is an integrity error.

The v1 fallback is not treated as a failed migration. It is the supported path for proposals N-1 creates after an
in-place rollback. Re-upgrade therefore remains correct without rescanning all proposal rows on every startup.

N-1 continues to read only `undecided_values` and `undecided_block_data`. Because both contain full bytes, an N
proposal committed by N-1 produces a full decided value, and N-1 value sync remains wire-complete while N-1 is active.

## One-time Reconciliation

On startup, N creates all tables and reads `undecided_storage_reconciliation_version` in the same write transaction.
If the marker is absent or lower than version `1`, it performs one full reconciliation:

1. Copy unique legacy block payloads into `undecided_block_data_v2`, accepting identical bytes and rejecting
   same-ID/different-byte conflicts.
2. Iterate `undecided_values`. Decode either a full N-1 row or the compact same-table row written by an earlier PR #22
   revision.
3. Resolve the v2 or exact legacy payload and verify the table key, proposal fields, payload-derived ID, and embedded
   bytes when present.
4. Insert compact metadata into `undecided_values_v2`.
5. If the v1 row is compact, replace it with a normal full `ProposedValue` so N-1 remains safe.
6. Backfill any missing legacy block-data row required by a proposal created by an earlier v2-only revision.
7. Reconcile pre-existing decided rows: promote valid missing decided payloads and repair valid ID-only decided values
   left by the unsafe same-table revision.
8. Write reconciliation version `1` only after every row has validated and every queued mutation has succeeded.
9. Commit all data changes and the marker together.

Validation completes before destructive row replacement. Any malformed record, missing payload, key mismatch, ID
mismatch, embedded-byte conflict, invalid SSZ payload, or header mismatch aborts the transaction. The old database and
marker state remain visible without partial migration metrics.

## Later Startup and Targeted Recovery

When version `1` is present, startup skips legacy payload migration and both proposal-table scans. It performs only
targeted recovery for the N-1 partial-commit window:

1. Iterate decided-value keys and first check whether the decided payload row exists.
2. Skip complete rows without decoding the value, loading payload bytes, hashing, SSZ decoding, or extracting a
   header.
3. Only for a missing decided payload, decode the value and certificate, resolve the certificate-bound v2 or exact
   legacy payload, and perform the existing ID, byte, SSZ, and header checks before promotion.

After reconciliation version `1`, current N writes decided payload, value, certificate, and header atomically. N-1 may
still create a partial commit during rollback, so the cheap missing-payload check remains necessary on every startup.

N-1 may also write new full proposal and legacy payload rows after the marker exists. N consumes those through the v1
and legacy runtime fallbacks; their existence does not invalidate the completed one-time migration marker. The later
activated cleanup release must reconcile any remaining v1-only rows immediately before deleting compatibility tables.

## Errors and Observability

Existing key-scoped storage errors remain sanitized and must not include payload contents. Reconciliation conflicts
identify table role, height, round where relevant, value ID, and byte lengths.

The successful migration event adds the reconciliation version and reports rows scanned, v2 rows inserted, compact
rows created, v1 rows restored, shadow rows backfilled, partial commits recovered, and ID-only decided values repaired.
It is emitted only when reconciliation actually runs and commits.

Ordinary version-1 startup may report a lightweight targeted-recovery summary only when it finds or repairs an
incomplete row. It must not emit a misleading full-migration event merely because compatibility tables still exist.

Test-only visit counters record legacy payload and proposal rows visited by full reconciliation. A second-startup test
requires both counters to remain zero. An ignored large-dataset benchmark records first-start and steady-state restart
durations and visit counts but does not impose machine-dependent timing thresholds.

## Verification

Deterministic tests cover:

- full N-1 proposal rows migrating to compact v2 while remaining full in v1;
- compact same-table rows from the current PR head being restored to full v1 and copied to compact v2;
- atomic rollback on every corruption class and marker absence after failure;
- N dual-writing full v1 and compact v2 proposal rows;
- N preferring v2 and falling back safely to full v1 data written after rollback;
- an exact N-1 decoder committing an N proposal and serializing a full value for sync;
- current strict decoding accepting that value, and an N-1 receiver retaining SSZ-decodable extensions;
- re-upgrade preserving or recovering the full decided value;
- version-1 second startup visiting zero legacy payload and proposal rows;
- targeted later-start recovery validating only decided rows whose payload is absent;
- pruning both proposal and payload layouts; and
- large multi-round payloads having one copy in the active v2 layout while compatibility duplication is reported
  separately.

The ignored opt-in qualification script accepts explicit current Emerald, N-1 Emerald, and custom-Reth binary paths.
It uses an isolated local testnet to exercise process restarts, an N/N-1 mixed-version sync interval, and final
re-upgrade convergence. The script records binary versions, block heights, process logs, and failure diagnostics. It
does not claim to deterministically create the exact stored-proposal interleaving; that boundary is pinned by the
database and codec tests above.

The real-binary script is a release qualification gate, not a normal CI test. Documentation must report whether it
was actually run and must not infer a passed mixed-version exercise from deterministic tests alone.

## Deployment and Cleanup

Operators continue to stop and back up each node before upgrading or downgrading it and never run two Emerald
processes against one database. The migration marker makes subsequent N restarts cheap while the v1 fallback preserves
data created during an N-1 interval.

The compatibility release intentionally stores full proposal and block-data shadows. Replacing or later deleting
logical rows makes redb pages reusable but does not guarantee that `store.db` shrinks. Startup never performs full file
compaction.

[Interoperability issue #325][issue-325] owns the activated phase-two cleanup. Its scope must include:

- stopping v1 proposal and legacy block-data dual-writes;
- reconciling any rows written by N-1 after the version-1 marker;
- deleting both `undecided_values` and `undecided_block_data` compatibility tables;
- retaining compact `undecided_values_v2` and shared `undecided_block_data_v2`; and
- documenting optional explicit offline redb compaction for filesystem-space reclamation.

[issue-325]: https://github.com/1Money-Co/1money-interoperability-protocol/issues/325
