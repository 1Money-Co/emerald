# Running Emerald (Consensus Engine)

> [!NOTE]
> This section applies to **all network participants** (both the coordinator and all validators). 
> Each validator must run their own Emerald node with the private key they generated earlier.

## Prerequisites

- Emerald binaries installed (see [Installing Emerald](installation.md#installing-emerald))
- Node configuration directory created (contains `config.toml`, `emerald.toml`, and `priv_validator_key.json`)
  - Recommended to set up a user `emerald` and use a home folder like `/home/emerald/.emerald`, with a config
    folder inside it for all files.
- Reth node must be running with Engine API enabled
- JWT secret file (same as used by Reth)
- `emerald.toml` has to contain the path to the genesis file used by Reth that contains the chain configuration
  (`eth-genesis.json` in our example).

## Configuration Files

Each Emerald node requires two configuration files in its home directory:

**1. `config.toml` (MalachiteBFT Configuration)**

See [malachitebft-config.toml](../config-examples/malachitebft-config.toml) for a complete example. Key sections:

- **Consensus settings**: Block timing, timeouts, and consensus parameters
- **P2P networking**: Listen addresses and peer connections
  - Consensus P2P: Port `27000` (default)
    -  persistent_peers must be filled out for p2p
  - Mempool P2P: Port `28000` (default)
    -  persistent_peers must be filled out for p2p
- **Metrics**: Prometheus metrics endpoint on port `30000`

This file must be in the config folder in `home_dir`, for example
`/home/emerald/.emerald/config/config.toml`, where the `--home` flag would be defined as
`--home=/home/emerald/.emerald`

**2. `emerald.toml` (Execution Integration)**

See [emerald-config.toml](../config-examples/emerald-config.toml) for a complete example. Key settings:

```toml
moniker = "validator-0"
ethereum_config.execution_authrpc_address = "http://<RETH_IP>:8545"
ethereum_config.engine_authrpc_address = "http://<RETH_IP>:8551"
ethereum_config.jwt_token_path = "/path/to/jwt.hex"
ethereum_config.eth_genesis_path="<PATH_TO_RETH_GENESIS>"
el_node_type = "archive"
retry_config.initial_delay = "100ms"
retry_config.max_delay = "2s"
retry_config.max_elapsed_time = "20s"
fee_recipient = "0x4242424242424242424242424242424242424242"
...
```

> [!IMPORTANT]
> The `jwt_token_path` must point to the same JWT token used by Reth.
> The `fee_recipient` must point to a valid address as this address will receive fees.

This is where you define how Emerald connects to Reth. Make sure to fill in the Reth http and authrpc address.

By default the validator key is read from `<home>/config/priv_validator_key.json`. To load it from
AWS Secrets Manager + KMS or GCP Secret Manager + Cloud KMS instead, add a `[key_provider]` section
— see [Validator Key Management](key-management.md).

## Configure Peer Connections

For a multi-node network, configure persistent peers in `config.toml`:

```toml
[consensus.p2p]
listen_addr = "/ip4/0.0.0.0/tcp/27000"
persistent_peers = [
    "/ip4/<PEER1_IP>/tcp/27000",
    "/ip4/<PEER2_IP>/tcp/27000",
    "/ip4/<PEER3_IP>/tcp/27000",
]

[mempool.p2p]
listen_addr = "/ip4/0.0.0.0/tcp/28000"
persistent_peers = [
    "/ip4/<PEER1_IP>/tcp/28000",
    "/ip4/<PEER2_IP>/tcp/28000",
    "/ip4/<PEER3_IP>/tcp/28000",
]
```

Replace `<PEER_IP>` with the actual IP addresses of your validator peers.

In the Malachite BFT `config.toml`, fill in the `persistent_peers` array in the `consensus.p2p` and `mempool.p2p`
sections. It uses the format `/ip4/<IP_ADDRESS_TO_REMOTE_PEER>/tcp/<PORT_FOR_REMOTE_PEER>`. Make sure to fill in all
peers in the testnet.

## Start Emerald Node

Start the Emerald consensus node:

```bash
emerald start \
  --home /home/emerald/.emerald \
  --config /home/emerald/.emerald/config/emerald.toml \
  --log-level info
```

The `--home` directory should contain:
- `<home>/config/config.toml` - Malachite BFT configuration
- `<home>/config/priv_validator_key.json` - Validator signing key (not needed when a cloud
  [key provider](key-management.md) is configured)
- `<home>/config/genesis.json` - Malachite BFT genesis file

An example Malachite BFT config file is provided:
[malachitebft-config.toml](../config-examples/malachitebft-config.toml)

The `--config` flag should contain the explicit file path to the Emerald config:
- Example: `--config=/home/emerald/.emerald/config/emerald.toml`

## Undecided Payload Storage Upgrade

The first startup with the round-independent schema reconciles four local redb tables. The active layout uses
`undecided_values_v2` for payload-free proposal metadata and `undecided_block_data_v2` for one payload per
`(height, value_id)`. During this compatibility release, `undecided_values` remains a full, round-keyed
`ProposedValue` table for N-1 and `undecided_block_data` remains its exact-round payload shadow. Runtime writes both
proposal tables and both payload tables. Current Emerald reads compact v2 metadata first, then verifies and hydrates
it from shared payload storage; a full v1 and exact-round payload fallback consumes state written during rollback.

Before upgrading each node:

1. Stop Emerald cleanly.
2. Back up `<home>/store.db`.
3. Reserve temporary free space for approximately one deduplicated undecided payload set plus redb overhead.
4. Start the new binary and wait for the `undecided_block_data_migration` event with reconciliation version `1`
   before upgrading another validator.

Reconciliation copies every unique payload into v2, validates duplicate bytes, preserves or restores full v1
proposal rows, and populates compact v2 proposal rows in one transaction. Only after every phase succeeds does it
write schema version `1`. That copy requires headroom even when many legacy rows are duplicates. Later version-1
starts skip legacy payload and proposal scans. They first check each decided key for its payload and perform the
certificate, ID, byte, SSZ, and header validation only when that payload is missing.

Both full proposal and exact-round payload shadows are deliberate compatibility duplicates. Replaced redb pages can
be reused, but neither reconciliation nor later logical deletion guarantees that `store.db` shrinks. Space is still
required for both active and compatibility layouts during this release.

[Interop issue #325](https://github.com/1Money-Co/1money-interoperability-protocol/issues/325) owns the later activated
release that removes both shadows and the operator procedure for explicit offline redb compaction when
filesystem-space reclamation is required. This release does not delete either shadow or run automatic compaction
during startup.

The same startup transaction repairs the N-1 crash window where decided value, certificate, and header rows committed
but their payload remained only in undecided storage. Emerald promotes the certificate-bound payload only after its
value ID, stored value bytes, SSZ encoding, and stored header all agree. Missing or conflicting data aborts startup
without changing the database. It also repairs an ID-only decided value left by an earlier unsafe PR revision, using
the same certificate, payload, SSZ, and header checks. Full v1 proposals prevent N-1 from creating another ID-only
decided value. New current-binary commits persist the payload and all three metadata rows in one transaction.

If legacy rows use the same `(height, value_id)` with different bytes, startup fails without changing the legacy
table. The error and structured log identify the incoming legacy round, the existing round when it came from legacy
data, and both payload lengths without logging payload bytes.
Emerald has no safe automated repair for this corrupted state: preserve the database and logs for diagnosis, or
restore the backup and continue with the previous binary. Do not delete either row without determining the
authoritative payload.

During this compatibility release, a node may return to N-1 using the same `store.db`: stop N completely before
starting N-1, and never let two Emerald processes open the database concurrently. N reads v2 first but atomically
dual-writes and prunes both N-1 tables. Keep the pre-upgrade backup despite this compatibility path; separately
persisted Malachite WAL and Reth state still make arbitrary partial filesystem restoration unsafe.

The deterministic test suite pins the N-proposal/N-1-commit database and wire contract. The opt-in
`scripts/tests/rollback_binary_qualification.sh` runner exercises real process and binary transitions when explicit
current Emerald, N-1 Emerald, and custom Reth binaries are supplied. Passing deterministic tests does not imply that
the mixed-binary release qualification was run or passed.

## Monitoring

Emerald exposes Prometheus metrics on port `30000` (configurable in `config.toml`):

```bash
curl http://<IP>:30000/metrics
```

### Decision and round-entry telemetry

The decision telemetry histograms below record wall-clock seconds. Their Prometheus series carry only the existing
`moniker` label. Height, round, value ID, outcome, and failure stage stay in logs rather than metric labels to avoid
high-cardinality time series.

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

Use the same PromQL pattern for any stage histogram by swapping the metric name and changing the quantile to `0.50`,
`0.95`, or `0.99`.

Per-validator `p95` over a 24-hour window:

```promql
histogram_quantile(
  0.95,
  sum by (le, moniker) (
    rate(app_channel_on_decided_duration_seconds_bucket[24h])
  )
)
```

Fleet-wide `p99` over a 24-hour window:

```promql
histogram_quantile(
  0.99,
  sum by (le) (
    rate(app_channel_on_decided_duration_seconds_bucket[24h])
  )
)
```

The first query preserves the validator `moniker`. The second aggregates the entire validator fleet.

The application also emits one `on_decided_timing` event per `on_decided` return. Every event includes `height`,
`round`, `value_id`, `outcome`, and `duration_seconds`. Successful events also include every completed stage duration:

- `block_data_read_duration_seconds`
- `payload_validation_duration_seconds`
- `forkchoice_update_duration_seconds`
- `commit_duration_seconds`
- `block_stats_persistence_duration_seconds`
- `validator_set_read_duration_seconds`

Failed events include `failed_stage` instead of unreached duration fields. The bounded `failed_stage` values are
`preparation`, `block_data_read`, `payload_validation`, `forkchoice_update`, `commit`,
`block_stats_persistence`, `validator_set_read`, and `completion`.

With synchronized validator clocks, query the current round-entry skew directly from Prometheus only after
independently confirming from `consensus_round_started` events that every validator series represents the same
`(height, round)`:

```promql
max(app_channel_consensus_round_started_timestamp_seconds)
- min(app_channel_consensus_round_started_timestamp_seconds)
```

The gauge has one bounded series per validator `moniker`. It intentionally does not label height or round. During a
round transition, the query can compare timestamps from different rounds and report a misleadingly large skew. Do not
use it until event data confirms alignment; use `consensus_round_started` events as the reliable source for historical
per-height and per-round correlation:

```text
round_entry_skew(height, round) =
    max(timestamp for consensus_round_started across monikers)
  - min(timestamp for consensus_round_started across monikers)
```

Before comparing validators, finish the node-by-node telemetry rollout across the validator set, confirm all expected
validator `moniker` values are present, and collect at least 24 hours of data. When recording results in
interoperability issue `#315`, include the observation window and the live config snapshot used for the capture. This
telemetry change is safe to deploy incrementally, but it does not close issue `#315` on its own.

## Systemd Service

For production deployments, use systemd to manage the Emerald process. See
[emerald.systemd.service.example](../config-examples/emerald.systemd.service.example) for a complete service
configuration.
