# `on_decided` Latency and Round-Entry Telemetry Design

## Context

[Interoperability issue #315][issue-315] follows the proposal-restream recovery fix from [Emerald PR #17][pr-17]. The
restream fix restores recovery after a missed round-0 polka, but the incident also showed that staggered validator
entry into round 0 can make a short prevote timeout easier to miss.

Emerald currently performs several awaited operations in `on_decided` before it replies to consensus with
`Next::Start` for the next height:

1. read the decided block data from redb;
2. validate the decided execution payload;
3. update the execution client's forkchoice state;
4. commit the decided value to redb;
5. persist cumulative block statistics to redb;
6. read the next validator set from the execution contract.

The block-data read and block-statistics persistence were not listed separately in the original issue, but both are
awaited redb operations on the same critical path. The code logs when a node starts a round, but the event does not have
a stable machine-oriented name, and Emerald does not expose application metrics for the six decision stages or their
total latency.

As verified for this design on 2026-08-31, `1money-interoperability-protocol` `origin/main` commit `256497c` has
`timeout_prevote = "5s"` and `timeout_prevote_delta = "500ms"` in all 13 checked-in mainnet validator TOMLs under
`deployment/mainnet/nodes/*/config/config.toml`; the deployment-introducing commit `d5b5bbb` has the same values.
The interop e2e TOMLs and Emerald's generated/example defaults remain at 1 second, and `cicd-manifests` contains no
`timeout_prevote` key. Checked-in TOMLs therefore do not prove which configuration is live, so rollout measurements
must include the deployed configuration snapshot. This design measures live behavior first and leaves timeout changes
for a later evidence-based decision.

## Goals

- Measure total `on_decided` wall time and each awaited stage independently.
- Record stage latency even when the awaited operation returns an error.
- Emit one structured summary event for each normally completed decision attempt, whether it succeeds or returns an
  error.
- Make current cross-validator round-entry skew directly calculable in Prometheus, while retaining structured logs for
  per-height and per-round correlation.
- Keep Prometheus cardinality bounded and preserve current consensus behavior.
- Document how operators collect the data required for the next phase of issue #315.

## Non-goals

- Change any consensus timeout or generated timeout default.
- Parallelize execution RPCs or change the validator-set block hash derivation.
- Cache validator sets or otherwise change consensus-critical state derivation.
- Add a Grafana dashboard or assume a particular production log aggregation implementation.
- Change consensus messages, persistent storage, retry policy, or error handling.
- Close issue #315 before production measurements have been collected and evaluated.

## Architecture

### Metrics component

Add a cloneable `ConsensusMetrics` component in `app/src/metrics.rs`, following the existing `DbMetrics` and
`TxStatsMetrics` pattern. Add it to the unified `Metrics` container so every `State` receives it through the existing
node initialization path. No new configuration or dependency is required.

Register these histograms under the existing `app_channel` registry prefix:

- `on_decided_block_data_read_duration_seconds`
- `on_decided_payload_validation_duration_seconds`
- `on_decided_forkchoice_update_duration_seconds`
- `on_decided_commit_duration_seconds`
- `on_decided_block_stats_persistence_duration_seconds`
- `on_decided_validator_set_read_duration_seconds`
- `on_decided_duration_seconds`

Each histogram uses 21 exponential buckets beginning at 100 microseconds and doubling through 104.8576 seconds,
followed by the Prometheus `+Inf` bucket. The range separates cache hits and fast database operations while still
covering the configured 10-20 second execution retry windows and multi-stage outliers.

Register one additional gauge under the same prefix:

- `consensus_round_started_timestamp_seconds`

The gauge records the Unix timestamp when the application most recently handled `StartedRound`. It carries only the
existing bounded `moniker` label.

Height, round, value ID, block hash, proposer, result, and failure stage must not be metric labels. The registry's
existing bounded `moniker` label remains unchanged.

### Decision timing boundary

Keep public `on_decided` as a thin telemetry boundary and move its existing behavior into a private inner handler. The
outer function:

1. extracts the decision identity needed for logging;
2. clones the consensus metrics handle before borrowing `State` mutably across awaits;
3. starts total timing and creates a small timing accumulator;
4. awaits the inner handler;
5. records total duration;
6. emits one structured summary event; and
7. returns the inner handler's result unchanged.

The inner handler retains the current operation order and updates the accumulator around each awaited stage. A stage
observation is recorded immediately after its future resolves and before applying `?`, so failed calls contribute to
the stage histogram. Synchronous preparation and state updates remain visible in total latency but do not gain
individual histograms.

The refactor must not turn existing panics into errors, catch unwinding, change retry behavior, or suppress an existing
error. The summary-event guarantee applies to normal `Ok` and `Err` returns, not panics or process aborts.

### Structured decision summary

Emit one `info` event with `event = "on_decided_timing"` for every normal return. It contains:

- `height`
- `round`
- `value_id`
- `outcome`: `success` or `error`
- `failed_stage`, present only for errors
- `duration_seconds`
- `block_data_read_duration_seconds`
- `payload_validation_duration_seconds`
- `forkchoice_update_duration_seconds`
- `commit_duration_seconds`
- `block_stats_persistence_duration_seconds`
- `validator_set_read_duration_seconds`

Stage-duration fields are present only when that stage was reached.

The bounded `failed_stage` vocabulary is:

- `preparation`
- `block_data_read`
- `payload_validation`
- `forkchoice_update`
- `commit`
- `block_stats_persistence`
- `validator_set_read`
- `completion`

`preparation` covers normal errors outside an awaited stage before payload validation. A missing block or suppressed
store-read error is an error at `block_data_read`; after a successful read the active stage returns to `preparation`
for decoding and local invariant checks. A stage duration that was not reached is represented as `None`; `tracing`
omits `None` fields rather than reporting a misleading zero. Invalid payload validity is an error at
`payload_validation`, even when the validation future itself returns successfully. `completion` covers a normal error
after the validator-set read while preparing the next-height reply.

The event supplements existing human-readable logs. It does not include raw payload bytes or dependency error text.
The original error continues through the existing return path and is logged by the existing application boundary.

### Round-entry event

Retain the existing `Started round` log and add the stable field:

```text
event = "consensus_round_started"
```

Keep its existing height, round, proposer, role, and message. Do not emit a second event. With synchronized validator
clocks, the round-entry skew for a `(height, round)` is the latest event timestamp minus the earliest event timestamp
across validator monikers.

At the same point, set `app_channel_consensus_round_started_timestamp_seconds` from `SystemTime`. Once structured
events independently confirm that all validator series refer to the same round, current fleet skew is directly
queryable as `max(metric) - min(metric)`. During transitions, the gauge can compare different rounds and report a
misleadingly large skew. Structured events remain the reliable source for historical per-height and per-round
correlation.

## Data Flow

```text
Malachite Decided message
        |
        v
on_decided telemetry wrapper ---- starts total timer
        |
        v
existing decision handler ------- records each completed stage histogram and duration
        |
        v
wrapper records total histogram -- emits one success/error summary -- returns unchanged result

Malachite StartedRound message
        |
        +--------------------------- sets latest round-start timestamp gauge
        |
        v
existing Started round log ------- adds stable event field for historical cross-node correlation
```

The metrics handle is cloned before entering the inner handler. The timing accumulator is local to one decision
attempt and is not persisted. Metrics and logs are observational outputs only and do not feed back into consensus.

## Operational Documentation

Extend Emerald's operational documentation with:

- the seven histogram names and the round-start timestamp gauge, with their units;
- the `on_decided_timing` and `consensus_round_started` event schemas;
- example Prometheus queries for per-node and fleet-wide P50, P95, and P99 latency;
- direct PromQL for current round-entry skew plus the log calculation for historical per-height skew; and
- the requirement to keep validator clocks synchronized before interpreting timestamp differences.

Do not add a dashboard in this change. Production monitoring ownership and query syntax are not established in the
Emerald repository, so the documentation should describe portable Prometheus and structured-log calculations.

## Testing

### Metrics tests

Use a fresh local registry to register `ConsensusMetrics`, observe representative values, and render the Prometheus
text format. Assert:

- all seven histogram families and the timestamp gauge are registered under the expected names;
- recorded counts and sums appear;
- the 100 microsecond and 104.8576 second bucket boundaries appear; and
- no height, round, value, hash, proposer, result, or failure-stage labels exist.

### Timing-model tests

Unit-test the timing accumulator and summary data independently of the execution engine:

- success includes all completed durations and no failure stage;
- errors preserve completed durations and omit stages not reached;
- every bounded failure stage maps to its documented value; and
- duration observations use seconds.

Do not add an execution-engine mock or change the model-based test harness solely for telemetry. Existing Emerald tests
remain the behavioral regression suite because the inner handler keeps the current data flow.

### Verification gates

Before pushing, run:

```bash
cargo nextest run --all-targets --all-features -p emerald
cargo clippy --tests -- -D warnings
cargo +nightly fmt --all --check
```

Also run `git diff --check` and keep documentation lines within 120 columns.

## Deployment and Measurement

This change is safe for a rolling deployment because it does not change configuration, storage, wire formats,
consensus messages, or decision behavior. Complete the rollout across all validators before comparing cross-node data
so every node emits the same schema.

After rollout:

1. Verify all seven histogram families and the round-start timestamp gauge are scraped for every validator moniker.
2. Verify one `on_decided_timing` event is emitted for each normal decision attempt.
3. Verify `consensus_round_started` appears at each round boundary.
4. Collect at least 24 hours of P50, P95, and P99 stage and total latency per validator and across the fleet.
5. Calculate round-entry skew for each observed `(height, round)` and summarize its distribution.
6. Record the measurements, configuration snapshot, observation window, and query method in issue #315.

Issue #315 remains open after the telemetry PR. Timeout adjustment or guarded RPC parallelization requires a new design
based on the collected data and must not be bundled into this observational change.

[issue-315]: https://github.com/1Money-Co/1money-interoperability-protocol/issues/315
[pr-17]: https://github.com/1Money-Co/emerald/pull/17
