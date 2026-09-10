- `[app]` Deduplicate undecided block payloads across consensus rounds. Startup atomically rewrites full legacy
  `undecided_values` rows to payload-free proposal metadata, and runtime reads hydrate them from the verified shared
  payload. During the compatibility release, Emerald retains and dual-writes the legacy round-keyed block-data table
  so a quiesced node can return to the previous binary without restoring an out-of-date application database.
  Replaced redb pages can be reused, but this migration does not guarantee that `store.db` shrinks. Removing the
  compatibility shadow and optionally compacting redb offline are tracked by
  [interop issue #325](https://github.com/1Money-Co/1money-interoperability-protocol/issues/325).
  ([#22](https://github.com/1Money-Co/emerald/pull/22))
