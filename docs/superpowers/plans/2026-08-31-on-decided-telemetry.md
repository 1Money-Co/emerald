# `on_decided` Telemetry Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add bounded Prometheus histograms and structured logs that measure every awaited `on_decided` stage, total
decision-processing latency, and cross-validator round-entry skew without changing consensus behavior.

**Architecture:** Extend Emerald's existing application metrics container with seven fixed histograms and one
round-start timestamp gauge. Keep public
`on_decided` as a telemetry wrapper around a behavior-preserving inner handler, and use a local timing accumulator to
record successful and failed stages before the original result propagates. Give the existing round-start log a stable
event field and document portable production queries.

**Tech Stack:** Rust, `std::time::Instant`, `tracing`, Malachite `SharedRegistry`, `prometheus-client`, mdBook.

**Spec:** `docs/superpowers/specs/2026-08-31-on-decided-telemetry-design.md`

## Global Constraints

- Do not change consensus timeouts, generated timeout defaults, operation ordering, retry policy, or validator-set
  derivation.
- Do not change consensus messages, wire formats, persistent storage, or existing error and panic behavior.
- Register seven fixed histograms and one gauge under `app_channel`; retain only the existing bounded `moniker`
  metric label.
- Use 21 exponential buckets from 0.0001 seconds through 104.8576 seconds, plus `+Inf`.
- Emit one `on_decided_timing` event for each normal `Ok` or `Err` return and no duplicate round-start event.
- Keep height, round, value ID, block hash, proposer, outcome, and failure stage out of metric labels.
- Do not add a Grafana dashboard or production-specific log-query syntax.
- Keep new and modified Markdown lines at or below 120 columns.
- Do not edit or format `Cargo.lock`.

---

## File Structure

- Modify `app/src/metrics.rs`: own `ConsensusMetrics`, histogram registration, observation helpers, and metric tests.
- Modify `app/src/app.rs`: own decision-stage timing, summary construction, structured events, and handler integration.
- Modify `docs/operational-docs/src/production-network/running-emerald.md`: document metrics, events, queries, and the
  production measurement procedure.

No new source file or dependency is needed. The timing model remains private to `app.rs` because it describes one
handler's control flow, while reusable Prometheus instruments remain in `metrics.rs`.

---

### Task 1: Add Fixed Consensus Decision Metrics

**Files:**

- Modify: `app/src/metrics.rs:1-347`
- Test: `app/src/metrics.rs` internal `tests` module

**Interfaces:**

- Consumes: `metrics::SharedRegistry`, `metrics::Registry`, `Histogram`, and `core::time::Duration`.
- Produces: `ConsensusMetrics::{new,register,observe_block_data_read,observe_payload_validation,
  observe_forkchoice_update,observe_commit,observe_block_stats_persistence,observe_validator_set_read,
  observe_total,set_round_started_timestamp}`.
- Produces: `Metrics::consensus: ConsensusMetrics` for Task 2.

- [ ] **Step 1: Write a failing Prometheus exposition test**

Add a `#[cfg(test)] mod tests` at the bottom of `app/src/metrics.rs`. The test registers the future component beneath
the production prefix, records one one-second sample in every histogram, and verifies names, counts, sums, boundary
buckets, and forbidden labels:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use metrics::prometheus::encoding::text::encode;

    const DECISION_METRIC_NAMES: [&str; 7] = [
        "on_decided_block_data_read_duration_seconds",
        "on_decided_payload_validation_duration_seconds",
        "on_decided_forkchoice_update_duration_seconds",
        "on_decided_commit_duration_seconds",
        "on_decided_block_stats_persistence_duration_seconds",
        "on_decided_validator_set_read_duration_seconds",
        "on_decided_duration_seconds",
    ];

    #[test]
    fn consensus_metrics_render_all_decision_histograms() {
        let mut registry = metrics::Registry::default();
        let consensus = ConsensusMetrics::new();
        consensus.register_into(registry.sub_registry_with_prefix("app_channel"));

        let sample = Duration::from_secs(1);
        consensus.observe_block_data_read(sample);
        consensus.observe_payload_validation(sample);
        consensus.observe_forkchoice_update(sample);
        consensus.observe_commit(sample);
        consensus.observe_block_stats_persistence(sample);
        consensus.observe_validator_set_read(sample);
        consensus.observe_total(sample);

        let mut output = String::new();
        encode(&mut output, &registry).unwrap();

        for name in DECISION_METRIC_NAMES {
            let full_name = format!("app_channel_{name}");
            assert!(output.contains(&format!("# TYPE {full_name} histogram")));
            assert!(output.contains(&format!("{full_name}_count 1")));
            assert!(output.contains(&format!("{full_name}_sum 1.0")));
        }

        assert!(output.contains(
            "app_channel_on_decided_duration_seconds_bucket{le=\"0.0001\"}"
        ));
        assert!(output.contains(
            "app_channel_on_decided_duration_seconds_bucket{le=\"104.8576\"}"
        ));
        assert!(output.contains(
            "# TYPE app_channel_consensus_round_started_timestamp_seconds gauge"
        ));

        for forbidden in [
            "height=",
            "round=",
            "value_id=",
            "block_hash=",
            "proposer=",
            "outcome=",
            "failed_stage=",
        ] {
            assert!(!output.contains(forbidden));
        }
    }
}
```

- [ ] **Step 2: Run the test and confirm the red state**

Run:

```bash
cargo test -p emerald --lib \
  metrics::tests::consensus_metrics_render_all_decision_histograms -- --exact
```

Expected: compilation fails because `ConsensusMetrics` does not exist.

- [ ] **Step 3: Add `ConsensusMetrics` and its fixed bucket constructor**

Import `Registry` beside `SharedRegistry`, then add a dedicated component after `TxStatsMetrics`. Keep metric objects
private and expose duration-based observation methods:

```rust
use metrics::{Registry, SharedRegistry};

fn decision_latency_buckets() -> impl Iterator<Item = f64> {
    exponential_buckets(0.0001, 2.0, 21)
}

#[derive(Clone, Debug)]
pub struct ConsensusMetrics(Arc<ConsensusMetricsInner>);

#[derive(Debug)]
struct ConsensusMetricsInner {
    block_data_read: Histogram,
    payload_validation: Histogram,
    forkchoice_update: Histogram,
    commit: Histogram,
    block_stats_persistence: Histogram,
    validator_set_read: Histogram,
    total: Histogram,
    round_started_timestamp: Gauge<f64, AtomicU64>,
}

impl ConsensusMetrics {
    pub fn new() -> Self {
        Self(Arc::new(ConsensusMetricsInner {
            block_data_read: Histogram::new(decision_latency_buckets()),
            payload_validation: Histogram::new(decision_latency_buckets()),
            forkchoice_update: Histogram::new(decision_latency_buckets()),
            commit: Histogram::new(decision_latency_buckets()),
            block_stats_persistence: Histogram::new(decision_latency_buckets()),
            validator_set_read: Histogram::new(decision_latency_buckets()),
            total: Histogram::new(decision_latency_buckets()),
            round_started_timestamp: Gauge::default(),
        }))
    }

    pub fn register(registry: &SharedRegistry) -> Self {
        let metrics = Self::new();
        registry.with_prefix("app_channel", |registry| metrics.register_into(registry));
        metrics
    }

    fn register_into(&self, registry: &mut Registry) {
        registry.register(
            "on_decided_block_data_read_duration_seconds",
            "Time spent reading decided block data (seconds)",
            self.0.block_data_read.clone(),
        );
        registry.register(
            "on_decided_payload_validation_duration_seconds",
            "Time spent validating a decided payload (seconds)",
            self.0.payload_validation.clone(),
        );
        registry.register(
            "on_decided_forkchoice_update_duration_seconds",
            "Time spent updating forkchoice for a decided payload (seconds)",
            self.0.forkchoice_update.clone(),
        );
        registry.register(
            "on_decided_commit_duration_seconds",
            "Time spent committing a decided value (seconds)",
            self.0.commit.clone(),
        );
        registry.register(
            "on_decided_block_stats_persistence_duration_seconds",
            "Time spent persisting decided block statistics (seconds)",
            self.0.block_stats_persistence.clone(),
        );
        registry.register(
            "on_decided_validator_set_read_duration_seconds",
            "Time spent reading the next validator set (seconds)",
            self.0.validator_set_read.clone(),
        );
        registry.register(
            "on_decided_duration_seconds",
            "Total on_decided processing time (seconds)",
            self.0.total.clone(),
        );
        registry.register(
            "consensus_round_started_timestamp_seconds",
            "Unix timestamp when the application started its latest consensus round",
            self.0.round_started_timestamp.clone(),
        );
    }

    pub fn observe_block_data_read(&self, duration: Duration) {
        self.0.block_data_read.observe(duration.as_secs_f64());
    }

    pub fn observe_payload_validation(&self, duration: Duration) {
        self.0.payload_validation.observe(duration.as_secs_f64());
    }

    pub fn observe_forkchoice_update(&self, duration: Duration) {
        self.0.forkchoice_update.observe(duration.as_secs_f64());
    }

    pub fn observe_commit(&self, duration: Duration) {
        self.0.commit.observe(duration.as_secs_f64());
    }

    pub fn observe_block_stats_persistence(&self, duration: Duration) {
        self.0
            .block_stats_persistence
            .observe(duration.as_secs_f64());
    }

    pub fn observe_validator_set_read(&self, duration: Duration) {
        self.0.validator_set_read.observe(duration.as_secs_f64());
    }

    pub fn observe_total(&self, duration: Duration) {
        self.0.total.observe(duration.as_secs_f64());
    }

    pub fn set_round_started_timestamp(&self, timestamp: SystemTime) {
        let seconds_since_epoch = timestamp
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        self.0.round_started_timestamp.set(seconds_since_epoch);
    }
}

impl Default for ConsensusMetrics {
    fn default() -> Self {
        Self::new()
    }
}
```

- [ ] **Step 4: Add the component to the unified metrics container**

Update both constructors so tests use unregistered metrics and production uses the moniker-aware shared registry:

```rust
pub struct Metrics {
    pub db: DbMetrics,
    pub tx_stats: TxStatsMetrics,
    pub consensus: ConsensusMetrics,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            db: DbMetrics::new(),
            tx_stats: TxStatsMetrics::new(),
            consensus: ConsensusMetrics::new(),
        }
    }

    pub fn register(registry: &SharedRegistry) -> Self {
        Self {
            db: DbMetrics::register(registry),
            tx_stats: TxStatsMetrics::register(registry),
            consensus: ConsensusMetrics::register(registry),
        }
    }
}
```

- [ ] **Step 5: Run the focused metrics tests and confirm the green state**

Run:

```bash
cargo test -p emerald --lib metrics::tests:: -- --nocapture
```

Expected: the exposition test passes and reports all seven histogram families plus the round-start timestamp gauge.

- [ ] **Step 6: Commit the metrics component**

```bash
git add app/src/metrics.rs
git commit -m "feat(app): add decision latency metrics"
```

---

### Task 2: Instrument Decision Processing and Round Entry

**Files:**

- Modify: `app/src/app.rs:1-21`
- Modify: `app/src/app.rs:87-170`
- Modify: `app/src/app.rs:330-474`
- Test: `app/src/app.rs` internal `tests` module

**Interfaces:**

- Consumes: `State::metrics.consensus` and every observation method produced by Task 1.
- Produces: private `DecisionStage`, `AwaitedStage`, `DecisionTimings`, and `DecisionSummary` types.
- Produces: public `on_decided` telemetry wrapper with the unchanged external signature.
- Produces: private `on_decided_inner` containing the original decision behavior and stage observations.
- Produces: structured `event = "on_decided_timing"` and `event = "consensus_round_started"` logs plus the
  round-start timestamp gauge update.

- [ ] **Step 1: Write failing timing-model tests**

Append a test module to `app/src/app.rs`. The success test fills every duration; the error test proves later stages stay
unset; the vocabulary test locks every bounded failure-stage value:

```rust
#[cfg(test)]
mod tests {
    use core::time::Duration;

    use super::*;

    #[test]
    fn decision_timings_build_success_summary() {
        let metrics = ConsensusMetrics::new();
        let mut timings = DecisionTimings::default();
        for stage in [
            AwaitedStage::BlockDataRead,
            AwaitedStage::PayloadValidation,
            AwaitedStage::ForkchoiceUpdate,
            AwaitedStage::Commit,
            AwaitedStage::BlockStatsPersistence,
            AwaitedStage::ValidatorSetRead,
        ] {
            timings.enter_awaited(stage);
            timings.observe(stage, Duration::from_millis(2), &metrics);
        }
        timings.enter_completion();

        let summary = timings.summary(Duration::from_millis(20), true);

        assert_eq!(summary.outcome, "success");
        assert_eq!(summary.failed_stage, None);
        assert_eq!(summary.duration_seconds, 0.02);
        assert_eq!(summary.block_data_read_duration_seconds, Some(0.002));
        assert_eq!(summary.payload_validation_duration_seconds, Some(0.002));
        assert_eq!(summary.forkchoice_update_duration_seconds, Some(0.002));
        assert_eq!(summary.commit_duration_seconds, Some(0.002));
        assert_eq!(summary.block_stats_persistence_duration_seconds, Some(0.002));
        assert_eq!(summary.validator_set_read_duration_seconds, Some(0.002));
    }

    #[test]
    fn decision_timings_build_error_summary_without_unreached_stages() {
        let metrics = ConsensusMetrics::new();
        let mut timings = DecisionTimings::default();
        timings.enter_awaited(AwaitedStage::BlockDataRead);
        timings.observe(
            AwaitedStage::BlockDataRead,
            Duration::from_millis(1),
            &metrics,
        );
        timings.enter_awaited(AwaitedStage::PayloadValidation);
        timings.observe(
            AwaitedStage::PayloadValidation,
            Duration::from_millis(3),
            &metrics,
        );
        timings.enter_awaited(AwaitedStage::ForkchoiceUpdate);

        let summary = timings.summary(Duration::from_millis(10), false);

        assert_eq!(summary.outcome, "error");
        assert_eq!(summary.failed_stage, Some("forkchoice_update"));
        assert_eq!(summary.block_data_read_duration_seconds, Some(0.001));
        assert_eq!(summary.payload_validation_duration_seconds, Some(0.003));
        assert_eq!(summary.forkchoice_update_duration_seconds, None);
        assert_eq!(summary.commit_duration_seconds, None);
        assert_eq!(summary.block_stats_persistence_duration_seconds, None);
        assert_eq!(summary.validator_set_read_duration_seconds, None);
    }

    #[test]
    fn decision_stage_names_are_stable() {
        let cases = [
            (DecisionStage::Preparation, "preparation"),
            (AwaitedStage::BlockDataRead.into(), "block_data_read"),
            (AwaitedStage::PayloadValidation.into(), "payload_validation"),
            (AwaitedStage::ForkchoiceUpdate.into(), "forkchoice_update"),
            (AwaitedStage::Commit.into(), "commit"),
            (
                AwaitedStage::BlockStatsPersistence.into(),
                "block_stats_persistence",
            ),
            (AwaitedStage::ValidatorSetRead.into(), "validator_set_read"),
            (DecisionStage::Completion, "completion"),
        ];

        for (stage, expected) in cases {
            assert_eq!(stage.as_str(), expected);
        }
    }
}
```

- [ ] **Step 2: Run the tests and confirm the red state**

Run:

```bash
cargo test -p emerald --lib app::tests::decision_ -- --nocapture
```

Expected: compilation fails because `DecisionStage` and `DecisionTimings` do not exist.

- [ ] **Step 3: Add the private timing and summary model**

Import `core::time::Duration` and `crate::metrics::ConsensusMetrics`. Add these types immediately before
`on_decided`:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DecisionStage {
    Preparation,
    BlockDataRead,
    PayloadValidation,
    ForkchoiceUpdate,
    Commit,
    BlockStatsPersistence,
    ValidatorSetRead,
    Completion,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AwaitedStage {
    BlockDataRead,
    PayloadValidation,
    ForkchoiceUpdate,
    Commit,
    BlockStatsPersistence,
    ValidatorSetRead,
}

impl From<AwaitedStage> for DecisionStage {
    fn from(stage: AwaitedStage) -> Self {
        match stage {
            AwaitedStage::BlockDataRead => Self::BlockDataRead,
            AwaitedStage::PayloadValidation => Self::PayloadValidation,
            AwaitedStage::ForkchoiceUpdate => Self::ForkchoiceUpdate,
            AwaitedStage::Commit => Self::Commit,
            AwaitedStage::BlockStatsPersistence => Self::BlockStatsPersistence,
            AwaitedStage::ValidatorSetRead => Self::ValidatorSetRead,
        }
    }
}

impl DecisionStage {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Preparation => "preparation",
            Self::BlockDataRead => "block_data_read",
            Self::PayloadValidation => "payload_validation",
            Self::ForkchoiceUpdate => "forkchoice_update",
            Self::Commit => "commit",
            Self::BlockStatsPersistence => "block_stats_persistence",
            Self::ValidatorSetRead => "validator_set_read",
            Self::Completion => "completion",
        }
    }
}

#[derive(Debug)]
struct DecisionTimings {
    current_stage: DecisionStage,
    block_data_read: Option<Duration>,
    payload_validation: Option<Duration>,
    forkchoice_update: Option<Duration>,
    commit: Option<Duration>,
    block_stats_persistence: Option<Duration>,
    validator_set_read: Option<Duration>,
}

impl Default for DecisionTimings {
    fn default() -> Self {
        Self {
            current_stage: DecisionStage::Preparation,
            block_data_read: None,
            payload_validation: None,
            forkchoice_update: None,
            commit: None,
            block_stats_persistence: None,
            validator_set_read: None,
        }
    }
}

#[derive(Debug, PartialEq)]
struct DecisionSummary {
    outcome: &'static str,
    failed_stage: Option<&'static str>,
    duration_seconds: f64,
    block_data_read_duration_seconds: Option<f64>,
    payload_validation_duration_seconds: Option<f64>,
    forkchoice_update_duration_seconds: Option<f64>,
    commit_duration_seconds: Option<f64>,
    block_stats_persistence_duration_seconds: Option<f64>,
    validator_set_read_duration_seconds: Option<f64>,
}

impl DecisionTimings {
    fn enter_awaited(&mut self, stage: AwaitedStage) {
        self.current_stage = stage.into();
    }

    fn enter_preparation(&mut self) {
        self.current_stage = DecisionStage::Preparation;
    }

    fn enter_completion(&mut self) {
        self.current_stage = DecisionStage::Completion;
    }

    fn observe(
        &mut self,
        stage: AwaitedStage,
        duration: Duration,
        metrics: &ConsensusMetrics,
    ) {
        match stage {
            AwaitedStage::BlockDataRead => {
                self.block_data_read = Some(duration);
                metrics.observe_block_data_read(duration);
            }
            AwaitedStage::PayloadValidation => {
                self.payload_validation = Some(duration);
                metrics.observe_payload_validation(duration);
            }
            AwaitedStage::ForkchoiceUpdate => {
                self.forkchoice_update = Some(duration);
                metrics.observe_forkchoice_update(duration);
            }
            AwaitedStage::Commit => {
                self.commit = Some(duration);
                metrics.observe_commit(duration);
            }
            AwaitedStage::BlockStatsPersistence => {
                self.block_stats_persistence = Some(duration);
                metrics.observe_block_stats_persistence(duration);
            }
            AwaitedStage::ValidatorSetRead => {
                self.validator_set_read = Some(duration);
                metrics.observe_validator_set_read(duration);
            }
        }
    }

    fn summary(&self, total: Duration, succeeded: bool) -> DecisionSummary {
        DecisionSummary {
            outcome: if succeeded { "success" } else { "error" },
            failed_stage: (!succeeded).then_some(self.current_stage.as_str()),
            duration_seconds: total.as_secs_f64(),
            block_data_read_duration_seconds: self.block_data_read.map(|value| value.as_secs_f64()),
            payload_validation_duration_seconds: self
                .payload_validation
                .map(|value| value.as_secs_f64()),
            forkchoice_update_duration_seconds: self
                .forkchoice_update
                .map(|value| value.as_secs_f64()),
            commit_duration_seconds: self.commit.map(|value| value.as_secs_f64()),
            block_stats_persistence_duration_seconds: self
                .block_stats_persistence
                .map(|value| value.as_secs_f64()),
            validator_set_read_duration_seconds: self
                .validator_set_read
                .map(|value| value.as_secs_f64()),
        }
    }
}
```

- [ ] **Step 4: Run the timing-model tests and confirm the green state**

Run:

```bash
cargo test -p emerald --lib app::tests::decision_ -- --nocapture
```

Expected: all three timing-model tests pass.

- [ ] **Step 5: Turn public `on_decided` into the telemetry wrapper**

Retain the public signature. Read the certificate identity by reference, clone the metrics handle, call the private
inner handler, observe total time, emit the summary, and return the unchanged result:

```rust
pub async fn on_decided(
    decided: AppMsg<EmeraldContext>,
    state: &mut State,
    engine: &Engine,
    emerald_config: &EmeraldConfig,
) -> eyre::Result<()> {
    let AppMsg::Decided { certificate, .. } = &decided else {
        unreachable!("on_decided called with non-Decided message");
    };
    let height = certificate.height;
    let round = certificate.round;
    let value_id = certificate.value_id;

    let metrics = state.metrics.consensus.clone();
    let mut timings = DecisionTimings::default();
    let started = Instant::now();
    let result = on_decided_inner(
        decided,
        state,
        engine,
        emerald_config,
        &metrics,
        &mut timings,
    )
    .await;
    let total = started.elapsed();
    metrics.observe_total(total);

    let summary = timings.summary(total, result.is_ok());
    info!(
        event = "on_decided_timing",
        %height,
        %round,
        %value_id,
        outcome = summary.outcome,
        failed_stage = summary.failed_stage,
        duration_seconds = summary.duration_seconds,
        block_data_read_duration_seconds = summary.block_data_read_duration_seconds,
        payload_validation_duration_seconds = summary.payload_validation_duration_seconds,
        forkchoice_update_duration_seconds = summary.forkchoice_update_duration_seconds,
        commit_duration_seconds = summary.commit_duration_seconds,
        block_stats_persistence_duration_seconds =
            summary.block_stats_persistence_duration_seconds,
        validator_set_read_duration_seconds = summary.validator_set_read_duration_seconds,
        "on_decided timing"
    );

    result
}
```

- [ ] **Step 6: Move the existing behavior into `on_decided_inner` and time every await**

Give the current body this private signature:

```rust
async fn on_decided_inner(
    decided: AppMsg<EmeraldContext>,
    state: &mut State,
    engine: &Engine,
    emerald_config: &EmeraldConfig,
    metrics: &ConsensusMetrics,
    timings: &mut DecisionTimings,
) -> eyre::Result<()> {
```

Keep all current statements and their order. Replace each direct awaited stage with the following pattern: set the
active stage before the call, retain its `Result` or `Option`, observe elapsed time, and only then apply `?` or
`ok_or_eyre`.

Block-data read:

```rust
timings.enter_awaited(AwaitedStage::BlockDataRead);
let started = Instant::now();
let block_bytes = state.get_block_data(height, round, value_id).await;
timings.observe(AwaitedStage::BlockDataRead, started.elapsed(), metrics);
let block_bytes = block_bytes.ok_or_eyre("app: certificate should have associated block data")?;
timings.enter_preparation();
```

Payload validation:

```rust
timings.enter_awaited(AwaitedStage::PayloadValidation);
let started = Instant::now();
let validity = validate_execution_payload(
    state.validated_cache_mut(),
    &block_bytes,
    height,
    round,
    engine,
    &emerald_config.retry_config,
)
.await;
timings.observe(AwaitedStage::PayloadValidation, started.elapsed(), metrics);
let validity = validity?;
```

Forkchoice update:

```rust
timings.enter_awaited(AwaitedStage::ForkchoiceUpdate);
let started = Instant::now();
let latest_valid_hash = engine
    .set_latest_forkchoice_state(block_hash, &emerald_config.retry_config)
    .await;
timings.observe(AwaitedStage::ForkchoiceUpdate, started.elapsed(), metrics);
let latest_valid_hash = latest_valid_hash?;
```

Commit:

```rust
timings.enter_awaited(AwaitedStage::Commit);
let started = Instant::now();
let commit_result = state.commit(certificate).await;
timings.observe(AwaitedStage::Commit, started.elapsed(), metrics);
commit_result?;
```

Block-statistics persistence:

```rust
timings.enter_awaited(AwaitedStage::BlockStatsPersistence);
let started = Instant::now();
let stats_result = state.log_block_stats(height, tx_count, block_bytes.len(), block_time_secs).await;
timings.observe(
    AwaitedStage::BlockStatsPersistence,
    started.elapsed(),
    metrics,
);
stats_result?;
```

Validator-set read and completion:

```rust
timings.enter_awaited(AwaitedStage::ValidatorSetRead);
let started = Instant::now();
let new_validator_set =
    read_validators_from_contract(engine.eth.url().as_ref(), &latest_valid_hash).await;
timings.observe(AwaitedStage::ValidatorSetRead, started.elapsed(), metrics);
let new_validator_set = new_validator_set?;
timings.enter_completion();
```

The invalid-validity return must remain after the validation observation, so its summary reports
`failed_stage = "payload_validation"`. Do not alter the existing payload decode `unwrap`, parent-hash assertion,
state mutations, `Next::Start` reply, or reply-send error log.

- [ ] **Step 7: Record the timestamp gauge and add the stable field to the existing round-start event**

Immediately before the existing `info!` call in `on_started_round`, record the event time, then add the stable event
field without emitting a second log:

```rust
state
    .metrics
    .consensus
    .set_round_started_timestamp(SystemTime::now());

info!(
    event = "consensus_round_started",
    %height,
    %round,
    %proposer,
    ?role,
    "🟢🟢 Started round"
);
```

- [ ] **Step 8: Run focused and package regression tests**

Run:

```bash
cargo test -p emerald --lib app::tests::decision_ -- --nocapture
cargo nextest run --all-targets --all-features -p emerald
```

Expected: the three timing-model tests and the complete Emerald package suite pass. Existing restream tests must stay
green because `on_decided_inner` retains their decision lookup and commit semantics.

- [ ] **Step 9: Review the behavioral diff and commit**

Run:

```bash
git diff -- app/src/app.rs
rg -n "timeout_prevote|timeout_propose|latest_valid_hash ==|join!|try_join!" app/src/app.rs
```

Expected: only telemetry structure, stage timing, and event fields changed; there is no timeout, RPC-concurrency, or
validator-set derivation change.

Commit:

```bash
git add app/src/app.rs
git commit -m "feat(app): instrument decision processing"
```

---

### Task 3: Document Production Measurement Queries

**Files:**

- Modify: `docs/operational-docs/src/production-network/running-emerald.md:106-114`

**Interfaces:**

- Consumes: the seven histogram names, round-start timestamp gauge, and two structured event schemas from Tasks 1
  and 2.
- Produces: portable Prometheus queries and a vendor-neutral round-entry skew calculation for operators.

- [ ] **Step 1: Add the decision telemetry inventory under `## Monitoring`**

After the existing `curl` example, add a `### Decision and round-entry telemetry` section. Reflow the touched file's
existing overlong paragraphs without changing their meaning, then include a table with all eight full metric names and
these meanings:

```markdown
| Metric | Meaning |
| --- | --- |
| `app_channel_on_decided_block_data_read_duration_seconds` | Read decided payload bytes from redb. |
| `app_channel_on_decided_payload_validation_duration_seconds` | Validate the payload, including a cache hit. |
| `app_channel_on_decided_forkchoice_update_duration_seconds` | Update the execution head through Engine API. |
| `app_channel_on_decided_commit_duration_seconds` | Commit the certificate and decided data to redb. |
| `app_channel_on_decided_block_stats_persistence_duration_seconds` | Persist cumulative block statistics. |
| `app_channel_on_decided_validator_set_read_duration_seconds` | Read the next validator set at the decided hash. |
| `app_channel_on_decided_duration_seconds` | Complete application handling of the `Decided` message. |
| `app_channel_consensus_round_started_timestamp_seconds` | Unix timestamp of the latest application round start. |
```

State explicitly that all values are seconds, histogram series carry only the existing `moniker` label, and height,
round, value ID, outcome, and failure stage belong only in logs.

- [ ] **Step 2: Add portable percentile queries**

Document these PromQL examples for a 24-hour observation window:

```promql
histogram_quantile(
  0.95,
  sum by (le, moniker) (
    rate(app_channel_on_decided_duration_seconds_bucket[24h])
  )
)
```

```promql
histogram_quantile(
  0.99,
  sum by (le) (
    rate(app_channel_on_decided_duration_seconds_bucket[24h])
  )
)
```

Explain that operators substitute any stage histogram for the total and change the quantile to `0.50`, `0.95`, or
`0.99`. The first query preserves the validator moniker; the second aggregates the fleet.

- [ ] **Step 3: Document the structured events and skew calculation**

List the `on_decided_timing` fields and bounded failure values exactly as specified. Then document:

```promql
max(app_channel_consensus_round_started_timestamp_seconds)
- min(app_channel_consensus_round_started_timestamp_seconds)
```

State that this gauge query is valid only after `consensus_round_started` events independently confirm every validator
represents the same `(height, round)`. During transitions it can compare different rounds and report a misleadingly
large skew. Use the structured events as the reliable historical source:

```text
round_entry_skew(height, round) =
    max(timestamp for consensus_round_started across monikers)
  - min(timestamp for consensus_round_started across monikers)
```

Require synchronized clocks, all expected validator monikers, and recording the observation window and live config
snapshot with the results in interoperability issue #315. State that the telemetry PR does not close #315.
Document that the binary is safe to deploy node by node, but operators must complete the validator rollout before
comparing nodes and must collect at least 24 hours of data.

- [ ] **Step 4: Check documentation formatting and build the book**

Run:

```bash
awk 'length($0) > 120 { print NR ":" length($0) ":" $0 }' \
  docs/operational-docs/src/production-network/running-emerald.md
cd docs/operational-docs
mdbook build
```

Expected: the line-length command prints nothing, and mdBook completes without a broken Markdown error.

- [ ] **Step 5: Commit the operational documentation**

```bash
git add docs/operational-docs/src/production-network/running-emerald.md
git commit -m "docs: describe decision latency telemetry"
```

---

### Task 4: Run Final Verification and Audit Scope

**Files:**

- Verify: `app/src/metrics.rs`
- Verify: `app/src/app.rs`
- Verify: `docs/operational-docs/src/production-network/running-emerald.md`
- Verify: `docs/superpowers/specs/2026-08-31-on-decided-telemetry-design.md`

**Interfaces:**

- Consumes: all prior task outputs.
- Produces: reproducible test, lint, formatting, documentation, and scope evidence for review.

- [ ] **Step 1: Build generated Solidity artifacts required by a clean Emerald build**

Run:

```bash
forge build
```

Expected: Foundry regenerates ignored artifacts under `solidity/out` without changing tracked Solidity sources or
`Cargo.lock`.

- [ ] **Step 2: Run the Emerald package test suite**

Run:

```bash
cargo nextest run --all-targets --all-features -p emerald
```

Expected: every Emerald package test passes, including metric exposition, timing summaries, and proposal restream
coverage.

- [ ] **Step 3: Run focused changed-file lint and formatting checks**

Run:

```bash
cargo clippy -p emerald --tests --no-deps -- \
  -D warnings
cargo +nightly fmt -p emerald -- --check
```

Expected: both commands pass. Keep every test module after production items so no lint allowance is needed.

- [ ] **Step 4: Run the repository-required full gates**

Run:

```bash
cargo clippy --tests -- -D warnings
cargo +nightly fmt --all --check
```

Expected: pass, or reproduce only the existing `om-emerald` baseline failures in untouched files. If either command
fails, run the same command from a clean `om-emerald` worktree and include both outputs in the handoff; do not describe
a gate as passing when only the focused check passed.

- [ ] **Step 5: Run whitespace, documentation, and scope checks**

Run:

```bash
git diff --check om-emerald...HEAD
awk 'length($0) > 120 { print FNR ":" length($0) ":" FILENAME }' \
  docs/superpowers/specs/2026-08-31-on-decided-telemetry-design.md \
  docs/superpowers/plans/2026-08-31-on-decided-telemetry.md \
  docs/operational-docs/src/production-network/running-emerald.md
git diff --stat om-emerald...HEAD
git diff om-emerald...HEAD -- ':!docs/superpowers/specs/*' ':!docs/superpowers/plans/*'
git status --short --branch
```

Expected:

- no whitespace errors or overlong design/plan lines;
- tracked changes are limited to the design, plan, two Rust files, and Emerald operational documentation;
- no timeout, RPC ordering, validator derivation, storage, wire-format, or dependency change exists; and
- the branch is clean.

- [ ] **Step 6: Record verification evidence without adding a synthetic final commit**

Keep the three implementation commits from Tasks 1-3 separate. If verification exposes a defect, return to the task
that owns it, add a focused failing regression test, fix it, rerun that task's checks, and amend only that task's commit
before repeating Task 4.
