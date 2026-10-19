# Metrics and build identity

When metrics are enabled, Emerald exposes Prometheus metrics at `/metrics` on the configured metrics listen address.
The endpoint includes application and Malachite metrics and one `target_info` gauge with value `1`.
Build information is registered once when the metrics service initializes, before any block needs to be decided.
Repeated scrapes and metrics initialization do not add series.

## Build metric contract

| Label | Compile-time source in `emerald-build-info::build_info` |
|---|---|
| `service_version` | `PKG_VERSION`, the package version also used by the CLI. |
| `service_build_time` | `BUILD_TIME_3339`, an RFC 3339 timestamp with an explicit UTC offset. |
| `service_rust_version` | `RUST_VERSION`, including the compiler version and revision. |
| `vcs_ref_head` | `COMMIT_HASH`, the full Git commit SHA. |
| `vcs_ref_head_branch` | `BRANCH`, with the detached and unavailable fallbacks below. |

These labels describe the running binary's build inputs. The CLI's `--version` uses the same generated metadata;
it displays a shortened commit hash and a human-readable representation of the same build timestamp.
`service_build_time` is build time, not process start time or commit time.

An empty or whitespace-only metadata value becomes `unknown`. An empty branch with a known commit, or a branch
reported as `HEAD`, becomes `detached`. If the build environment supplies a branch/ref through `shadow-rs`, that
compile-time value is retained. The CLI may display the raw empty branch or `HEAD` for a detached build.

Source archives without a `.git` directory or worktree `.git` file at the Emerald workspace root report `unknown`
for their Git branch and commit in both CLI and metrics output. They never inherit a surrounding checkout's identity.
Build timestamp, Rust version, and package version remain available independently of Git. Archive builds do not
attempt to guess a release's SHA or branch from an archive filename, deployment location, or another repository.

Metadata is refreshed on subsequent Cargo builds, including debug builds after HEAD or branch changes.
No Git commands or filesystem discovery run when the binary starts or when `/metrics` is scraped.
Quotes, backslashes, and newlines in label values are escaped for Prometheus text exposition.

Environment, node, host address, region, application, and infrastructure provider labels belong in scrape/relabel
configuration. Emerald does not add those deployment-specific labels. Consumers should select Emerald targets using
their collection labels and query `target_info` as an instant table, with one row per target.

## Verification

Run the focused metric and fallback tests with:

```sh
cargo test -p emerald-build-info -p malachitebft-eth-cli
```
