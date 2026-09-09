- `[app]` Deduplicate undecided block payloads across consensus rounds. The first startup migrates the local redb
  database in place; downgrading requires restoring the pre-upgrade `<home>/store.db` backup.
  ([#22](https://github.com/1Money-Co/emerald/pull/22))
