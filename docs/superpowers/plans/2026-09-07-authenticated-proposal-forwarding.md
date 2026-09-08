# Authenticated Proposal Forwarding Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Preserve and forward the selected proposer's authenticated proposal stream so Malachite's hidden-lock
restream can recover a value on a validator that did not author it.

**Architecture:** Add a storage-only proposal attestation and make payload, proposal metadata, and attestation one
coherent redb aggregate. Local and received streams persist the signed envelope before consensus or network
publication; restream dispatch either replays that envelope exactly, performs the existing local re-proposal flow,
or drops safely. Deterministic tests drive the pinned Malachite state machine and the production Emerald handlers
from hidden-lock effect through late decision.

**Tech Stack:** Rust 1.83, Tokio, redb, Prost/Protobuf, secp256k1/K256, Malachite core consensus at
`bcac2b2dbd369a6c29b9151fcd0889b133d7221a`, Alloy execution payload types, Cargo, Clippy, rustfmt

**Spec:** `docs/superpowers/specs/2026-09-04-authenticated-proposal-forwarding-design.md`

## Global Constraints

- Work on `seb/authenticated-proposal-forwarding`, based on `origin/om-emerald` at
  `6e8dcfd696aa1cbfbd13aa5d482fd9e14028a277`; the approved spec commits are `697f224` and `8dcfaea`.
- Keep `value_payload = "parts-only"`; do not change Malachite, the proposal-part wire format, the signed digest,
  genesis, or the protocol version.
- Do not relax `validate_proposal_parts` or `verify_proposal_parts_signature`.
- Do not recompute `ValueId` as a new validation rule. Compare the stored key, the serialized `Value`, and both
  payload copies byte-for-byte as specified.
- Treat sync's `Round::Nil` valid round as unknown only for an unattested sync write against an existing coherent
  proposal. Never rewrite an existing proposal or attestation during that merge.
- Persist local attestations before replying to `GetValue` or publishing parts. On a conflict or integrity/storage
  error, do not reply with `Some` and do not publish.
- Preserve raw block-data reads for execution and commit handling, but never use them alone to restore proposal
  metadata.
- Add no production dependency. Add `malachitebft-core-consensus` and `malachitebft-metrics` only as `emerald`
  dev-dependencies for deterministic state-machine tests.
- The optional Quint restream action is excluded because Emerald PR #19 is not present on this base. If that PR
  lands, cover it in a separately reviewed follow-up; the deterministic composite test in Task 5 remains mandatory.
- Keep Markdown at 120 columns and do not format `Cargo.lock`.
- Before pushing, run `cargo clippy --tests -- -D warnings` and `cargo +nightly fmt --all --check`.

## File Structure

- Modify `types/proto/consensus.proto`: add the storage-only `ProposalAttestation` protobuf message.
- Modify `types/src/proposal_part.rs`: add the Rust attestation type and individual protobuf conversions for init,
  fin, and the aggregate.
- Modify `types/src/codec/proto/mod.rs`: add `ProtobufCodec` support for `ProposalAttestation`.
- Modify `app/src/store.rs`: define the aggregate write/read contract, create and prune the attestation table,
  enforce all transition and sync-merge rules atomically, and add raw-state storage tests.
- Modify `app/src/state.rs`: use the aggregate store API, persist locally built attestations before returning stream
  messages, verify stored signatures before replay, and generate stream IDs from explicit height and round values.
- Modify `app/src/app.rs`: enforce caller-specific reply/publication behavior and dispatch restream effects between
  replay, local build, and safe drop.
- Modify `app/Cargo.toml`: add test-only access to Malachite core consensus and metrics.
- Modify `app/src/lib.rs`: include the forwarding acceptance-test module under `#[cfg(test)]`.
- Create `app/src/proposal_forwarding_tests.rs`: hold multi-validator fixtures, effect-driving helpers, handler
  outcome tests, the Malachite hidden-lock contract, and the end-to-end deterministic acceptance test.

---

### Task 1: Add the storage-only proposal attestation codec

**Files:**

- Modify: `types/proto/consensus.proto:45-66`
- Modify: `types/src/proposal_part.rs:59-151`
- Modify: `types/src/codec/proto/mod.rs:1-45`
- Test: `types/src/proposal_part.rs` in a new local `#[cfg(test)]` module

**Interfaces:**

- Consumes: existing `ProposalInit`, `ProposalFin`, `malachitebft_proto::Protobuf`, and `ProtobufCodec`.
- Produces:

```rust
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalAttestation {
    pub init: ProposalInit,
    pub fin: ProposalFin,
}

impl ProposalAttestation {
    pub fn new(init: ProposalInit, fin: ProposalFin) -> Self;
}

impl Protobuf for ProposalInit { type Proto = crate::proto::ProposalInit; }
impl Protobuf for ProposalFin { type Proto = crate::proto::ProposalFin; }
impl Protobuf for ProposalAttestation { type Proto = crate::proto::ProposalAttestation; }
impl Codec<ProposalAttestation> for ProtobufCodec;
```

- [ ] **Step 1: Add failing round-trip tests for nil and defined POL rounds**

Add two tests that construct deterministic K256 signatures, encode through `ProtobufCodec`, decode, and assert full
equality:

```rust
#[test]
fn proposal_attestation_round_trips_nil_pol_round() {
    proposal_attestation_round_trip(Round::Nil);
}

#[test]
fn proposal_attestation_round_trips_defined_pol_round() {
    proposal_attestation_round_trip(Round::new(7));
}

fn proposal_attestation_round_trip(pol_round: Round) {
    let key = PrivateKey::from_slice(&[9_u8; 32]).unwrap();
    let attestation = ProposalAttestation::new(
        ProposalInit::new(
            Height::new(42),
            Round::new(10),
            pol_round,
            Address::from_public_key(&key.public_key()),
        ),
        ProposalFin::new(key.sign(b"attestation-codec")),
    );

    let bytes = ProtobufCodec.encode(&attestation).unwrap();
    let decoded: ProposalAttestation = ProtobufCodec.decode(bytes).unwrap();
    assert_eq!(decoded, attestation);
}
```

- [ ] **Step 2: Run the focused tests and confirm the missing type is the failure**

Run:

```bash
cargo test -p malachitebft-eth-types proposal_attestation_round_trips -- --nocapture
```

Expected: compilation fails because `ProposalAttestation` and its codec do not exist.

- [ ] **Step 3: Add the protobuf message and domain conversions**

Append this message after `ProposalFin`:

```protobuf
message ProposalAttestation {
    ProposalInit init = 1;
    ProposalFin fin = 2;
}
```

Move the existing init and fin field conversion code into individual `Protobuf` implementations. Make
`ProposalPart::from_proto` and `ProposalPart::to_proto` delegate to those implementations, then implement the
aggregate with required-field checks:

```rust
impl Protobuf for ProposalAttestation {
    type Proto = crate::proto::ProposalAttestation;

    fn from_proto(proto: Self::Proto) -> Result<Self, ProtoError> {
        Ok(Self {
            init: ProposalInit::from_proto(
                proto.init.ok_or_else(|| {
                    ProtoError::missing_field::<Self::Proto>("init")
                })?,
            )?,
            fin: ProposalFin::from_proto(
                proto.fin.ok_or_else(|| {
                    ProtoError::missing_field::<Self::Proto>("fin")
                })?,
            )?,
        })
    }

    fn to_proto(&self) -> Result<Self::Proto, ProtoError> {
        Ok(Self::Proto {
            init: Some(self.init.to_proto()?),
            fin: Some(self.fin.to_proto()?),
        })
    }
}
```

Add the codec implementation next to the existing proposal-part codec:

```rust
impl Codec<ProposalAttestation> for ProtobufCodec {
    type Error = ProtoError;

    fn decode(&self, bytes: Bytes) -> Result<ProposalAttestation, Self::Error> {
        Protobuf::from_bytes(&bytes)
    }

    fn encode(&self, msg: &ProposalAttestation) -> Result<Bytes, Self::Error> {
        Protobuf::to_bytes(msg)
    }
}
```

- [ ] **Step 4: Run the type tests**

Run:

```bash
cargo test -p malachitebft-eth-types proposal_attestation_round_trips -- --nocapture
```

Expected: both tests pass, including preservation of `Round::Nil`.

- [ ] **Step 5: Commit the codec boundary**

```bash
git add types/proto/consensus.proto types/src/proposal_part.rs types/src/codec/proto/mod.rs
git commit -m "feat: add proposal attestation codec"
```

---

### Task 2: Enforce the atomic undecided-proposal state machine

**Files:**

- Modify: `app/src/store.rs:19-103,203-310,418-548,607-670,793-924,973-end`

**Interfaces:**

- Consumes: `ProposalAttestation` and the existing `UndecidedValueKey`.
- Produces:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UndecidedWriteSource {
    LocalProposal,
    ReceivedProposal,
    Sync,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UndecidedConflictField {
    ValidRound,
    Proposer,
    Value,
    Payload,
    Attestation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UndecidedConflict {
    pub field: UndecidedConflictField,
    pub stored: ProposedValue<EmeraldContext>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UndecidedWriteOutcome {
    Canonical(ProposedValue<EmeraldContext>),
    Conflict(UndecidedConflict),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UndecidedProposalWrite {
    pub proposal: ProposedValue<EmeraldContext>,
    pub payload: Bytes,
    pub attestation: Option<ProposalAttestation>,
    pub source: UndecidedWriteSource,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredUndecidedProposal {
    pub proposal: ProposedValue<EmeraldContext>,
    pub payload: Bytes,
    pub attestation: Option<ProposalAttestation>,
}

impl Store {
    pub async fn write_undecided_proposal(
        &self,
        write: UndecidedProposalWrite,
    ) -> Result<UndecidedWriteOutcome, StoreError>;

    pub async fn get_undecided_record(
        &self,
        height: Height,
        round: Round,
        value_id: ValueId,
    ) -> Result<Option<StoredUndecidedProposal>, StoreError>;
}
```

- Add `StoreError::Integrity(String)`. Input incoherence and corrupt stored state use this variant; ordinary
  first-writer disagreement returns `UndecidedWriteOutcome::Conflict`.

- [ ] **Step 1: Replace the prune fixture's split writes with an aggregate test fixture**

Introduce these helpers in `app/src/store.rs` tests before changing production methods:

```rust
fn make_write(
    height: u64,
    round: u32,
    valid_round: Round,
    proposer: Address,
    payload: Bytes,
    attestation: Option<ProposalAttestation>,
    source: UndecidedWriteSource,
) -> UndecidedProposalWrite {
    UndecidedProposalWrite {
        proposal: ProposedValue {
            height: Height::new(height),
            round: Round::new(round),
            valid_round,
            proposer,
            value: Value::new(payload.clone()),
            validity: Validity::Valid,
        },
        payload,
        attestation,
        source,
    }
}

fn assert_canonical(
    outcome: UndecidedWriteOutcome,
) -> ProposedValue<EmeraldContext> {
    match outcome {
        UndecidedWriteOutcome::Canonical(value) => value,
        UndecidedWriteOutcome::Conflict(conflict) => {
            panic!("expected canonical write, got conflict: {conflict:?}")
        }
    }
}
```

Update `test_prune` to insert proposal rows only through the new write shape and to use each proposal's actual
`value.id()` for lookups. This prevents the existing fixture from creating deliberate key/payload corruption.

- [ ] **Step 2: Add the complete transition-table test set**

Add tests with these exact names and assertions. Use private `Db` access to seed partial states in one redb
transaction; do not add a production escape hatch.

| Test | Seed and write | Required assertion |
| --- | --- | --- |
| `undecided_write_empty_unattested` | Empty; local-source write | Payload and metadata present; no attestation |
| `undecided_write_empty_attested` | Empty; attested proposal write | All three records present |
| `undecided_write_repeat_is_idempotent` | Coherent full state; same write | Canonical value; bytes unchanged |
| `undecided_write_backfills_matching_attestation` | Coherent unattested proposal | Only attestation is added |
| `undecided_write_rejects_backfill_pol_round_conflict` | Nil valid round; defined POL | `ValidRound`; no backfill |
| `undecided_write_rejects_colliding_payload` | Same key; different bytes | `Value`; bytes unchanged |
| `undecided_write_completes_equal_orphan` | Payload only with equal bytes | Metadata added for both write shapes |
| `undecided_write_replaces_colliding_orphan` | Unequal payload orphan | Incoming payload replaces orphan |
| `undecided_write_unattested_against_attested` | Attested row | Match is no-op; disagreement is conflict |
| `undecided_write_rejects_incoherent_input` | Embedded and input payload differ | Integrity; no row |
| `undecided_write_surfaces_partial_record_corruption` | Metadata-only or attestation-only | Integrity |
| `undecided_write_surfaces_attestation_identity_corruption` | Init differs from metadata | Integrity; no repair |
| `undecided_write_surfaces_split_legacy_payload` | Embedded B, separate colliding A | Integrity for both writes |
| `undecided_write_surfaces_storage_key_mismatch` | Metadata ID differs from key | Integrity; no value |
| `undecided_sync_preserves_stored_valid_round` | Defined-POL row, then sync nil | Stored proposal unchanged |
| `undecided_proposal_after_sync_conflicts` | Sync nil, then defined POL | Conflict; no backfill |
| Local nil-POL recovery | Local defined POL, then local nil | Replace metadata and attestation |
| Received POL conflict | Received defined POL, then received nil | Conflict; bytes unchanged |
| Local reverse-POL conflict | Local nil POL, then local defined POL | Conflict; bytes unchanged |
| `undecided_reads_reject_split_records` | Corrupt key, payload, and init seeds | Both read APIs return integrity |

Name those three source-and-direction tests `undecided_local_attested_write_replaces_stored_valid_round`,
`undecided_received_attested_write_rejects_stored_valid_round`, and
`undecided_local_defined_valid_round_does_not_replace_nil`, respectively.

For the collision-only seeds, construct the redb key explicitly with a chosen `ValueId`; do not claim the test has
found a natural `DefaultHasher` collision. The invariant under test is byte comparison despite an equal key.

- [ ] **Step 3: Run the transition tests and confirm the aggregate API is absent**

Run:

```bash
cargo test -p emerald undecided_write_ -- --nocapture
cargo test -p emerald undecided_sync_ -- --nocapture
cargo test -p emerald undecided_proposal_after_sync_conflicts -- --nocapture
```

Expected: compilation fails on the new aggregate types and methods.

- [ ] **Step 4: Create the table and implement input/stored-record validation**

Add:

```rust
const UNDECIDED_PROPOSAL_ATTESTATIONS_TABLE:
    redb::TableDefinition<'_, UndecidedValueKey, Vec<u8>> =
    redb::TableDefinition::new("undecided_proposal_attestations");
```

Open it in `create_tables` and prune it at the same `block_data_retain_height` as undecided metadata. Implement two
pure validation helpers used by reads and writes:

```rust
fn validate_write(write: &UndecidedProposalWrite) -> Result<UndecidedValueKey, StoreError>;

fn validate_stored_record(
    key: (Height, Round, ValueId),
    proposal: ProposedValue<EmeraldContext>,
    payload: Bytes,
    attestation: Option<ProposalAttestation>,
) -> Result<StoredUndecidedProposal, StoreError>;
```

`validate_write` derives the key from metadata, requires `proposal.value.extensions == payload`, and, when an
attestation exists, requires init height, round, POL round, and proposer to equal metadata. The signature is a caller
precondition because only `State` has validator-key context.

`validate_stored_record` checks the explicit redb key against metadata, compares the complete `Value` and separate
payload, and checks the same four init fields. Include the key and field name in integrity messages, but never include
payload bytes.

- [ ] **Step 5: Implement the single-transaction transition function**

Replace `insert_undecided_proposal` and the undecided use of `insert_undecided_block_data` with:

```rust
fn write_undecided_proposal(
    &self,
    write: UndecidedProposalWrite,
) -> Result<UndecidedWriteOutcome, StoreError>;
```

Inside one redb write transaction, load all three optional values and classify exactly:

```rust
match (payload, proposal, attestation) {
    (None, None, None) => insert_required_components(write),
    (Some(orphan), None, None) => reconcile_orphan_and_insert(orphan, write),
    (Some(payload), Some(proposal), attestation) => {
        let stored = validate_stored_record(key, proposal, payload, attestation)?;
        compare_and_merge(stored, write)
    }
    _ => Err(StoreError::Integrity(format!(
        "invalid undecided proposal record shape for {key:?}"
    ))),
}
```

`compare_and_merge` compares proposer, complete value, payload, and then valid round. For `Sync`, skip only the valid
round comparison and return the stored proposal. For `Proposal`, compare it. A matching attested write against an
unattested proposal inserts only the attestation. A matching unattested write against an attested proposal is a no-op.
Commit once, then update write metrics with only the values actually inserted or replaced.

- [ ] **Step 6: Implement coherent aggregate reads**

Make `get_undecided_record` open all three tables in one redb read transaction and apply the same state classifier.
Return `None` for Empty and Orphaned payload states, a validated aggregate for either coherent proposal state, and
`StoreError::Integrity` for every corrupt shape.

Keep `get_undecided_proposal` and `get_undecided_proposals` as compatibility projections over validated aggregate
reads. For the list operation, iterate metadata keys for the requested height/round and validate the matching payload
and optional attestation before returning any proposal.

- [ ] **Step 7: Run storage tests and the pre-existing prune tests**

Run:

```bash
cargo test -p emerald undecided_ -- --nocapture
cargo test -p emerald test_prune -- --nocapture
```

Expected: every transition, sync merge, corruption read, and prune assertion passes. The prune test confirms the
attestation disappears at the same height as its metadata and payload.

- [ ] **Step 8: Commit the storage invariant**

```bash
git add app/src/store.rs
git commit -m "feat: store coherent proposal attestations"
```

---

### Task 3: Route every proposal write through the aggregate boundary

**Files:**

- Modify: `app/src/state.rs:396-453,516-533,631-826`
- Modify: `app/src/app.rs:194-294,302-344,725-788`
- Modify: `app/src/lib.rs:1-10`
- Create: `app/src/proposal_forwarding_tests.rs`

**Interfaces:**

- Consumes: `Store::write_undecided_proposal`, `UndecidedWriteSource`, and `UndecidedWriteOutcome` from Task 2.
- Produces:

```rust
impl State {
    pub async fn store_unattested_proposal(
        &self,
        proposal: ProposedValue<EmeraldContext>,
        payload: Bytes,
        source: UndecidedWriteSource,
    ) -> Result<UndecidedWriteOutcome, StoreError>;

    pub async fn stream_proposal(
        &mut self,
        value: LocallyProposedValue<EmeraldContext>,
        data: Bytes,
        pol_round: Round,
    ) -> eyre::Result<Vec<StreamMessage<ProposalPart>>>;
}
```

- `process_complete_proposal_parts` returns `Ok(None)` on validation failure or conflict, `Ok(Some(canonical))` on
  success, and `Err` on integrity/storage failure.
- `on_process_synced_value` replies with the canonical value returned by storage, which may carry a defined stored
  valid round.

- [ ] **Step 1: Add reusable forwarding fixtures**

Register the new test module in `app/src/lib.rs`:

```rust
#[cfg(test)]
mod proposal_forwarding_tests;
```

In that file, define `TestValidator { key, provider, validator }`, a four-validator `ValidatorSet`, and
`make_state(local_index, validator_set)` backed by a temporary store. Set the returned state's validator set and
consensus height explicitly. Add `make_execution_payload()` with an SSZ-encodable `ExecutionPayloadV3` whose block
hash is deterministic, and seed `state.validated_cache_mut()` with that hash and `Validity::Valid` before invoking
the receive handler. Construct a real `Engine` pointed at unused localhost URLs; the cache hit guarantees no RPC.

The fixture must return the temporary directory with the state, and its JWT helper must write a valid 32-byte hex
secret before constructing `EngineRPC`.

Use this payload and engine shape so the fixture does not depend on Reth:

```rust
fn make_execution_payload() -> (Bytes, B256) {
    let block_hash = B256::repeat_byte(0x42);
    let payload = ExecutionPayloadV3 {
        payload_inner: ExecutionPayloadV2 {
            payload_inner: ExecutionPayloadV1 {
                parent_hash: B256::ZERO,
                fee_recipient: AlloyAddress::ZERO,
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Bloom::default(),
                prev_randao: B256::ZERO,
                block_number: 1,
                gas_limit: 30_000_000,
                gas_used: 0,
                timestamp: 1,
                extra_data: AlloyBytes::new(),
                base_fee_per_gas: U256::from(1),
                block_hash,
                transactions: Vec::new(),
            },
            withdrawals: Vec::new(),
        },
        blob_gas_used: 0,
        excess_blob_gas: 0,
    };
    (Bytes::from(payload.as_ssz_bytes()), block_hash)
}

fn make_test_engine(dir: &tempfile::TempDir) -> Engine {
    let jwt_path = dir.path().join("jwt.hex");
    std::fs::write(&jwt_path, "11".repeat(32)).unwrap();
    Engine::new(
        EngineRPC::new(Url::parse("http://127.0.0.1:1").unwrap(), &jwt_path).unwrap(),
        EthereumRPC::new(Url::parse("http://127.0.0.1:1").unwrap()).unwrap(),
    )
}
```

- [ ] **Step 2: Add failing tests for attested receive and local preparation order**

Add these exact handler/state tests:

```text
received_stream_persists_attestation_and_returns_canonical_value
received_stream_conflict_replies_none_without_mutating_storage
received_valid_round_conflict_is_not_delivered_to_consensus
received_stream_storage_error_sends_no_reply
local_stream_persists_attestation_before_returning_messages
local_stream_conflict_returns_error_without_messages
local_stream_storage_error_returns_no_messages
get_value_attestation_conflict_sends_no_reply_or_network_message
local_reproposal_conflict_publishes_nothing
local_reproposal_storage_error_publishes_nothing
synced_value_returns_existing_defined_valid_round
synced_value_conflict_replies_none
synced_value_storage_error_sends_no_reply
synced_value_recovers_attested_reproposal_after_restart
```

Use Tokio oneshot receivers to distinguish `Some`, `None`, and sender-drop. For network assertions, drain a bounded
`Channels::network` receiver with `try_recv()` after the handler returns. For conflict and integrity setup, use
`#[cfg(test)] pub(crate)` store seeding helpers that write raw records only from this crate's tests.

For each storage-error case, seed a matching payload plus malformed proposal protobuf bytes at the target key. The
aggregate decoder must return `StoreError::Protobuf`; this distinguishes a database/codec failure from a typed
first-writer conflict without adding runtime failure injection.

For `synced_value_recovers_attested_reproposal_after_restart`, persist the defined-POL attested row, discard its
original consensus reply, construct a new `State` over the same store at a different round, and invoke
`on_process_synced_value` with the matching certificate payload. Assert the reply contains the stored defined POL
round, feed it to a consensus fixture that already has the precommit quorum, and assert it decides.

- [ ] **Step 3: Run the focused tests and verify the old two-write behavior fails them**

Run:

```bash
cargo test -p emerald received_stream_ -- --nocapture
cargo test -p emerald local_stream_ -- --nocapture
cargo test -p emerald get_value_attestation_conflict -- --nocapture
cargo test -p emerald local_reproposal_ -- --nocapture
cargo test -p emerald synced_value_ -- --nocapture
```

Expected: compilation fails because `stream_proposal` is not async/fallible and the aggregate call paths are absent.

- [ ] **Step 4: Persist received attestations in the validated receive path**

After the unchanged proposer/signature and execution-payload validation, obtain the already validated envelope:

```rust
let attestation = ProposalAttestation::new(
    parts.init().expect("validated proposal has init").clone(),
    parts.fin().expect("validated proposal has fin").clone(),
);
let write = UndecidedProposalWrite {
    proposal: value,
    payload: data,
    attestation: Some(attestation),
    source: UndecidedWriteSource::ReceivedProposal,
};
```

Map `Canonical(value)` to `Some(value)`. Log `Conflict` at warning with height, round, value ID, stored and incoming
proposers/POL rounds, and `conflict.field`, then return `Ok(None)`. Propagate `StoreError`, which makes
`on_received_proposal_part` return before sending its reply. Malachite deduplicates a same-value proposal before
reconsidering `pol_round`, so forwarding a `ValidRound` conflict to consensus would not repair the retained envelope.

- [ ] **Step 5: Convert local construction and streaming to the two-phase aggregate write**

Make `propose_value` and the build branch of `prepare_restream_proposal` call `store_unattested_proposal` with
`UndecidedWriteSource::LocalProposal`. Accept only `Canonical`; turn `Conflict` into an `eyre` error carrying the key
and field.

Change `stream_id` and `stream_proposal` as follows:

```rust
fn stream_id(&mut self, height: Height, round: Round) -> StreamId;

pub async fn stream_proposal(
    &mut self,
    value: LocallyProposedValue<EmeraldContext>,
    data: Bytes,
    pol_round: Round,
) -> eyre::Result<Vec<StreamMessage<ProposalPart>>> {
    let parts = self.make_proposal_parts(value.clone(), data.clone(), pol_round);
    let init = parts.first().and_then(ProposalPart::as_init).unwrap().clone();
    let fin = parts.last().and_then(ProposalPart::as_fin).unwrap().clone();
    let proposal = ProposedValue {
        height: value.height,
        round: value.round,
        valid_round: pol_round,
        proposer: self.address,
        value: value.value,
        validity: Validity::Valid,
    };

    match self.store.write_undecided_proposal(UndecidedProposalWrite {
        proposal,
        payload: data,
        attestation: Some(ProposalAttestation::new(init, fin)),
        source: UndecidedWriteSource::LocalProposal,
    }).await? {
        UndecidedWriteOutcome::Canonical(_) => {
            Ok(self.make_stream_messages(value.height, value.round, parts))
        }
        UndecidedWriteOutcome::Conflict(conflict) => Err(eyre::eyre!(
            "local proposal attestation conflict at height {}, round {}, field {:?}",
            value.height,
            value.round,
            conflict.field,
        )),
    }
}
```

Extract `make_stream_messages(height, round, parts)` from the current sequence-building loop. It appends the stream
terminator and calls `stream_id(height, round)`, eliminating the consensus-round unwrap.

- [ ] **Step 6: Prepare before replying in `on_get_value`**

Call `state.stream_proposal(..., Round::Nil).await?` before `reply.send(proposal.clone())`. Malachite reaches
`GetValue` only when it has no valid value and its `propose()` transition always uses a nil POL round. When a recovered
local row has a defined `valid_round`, atomically replace its metadata and attestation with the freshly signed nil-POL
envelope. Only local attested writes may use this replacement. Then send the consensus reply and publish the messages.

- [ ] **Step 7: Apply the sync merge outcome**

Replace the split sync store call with one unattested `UndecidedWriteSource::Sync` write. Send
`Some(canonical_value)` for `Canonical`, warn and send `None` for `Conflict`, and propagate integrity/storage errors
before touching the reply sender.

- [ ] **Step 8: Run caller-outcome and regression tests**

Run:

```bash
cargo test -p emerald received_stream_ -- --nocapture
cargo test -p emerald local_stream_ -- --nocapture
cargo test -p emerald get_value_attestation_conflict -- --nocapture
cargo test -p emerald synced_value_ -- --nocapture
cargo test -p emerald restream_proposal_stores_reproposal_at_current_round -- --nocapture
cargo test -p emerald restream_proposal_preserves_nil_valid_round -- --nocapture
```

Expected: all tests pass. The last two retain the current proposer-driven behavior while adapting to the fallible
stream API.

- [ ] **Step 9: Commit the write-path integration**

```bash
git add app/src/state.rs app/src/app.rs app/src/lib.rs app/src/proposal_forwarding_tests.rs
git commit -m "feat: persist proposal attestations before publication"
```

---

### Task 4: Replay an original proposer's stored stream

**Files:**

- Modify: `app/src/state.rs:347-394,657-826`
- Modify: `app/src/app.rs:846-905`
- Test: `app/src/proposal_forwarding_tests.rs`

**Interfaces:**

- Consumes: `Store::get_undecided_record` and `State::make_stream_messages`.
- Produces:

```rust
#[derive(Debug)]
pub enum AttestedReplay {
    Ready(Vec<StreamMessage<ProposalPart>>),
    Absent,
    IdentityMismatch { stored_init: ProposalInit },
}

impl State {
    pub async fn prepare_attested_replay(
        &mut self,
        height: Height,
        round: Round,
        valid_round: Round,
        proposer: Address,
        value_id: ValueId,
    ) -> eyre::Result<AttestedReplay>;
}
```

- [ ] **Step 1: Add replay behavior and security tests**

Add these exact tests:

```text
foreign_restream_replays_original_parts_byte_for_byte
forwarded_parts_validate_and_store_on_independent_receiver
forwarded_payload_mutation_fails_signature_validation
forwarded_proposer_substitution_is_wrong_proposer
restream_identity_mismatch_publishes_nothing
foreign_restream_without_attestation_publishes_nothing
stored_invalid_signature_is_integrity_error_and_publishes_nothing
local_unattested_restream_uses_build_branch
round_zero_and_defined_pol_local_streams_match_original_shape
```

Capture every `PublishProposalPart` message, strip only the fresh `StreamId` and sequence wrapper, and compare the
ordered `ProposalPart` values. The byte-for-byte assertion compares `ProtobufCodec.encode(part)` results for init,
all data chunks, and fin.

- [ ] **Step 2: Run the replay tests and verify foreign replay fails**

Run:

```bash
cargo test -p emerald foreign_restream_ -- --nocapture
cargo test -p emerald forwarded_ -- --nocapture
cargo test -p emerald restream_identity_mismatch -- --nocapture
cargo test -p emerald stored_invalid_signature -- --nocapture
```

Expected: the foreign cases fail because `on_restream_proposal` discards the effect address and signs as the local
node.

- [ ] **Step 3: Implement complete identity matching and replay-time signature verification**

`prepare_attested_replay` loads `(height, round, value_id)` through `get_undecided_record`. Return `Absent` when
metadata or attestation is absent. With an attestation, compare all five effect identity fields before preparing any
message:

```rust
let identity_matches = attestation.init.height == height
    && attestation.init.round == round
    && attestation.init.pol_round == valid_round
    && attestation.init.proposer == proposer
    && stored.proposal.value.id() == value_id;
```

Return `IdentityMismatch` when false. When true, reconstruct `ProposalParts` from stored init, payload chunks, and
stored fin, then call the existing `verify_proposal_parts_signature`. Map signature failure to
`StoreError::Integrity` with the key and no signature bytes. Only then return `Ready(make_stream_messages(...))`.
The replay path performs no write.

- [ ] **Step 4: Split `on_restream_proposal` into replay, local build, and drop branches**

Bind the effect's `address`. First call `prepare_attested_replay` at the effect's `(height, round, value_id)` key:

```rust
match state
    .prepare_attested_replay(height, round, valid_round, address, value_id)
    .await?
{
    AttestedReplay::Ready(messages) => publish(messages).await?,
    AttestedReplay::IdentityMismatch { stored_init } => {
        warn!(%height, %round, %valid_round, %address, %value_id,
              stored_round = %stored_init.round,
              stored_valid_round = %stored_init.pol_round,
              stored_proposer = %stored_init.proposer,
              "Stored proposal attestation does not match restream effect");
    }
    AttestedReplay::Absent if address == state.address => {
        let proposal_round = if valid_round == Round::Nil { round } else { valid_round };
        if let Some((proposal, bytes)) = state
            .prepare_restream_proposal(height, proposal_round, round, value_id)
            .await?
        {
            let messages = state.stream_proposal(proposal, bytes, valid_round).await?;
            publish(messages).await?;
        }
    }
    AttestedReplay::Absent => {
        warn!(%height, %round, %valid_round, %address, %value_id,
              "No proposer-signed proposal attestation to forward");
    }
}
```

Inline the small publish loop or add a private handler helper taking `Vec<StreamMessage<ProposalPart>>` and
`&Channels<EmeraldContext>`. Log replay success at info with the original proposer. Do not fall through from an
identity mismatch to the build branch, even when the effect address is local.

- [ ] **Step 5: Run all replay and state regressions**

Run:

```bash
cargo test -p emerald foreign_restream_ -- --nocapture
cargo test -p emerald forwarded_ -- --nocapture
cargo test -p emerald restream_ -- --nocapture
cargo test -p emerald round_zero_and_defined_pol_local_streams_match_original_shape -- --nocapture
```

Expected: all replay, mutation, wrong-proposer, drop, signature-integrity, and local-build cases pass.

- [ ] **Step 6: Commit authenticated replay dispatch**

```bash
git add app/src/state.rs app/src/app.rs app/src/proposal_forwarding_tests.rs
git commit -m "feat: forward stored proposer-signed proposals"
```

---

### Task 5: Prove hidden-lock recovery through production handlers

**Files:**

- Modify: `app/Cargo.toml:55-57`
- Test: `app/src/proposal_forwarding_tests.rs`

**Interfaces:**

- Consumes: pinned `malachitebft_core_consensus::{process, Effect, Input, Params, State}`, the production
  `on_restream_proposal` and `on_received_proposal_part` handlers, and the fixtures from Task 3.
- Produces these test-only helpers:

```rust
fn run_consensus_input(
    state: &mut ConsensusState<EmeraldContext>,
    input: ConsensusInput<EmeraldContext>,
    harness: &mut ConsensusHarness,
) -> eyre::Result<()>;

fn drive_to_round(
    state: &mut ConsensusState<EmeraldContext>,
    harness: &mut ConsensusHarness,
    target: Round,
) -> eyre::Result<()>;

fn signed_prevote(
    validator: &TestValidator,
    height: Height,
    round: Round,
    value_id: ValueId,
) -> SignedVote<EmeraldContext>;

fn signed_precommit(
    validator: &TestValidator,
    height: Height,
    round: Round,
    value_id: ValueId,
) -> SignedVote<EmeraldContext>;
```

- [ ] **Step 1: Add test-only Malachite dependencies**

Add:

```toml
[dev-dependencies]
malachitebft-core-consensus = { workspace = true }
malachitebft-metrics        = { workspace = true }
tempfile                    = "3"
```

Keep `tempfile` once; do not change workspace dependency revisions.

- [ ] **Step 2: Build an effect-observing consensus harness**

`ConsensusHarness` owns the local K256 provider, validator public keys, `malachitebft_metrics::Metrics`, and an
ordered `Vec<ObservedEffect>`. Its effect handler must:

- resume timeout, round, publication, restream, WAL, and decision effects with `()`;
- sign `SignVote` and `SignProposal` with the local deterministic key;
- verify `VerifySignature` against the supplied public key;
- resume certificate-verification effects with their actual verification result;
- resume `ExtendVote` with `None`; and
- reject any vote-extension verification effect because extensions are disabled in this fixture.

Record these values before resuming:

```rust
enum ObservedEffect {
    Restream {
        height: Height,
        round: Round,
        valid_round: Round,
        proposer: Address,
        value_id: ValueId,
    },
    SignPrecommit { height: Height, round: Round, value_id: ValueId },
    PublishPrecommit { height: Height, round: Round, value_id: ValueId },
    Decide { height: Height, round: Round, value_id: ValueId },
}
```

Use `process!` directly in `run_consensus_input`; do not assert on `DriverOutput`, which the macro consumes.

- [ ] **Step 3: Add the hidden-lock trigger test**

Add `hidden_lock_emits_foreign_restream_before_own_precommit`. Create four equal-power validators, make the local
node an active validator other than the round-10 proposer, and configure:

```rust
Params {
    initial_height: Height::new(1),
    initial_validator_set: validator_set.clone(),
    address: local.validator.address,
    threshold_params: ThresholdParams::default(),
    value_payload: ValuePayload::PartsOnly,
    enabled: true,
}
```

Advance through rounds by delivering propose, prevote, and precommit timeout inputs without sleeping. At
`HIDDEN_LOCK_ROUND`, deliver the foreign proposer's valid `ProposedValue`, then real signed prevotes for a quorum.
Assert the observation order contains:

```text
Restream(height, round 10, POL round, foreign proposer, value ID)
SignPrecommit(height, round 10, value ID)
PublishPrecommit(height, round 10, value ID)
```

Assert the complete restream tuple field by field and assert the foreign proposer differs from the local address.

- [ ] **Step 4: Add the late-proposal decision dependency test**

Add `late_forwarded_proposal_unlocks_decision`. Create identical receiver consensus states C and D. Deliver a real
signed precommit quorum to both before either has the proposal and assert neither observed `Decide`. Deliver the
same valid `ProposedValue` only to C and assert C observes `Decide(height, round, value_id)` while D still does not.

- [ ] **Step 5: Run the consensus contract tests**

Run:

```bash
cargo test -p emerald hidden_lock_emits_foreign_restream_before_own_precommit -- --nocapture
cargo test -p emerald late_forwarded_proposal_unlocks_decision -- --nocapture
```

Expected: both pass without wall-clock timeout waits or network processes.

- [ ] **Step 6: Add the composite issue-317 acceptance test**

Add `hidden_lock_forwarding_decides_only_the_receiving_node` with this exact sequence:

1. Have proposer A build and stream a round-10 execution payload; have application state B ingest it through
   `on_received_proposal_part`, producing and storing the attested canonical value.
2. Drive B's Malachite state with that foreign-proposer value and a prevote quorum; capture its real
   `ObservedEffect::Restream` identity.
3. Pass that identity to B's production `on_restream_proposal` and collect its published stream messages.
4. Feed a precommit quorum to receiver consensus states C and D first; assert neither decides.
5. Deliver B's emitted messages one by one through C's production `on_received_proposal_part`. Seed C's validation
   cache for the fixture payload so the production validation function returns `Validity::Valid` without RPC.
6. Submit the handler's returned canonical `ProposedValue` to C's consensus state and assert C decides the expected
   value. Do not deliver the stream or proposal to D and assert D remains undecided.
7. Compare B's emitted part encodings with A's original part encodings to prove the forward retained the original
   init, payload, and fin.

Every handler invocation uses its actual Tokio reply/network channels. The only substituted boundary is the
execution engine response, represented by the production cache hit.

- [ ] **Step 7: Run the composite test repeatedly**

Run:

```bash
cargo test -p emerald hidden_lock_forwarding_decides_only_the_receiving_node -- --nocapture
for run in 1 2 3 4 5; do
  cargo test -p emerald hidden_lock_forwarding_decides_only_the_receiving_node
done
```

Expected: all six executions pass deterministically; C decides and D does not on every run.

- [ ] **Step 8: Commit the consensus and composite proof**

```bash
git add app/Cargo.toml app/src/proposal_forwarding_tests.rs
git commit -m "test: prove hidden-lock proposal forwarding"
```

---

### Task 6: Run the full verification gate

**Files:**

- Modify only files required to fix failures caused by Tasks 1-5.

**Interfaces:**

- Consumes: all production and test interfaces from Tasks 1-5.
- Produces: a clean, reviewable branch with focused, crate-wide, workspace, MBT, lint, and format evidence.

- [ ] **Step 1: Run focused feature coverage**

Run:

```bash
cargo test -p malachitebft-eth-types proposal_attestation -- --nocapture
cargo test -p emerald undecided_ -- --nocapture
cargo test -p emerald restream_ -- --nocapture
cargo test -p emerald forwarded_ -- --nocapture
cargo test -p emerald hidden_lock_ -- --nocapture
```

Expected: all focused tests pass.

- [ ] **Step 2: Run crate and workspace tests**

Run:

```bash
cargo test -p emerald
cargo test --workspace
```

Expected: both commands pass. If an unrelated pre-existing failure appears, capture its exact command and output,
verify the focused feature tests still pass, and report the baseline separately rather than changing unrelated code.

- [ ] **Step 3: Run existing model-based coverage**

Run:

```bash
make mbt-test
```

Expected: the existing MBT suite passes. This command validates anti-erosion behavior only; it is not evidence for
the hidden-lock trigger, which Task 5 proves directly.

- [ ] **Step 4: Run mandatory lint and formatting gates**

Run:

```bash
cargo clippy --tests -- -D warnings
cargo +nightly fmt --all --check
```

Expected: both commands pass with no warnings and no formatting diff.

- [ ] **Step 5: Inspect the final diff and commit any verification fixes**

Run:

```bash
git diff --check
git status --short
git diff origin/om-emerald...HEAD --stat
git log --oneline origin/om-emerald..HEAD
```

Expected: `git diff --check` is silent, only intended files are changed, and the history contains the approved design
plus the focused implementation commits. If verification required code changes, commit them with:

```bash
git add types/proto/consensus.proto types/src/proposal_part.rs types/src/codec/proto/mod.rs \
  app/Cargo.toml app/src/store.rs app/src/state.rs app/src/app.rs app/src/lib.rs \
  app/src/proposal_forwarding_tests.rs
git commit -m "fix: complete proposal forwarding verification"
```
