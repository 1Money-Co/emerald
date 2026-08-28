# Proposal Restream Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Emerald restream a value stored in an earlier polka round as a proposal for the current round.

**Architecture:** Move stored-proposal lookup and current-round reconstruction behind one focused `State` helper. The
handler derives the storage round, calls that helper, and streams the returned value with the storage round preserved
as `pol_round`; deterministic state tests cover the full storage-to-`ProposalInit` transformation.

**Tech Stack:** Rust, Tokio tests, redb-backed `Store`, Malachite channel proposal types, Cargo, Clippy, rustfmt

**Spec:** `docs/superpowers/specs/2026-08-28-proposal-restream-design.md`

## Global Constraints

- Start from branch `seb/fix-proposal-restream`, which is derived from `origin/om-emerald` at `e2cbc98`.
- Keep the change local to Emerald; do not modify Malachite or the interoperability repository.
- Do not change wire formats, database schemas, genesis data, protocol versions, or external APIs.
- Do not add multi-validator timing or fault-injection coverage in this change.
- Do not change timeouts, `on_decided`, or unrelated proposal paths.
- Do not add dependencies; use the existing `tempfile`, `toml`, Tokio, redb, and Malachite types.
- Preserve the 120-column Markdown limit and do not format `Cargo.lock`.
- Before pushing, run `cargo clippy --tests -- -D warnings` and `cargo +nightly fmt --all --check`.

## File Structure

- Modify `app/src/state.rs`: replace the proposer-filtered lookup with the restream retrieval boundary and add focused
  state/store/streaming regression tests in a local `#[cfg(test)]` module.
- Modify `app/src/app.rs`: ignore the re-proposer address, call the new state helper, remove duplicate block-data
  retrieval, and warn when stored proposal metadata is absent.
- No production files are created. Keeping retrieval next to `State::stream_proposal` makes the round transformation
  independently understandable and testable without constructing Malachite network actors.

---

### Task 1: Correct and cover the stored-proposal restream path

**Files:**

- Modify: `app/src/state.rs:657-685`
- Modify: `app/src/state.rs` after `decode_value`
- Modify: `app/src/app.rs:604-658`

**Interfaces:**

- Consumes: `Store::get_undecided_proposal(height, round, value_id)` and
  `Store::get_block_data(height, round, value_id)`.
- Produces:

```rust
pub async fn get_restream_proposal(
    &self,
    height: Height,
    proposal_round: Round,
    current_round: Round,
    value_id: ValueId,
) -> eyre::Result<Option<(LocallyProposedValue<EmeraldContext>, Bytes)>>
```

- `on_restream_proposal` consumes the returned tuple and calls
  `State::stream_proposal(value, bytes, proposal_round)`.

- [ ] **Step 1: Add the failing primary regression test and its test-state fixture**

Append a `#[cfg(test)]` module to `app/src/state.rs`. Build `State` with an actual temporary `Store`, the repository's
EVM genesis file, and a deterministic signing key. The stored proposal must use an address different from
`state.address` so the test covers removal of the original-proposer equality check.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{DbMetrics, Metrics};
    use malachitebft_eth_types::secp256k1::PrivateKey;
    use malachitebft_eth_types::Validator;

    async fn make_test_state() -> (State, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            dir.path().join("store.redb"),
            1024 * 1024,
            DbMetrics::new(),
        )
        .await
        .unwrap();

        let private_key = PrivateKey::from_slice(&[1_u8; 32]).unwrap();
        let public_key = private_key.public_key();
        let address = Address::from_public_key(&public_key);
        let genesis = Genesis {
            validator_set: ValidatorSet::new([Validator::new(public_key, 1)]),
        };

        let mut emerald_config: EmeraldConfig = toml::from_str(
            r#"
moniker = "restream-test"

[ethereum_config]
execution_authrpc_address = "http://127.0.0.1:8551"
engine_authrpc_address = "http://127.0.0.1:8552"
jwt_token_path = "./assets/jwt.hex"
"#,
        )
        .unwrap();
        emerald_config.ethereum_config.eth_genesis_path = format!(
            "{}/../assets/genesis.json",
            env!("CARGO_MANIFEST_DIR")
        );

        let state = State::new(
            genesis,
            EmeraldContext::new(),
            K256Provider::new(private_key),
            address,
            Height::new(1426),
            store,
            StateMetrics {
                txs_count: 0,
                chain_bytes: 0,
                elapsed_seconds: 0,
                metrics: Metrics::new(),
            },
            emerald_config,
        );

        (state, dir)
    }

    #[tokio::test]
    async fn restream_proposal_uses_stored_round_and_current_round() {
        let (mut state, _dir) = make_test_state().await;
        let height = Height::new(1426);
        let proposal_round = Round::new(0);
        let current_round = Round::new(1);
        let bytes = Bytes::from_static(b"round-zero-block");
        let value = Value::new(bytes.clone());
        let original_proposer = Address::new([2_u8; 20]);
        assert_ne!(original_proposer, state.address);

        let stored_proposal = ProposedValue {
            height,
            round: proposal_round,
            valid_round: Round::Nil,
            proposer: original_proposer,
            value: value.clone(),
            validity: Validity::Valid,
        };
        state
            .store_undecided_value(&stored_proposal, bytes.clone())
            .await
            .unwrap();

        let (restreamed, restreamed_bytes) = state
            .get_restream_proposal(height, proposal_round, current_round, value.id())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(restreamed.height, height);
        assert_eq!(restreamed.round, current_round);
        assert_eq!(restreamed.value, value);
        assert_eq!(restreamed_bytes, bytes);

        let init = state
            .stream_proposal(restreamed, restreamed_bytes, proposal_round)
            .find_map(|message| {
                message
                    .content
                    .into_data()
                    .and_then(|part| part.as_init().cloned())
            })
            .unwrap();

        assert_eq!(init.height, height);
        assert_eq!(init.round, current_round);
        assert_eq!(init.pol_round, proposal_round);
        assert_eq!(init.proposer, state.address);
    }
}
```

- [ ] **Step 2: Run the primary regression test to verify it fails**

Run:

```bash
cargo test -p emerald restream_proposal_uses_stored_round_and_current_round -- --nocapture
```

Expected: compilation fails because `State::get_restream_proposal` does not exist. Do not weaken the test or call the
old proposer-filtered method.

- [ ] **Step 3: Replace the proposer-filtered state method with the restream retrieval boundary**

In `app/src/state.rs`, delete `get_previous_proposal_by_value_and_proposer` and replace it with this implementation:

```rust
/// Retrieves a stored proposal and prepares it for restreaming in the current round.
pub async fn get_restream_proposal(
    &self,
    height: Height,
    proposal_round: Round,
    current_round: Round,
    value_id: ValueId,
) -> eyre::Result<Option<(LocallyProposedValue<EmeraldContext>, Bytes)>> {
    let Some(proposal) = self
        .store
        .get_undecided_proposal(height, proposal_round, value_id)
        .await?
    else {
        return Ok(None);
    };

    let bytes = self
        .store
        .get_block_data(height, proposal_round, value_id)
        .await?
        .ok_or_else(|| {
            eyre::eyre!(
                "Block data not found for restream proposal at height {height}, \
                 proposal round {proposal_round}, value {value_id}"
            )
        })?;

    Ok(Some((
        LocallyProposedValue::new(proposal.height, current_round, proposal.value),
        bytes,
    )))
}
```

Do not accept an `Address` argument. Proposal ownership is deliberately irrelevant to re-proposing a value selected
by a valid polka.

- [ ] **Step 4: Wire `on_restream_proposal` to the new helper**

In `app/src/app.rs`, bind `address: _`, keep the existing `proposal_round` derivation, and replace the current match
body with:

```rust
match state
    .get_restream_proposal(height, proposal_round, round, value_id)
    .await?
{
    Some((proposal, bytes)) => {
        info!(value = %proposal.value.id(), "Re-using previously built value");
        for stream_message in state.stream_proposal(proposal, bytes, proposal_round) {
            debug!(%height, %round, "Streaming proposal part: {stream_message:?}");
            channels
                .network
                .send(NetworkMsg::PublishProposalPart(stream_message))
                .await?;
        }

        debug!(%height, %round, "✅ Re-sent proposal");
    }
    None => {
        warn!(
            %height,
            %round,
            %proposal_round,
            %value_id,
            "No proposal to re-send"
        );
    }
}
```

Remove the handler's duplicate `get_block_data` lookup. Do not fall back to `round` after a lookup miss at
`proposal_round`.

- [ ] **Step 5: Run the primary regression test to verify it passes**

Run:

```bash
cargo test -p emerald restream_proposal_uses_stored_round_and_current_round -- --nocapture
```

Expected: PASS. The emitted init part reports round 1, polka round 0, and the local re-proposer address.

- [ ] **Step 6: Add deterministic missing-proposal and missing-block-data tests**

Add these tests to the same `app/src/state.rs` test module:

```rust
#[tokio::test]
async fn restream_proposal_returns_none_when_proposal_is_missing() {
    let (state, _dir) = make_test_state().await;

    let result = state
        .get_restream_proposal(
            Height::new(1426),
            Round::new(0),
            Round::new(1),
            ValueId::new(42),
        )
        .await
        .unwrap();

    assert!(result.is_none());
}

#[tokio::test]
async fn restream_proposal_errors_when_block_data_is_missing() {
    let (state, _dir) = make_test_state().await;
    let height = Height::new(1426);
    let proposal_round = Round::new(0);
    let value = Value::new(Bytes::from_static(b"missing-block-data"));
    let stored_proposal = ProposedValue {
        height,
        round: proposal_round,
        valid_round: Round::Nil,
        proposer: Address::new([2_u8; 20]),
        value: value.clone(),
        validity: Validity::Valid,
    };
    state
        .store
        .store_undecided_proposal(stored_proposal)
        .await
        .unwrap();

    let error = match state
        .get_restream_proposal(height, proposal_round, Round::new(1), value.id())
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("missing block data must be an error"),
    };

    assert!(error
        .to_string()
        .contains("Block data not found for restream proposal"));
    assert!(error.to_string().contains("1426"));
}
```

- [ ] **Step 7: Run all focused restream tests and the Emerald crate suite**

Run:

```bash
cargo test -p emerald restream_proposal -- --nocapture
cargo test -p emerald
```

Expected: all three restream tests pass, followed by the complete `emerald` package test suite.

- [ ] **Step 8: Review the focused diff and commit the working fix**

Run:

```bash
git diff --check
git diff -- app/src/state.rs app/src/app.rs
git status --short
git add app/src/state.rs app/src/app.rs
git commit -m "fix(app): restream re-proposed values"
```

Expected: the diff contains only the helper, handler wiring, warning, and three regression tests. The commit succeeds
without modifying `Cargo.lock`.

---

### Task 2: Run the branch-level verification gates

**Files:**

- Verify only: `app/src/state.rs`
- Verify only: `app/src/app.rs`

**Interfaces:**

- Consumes: the committed `State::get_restream_proposal` and `on_restream_proposal` wiring from Task 1.
- Produces: a clean, formatted, warning-free branch ready for review and push.

- [ ] **Step 1: Run the full workspace test suite**

Run:

```bash
cargo test --workspace
```

Expected: PASS. If an unrelated environmental test cannot run, record the exact command, failure, and why it is
unrelated; do not silently replace this gate with a narrower command.

- [ ] **Step 2: Run Clippy with warnings denied**

Run:

```bash
cargo clippy --tests -- -D warnings
```

Expected: PASS with no warnings.

- [ ] **Step 3: Run the required nightly formatting check**

Run:

```bash
cargo +nightly fmt --all --check
```

Expected: PASS without changing `Cargo.lock` or any unrelated file.

- [ ] **Step 4: Confirm repository state and commit history**

Run:

```bash
git diff --check
git status --short --branch
git log --oneline --decorate -3
```

Expected: the working tree is clean; the branch contains the design commit, implementation-plan commit, and focused
fix commit on top of `origin/om-emerald`.
