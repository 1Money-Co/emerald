- `[metrics]` Export one `target_info` gauge with the running binary's version, build time, Rust compiler,
  commit SHA, and branch/ref so operators can identify Emerald builds through the existing metrics endpoint.
  Archive builds report unavailable Git identity in the metric and CLI version output.
  Git branch and tag values come from the source checkout, with unverified CI overrides ignored.
  ([#25](https://github.com/1Money-Co/emerald/pull/25))
