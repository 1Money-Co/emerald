# Undecided Block Data Deduplication Design

## Status

Approved in design discussion on 2026-09-09. Implementation remains gated on review of this written specification.

## Context

[Interoperability issue #318][issue-318] follows the proposal-restream repair in [Emerald PR #17][pr-17]. That repair
stores round-specific `ProposedValue` metadata for every re-proposal so decision handling can resolve the certificate
round. The current redb schema also stores the complete execution payload under every `(height, round, value_id)` key.

During a height that keeps advancing rounds without deciding, each re-proposal of the same value therefore adds another
payload-sized row. Undecided data is pruned only after a later commit advances the retention boundary, so restart does
not remove these rows and the stalled-height growth has no useful implementation bound.

The round is part of a proposal instance, not the execution payload's identity. A re-proposal changes the current
round, proposer, and valid-round metadata while preserving the same `Value` and execution bytes. Decided block data is
already stored without a round.

## Goals

- Store one undecided payload per `(height, value_id)` regardless of how many rounds re-propose it.
- Retain round-specific `ProposedValue` metadata, including proposer and `valid_round`, for every required round.
- Make proposal reuse, restream, decision, commit, recovery, and pruning use the shared payload safely.
- Preserve distinct payloads with different value IDs at the same height.
- Detect rather than overwrite different payload bytes presented for the same `(height, value_id)`.
- Migrate existing round-keyed redb data atomically and document the temporary rollback compatibility boundary.
- Count only committed physical payload insertions and bytes in the existing database write metrics.
- Add deterministic coverage for deduplication, migration, reopen, decision, recovery, and pruning.

## Non-Goals

- Change decided block-data retention or its height-keyed schema.
- Deduplicate payloads across heights.
- Change `ProposedValue`, certificate, proposal-part, or other consensus wire formats.
- Change proposal authentication, hidden-lock behavior, consensus timeouts, or round-change policy.
- Add automatic redb compaction during node startup.
- Repair conflicting payload data automatically.

## Considered Approaches

### Versioned height-and-value table

Create a new table keyed by `(height, value_id)`, migrate legacy rows eagerly, and remove `round` from undecided
payload APIs. This is the selected approach because it directly models the required identity with one table and makes
all pre-decision readers independent of duplicate round copies.

### Content-addressed payloads plus an index

Store payloads by a full cryptographic hash and map `(height, value_id)` to that hash. This would provide a stronger
global content identity, but it adds an index, reference management, and pruning complexity without a requirement to
deduplicate across heights.

### Legacy rows plus a canonical-round index

Keep the existing table and point each value to one selected round. This reduces immediate schema movement but retains
existing duplicates, preserves accidental round coupling, and makes recovery and pruning more fragile. It does not
fully address the storage objective.

## Storage Model

Round-specific and payload-specific data remain separate:

| Data | Key | Contents |
|------|-----|----------|
| Undecided proposal metadata | `(height, round, value_id)` | Identity and round fields; no payload |
| Undecided block data v2 | `(height, value_id)` | Complete execution payload bytes |
| Decided block data | `height` | Complete execution payload bytes after commitment |

Add an `UndecidedBlockDataKey` redb key type composed of `HeightKey` and `ValueIdKey`. Keep the current
`UndecidedValueKey` for proposal metadata and the legacy migration reader.

Use `undecided_block_data_v2` as the redb table name for the height-and-value schema. Do not open the existing
`undecided_block_data` table with a changed typed key: redb records the key type and rejects a mismatched
`TableDefinition`.

## Runtime Write Contract

`State::store_undecided_value` retains the existing payload-before-metadata order:

1. Insert or verify block data under `(height, value_id)`.
2. Store the `ProposedValue` under `(height, round, value_id)`.

The block-data insertion has three outcomes:

- no existing row: insert the payload and commit;
- existing identical bytes: return success without committing a payload write;
- existing different bytes: return a storage-integrity error without changing either row.

This preserves the crash invariant that proposal metadata never deliberately references missing block data. A crash
after a new payload insertion but before metadata leaves only safe orphaned block data. On re-proposal, the payload
step is an identical-bytes no-op and the new round's metadata is stored normally.

`ValueId` is currently a 64-bit hash. The design does not assume collisions are impossible. Exact byte comparison
makes a collision or inconsistent decoded value fail closed instead of aliasing or overwriting another payload.

## Runtime Read Contract

Replace the combined fallback-to-decided lookup with two explicit operations:

- `get_undecided_block_data(height, round, value_id)` for proposal reuse, restream, `on_decided`, and `commit`;
- `get_decided_block_data(height)` for already committed block retrieval.

The current combined lookup tries an undecided `(height, round, value_id)` row and then silently falls back to decided
data by height. Splitting the operations prevents a missing undecided value ID from returning an unrelated decided
payload at that height.

Round remains required when retrieving proposal metadata. A decision certificate selects metadata by
`(height, certificate.round, certificate.value_id)` and selects its shared bytes by
`(height, certificate.value_id)`. During the one-release compatibility window, the undecided payload API also carries
the exact round so a missing v2 row can fall back to the legacy `(height, round, value_id)` row. The primary v2 lookup
ignores round. Restream similarly selects source metadata by proposal round while loading shared payload bytes.
Recovery and synced-value ingestion use the same shared write and read boundaries.

Runtime proposal reads hydrate compact metadata from v2 or the exact legacy fallback, recompute `Value::new(payload)`,
and require its `ValueId` to match both the table key and metadata before returning a full `ProposedValue`.

Pruning remains height-based. The v2 block-data table retains rows whose height is at or above the existing temporary
data retention boundary and removes older rows alongside undecided proposal metadata.

## Legacy Migration

Store initialization uses the transaction's table listing to detect the legacy `undecided_block_data` table without
opening and accidentally creating it. When present, initialization performs migration in one redb write transaction:

1. Open or create the versioned height-and-value table.
2. Iterate legacy `(height, round, value_id)` rows.
3. Insert the first payload for each `(height, value_id)`.
4. Skip later rows when their bytes are identical.
5. Abort on any same-key row whose bytes differ.
6. Retain the legacy table as a rollback shadow for one compatibility release.
7. Validate all `undecided_values` rows and atomically rewrite full legacy proposals as compact metadata.
8. Backfill legacy rows for v2-backed proposal metadata written by an earlier v2-only build.
9. Recover valid N-1 partial decided commits from certificate-bound undecided data.
10. Commit the transaction.

An interrupted or conflicting migration leaves the legacy table intact and publishes no successful migration
metrics. The procedure also handles an existing v2 table defensively: identical rows merge as no-ops and conflicting
rows abort the transaction. A new database creates both tables during the compatibility release.

N uses v2 as the primary layout but temporarily dual-writes and prunes the legacy table. This preserves a quiesced
N -> N-1 -> N database path. The legacy table necessarily retains round duplicates until a later activated release
removes the compatibility shadow.

## Error Handling and Observability

Add `StoreError::ConflictingUndecidedBlockData` for different bytes under the same `(height, value_id)`. The error
includes the height and value ID but never payload bytes. A runtime conflict prevents the associated proposal metadata
write. A migration conflict prevents Emerald from starting and preserves the legacy database for diagnosis or
rollback.

Missing proposal metadata remains a normal absence where the calling consensus path already permits it. Existing
proposal metadata with missing shared block data remains a hard contextual integrity error.

Emit one successful `undecided_block_data_migration` event containing legacy row count, newly inserted payload count
and bytes, identical duplicate count, compatibility-row count and bytes, recovered decided-payload count and bytes,
and elapsed time. Emit a conflict error with the key and abort startup. Do not log payload contents.

The existing database write count and byte counters advance only after physical payload rows commit. Here,
"payload bytes" means the payload value bytes accepted by redb, consistent with the current metrics; it does not claim
to measure filesystem pages or write amplification. Identical duplicates, conflicts, and aborted migrations add zero
successful payload writes and zero payload bytes. A successful migration counts each newly inserted v2 payload once;
runtime dual-writing counts the v2 and legacy rows actually inserted, and skipped duplicates are not counted.

## Testing

### Store schema and migration tests

- Store the same value through multiple proposal rounds and assert one v2 payload row.
- Store different value IDs at the same height and assert separate payload rows.
- Attempt different bytes for the same `(height, value_id)` and assert an error with the original bytes unchanged.
- Build a legacy database with identical cross-round duplicates, migrate it, and assert one v2 row while the legacy
  rollback rows remain available.
- Build a legacy database with conflicting duplicates and assert migration rollback preserves the legacy table.
- Close and reopen both newly created and migrated databases and retrieve the expected payloads.
- Assert write count and bytes reflect both compatibility rows for a new payload and do not advance for a same-round
  duplicate or conflict.
- Exercise N-1 schema -> N -> N-1-compatible access -> N reopen, including v2-only compatibility backfill.
- Recover an N-1 partial decided commit and fail without mutation on missing or inconsistent recovery data.
- Inject failure after every decided payload/value/certificate/header write and assert no partial rows survive reopen.

### State-flow tests

- Restream one value across multiple rounds and assert all round-specific proposals remain available while one payload
  row is stored.
- Commit a certificate for a later re-proposal round using a valid minimal SSZ execution payload and assert the shared
  payload is promoted to the decided table.
- Exercise synced or recovered value ingestion and assert it uses the same round-independent payload key.

### Retention tests

- Prune below the configured temporary-data boundary and assert both v2 payload rows and proposal metadata are
  removed by height.
- Assert multiple surviving values at one height remain distinct after pruning.

### Validation

Run focused migration, deduplication, restream, decision, and pruning tests first. Then run:

1. `cargo nextest run --all-targets --all-features -p emerald`
2. the practical non-MBT workspace test gate, reporting any missing Quint tooling separately;
3. `cargo clippy --tests -- -D warnings`
4. `cargo +nightly fmt --all --check`

## Deployment and Operations

The change is local persistent-storage behavior. It changes no consensus or network encoding, so mixed-version nodes
remain wire-compatible and can be upgraded one at a time while preserving the validator availability threshold.

Before upgrading each node, operators must stop it cleanly, back up `<home>/store.db`, and verify temporary free space
for approximately one deduplicated payload set plus redb overhead. The compatibility release retains the legacy table,
so full storage reclamation begins only after a later activated release removes it.

Deleting the legacy table frees its redb pages for reuse but may not immediately shrink the database file on the
filesystem. Startup will not invoke redb's potentially slow full compaction. If returning file space to the filesystem
is operationally necessary, operators may perform an explicit offline compaction as a separate maintenance action.
Replacing full proposal rows with compact metadata has the same distinction: redb can reuse the released pages, but
the logical migration does not guarantee that `store.db` shrinks.

[Interop issue #325][issue-325] owns the activated removal of the temporary legacy shadow and the explicit offline
compaction procedure. Until that release, the shadow is the only remaining round-keyed payload duplication.

Update Emerald's production operator documentation with the backup, headroom, compatibility-window downgrade,
migration-summary, and later-removal expectations.

[issue-318]: https://github.com/1Money-Co/1money-interoperability-protocol/issues/318
[issue-325]: https://github.com/1Money-Co/1money-interoperability-protocol/issues/325
[pr-17]: https://github.com/1Money-Co/emerald/pull/17
