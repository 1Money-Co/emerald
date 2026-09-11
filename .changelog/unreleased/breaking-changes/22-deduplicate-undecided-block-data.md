- `[app]` Deduplicate undecided block payloads across consensus rounds. `undecided_values_v2` stores compact proposal
  metadata and `undecided_block_data_v2` stores one shared payload per `(height, value_id)`. During the compatibility
  release, Emerald dual-writes full round-keyed `undecided_values` proposals and `undecided_block_data` payloads so a
  quiesced node can return to N-1 safely. Successful one-time reconciliation writes schema version `1`; later starts
  skip legacy payload and proposal scans while retaining certificate-bound recovery for a missing decided payload.
  Compatibility rows can reuse freed redb pages later, but this migration does not guarantee that `store.db` shrinks.
  Removing both shadows and optionally compacting redb offline are tracked by
  [interop issue #325](https://github.com/1Money-Co/1money-interoperability-protocol/issues/325).
  ([#22](https://github.com/1Money-Co/emerald/pull/22))
