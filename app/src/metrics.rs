use core::ops::Deref;
use core::time::Duration;
use std::sync::Arc;

use malachitebft_app_channel::app::metrics;
use metrics::prometheus::metrics::counter::Counter;
use metrics::prometheus::metrics::gauge::Gauge;
use metrics::prometheus::metrics::histogram::{exponential_buckets, Histogram};
use metrics::{Registry, SharedRegistry};

#[derive(Clone, Debug)]
pub struct DbMetrics(Arc<Inner>);

impl Deref for DbMetrics {
    type Target = Inner;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug)]
pub struct Inner {
    /// Size of the database database (bytes)
    db_size: Gauge,

    /// Amount of data written to the database (bytes)
    db_write_bytes: Counter,

    /// Amount of data read from the database (bytes)
    db_read_bytes: Counter,

    /// Amount of key data read from the database (bytes)
    db_key_read_bytes: Counter,

    /// Total number of reads from the database
    db_read_count: Counter,

    /// Total number of writes to the database
    db_write_count: Counter,

    /// Total number of deletions to the database
    db_delete_count: Counter,

    /// Time taken to read from the database (seconds)
    db_read_time: Histogram,

    /// Time taken to write to the database (seconds)
    db_write_time: Histogram,

    /// Time taken to delete from the database (seconds)
    db_delete_time: Histogram,
}

impl Inner {
    pub fn new() -> Self {
        Self {
            db_size: Gauge::default(),
            db_write_bytes: Counter::default(),
            db_read_bytes: Counter::default(),
            db_key_read_bytes: Counter::default(),
            db_read_count: Counter::default(),
            db_write_count: Counter::default(),
            db_delete_count: Counter::default(),
            db_read_time: Histogram::new(exponential_buckets(0.001, 2.0, 10)), // Start from 1ms
            db_write_time: Histogram::new(exponential_buckets(0.001, 2.0, 10)),
            db_delete_time: Histogram::new(exponential_buckets(0.001, 2.0, 10)),
        }
    }
}

impl Default for Inner {
    fn default() -> Self {
        Self::new()
    }
}

impl DbMetrics {
    pub fn new() -> Self {
        Self(Arc::new(Inner::new()))
    }

    pub fn register(registry: &SharedRegistry) -> Self {
        let metrics = Self::new();

        registry.with_prefix("app_channel", |registry| {
            registry.register(
                "db_size",
                "Size of the database (bytes)",
                metrics.db_size.clone(),
            );

            registry.register(
                "db_write_bytes",
                "Amount of data written to the database (bytes)",
                metrics.db_write_bytes.clone(),
            );

            registry.register(
                "db_read_bytes",
                "Amount of data read from the database (bytes)",
                metrics.db_read_bytes.clone(),
            );

            registry.register(
                "db_key_read_bytes",
                "Amount of key data read from the database (bytes)",
                metrics.db_key_read_bytes.clone(),
            );

            registry.register(
                "db_read_count",
                "Total number of reads from the database",
                metrics.db_read_count.clone(),
            );

            registry.register(
                "db_write_count",
                "Total number of writes to the database",
                metrics.db_write_count.clone(),
            );

            registry.register(
                "db_delete_count",
                "Total number of deletions to the database",
                metrics.db_delete_count.clone(),
            );

            registry.register(
                "db_read_time",
                "Time taken to read bytes from the database (seconds)",
                metrics.db_read_time.clone(),
            );

            registry.register(
                "db_write_time",
                "Time taken to write bytes to the database (seconds)",
                metrics.db_write_time.clone(),
            );

            registry.register(
                "db_delete_time",
                "Time taken to delete bytes from the database (seconds)",
                metrics.db_delete_time.clone(),
            );
        });

        metrics
    }

    #[allow(dead_code)]
    pub fn set_db_size(&self, size: usize) {
        self.db_size.set(size as i64);
    }

    pub fn add_write_bytes(&self, bytes: u64) {
        self.db_write_bytes.inc_by(bytes);
        self.db_write_count.inc();
    }

    pub fn add_read_bytes(&self, bytes: u64) {
        self.db_read_bytes.inc_by(bytes);
        self.db_read_count.inc();
    }

    pub fn add_key_read_bytes(&self, bytes: u64) {
        self.db_key_read_bytes.inc_by(bytes);
    }

    pub fn observe_read_time(&self, duration: Duration) {
        self.db_read_time.observe(duration.as_secs_f64());
    }

    pub fn observe_write_time(&self, duration: Duration) {
        self.db_write_time.observe(duration.as_secs_f64());
    }

    pub fn observe_delete_time(&self, duration: Duration) {
        self.db_delete_time.observe(duration.as_secs_f64());
    }
}

impl Default for DbMetrics {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug)]
pub struct TxStatsMetrics(Arc<TxStatsInner>);

impl Deref for TxStatsMetrics {
    type Target = TxStatsInner;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug)]
pub struct TxStatsInner {
    /// Total number of transactions committed
    pub txs_count: Counter,

    /// Total chain bytes committed
    pub chain_bytes: Counter,

    /// Transactions per second
    pub txs_per_second: Gauge,

    /// Chain bytes per second
    pub bytes_per_second: Gauge,

    /// Transactions in the last committed block
    pub block_tx_count: Gauge,

    /// Size of the last committed block (bytes)
    pub block_size: Gauge,
}

impl TxStatsInner {
    pub fn new() -> Self {
        Self {
            txs_count: Counter::default(),
            chain_bytes: Counter::default(),
            txs_per_second: Gauge::default(),
            bytes_per_second: Gauge::default(),
            block_tx_count: Gauge::default(),
            block_size: Gauge::default(),
        }
    }
}

impl Default for TxStatsInner {
    fn default() -> Self {
        Self::new()
    }
}

impl TxStatsMetrics {
    pub fn new() -> Self {
        Self(Arc::new(TxStatsInner::new()))
    }

    pub fn register(registry: &SharedRegistry) -> Self {
        let metrics = Self::new();

        registry.with_prefix("app_channel", |registry| {
            registry.register(
                "txs_count",
                "Total number of transactions committed",
                metrics.txs_count.clone(),
            );

            registry.register(
                "chain_bytes",
                "Total chain bytes committed",
                metrics.chain_bytes.clone(),
            );

            registry.register(
                "txs_per_second",
                "Transactions per second",
                metrics.txs_per_second.clone(),
            );

            registry.register(
                "bytes_per_second",
                "Chain bytes per second",
                metrics.bytes_per_second.clone(),
            );

            registry.register(
                "block_tx_count",
                "Transactions in the last committed block",
                metrics.block_tx_count.clone(),
            );

            registry.register(
                "block_size",
                "Size of the last committed block (bytes)",
                metrics.block_size.clone(),
            );
        });

        metrics
    }

    pub fn add_txs(&self, count: u64) {
        self.txs_count.inc_by(count);
    }

    pub fn add_chain_bytes(&self, bytes: u64) {
        self.chain_bytes.inc_by(bytes);
    }

    pub fn set_txs_per_second(&self, tps: f64) {
        self.txs_per_second.set(tps as i64);
    }

    pub fn set_bytes_per_second(&self, bps: f64) {
        self.bytes_per_second.set(bps as i64);
    }

    pub fn set_block_tx_count(&self, count: u64) {
        self.block_tx_count.set(count as i64);
    }

    pub fn set_block_size(&self, size: u64) {
        self.block_size.set(size as i64);
    }
}

impl Default for TxStatsMetrics {
    fn default() -> Self {
        Self::new()
    }
}

fn decision_latency_buckets() -> impl Iterator<Item = f64> {
    exponential_buckets(0.001, 2.0, 18)
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
}

impl Default for ConsensusMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Unified metrics container for all application metrics
#[derive(Clone, Debug)]
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

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use metrics::prometheus::encoding::text::encode;

    use super::*;

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

        assert!(output.contains("app_channel_on_decided_duration_seconds_bucket{le=\"0.001\"}"));
        assert!(output.contains("app_channel_on_decided_duration_seconds_bucket{le=\"131.072\"}"));

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
