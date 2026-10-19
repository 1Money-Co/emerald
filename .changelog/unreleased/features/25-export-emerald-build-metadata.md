- `[metrics]` Export one `target_info` gauge with the running binary's version, build time, Rust compiler,
  commit SHA, and branch/ref so operators can identify Emerald builds through the existing metrics endpoint.
  Archive builds use explicit unavailable Git metadata and cannot inherit another checkout's identity.
  ([#25](https://github.com/1Money-Co/emerald/pull/25))
