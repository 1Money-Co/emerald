# Authenticated Proposal Forwarding Design

## Status

Approved on 2026-09-07. Revised through 2026-09-07 in response to design review.

The first review raised two blocking findings, on effect-to-attestation coherence and on the hidden-lock
verification contract, plus a narrowing of the authentication claims. The second raised two implementation-defining
gaps: partial and legacy records defeating a plain insert-if-absent rule, and a trigger recipe that named a received
precommit rather than the validator's own.

The third raised two storage-contract blockers, orphan recovery without a byte-equality decision and undefined
legitimate unattested writes, and two test-contract corrections, `DriverOutput` not being observable through
`process!` and a decision test that proved the proposal-first path rather than late recovery. The storage transition
rules now define attested and unattested writes over four reachable states, and the consensus-driver contract
asserts on effects with the votes delivered before the proposal.

The fourth found that `ProposedValue` metadata duplicates the payload in `Value.extensions`, so coherence must cover
both persisted copies, and that sync's synthetic nil valid round must not conflict with a coherent proposal whose
actual valid round is known. The invariant and sync merge rules below address both cases.

## Context

[1money-interoperability-protocol issue #317][issue-317] reports that Malachite's hidden-lock liveness backstop is
inert in Emerald. From `HIDDEN_LOCK_ROUND` onward, any active validator that precommits a valid value is asked to
restream that value's proposal so every correct validator learns the most recently locked value. Emerald uses
`value_payload = "parts-only"`, so peers learn proposals only through streamed proposal parts, and proposal
authentication lives entirely in Emerald's `ProposalInit` and `ProposalFin`.

Emerald cannot currently forward another validator's proposal:

1. `make_proposal_parts` always writes the local address into `ProposalInit.proposer` and signs the parts with the
   local key.
2. `validate_proposal_parts` requires `ProposalInit.proposer` to equal the proposer selected for the part stream's
   height and round.
3. `verify_proposal_parts_signature` verifies `ProposalFin.signature` against the public key identified by
   `ProposalInit.proposer`.

A forwarded stream is therefore rejected as `WrongProposer`, and substituting the original proposer's address alone
still fails signature verification because the signature was produced by the restreamer. The backstop only works when
the selected proposer happens to be the validator that restreams.

Three observations from the pinned Malachite revision `bcac2b2dbd369a6c29b9151fcd0889b133d7221a` determine the shape
of the fix:

- `Effect::RestreamProposal` already carries the original proposer's address, read from the stored signed proposal.
  Emerald discards it as `address: _` in `on_restream_proposal`.
- On the hidden-lock path, the effect's `round` is the round the proposal already lives at.
  `core-consensus/src/handle/driver.rs` selects the proposal with
  `proposal_and_validity_for_round_and_value(vote.round(), value_id)` and forwards `signed_proposal.round()` and
  `signed_proposal.pol_round()`. The value therefore already has a proposer-signed part stream for exactly the
  `(height, round, pol_round, proposer)` tuple the effect names.
- The only datum Emerald discards that is needed to reproduce that stream is `ProposalFin.signature`. The persisted
  `ProposedValue` already carries `round`, `valid_round`, `proposer`, and the full `Value`; the undecided block-data
  table contains a second copy of `Value.extensions` for execution and commit handling.

The consequence is that a non-proposer does not need a new identity model to forward a proposal. It needs to
rebroadcast the bytes the original proposer signed.

Note also that Malachite's own reference application signs restreamed parts with the local key
(`crates/test/app/src/state.rs`) and checks parts against the selected proposer, so the backstop is inert upstream as
well. Upstream provides no working template, but it also requires no change.

## Goals

- Let a validator other than the round's selected proposer execute the hidden-lock restream path.
- Have peers accept the forwarded parts without relaxing proposer selection or signature verification.
- Ensure a forwarder cannot produce a different payload that verifies under the selected proposer's signature. See
  [Scope of the Guarantee](#scope-of-the-guarantee) for what this does and does not cover.
- Keep round-0 proposals and proposer-driven valid-value re-proposals byte-identical to today.
- Separate the two restream cases that `on_restream_proposal` currently conflates: forwarding an existing
  proposer-signed proposal, and re-proposing an older value at a new round.
- Add deterministic coverage that a forwarded stream is accepted by an independent receiver.

## Non-Goals

- No Malachite dependency, consensus algorithm, or upstream effect change.
- No proposal-part wire-format change, and therefore no coordinated network upgrade.
- No change to `validate_proposal_parts` or `verify_proposal_parts_signature`.
- No change to the signed digest. The digest binds neither `pol_round` nor `proposer`, which is a pre-existing
  malleability tracked in [issue #322][issue-322]; see [Out-of-Scope Finding](#out-of-scope-finding).
- No timeout tuning or round-entry skew work, tracked from [issue #314][issue-314].
- No payload deduplication across rounds, tracked from [issue #318][issue-318].
- No multi-validator timing or fault-injection test in this change.

## Design

### Proposal Attestation Record

Persist the proposer-signed envelope of every accepted proposal alongside its metadata and payload, in a new
table:

```text
undecided_proposal_attestations: (height, round, value_id) -> ProposalAttestation { init, fin }
```

The table reuses `UndecidedValueKey` and `ProtobufCodec`, matching `UNDECIDED_PROPOSALS_TABLE` rather than the
`serde_json` encoding used for pending proposal parts. `ProposalAttestation` is a new message in
`types/proto/consensus.proto` composed of the existing `ProposalInit` and `ProposalFin` messages. It is written to
storage only and never appears on the wire, so it adds no compatibility surface.

The record stores the whole `init` rather than only the signature. `pol_round` is then replayed exactly as it was
received and stored, rather than reconstructed from `ProposedValue.valid_round` at replay time. It is stored, not
signed: the digest does not cover it until [issue #322][issue-322] lands, which is why the transition rules below
have to police it. Keeping the field in the record also keeps that record correct once a later change does bind more
fields into the digest. The cost is roughly a hundred bytes per undecided proposal, negligible against execution
payloads, and it is pruned on the same schedule as the proposal metadata it accompanies.

Attestations are written for received proposals and for the two locally streamed proposal cases, under the
transition rules below. Synced values have no envelope and therefore remain unattested.

### Coherence Invariant

Proposal metadata and its attestation must never disagree, because the replay branch trusts the attestation to
describe the proposal Malachite retained. Since `pol_round` is unsigned until [issue #322][issue-322] lands, two
streams that differ only in `init.pol_round` both pass validation and both carry the same `(height, round, value_id)`
key, so insert-if-absent alone does not keep the pair coherent.

Four properties of the existing storage make this sharper than a fresh-database analysis suggests:

- Databases upgraded into this release legitimately hold payload and metadata with no attestation, for every
  undecided proposal carried across the upgrade. A later stream for the same key would otherwise attach its own
  attestation to first-writer metadata it does not describe.
- Two write paths have no envelope available when they first write. The build branch in `prepare_restream_proposal`
  stores payload and metadata before `stream_proposal` builds the parts, so its signature arrives later;
  `on_process_synced_value` stores a value that came through a verified certificate and never had parts. Fresh local
  proposals instead build and sign their parts before one aggregate write, avoiding a second payload-sized database
  pass on the GetValue deadline. The rules below retain attested and unattested writes for recovery and sync.
- `ValueId` is a 64-bit `DefaultHasher` digest of the payload (`Value::new`), so the storage key does not pin the
  payload bytes. Byte equality must be checked, not inferred from a key match.
- `ProposedValue` is not metadata-only on disk: its protobuf encoding includes `Value.extensions`, while the
  undecided block-data table stores those payload bytes again. A coherent record therefore requires the embedded
  value, the separate payload, and the storage key to agree. This is especially important for a database written by
  the old two-transaction path: an orphaned payload for one colliding value could have been followed after restart by
  metadata for the other.

### Storage Transition Rules

Every write of an undecided proposal goes through one function with two input shapes and an explicit source:

- An **attested write** carries payload, metadata, and a `ProposalInit` and `ProposalFin` whose signature has
  verified. Fresh local preparation, `process_complete_proposal_parts`, and `stream_proposal` produce this shape.
- An **unattested write** carries payload and metadata with no envelope, because none is available at that moment.
  Two paths produce one: the build branch in `prepare_restream_proposal` and `on_process_synced_value`. Proposal
  construction and received streams share the proposal source. Sync has its own source because it does not know the
  proposal's `pol_round` and synthesizes `Round::Nil`.

Before inspecting stored state, every input must be internally coherent:

- `metadata.height`, `metadata.round`, and `metadata.value.id()` equal the storage key.
- `metadata.value.extensions` is byte-equal to the incoming payload.
- For an attested write, the envelope identity matches the metadata and its signature has either been verified for a
  received stream or produced locally for that exact payload.

An internally incoherent input is rejected without writing.

The record set at `(height, round, value_id)` has three components. New writes update all required components in one
redb transaction; the orphan state remains supported for databases written by the old payload-first,
two-transaction path. A present metadata record is coherent only when its embedded value ID equals the key, its
`value.extensions` is byte-equal to the separate payload, and any stored attestation init matches its height, round,
valid round, and proposer. Four states are valid, and every other state is corruption:

| Payload | Metadata | Attestation | State                                                     |
| ------- | -------- | ----------- | --------------------------------------------------------- |
| absent  | absent   | absent      | Empty                                                     |
| present | absent   | absent      | Orphaned payload, from a crash before the metadata write   |
| present | present  | absent      | Coherent unattested proposal: local, synced, or upgraded   |
| present | present  | present     | Coherent attested proposal                                 |
| —       | —        | —           | Metadata without payload: corruption                       |
| —       | —        | —           | Attestation without metadata: corruption                   |
| present | present  | any         | Key, embedded value, payload, or envelope disagree: corruption |

`matches` means all of the following:

- The stored metadata's `height`, `round`, and `value.id()` equal the storage key.
- The stored metadata's `value.extensions` is byte-equal to the stored payload.
- When a stored attestation is present, its `init.height`, `init.round`, `init.pol_round`, and `init.proposer` equal
  the stored metadata's `height`, `round`, `valid_round`, and `proposer`.
- The incoming metadata's `height`, `round`, `valid_round`, `proposer`, and complete `value` equal the stored
  metadata's.
- The incoming payload is byte-equal to the stored payload.
- For an attested write, the envelope's `init.height`, `init.round`, `init.pol_round`, and `init.proposer` equal the
  stored metadata's `height`, `round`, `valid_round`, and `proposer`.

Each write applies one transaction:

| State               | Unattested write                         | Attested write                            |
| ------------------- | ---------------------------------------- | ----------------------------------------- |
| Empty               | Insert payload and metadata              | Insert payload, metadata, attestation     |
| Orphaned payload    | Reconcile payload, insert metadata       | Reconcile, insert metadata + attestation  |
| Unattested proposal | Matches: no-op. Else conflict            | Matches: backfill envelope. Else conflict |
| Attested proposal   | Matches: no-op. Else conflict            | Matches: no-op. Else conflict             |
| Corruption          | Surface an integrity error               | Surface an integrity error                |

The function returns the canonical stored `ProposedValue` on insert, no-op, backfill, and sync merge, and distinct
Conflict and IntegrityError outcomes otherwise. Ordinary writes receive the same value they supplied. The sync merge
may instead return the pre-existing value whose `valid_round` is more precise than sync's synthetic nil.

There is no proposal-source exception to these transition rules. In particular, a defined `valid_round` is never
rewritten to nil. Malachite emits `GetValue` only when it has no valid value and constructs every reply as a nil-POL
proposal. During restart, however, it queues proposals returned by `StartedRound` before issuing that asynchronous
request. If the one stored local proposal for the round has a defined `valid_round`, Emerald leaves its metadata and
attestation unchanged and immediately declines `GetValue` by dropping the reply sender. In the pinned Malachite
connector, the failed response is logged and absorbed; `call_and_forward` receives no value to forward to consensus.
The queued or WAL-restored proposal therefore remains the sole envelope used for recovery. This avoids blocking the
application and connector actors for the request timeout while preserving one proposer-signed envelope instead of
creating a second envelope for the same height and round.

`get_previously_built_value` classifies the stored candidate set as `Absent`, `Reusable`, or `UnsafeCandidates`. No
proposal means normal construction may proceed. Exactly one proposal is reusable only when its proposer is the local
validator. Multiple rows or a row for another proposer are unsafe rather than absent: `on_get_value` warns and drops
the reply sender without building, replying, or publishing. Storage and coherence errors remain errors and propagate.

Reconciling an orphaned payload means: if the stored bytes are byte-equal to the incoming payload, keep them; if
they disagree, replace them with the incoming payload in the same transaction. Replacement rather than refusal is
deliberate. An orphan has no metadata, so no accepted proposal or restart-restored value can reference it. Refusing
instead would let a single colliding orphan wedge that `(height, round, value_id)` permanently and block the genuine
proposal, converting a 64-bit `ValueId` collision from a storage curiosity into a liveness hole.

Backfill is safe only after the stored coherence checks above pass. The incoming attested write's signature has
already verified against the public key of the proposer selected for that height and round, and the metadata it must
match names that same proposer. Only that validator could have produced an envelope that both verifies and matches.

#### Sync merge rule

Value sync writes `valid_round: Round::Nil` because a commit certificate authenticates the height, round, and value
ID but does not carry the proposal's `pol_round`. Nil is therefore missing envelope information on this path, not an
assertion that may overwrite or conflict with better stored information.

An unattested sync write uses the ordinary table above for Empty and Orphaned payload states. Against a coherent
Unattested or Attested proposal, it applies this narrower merge rule:

- The storage key, proposer, complete `Value`, and separate payload must match after the coherence checks above.
- `valid_round` is not compared with sync's synthetic nil value. The stored metadata, including its actual
  `valid_round`, is authoritative and remains unchanged.
- The operation returns that stored `ProposedValue` to `on_process_synced_value`, which replies to consensus with it.
  Any stored attestation remains attached and unchanged.
- A disagreement in the height, round, selected proposer, embedded value, or payload supplied by the sync path is a
  conflict; corruption is still an integrity error. Malachite separately verifies the commit certificate and checks
  the returned value ID against it.

This rule covers the crash ordering in which an attested re-proposal is committed to redb but the process exits
before replying to consensus. If restart begins at a different round, `on_started_round` does not restore that row;
a later sync certificate can still recover it because `on_process_synced_value` returns the coherent stored proposal
instead of failing on its defined `valid_round`.

The application receives the expected certificate value ID only after `on_process_synced_value` returns: the pinned
Malachite callback compares the returned value ID with the certificate ID. `AppMsg::ProcessSyncedValue` does not carry
that expected ID, so Emerald cannot reject mismatched synced bytes before the storage write without changing the host
contract. The unsafe-candidate classification prevents an extra row from stopping later `GetValue` handling, but
preventing the source write requires a separately reviewed upstream change.

The reverse ordering remains intentionally asymmetric. If sync created nil-valid-round metadata first, a later
received stream with a defined `pol_round` conflicts and is dropped without rewriting metadata already returned to
consensus. Malachite deduplicates same-value proposals by value ID before reconsidering `pol_round`, so delivering the
second value would not repair its retained envelope. Preventing a mutated first arrival requires binding `pol_round`
into the signed digest as tracked by [issue #322][issue-322].

#### Read contract

The write invariant is also enforced when proposal metadata is read. Store operations that expose undecided
proposals, including `get_undecided_proposal` and `get_undecided_proposals`, load the matching block-data record in the
same redb read transaction and validate the storage key, embedded value, separate payload, and any attestation init
before returning. Missing or disagreeing components produce an IntegrityError.

This prevents `on_started_round`, `get_previously_built_value`, and re-proposal preparation from restoring a split
legacy record into consensus. The replay path additionally verifies the stored `ProposalFin.signature` over the
stored height, round, and payload against the selected proposer's public key before publishing. Signature failure is
an IntegrityError, not an effect-identity mismatch or an ordinary drop. The low-level store does not repeat this
cryptographic check for reads that never use the attestation; validator-key context remains in `State`.

A raw block-data lookup remains available for execution and commit handling, but it does not establish that an
undecided proposal exists and is never used by itself to restore proposal metadata.

### Conflict Handling

A completed received stream that conflicts on proposer, value, payload, valid round, or attestation is dropped before
replying to consensus. Malachite's parts-only proposal keeper deduplicates the same value ID without reconsidering its
`pol_round`, so returning a second same-value `ProposedValue` cannot repair the retained envelope. The unsigned
`pol_round` exposure remains explicitly deferred to [issue #322][issue-322].

The first-writer rule is retained rather than replaced by multi-envelope storage. Keeping every distinct envelope
would let an attacker mint unbounded rows per key by varying `pol_round`, which trades a coherence bug for a storage
exhaustion vector.

All four write paths use the same function and therefore share these rules:

- Received proposals, from `process_complete_proposal_parts`: one attested write. Every conflict returns no value.
- Fresh local proposals, from `prepare_local_proposal`: build and sign the envelope, then persist payload, metadata,
  and attestation in one aggregate write before returning the prepared stream.
- Locally rebuilt re-proposals, from the build branch in `prepare_restream_proposal` then `stream_proposal`: the same
  two-phase shape.
- Synced values, from `on_process_synced_value`: one unattested sync write. It either inserts a new nil-valid-round
  proposal or returns a matching coherent proposal already in storage, since sync delivers no proposal parts of its
  own.

Caller behavior is explicit so that a storage outcome cannot leave consensus and the network with different views:

| Caller | Success | Conflict | Integrity or storage error |
| ------ | ------- | -------- | -------------------------- |
| Received stream | Reply `Some(canonical)` | Warn and reply `None` | Return error; no reply |
| New local proposal | Prepare attested stream, then reply and publish | Return error; no reply or publish | Same |
| Restart GetValue with defined POL | Decline immediately; no reply, write, or publish | n/a | Same |
| GetValue with unsafe candidates | Warn and decline; no reply, write, or publish | n/a | Same |
| Local re-proposal | Prepare attested stream, then publish | Return error; do not publish | Same |
| Sync | Reply `Some(canonical value)` | Warn and reply `None` | Return error; no reply |
| Foreign replay | Publish after all checks | Warn; publish nothing | Log and publish nothing |

`prepare_local_proposal` builds and signs a fresh proposal, performs one attested aggregate write, and returns the
proposal and stream messages only after persistence succeeds. `on_get_value` can therefore reply and publish without
a second database pass. `stream_proposal` remains the fallible attestation/backfill step for stored nil-POL proposals
and locally rebuilt re-proposals. This closes the window in which consensus could retain a local proposal whose
attestation failed to persist while keeping upgrade and sync backfill behavior intact.

Both local construction and authenticated replay make `ProposalData` chunks with `Bytes::slice`, so the parts share
one owning payload allocation instead of copying every chunk. A stored defined-POL proposal is left for recovery, and
the handler returns immediately without replying. The pinned connector absorbs the closed reply channel and forwards
no local value to consensus.

### Restream Dispatch

The replay branch matches on the complete effect identity rather than on the storage key alone. This is the third
rule that keeps a replay honest, alongside the transition rules and conflict handling above.

`Effect::RestreamProposal` identifies a proposal by five fields: height, round, valid round, proposer address, and
value id. The storage key carries only three of them, so a key hit is not proof that the stored envelope is the
proposal Malachite asked for. The dispatch rule therefore matches the loaded attestation against the complete effect
identity before replaying it:

```text
matches = attestation.init.height   == effect.height
       && attestation.init.round    == effect.round
       && attestation.init.pol_round == effect.valid_round
       && attestation.init.proposer == effect.address
       && stored value id           == effect.value_id
```

| Attestation for the key | Matches effect identity | Effect address | Branch | Behavior                          |
| ----------------------- | ----------------------- | -------------- | ------ | --------------------------------- |
| present                 | yes                     | any            | replay | Emit the stored `init` and `fin`  |
| present                 | no                      | any            | drop   | Warn and publish nothing          |
| absent                  | n/a                     | local          | build  | Rebuild from `valid_round`, sign  |
| absent                  | n/a                     | foreign        | drop   | Warn and publish nothing          |

Dropping on mismatch is the safe failure. The node declines to forward rather than publishing an envelope that
describes a different proposal than the one consensus locked, and it never lends its own delivery to a mutated
envelope. The cost is a lost forwarding opportunity in exactly the case where a `pol_round`-mutated stream won the
race into storage, which is a liveness consequence of the malleability tracked in [issue #322][issue-322], not of
this design. A later same-value stream cannot repair Malachite's retained envelope because its parts-only keeper
deduplicates by value ID before reconsidering `pol_round`.

The replay branch loads the metadata, attestation, and payload from `(height, round, value_id)` and checks the same
key, embedded-value, separate-payload, and envelope coherence invariant before publishing. It then emits
`ProposalPart::Init(stored init)`, re-chunks the payload at `CHUNK_SIZE` into zero-copy `ProposalPart::Data` slices,
and emits `ProposalPart::Fin(stored fin)`. Re-chunking is safe because the digest hashes the concatenated chunk bytes
rather than the chunk boundaries, so a payload rechunked at a different size produces the same hash.

The build branch is unchanged in behavior and keeps the `valid_round` lookup. That lookup exists to re-propose an
older value at a new round, which legitimately requires a fresh signature over the new round, and it is reached only
when the local node is the proposer named by the effect. The `proposer: self.address` write in
`prepare_restream_proposal` therefore becomes correct by construction rather than merely guarded, and the
insert-if-absent comment in `insert_undecided_proposal` can be simplified to describe an ordinary idempotent write.

A node that reaches the replay branch for a proposal it authored itself emits bytes identical to what the build
branch would produce, because its own attestation was stored when it first streamed the proposal. This is one path,
not a special case.

`State::stream_id` currently derives the stream identifier from `self.consensus_height` and `self.consensus_round`
and unwraps the round. Since a forward streams a proposal for a specific round, `stream_id` takes the streamed
height and round as arguments instead, which also removes a nil-round unwrap.

### Receive Path

Validation is unchanged. A forwarded stream carries the original `init`, so `ProposalInit.proposer` is the proposer
selected for `ProposalInit.round`, and `ProposalFin.signature` verifies against that validator's public key.
`validate_proposal_parts` accepts it under its existing rules. The only receive-path change is the conflict rule in
the coherence invariant: proposer, value, payload, valid-round, and attestation conflicts are dropped without changing
the stored aggregate.

### Scope of the Guarantee

The stored envelope is signature-preserving, not fully authenticated. The digest covers the height, the round, and
the payload; it does not cover `pol_round` or `proposer`. The guarantee this design provides is therefore bounded,
and it is worth stating exactly:

- A forwarder cannot present a different payload for the selected proposer's height and round. Any payload change
  breaks the digest and fails verification at every receiver.
- A forwarder cannot attribute a proposal to a validator other than the proposer selected for that height and round,
  because `validate_proposal_parts` pins `init.proposer` to the selected proposer, which also means a forwarder
  gains nothing by altering that field.
- A forwarder can still alter `init.pol_round`, exactly as any peer on the proposal-parts topic can today. Replay
  does not widen that exposure, since the mutation requires no stored envelope and no forwarding path. Closing it is
  [issue #322][issue-322].

The `pol_round` gap is what makes the coherence invariant and the identity match load-bearing rather than defensive.
Once #322 binds `pol_round` and `proposer` into the digest, the envelope becomes fully authenticated and the
identity match degenerates into a consistency assertion.

The forward arrives as a distinct `(peer_id, stream_id)` stream in `PartStreamsMap`, assembles to the same value, and
is absorbed as a duplicate by the aggregate writer and by Malachite's proposal keeper.

### Data Flow

For a hidden-lock restream on a validator that is not the selected proposer:

1. Malachite precommits a valid value at a round at or after `HIDDEN_LOCK_ROUND` and emits
   `Effect::RestreamProposal(height, round, pol_round, original_proposer, value_id)`.
2. `on_restream_proposal` reads the effect's address instead of discarding it and looks up the attestation at
   `(height, round, value_id)`.
3. The attestation is present, because the node validated and stored that proposal when it arrived, and it matches
   the effect identity on all five fields. The handler takes the replay branch.
4. The handler publishes the stored `init`, the re-chunked payload, and the stored `fin` under a fresh stream
   identifier.
5. Peers validate the stream against the original proposer and store the value. No peer distinguishes the forward
   from the original stream, so peers running the current release accept forwards from upgraded nodes.

The replay branch writes nothing to storage, so forwarding adds no payload copy and does not contribute to the
per-round storage growth described in the [issue #314][issue-314] design.

## Error Handling and Observability

The drop branch is the correct outcome, not a degradation: the node holds no proposer-signed artifact for that
value and round, and the alternative is publishing a stream every peer will reject. It emits a warning carrying
height, round, value id, and the effect's proposer address, and publishes nothing. Four cases reach it:

- Proposals carried across the upgrade, which have no attestation until a matching stream backfills one. A node that
  restarts on the new release mid-height cannot forward those values, which affects at most that height.
- Values obtained through value sync, which never had a part stream. Fabricating a stream for these is exactly the
  impersonation this design removes.
- A genuine absence of the named proposal, which is the existing missing-proposal case.
- An attestation that fails the identity match, where the stored envelope describes a different proposal than the
  one consensus requested. The warning names both identities so the divergence is diagnosable in the field.

A conflict outcome in the storage transition rules is distinct from a drop and is logged distinctly, at warning, with
the stored and incoming identities and which field disagreed. On the current release a conflict means either a
proposer equivocation or a `pol_round` mutation in flight, so the log line is the field signal that
[issue #322][issue-322] is being exercised in anger. Once #322 lands, a conflict can only mean equivocation.

An attestation present without its payload is a data-integrity failure. Proposal receive and sync callers propagate
that error before replying. Restream handling logs verification, integrity, and storage failures and publishes
nothing, because failure of a liveness backstop must not terminate the long-running application task.

The same integrity-error path handles metadata whose embedded value disagrees with either its storage key or its
separate block-data payload. Neither backfill nor replay proceeds until all persisted copies agree.

The replay branch logs at info that it is forwarding an existing proposal, including the original proposer, so that
operators can distinguish forwarding from proposer-driven restreaming in the field. The build branch keeps its
existing logs unchanged.

## Testing

The acceptance criterion for this work is that Malachite reaches `HIDDEN_LOCK_ROUND`, emits a restream effect naming
a proposer other than the local node, and that the height decides because the forwarded stream was delivered.
Coverage is layered so that each link in that chain is asserted directly.

### Consensus-driver contract

`HIDDEN_LOCK_ROUND` is a non-configurable constant, so the round-10 trigger has to be reached rather than
configured. It does not have to be reached by waiting. `malachitebft-core-consensus` exports `process!` and its
`State`, `Input`, `Effect`, and `Params` types, and `tests/mbt` already depends on the crate, so a test can drive a
real consensus instance over `EmeraldContext` and supply every input itself. No timeouts elapse in wall-clock time
and no network is involved, which makes the scenario deterministic by construction rather than by fault injection.

Upstream's `polka_certificate_for_hidden_lock` in `crates/test/tests/it/liveness.rs` reaches the same trigger with a
`PrevoteNil` middleware across three nodes. That pattern is not portable here: `Middleware` is concrete over
`TestContext` rather than generic over `Ctx`, and `TestNode`'s configuration type is
`malachitebft_test_app::config::Config`. Emerald has no multi-node harness to host it. Building one is out of scope
here and is not currently tracked by an issue.

The trigger is the forwarding validator's own precommit, not a precommit it receives. Consensus emits
`Effect::RestreamProposal` while processing `DriverOutput::Vote(Precommit(value))`, which is the vote this node
casts: the same match arm extends it, signs it with the local key, and publishes it. Received votes enter as
`DriverInput::Vote` and never produce that output. The scenario therefore has to make the node lock on its own,
which means driving it through a prevote quorum rather than handing it a precommit.

The driver test asserts two things.

1. Trigger and identity:
   1. Advance the driver to a round at or after `HIDDEN_LOCK_ROUND`.
   2. Install the proposal authored by a different validator at that round, with `Validity::Valid`, so that
      `proposal_and_validity_for_round_and_value(round, value_id)` resolves it.
   3. Feed a prevote quorum for that value at that round, as genuine signed prevotes from the validator set. Real
      signatures are required, not synthesized driver state: the same arm looks up
      `polka_certificate_at_round(vote.round())` and panics when the certificate is missing.
   4. Assert on the yielded effects. `process!` exposes `Effect` values; `DriverOutput` is consumed inside
      `process_driver_output` and is never observable from a test driving consensus, so the locally generated
      precommit has to be observed through its effects instead. The hidden-lock block runs before the voting block
      in the same match arm, so a single `process!` run yields `Effect::RestreamProposal` followed by
      `Effect::SignVote` and `Effect::PublishConsensusMsg(SignedConsensusMsg::Vote(..))` for the precommit. Assert
      the restream effect tuple field by field, height, round, valid round, proposer address, and value id, with the
      proposer address belonging to a validator other than the local node, and assert it precedes the precommit's
      own effects. Asserting `DriverOutput::Vote` directly requires driving `core-driver` rather than
      `core-consensus`, which is a separate lower-level test if that coupling is ever wanted.

   The node must also be an active validator for the height, since both the hidden-lock branch and the voting branch
   are gated on `state.is_active_validator()`.

2. Decision dependency, ordered so that it exercises late recovery rather than the ordinary proposal-first path. The
   hidden-lock backstop exists precisely for the case where the votes have already arrived and the proposal has not,
   so the test delivers them in that order:

   1. Feed the same precommit quorum to two receiving consensus instances, neither of which holds the proposal.
   2. Assert that neither instance produces `Decide`. The quorum alone is not sufficient.
   3. Deliver the `ProposedValue` that the forwarded stream produces to one instance only.
   4. Assert that instance produces `Decide` for that value, and that the other still does not.

   Delivery of the forwarded stream is the only difference between the two instances, and it arrives after the votes,
   which is the sequence the backstop is meant to rescue.

### Composite recovery contract

One deterministic test joins the layers above rather than relying only on separate unit proofs:

1. Drive validator B's real consensus state through a foreign-proposer proposal and prevote quorum at
   `HIDDEN_LOCK_ROUND`, and capture its `Effect::RestreamProposal`.
2. Pass that exact effect identity to B's `on_restream_proposal` handler and capture the emitted network parts.
3. Feed a precommit quorum to otherwise identical receiver consensus states C and D while neither has the proposal;
   assert neither decides.
4. Deliver B's actual emitted parts through C's reassembly, validation, and storage path, then submit the returned
   `ProposedValue` to C's consensus state. Do not deliver the parts to D.
5. Assert C emits `Decide` for the forwarded value and D does not.

The test uses real Emerald proposal-part validation and real Malachite state transitions. It may supply deterministic
signing and execution-engine fixtures, but it does not bypass the production forwarding or receive handlers. This is
the acceptance test for issue #317; the narrower tests below localize failures within it.

### Handler and state tests

1. Node A builds and streams a proposal. Node B ingests it through `process_complete_proposal_parts`. B is driven
   with `RestreamProposal` naming A as proposer. Assert B's emitted parts are byte-identical to A's.
2. Feed B's forwarded parts into an independent state C and assert `validate_proposal_parts` accepts them and C
   stores the value, with no check relaxed.
3. Negative case: B alters the payload before forwarding. Assert C rejects the stream.
4. Negative case: B substitutes its own address into the init. Assert C rejects the stream as `WrongProposer`.
5. Identity mismatch: an attestation exists for `(height, round, value_id)` whose `init.pol_round` differs from the
   effect's valid round. Assert the drop branch is taken and nothing is published, rather than the stored envelope
   being replayed under a different identity than the one consensus requested.
6. Conflict rule: a second complete received stream for an already attested `(height, round, value_id)` with a
   different `init.pol_round` leaves the stored metadata and attestation unchanged and returns no proposal.
7. Drop branch: no attestation and a foreign proposer address. Assert nothing is published.
8. Regression: a round-0 proposal and a proposer-driven re-proposal with a defined `valid_round` produce the parts
   the current release produces, including nil `pol_round` handling.
9. Attestation persistence: an attestation is written for both a received proposal and a locally built proposal, in
   the same transaction as any metadata written with it, and is pruned with that metadata.
10. Stored signature integrity: corrupt a stored `ProposalFin.signature`, request replay, and assert an integrity
    error with no published parts.

Storage transition tests, covering each cell of the state machine:

11. Empty key: an unattested write inserts payload and metadata; an attested write inserts all three.
12. Full match: a byte-identical repeat of a stored proposal is idempotent under both write shapes and writes
    nothing.
13. Backfill: coherent payload and metadata exist with no attestation, as after an upgrade, a locally rebuilt
    re-proposal, or a synced value, and a matching attested write backfills only the attestation.
14. Backfill conflict: any attested write whose `init.pol_round` disagrees with the stored `valid_round` leaves the
    record unchanged and returns no proposal. This holds in both directions and for local and received streams.
15. Payload disagreement on an attested proposal: a write whose payload bytes differ from the stored payload for the
    same key is refused, covering two payloads that share a `value_id`.
16. Orphan with equal bytes: a payload written without metadata is completed by the next write of either shape.
17. Orphan with disagreeing bytes: the orphan is replaced in the same transaction and the write completes, so a
    colliding orphan cannot wedge the key. Assert the stored payload afterward is the incoming one.
18. Unattested write against an attested proposal: matching is a no-op, disagreement is a conflict.
19. Corruption: metadata without payload, an attestation without metadata, and an attestation whose init identity
    disagrees with metadata each surface an integrity error rather than being repaired.
20. Split legacy record: metadata embeds payload B while the block-data table contains colliding payload A. Both
    unattested and attested writes surface an integrity error; no attestation is attached and nothing reaches
    consensus or replay.
21. Key mismatch: metadata's embedded value ID differs from the storage key. Surface an integrity error without a
    write or reply.
22. Sync after proposal: an attested re-proposal with a defined `valid_round` exists, then an identical sync value
    arrives with synthetic nil. Preserve storage and return the stored proposal, including its defined valid round.
23. Proposal after sync: sync stores a new nil-valid-round proposal, then an otherwise matching attested stream with
    a defined `pol_round` arrives. Assert conflict, unchanged metadata, and no attestation backfill.
24. Crash-recovery ordering: persist an attested re-proposal, omit its consensus reply, restart at a different round,
    and process a matching sync certificate. Assert sync returns the stored proposal and consensus can decide.
25. Caller outcomes: inject a conflict and a storage error into each write path. Assert the reply and publication
    behavior in the caller table, including that a local attestation failure occurs before the GetValue reply.
26. Restart ordering: persist a local attested proposal with a defined POL, call `StartedRound`, then call `GetValue`
    in Malachite's observed order. Assert the stored proposal is restored unchanged, `GetValue` returns promptly,
    its response channel closes, and it causes no write or publication.
27. Stored candidate safety: assert `get_previously_built_value` marks both multiple candidates for one round and a
    single candidate authored by a non-local proposer as unsafe. Exercise `on_get_value` for both cases and assert it
    returns successfully without replying, building, or publishing.
28. Fresh local persistence: prepare the maximum test payload, assert exactly one aggregate database write, and
    require the durable attested proposal to be ready within Malachite's default proposal timeout.
29. Payload allocation: assert locally built and authenticated replay data parts share one `Bytes` allocation rather
    than owning per-chunk copies.

### Model-based coverage

The Quint model decides directly: `listen_decided` selects a local proposal for the consensus height and round and
enables a decision, with a comment noting that it mimics Malachite rather than modeling it. It has no votes, locks,
polka certificates, or hidden-lock trigger. Model-based coverage therefore cannot establish the trigger-to-decision
contract, and is not used for it here. Its role is narrower: extend the restream action so that a foreign-proposer
restream is exercised alongside the existing cases and the current anti-erosion coverage keeps holding.

That extension depends on [Emerald PR #19][pr-19], which adds `tests/mbt/src/sut/restream_proposal.rs` and the
restream action it builds on. The file does not exist on `om-emerald`. Sequence the model-based item after #19
merges, or drop it from this change; nothing else in this design depends on it.

## Validation

Run, in order:

1. The focused Emerald restream and forwarding tests.
2. The Emerald crate test suite and broader workspace tests where practical.
3. The model-based test suite.
4. `cargo clippy --tests -- -D warnings`.
5. `cargo +nightly fmt --all --check`.

## Deployment

This change requires no coordinated upgrade, protocol version bump, resync, or genesis change. Forwarded parts are
byte-identical to the original proposer's parts, so nodes running the current release accept forwards from upgraded
nodes, and the backstop strengthens incrementally as validators upgrade.

The new redb table is created on first open, and no migration pass is run over existing data. Undecided proposals
carried across the upgrade have no attestation, so they cannot be forwarded until one is backfilled by a matching
stream for the same key. Some legacy locally rebuilt rows can disagree with a subsequently received proposer; that
received proposal is dropped for the key and the row remains unforwardable for the height. Operators need no migration,
and the condition disappears as consensus advances.

The first access to a legacy record also validates the payload embedded in `ProposedValue` against the separate
block-data record. A mismatch is surfaced as an integrity error rather than guessed or migrated; creating one
requires both an interrupted old-format write and a second payload with the same 64-bit value ID.

The backfill row in the transition rules makes upgrade and locally rebuilt re-proposal transitions quiet rather than
requiring an operator step. Fresh local proposals no longer depend on backfill because their attestation is included
in the initial aggregate write.

## Out-of-Scope Finding

The signed digest is `keccak(height || round || payload)`. It binds neither `ProposalInit.pol_round` nor
`ProposalInit.proposer`. Any network participant can therefore capture a proposer's part stream, alter
`init.pol_round`, and rebroadcast it with the signature still verifying, causing receivers to build a `ProposedValue`
with an attacker-chosen `valid_round`. This is reachable on the current release and is independent of this design,
which replays the digest-covered bytes unaltered.

Binding `pol_round` and `proposer` into the digest changes the digest for every proposal and requires a coordinated
upgrade, so it is deliberately excluded here to keep the liveness repair independently shippable. It is tracked in
[issue #322][issue-322] and should be sequenced after this change rather than bundled with it.

#322 must also decide what happens to attestations stored under the old digest. Their signatures cannot verify under
the new one, so replaying them after the upgrade would produce streams that every upgraded peer rejects. The
recommended handling is to treat a pre-upgrade attestation as absent: the identity match already gates replay, so
adding a digest-version field to `ProposalAttestation` and dropping records that do not carry the current version
reuses the existing drop branch and self-heals within a height. Recording that decision in #322 is a prerequisite
for its rollout, not for this change.

[issue-314]: https://github.com/1Money-Co/1money-interoperability-protocol/issues/314
[issue-317]: https://github.com/1Money-Co/1money-interoperability-protocol/issues/317
[issue-318]: https://github.com/1Money-Co/1money-interoperability-protocol/issues/318
[issue-322]: https://github.com/1Money-Co/1money-interoperability-protocol/issues/322
[pr-19]: https://github.com/1Money-Co/emerald/pull/19
