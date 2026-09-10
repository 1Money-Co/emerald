- `[app]` Deduplicate undecided block payloads across consensus rounds. During the compatibility release, Emerald
  retains and dual-writes the legacy round-keyed table so a quiesced node can return to the previous binary without
  restoring an out-of-date application database.
  ([#22](https://github.com/1Money-Co/emerald/pull/22))
