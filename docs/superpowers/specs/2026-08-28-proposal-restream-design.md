# Re-Proposed Value Restream Design

## Status

Approved for implementation on 2026-08-28.

## Context

[1money-interoperability-protocol issue #314][issue-314] identifies a permanent liveness failure when Emerald
re-proposes a value from an earlier polka round. Emerald uses `value_payload = "parts-only"`, so peers learn proposals
through streamed proposal parts rather than the consensus `Proposal` message.

For a request such as `height = 1426`, `round = 1`, and `valid_round = 0`, the current
`on_restream_proposal` path:

1. Looks up proposal metadata and block data at round 1 even though both were stored at round 0.
2. Requires the stored proposal's original proposer to equal the current re-proposer.
3. Reuses the stored round in `LocallyProposedValue`, causing `ProposalInit.round` to remain 0.
4. Logs a missing proposal only at debug level, making a fatal inability to restream easy to miss.
5. Does not store a re-proposed value under the current round, so a successful decision makes `on_decided` fail its
   certificate-round block-data lookup on the re-proposer.
6. Uses the storage lookup round as `ProposalInit.pol_round`; when `valid_round` is nil, this emits the current round
   instead of nil and prevents peers from voting for the proposal.

The result is that no peer can assemble the round-1 proposal. Validators repeatedly time out and advance rounds
without deciding the height.

## Goals

- Retrieve the previously accepted value from its stored proposal round.
- Restream that value as a proposal for the current round.
- Preserve the earlier round separately as `ProposalInit.pol_round`.
- Allow a validator to re-propose a value originally proposed by a different validator.
- Make missing proposal metadata operationally visible.
- Store the reconstructed proposal and block bytes under the current round for decision and commit processing.
- Preserve `valid_round` exactly in `ProposalInit.pol_round`, including the nil hidden-lock case.
- Add deterministic handler-level coverage for the complete stored-value-to-streamed-parts transformation.

## Non-Goals

- No Malachite dependency or consensus algorithm changes.
- No wire-format, database schema, genesis, or external API changes.
- No multi-validator timing or fault-injection test in this change.
- No timeout, `on_decided`, deployment, or unrelated proposal-path changes.

## Design

### Restream Retrieval Boundary

Replace `State::get_previous_proposal_by_value_and_proposer` with a focused helper that accepts:

- `height`: the consensus height;
- `proposal_round`: the round under which proposal metadata and block data are stored;
- `current_round`: the round for which the value is being re-proposed; and
- `value_id`: the value selected by the consensus engine.

The helper prepares a restream and returns either no stored proposal or a tuple containing:

- a `LocallyProposedValue` whose height and value come from storage and whose round is `current_round`; and
- the block bytes loaded from the same `(height, proposal_round, value_id)` storage key.

When `proposal_round` differs from `current_round`, the helper also stores a reconstructed `ProposedValue` and the
block bytes under `(height, current_round, value_id)`. The reconstructed proposal uses the local node as proposer and
`proposal_round` as `valid_round`. This mirrors the state peers build from the streamed proposal and makes the data
available to certificate-round lookups in `on_decided` and `commit`.

When both rounds are equal, the proposal is already stored under the current round. The helper does not rewrite it,
which preserves a nil `valid_round` in the hidden-lock path.

The helper does not accept or compare a proposer address. A valid value can be re-proposed by a validator other than
its original proposer.

### Handler Data Flow

`on_restream_proposal` continues to derive `proposal_round` as follows:

- use `round` when `valid_round` is nil;
- otherwise use `valid_round`.

The handler ignores the `address` field from `AppMsg::RestreamProposal` and passes `height`, `proposal_round`, `round`,
and `value_id` to the new state helper.

On success, the handler passes the reconstructed value and stored bytes to
`State::stream_proposal(value, bytes, valid_round)`. `make_proposal_parts` therefore emits:

- `ProposalInit.round = current_round`; and
- `ProposalInit.pol_round = valid_round`.

The stream continues to use the local node's address and a fresh nonce-derived stream ID. The receive path is
unchanged.

## Error Handling and Observability

If proposal metadata is absent at `(height, proposal_round, value_id)`, the state helper returns `None`. The handler
emits a warning containing both the current round and proposal round, then returns without publishing proposal parts.
It does not retry using the current round or filter by proposer identity.

If proposal metadata exists but the corresponding block data is absent, the helper returns a contextual error. This
is a data-integrity failure because `store_undecided_value` writes block data before proposal metadata. The error
continues through the consensus-message processing path rather than being treated as a normal missing proposal.

On success, the handler retains the existing `Re-using previously built value` info log and completion debug log.

## Known Storage Trade-off

The existing undecided block-data table is keyed by `(height, round, value_id)`. Persisting a re-proposal under its
current round therefore stores another complete execution payload. Because temporary data is pruned only after a
successful commit, storage growth at a height that continues advancing rounds is approximately:

```text
additional bytes per node = re-proposal rounds x execution payload bytes
```

There is no protocol-level upper bound if the height never decides. This PR accepts that risk to keep the liveness
repair independent of a redb storage redesign; [issue #318][issue-318] tracks deduplicating payloads across rounds.

Before rollout, operators must choose a maximum recovery-round budget based on the largest expected execution payload
and reserve at least that product in free disk space, in addition to the node's normal disk reserve. Alert on both
round count and free disk. If the budget is reached without a decision, stop expanding the rollout and investigate
before the reserved space is consumed. Restarting a node is not cleanup because the undecided data is persistent.

The expected ordinary path is substantially smaller: once a fixed validator is selected proposer, its valid-value
re-proposal should decide and the next commit prunes temporary data. That expectation is not treated as a storage
bound.

## Testing

Add deterministic async tests around the handler and state helper using a temporary real store and Malachite
application channels.

The primary regression test will:

1. Store proposal metadata and block data at round 0.
2. Give the stored proposal an original proposer different from the local node that will restream it.
3. Request the value with `proposal_round = 0` and `current_round = 1`.
4. Invoke `on_restream_proposal` for round 1 with valid round 0.
5. Assert the proposal and block bytes are stored under round 1 with the local proposer and valid round 0.
6. Inspect the emitted init part and assert `round = 1` and `pol_round = 0`.

Additional cases will assert:

- missing proposal metadata returns `None`; and
- present proposal metadata with missing block data returns a contextual error; and
- a nil `valid_round` remains nil in the emitted `ProposalInit`.

The tests guard emitted restream behavior rather than the exact text of tracing messages. This protects the liveness
contract without coupling the suite to log wording.

## Validation

Run, in order:

1. The focused Emerald restream regression tests.
2. The Emerald crate test suite and broader workspace tests where practical.
3. `cargo clippy --tests -- -D warnings`.
4. `cargo +nightly fmt --all --check`.

## Deployment

This is a liveness-only change to proposal publication. It requires no coordinated upgrade, data migration, resync,
protocol version bump, or genesis change. Mixed old and new nodes use the existing proposal-part receive path.

[issue-314]: https://github.com/1Money-Co/1money-interoperability-protocol/issues/314
[issue-318]: https://github.com/1Money-Co/1money-interoperability-protocol/issues/318
