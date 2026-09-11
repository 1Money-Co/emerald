# Compact Undecided Proposal Metadata Design

## Status

Superseded for the N-1 compatibility release by
`2026-09-11-versioned-proposal-storage-review-hardening-design.md`. The same-table compact encoding below is retained
as decision history; it is unsafe because N-1 can commit and serve an ID-only value while N-1 is still active. The
selected design preserves full `undecided_values` rows and stores compact metadata in `undecided_values_v2`.

## Context

PR #22 moves undecided execution payloads from the round-keyed `undecided_block_data` table to the
round-independent `undecided_block_data_v2` table. The proposal table still serializes a complete
`ProposedValue`, including `Value.extensions`, so every re-proposal stores the execution payload again under a new
round key. This violates issue #318's separation between small round-specific metadata and shared payload bytes.

The current review hardening also preserves an in-place N -> N-1 -> N database path for one compatibility release.
The proposal encoding must therefore remove payload bytes without making the table unreadable by N-1.

## Goals

- Store no execution payload bytes in new `undecided_values` rows.
- Reconstruct the full `ProposedValue` from compact metadata and the certificate- or key-bound shared payload.
- Atomically migrate existing full proposal rows and fail without mutation on inconsistent data.
- Preserve the approved quiesced N -> N-1 -> N database path for the compatibility release.
- Keep strict `Value::from_proto` validation for all wire data.
- Preserve proposer, round, valid-round, and validity fields exactly.

## Non-goals

- Removing the temporary round-keyed N-1 block-data shadow in this release.
- Changing the public Malachite `ProposedValue` type or network protobuf contract.
- Changing decided block-data retention or the existing height-based pruning boundary.
- Claiming that deterministic database compatibility tests replace mixed-binary release qualification.

## Selected Encoding

Keep the existing `undecided_values` table, key type, and protobuf envelope. New rows encode the normal
`proto::ProposedValue` fields, but its nested `proto::Value.value` contains exactly the eight-byte big-endian
`ValueId`. It does not contain `Value.extensions`.

This is a storage-only encoding. The transport `ProtobufCodec` remains unchanged and continues to require that an
encoded value ID matches the attached payload bytes. Store code uses dedicated compact encode/decode helpers and
never routes an ID-only record through the wire decoder.

The eight-byte nested value distinguishes compact rows from legacy full rows. A valid SSZ `ExecutionPayloadV3` is
not empty, so a valid full stored proposal always contains more than the eight-byte ID prefix.

The same-table encoding is preferred over a new table because the N-1 `Value::from_proto` decoder accepts the ID
prefix with empty extensions. N-1 can therefore recover the value identity from a compact row and retrieve the
actual payload through its existing round-keyed block-data lookup. A new table with deleted legacy rows would break
rollback, while retaining full rows beside a new table would preserve the duplication reported in review.

## Runtime Write and Read Contracts

`State::store_undecided_value` keeps the payload-before-metadata order:

1. Insert or verify v2 and compatibility-shadow payload rows.
2. Insert compact proposal metadata under `(height, round, value_id)`.

The proposal write remains first-write-wins for a key. An identical retry does not update metrics. A different
proposal for an occupied key remains unable to overwrite the authoritative proposer metadata.

Proposal reads perform these steps in one redb read transaction:

1. Decode the storage protobuf and require its height, round, and encoded value ID to match the table key.
2. Load payload bytes from `(height, value_id)` in v2, with exact-round legacy fallback during the compatibility
   window.
3. Recompute `Value::new(payload)` and require the resulting ID to match the stored key and metadata.
4. Reconstruct the full `ProposedValue` with the verified payload and stored round-specific fields.

Missing or inconsistent payload data is a storage-integrity error, not a normal missing-proposal result. The list
and single-row APIs keep returning fully reconstructed `ProposedValue` values, so callers do not change.

## Startup Migration

Proposal compaction runs inside the existing schema initialization write transaction after legacy block data has
been copied into v2. It collects validated rewrites before mutating the proposal table so iteration does not overlap
row replacement.

For every `undecided_values` row:

- Decode the protobuf envelope and validate height, round, and value ID against the table key.
- If the nested value is a compact eight-byte ID, resolve and verify its shared payload without rewriting it.
- If the nested value contains a full value, decode it through the strict `Value::from_proto` path, verify that its
  extensions equal the v2 payload for the key, and queue the compact replacement.
- Reject missing payloads, mismatched IDs, mismatched embedded bytes, malformed metadata, or key/record mismatch.

Only after all proposal rows validate does initialization replace queued full rows with compact rows. Any error
aborts the complete initialization transaction, including payload migration, legacy backfill, and decided-state
repair. These logical replacements allow redb to reuse released pages but do not guarantee that `store.db` shrinks.

## N-1 Rollback and Re-upgrade

The compatibility test pins the relevant N-1 decoder behavior: an ID-only `proto::Value` decodes as a `Value` with
the stored ID and empty extensions. With Malachite configured for `value_payload = "parts-only"`, N-1 uses that value
identity while retrieving and streaming execution bytes from the retained legacy block-data row.

N-1 can produce two forms of data while running after rollback:

- Newly built or received proposals use the old full proposal encoding. N compacts them on re-upgrade.
- A compact proposal committed by N-1 can produce an ID-only decided-value row because N-1 persists the proposal's
  empty extensions. On re-upgrade, N recognizes that storage-only form, resolves the certificate-bound decided or
  undecided payload, validates its ID, SSZ payload, and header, then atomically replaces the decided value with the
  full verified `Value`.

The compact decided-value exception exists only in database recovery. Network `Value` decoding stays fail-closed and
does not accept an unattached ID.

## Errors and Observability

Add a height-, round-, and value-scoped storage error for irrecoverable proposal metadata. Error text and logs may
identify keys, record kind, and lengths but must not include payload contents.

Extend migration statistics with compacted proposal row count, bytes read, bytes written, and repaired compact
decided-value count. Successful write metrics advance only after the initialization transaction commits and count
the compact metadata bytes physically written. Existing payload write metrics continue to count v2 and temporary
legacy-shadow writes; proposal compaction must not count metadata bytes as payload bytes in documentation.

## Compatibility Boundary

The primary `undecided_values` plus `undecided_block_data_v2` layout stores one execution payload per
`(height, value_id)`. The retained N-1 `undecided_block_data` shadow still contains round-keyed payload copies during
the one-release compatibility window. That temporary duplication is explicit and is removed only by the later
activated release that ends N-1 rollback support. It is the only remaining round-keyed payload duplication in this
release.

[Interop issue #325](https://github.com/1Money-Co/1money-interoperability-protocol/issues/325) owns the activated
shadow deletion and the optional explicit offline redb compaction procedure. Neither proposal-row replacement nor
later table deletion guarantees immediate filesystem-space reclamation, and this release does not compact at startup.

## Testing

- Store a multi-megabyte value across several rounds and assert each raw proposal row is compact, contains no payload
  bytes, preserves all metadata fields, and reconstructs the original full proposal.
- Count payload bytes in `undecided_values` plus `undecided_block_data_v2` and assert one primary payload copy.
- Migrate full legacy proposal rows, reopen, and assert compact raw rows plus fully reconstructed reads.
- Fail migration without mutation for missing shared payload, key mismatch, ID mismatch, embedded-byte conflict, and
  malformed metadata.
- Decode a compact row with an N-1-compatible test decoder and retrieve its exact legacy payload.
- Simulate an N-1 commit of compact proposal metadata, re-upgrade, and assert the decided value is repaired and the
  node reopens.
- Re-run restream, `on_decided`, commit, recovery, and pruning tests through reconstructed proposal reads.
- Remove the redundant function-local `TableHandle` import identified by Frank's non-blocking review comment.

Real mixed-binary rolling-network validation remains a release qualification step and is not claimed by these
deterministic database contract tests.
