use core::time::Duration;
use std::path::Path;

use alloy_genesis::Genesis as EvmGenesis;
use alloy_rpc_types_engine::{ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3};
use malachitebft_app_channel::app::streaming::StreamContent;
use malachitebft_app_channel::app::types::codec::Codec;
use malachitebft_app_channel::app::types::core::{
    Context, NilOrVal, Round, SignedMessage, Timeout, Validity, ValueOrigin, ValuePayload, VoteType,
};
use malachitebft_app_channel::app::types::{LocallyProposedValue, PeerId, ProposedValue};
use malachitebft_app_channel::{AppMsg, Channels, NetworkMsg};
use malachitebft_core_consensus::{
    process, ConsensusMsg, Effect, Input, Params, Resumable, Resume, SignedConsensusMsg,
    State as ConsensusState, ThresholdParams,
};
use malachitebft_eth_cli::config::EmeraldConfig;
use malachitebft_eth_engine::engine::Engine;
use malachitebft_eth_engine::engine_rpc::EngineRPC;
use malachitebft_eth_engine::ethereum_rpc::EthereumRPC;
use malachitebft_eth_types::codec::proto::ProtobufCodec;
use malachitebft_eth_types::secp256k1::{K256Provider, PrivateKey};
use malachitebft_eth_types::{
    Address, EmeraldContext, Genesis, Height, ProposalPart, Validator, ValidatorSet, Value,
    ValueId, Vote,
};
use malachitebft_metrics::Metrics;
use ssz::Encode;
use tokio::sync::mpsc;
use url::Url;

use crate::app::{on_get_value, on_received_proposal_part, on_restream_proposal};
use crate::metrics::{DbMetrics, Metrics as AppMetrics};
use crate::state::{State, StateMetrics};
use crate::store::Store;

#[derive(Clone, Debug, Eq, PartialEq)]
enum ObservedEffect {
    Restream {
        height: Height,
        round: Round,
        valid_round: Round,
        proposer: Address,
        value_id: ValueId,
    },
    SignPrecommit {
        height: Height,
        round: Round,
        value_id: ValueId,
    },
    PublishPrecommit {
        height: Height,
        round: Round,
        value_id: ValueId,
    },
    Decide {
        height: Height,
        round: Round,
        value_id: ValueId,
    },
}

struct ConsensusHarness {
    provider: K256Provider,
    observed: Vec<ObservedEffect>,
}

impl ConsensusHarness {
    fn handle(
        &mut self,
        effect: Effect<EmeraldContext>,
    ) -> color_eyre::eyre::Result<Resume<EmeraldContext>> {
        let resume = match effect {
            Effect::RestreamProposal(height, round, valid_round, proposer, value_id, resume) => {
                self.observed.push(ObservedEffect::Restream {
                    height,
                    round,
                    valid_round,
                    proposer,
                    value_id,
                });
                resume.resume_with(())
            }
            Effect::SignVote(vote, resume) => {
                if vote.typ == VoteType::Precommit {
                    if let NilOrVal::Val(value_id) = vote.value {
                        self.observed.push(ObservedEffect::SignPrecommit {
                            height: vote.height,
                            round: vote.round,
                            value_id,
                        });
                    }
                }
                let signature = self.provider.sign(&vote.to_sign_bytes());
                resume.resume_with(SignedMessage::new(vote, signature))
            }
            Effect::PublishConsensusMsg(message, resume) => {
                if let SignedConsensusMsg::Vote(vote) = message {
                    if vote.typ == VoteType::Precommit {
                        if let NilOrVal::Val(value_id) = vote.value {
                            self.observed.push(ObservedEffect::PublishPrecommit {
                                height: vote.height,
                                round: vote.round,
                                value_id,
                            });
                        }
                    }
                }
                resume.resume_with(())
            }
            Effect::SignProposal(proposal, resume) => {
                let signature = self.provider.sign(&proposal.to_sign_bytes());
                resume.resume_with(SignedMessage::new(proposal, signature))
            }
            Effect::VerifySignature(message, public_key, resume) => {
                let sign_bytes = match &message.message {
                    ConsensusMsg::Vote(vote) => vote.to_sign_bytes(),
                    ConsensusMsg::Proposal(proposal) => proposal.to_sign_bytes(),
                };
                resume.resume_with(self.provider.verify(
                    &sign_bytes,
                    &message.signature,
                    &public_key,
                ))
            }
            Effect::VerifyCommitCertificate(_, _, _, resume)
            | Effect::VerifyPolkaCertificate(_, _, _, resume)
            | Effect::VerifyRoundCertificate(_, _, _, resume) => resume.resume_with(Ok(())),
            Effect::ExtendVote(_, _, _, resume) => resume.resume_with(None),
            Effect::Decide(certificate, _, resume) => {
                self.observed.push(ObservedEffect::Decide {
                    height: certificate.height,
                    round: certificate.round,
                    value_id: certificate.value_id,
                });
                resume.resume_with(())
            }
            Effect::VerifyVoteExtension(_, _, _, _, _, _) => {
                return Err(color_eyre::eyre::eyre!(
                    "vote extensions are disabled in the forwarding fixture"
                ));
            }
            Effect::ResetTimeouts(resume)
            | Effect::CancelAllTimeouts(resume)
            | Effect::CancelTimeout(_, resume)
            | Effect::ScheduleTimeout(_, resume)
            | Effect::StartRound(_, _, _, _, resume)
            | Effect::PublishLivenessMsg(_, resume)
            | Effect::RepublishVote(_, resume)
            | Effect::RepublishRoundCertificate(_, resume)
            | Effect::GetValue(_, _, _, resume)
            | Effect::ValidSyncValue(_, _, resume)
            | Effect::InvalidSyncValue(_, _, _, resume)
            | Effect::WalAppend(_, _, resume) => resume.resume_with(()),
        };
        Ok(resume)
    }
}

fn run_consensus_input(
    state: &mut ConsensusState<EmeraldContext>,
    input: Input<EmeraldContext>,
    harness: &mut ConsensusHarness,
    metrics: &Metrics,
) -> color_eyre::eyre::Result<()> {
    process!(
        input: input,
        state: state,
        metrics: metrics,
        with: effect => harness.handle(effect)
    )
}

fn signed_prevote(
    key: &PrivateKey,
    height: Height,
    round: Round,
    value_id: ValueId,
) -> malachitebft_app_channel::app::types::core::SignedVote<EmeraldContext> {
    let address = Address::from_public_key(&key.public_key());
    let vote = Vote::new_prevote(height, round, NilOrVal::Val(value_id), address);
    SignedMessage::new(vote.clone(), key.sign(&vote.to_sign_bytes()))
}

fn signed_precommit(
    key: &PrivateKey,
    height: Height,
    round: Round,
    value_id: ValueId,
) -> malachitebft_app_channel::app::types::core::SignedVote<EmeraldContext> {
    let address = Address::from_public_key(&key.public_key());
    let vote = Vote::new_precommit(height, round, NilOrVal::Val(value_id), address);
    SignedMessage::new(vote.clone(), key.sign(&vote.to_sign_bytes()))
}

fn advance_to_round(
    state: &mut ConsensusState<EmeraldContext>,
    round: Round,
    harness: &mut ConsensusHarness,
    metrics: &Metrics,
) {
    while state.round() < round {
        let current = state.round();
        for timeout in [
            Timeout::propose(current),
            Timeout::prevote(current),
            Timeout::precommit(current),
        ] {
            run_consensus_input(state, Input::TimeoutElapsed(timeout), harness, metrics).unwrap();
        }
    }
}

async fn make_app_state(
    key: PrivateKey,
    height: Height,
    validator_set: ValidatorSet,
) -> (State, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("store.redb"), 1024 * 1024, DbMetrics::new())
        .await
        .unwrap();
    let public_key = key.public_key();
    let address = Address::from_public_key(&public_key);
    let genesis = Genesis {
        validator_set: validator_set.clone(),
    };
    let mut config: EmeraldConfig = toml::from_str(
        r#"
moniker = "proposal-forwarding-test"

[ethereum_config]
execution_authrpc_address = "http://127.0.0.1:8551"
engine_authrpc_address = "http://127.0.0.1:8552"
jwt_token_path = "./assets/jwt.hex"
"#,
    )
    .unwrap();
    let eth_genesis_path = dir.path().join("genesis.json");
    std::fs::write(
        &eth_genesis_path,
        serde_json::to_vec(&EvmGenesis::default()).unwrap(),
    )
    .unwrap();
    config.ethereum_config.eth_genesis_path = eth_genesis_path.display().to_string();

    let mut state = State::new(
        genesis,
        EmeraldContext::new(),
        K256Provider::new(key),
        address,
        height,
        store,
        StateMetrics {
            txs_count: 0,
            chain_bytes: 0,
            elapsed_seconds: 0,
            metrics: AppMetrics::new(),
        },
        config,
    );
    state.set_validator_set(height, validator_set);
    (state, dir)
}

fn make_test_engine(dir: &Path) -> Engine {
    let jwt_path = dir.join("jwt.hex");
    std::fs::write(
        &jwt_path,
        "0000000000000000000000000000000000000000000000000000000000000000",
    )
    .unwrap();
    Engine::new(
        EngineRPC::new(Url::parse("http://127.0.0.1:8552").unwrap(), &jwt_path).unwrap(),
        EthereumRPC::new(Url::parse("http://127.0.0.1:8551").unwrap()).unwrap(),
    )
}

fn fixture_execution_payload() -> ExecutionPayloadV3 {
    ExecutionPayloadV3 {
        payload_inner: ExecutionPayloadV2 {
            payload_inner: ExecutionPayloadV1 {
                parent_hash: Default::default(),
                fee_recipient: Default::default(),
                state_root: Default::default(),
                receipts_root: Default::default(),
                logs_bloom: Default::default(),
                prev_randao: Default::default(),
                block_number: 1,
                gas_limit: 1,
                gas_used: 0,
                timestamp: 1,
                extra_data: Default::default(),
                base_fee_per_gas: Default::default(),
                block_hash: [7; 32].into(),
                transactions: Vec::new(),
            },
            withdrawals: Vec::new(),
        },
        blob_gas_used: 0,
        excess_blob_gas: 0,
    }
}

fn make_network_channels() -> (
    Channels<EmeraldContext>,
    mpsc::Receiver<NetworkMsg<EmeraldContext>>,
) {
    let (_consensus_tx, consensus_rx) = mpsc::channel(1);
    let (network_tx, network_rx) = mpsc::channel(16);
    let (requests_tx, _requests_rx) = mpsc::channel(1);
    (
        Channels {
            consensus: consensus_rx,
            network: network_tx,
            events: Default::default(),
            requests: requests_tx,
        },
        network_rx,
    )
}

fn encode_stream_parts(
    messages: &[malachitebft_app_channel::app::streaming::StreamMessage<ProposalPart>],
) -> Vec<Vec<u8>> {
    messages
        .iter()
        .filter_map(|message| match &message.content {
            StreamContent::Data(part) => Some(ProtobufCodec.encode(part).unwrap().to_vec()),
            StreamContent::Fin => None,
        })
        .collect()
}

async fn receive_stream(
    state: &mut State,
    engine: &Engine,
    messages: Vec<malachitebft_app_channel::app::streaming::StreamMessage<ProposalPart>>,
) -> ProposedValue<EmeraldContext> {
    receive_stream_result(state, engine, messages)
        .await
        .expect("stream must produce one complete proposal")
}

async fn receive_stream_result(
    state: &mut State,
    engine: &Engine,
    messages: Vec<malachitebft_app_channel::app::streaming::StreamMessage<ProposalPart>>,
) -> Option<ProposedValue<EmeraldContext>> {
    let peer_id = PeerId::from_multihash(Default::default()).unwrap();
    let mut completed = None;
    for part in messages {
        let (reply, response) = tokio::sync::oneshot::channel();
        on_received_proposal_part(
            AppMsg::ReceivedProposalPart {
                from: peer_id,
                part,
                reply,
            },
            state,
            engine,
            &state.emerald_config.clone(),
        )
        .await
        .unwrap();
        if let Some(proposal) = response.await.unwrap() {
            completed = Some(proposal);
        }
    }
    completed
}

#[test]
fn hidden_lock_emits_foreign_restream_before_own_precommit() {
    let keys: Vec<_> = (1..=4)
        .map(|byte| PrivateKey::from_slice(&[byte; 32]).unwrap())
        .collect();
    let validator_set = ValidatorSet::new(
        keys.iter()
            .map(|key| Validator::new(key.public_key(), 1))
            .collect::<Vec<_>>(),
    );
    let ctx = EmeraldContext::new();
    let height = Height::new(1);
    let round = Round::new(10);
    let proposer = ctx.select_proposer(&validator_set, height, round).address;
    let local_key = keys
        .iter()
        .find(|key| Address::from_public_key(&key.public_key()) != proposer)
        .unwrap()
        .clone();
    let local_address = Address::from_public_key(&local_key.public_key());
    let mut state = ConsensusState::new(
        ctx,
        Params {
            initial_height: height,
            initial_validator_set: validator_set.clone(),
            address: local_address,
            threshold_params: ThresholdParams::default(),
            value_payload: ValuePayload::PartsOnly,
            enabled: true,
        },
        128,
    );
    let metrics = Metrics::default();
    let mut harness = ConsensusHarness {
        provider: K256Provider::new(local_key),
        observed: Vec::new(),
    };

    run_consensus_input(
        &mut state,
        Input::StartHeight(height, validator_set, false),
        &mut harness,
        &metrics,
    )
    .unwrap();
    while state.round() < round {
        let current = state.round();
        for timeout in [
            Timeout::propose(current),
            Timeout::prevote(current),
            Timeout::precommit(current),
        ] {
            run_consensus_input(
                &mut state,
                Input::TimeoutElapsed(timeout),
                &mut harness,
                &metrics,
            )
            .unwrap();
        }
    }
    harness.observed.clear();

    let value = Value::new(bytes::Bytes::from_static(b"hidden-lock-value"));
    let value_id = value.id();
    run_consensus_input(
        &mut state,
        Input::ProposedValue(
            ProposedValue {
                height,
                round,
                valid_round: Round::Nil,
                proposer,
                value,
                validity: Validity::Valid,
            },
            ValueOrigin::Consensus,
        ),
        &mut harness,
        &metrics,
    )
    .unwrap();
    for key in keys.iter().take(3) {
        run_consensus_input(
            &mut state,
            Input::Vote(signed_prevote(key, height, round, value_id)),
            &mut harness,
            &metrics,
        )
        .unwrap();
    }

    let restream = harness
        .observed
        .iter()
        .position(|effect| matches!(effect, ObservedEffect::Restream { value_id: id, .. } if *id == value_id))
        .expect("hidden-lock path must request a restream");
    let sign = harness
        .observed
        .iter()
        .position(|effect| matches!(effect, ObservedEffect::SignPrecommit { value_id: id, .. } if *id == value_id))
        .expect("hidden-lock path must sign a precommit");
    let publish = harness
        .observed
        .iter()
        .position(|effect| matches!(effect, ObservedEffect::PublishPrecommit { value_id: id, .. } if *id == value_id))
        .expect("hidden-lock path must publish a precommit");

    assert!(restream < sign && sign < publish);
    assert_eq!(
        harness.observed[restream],
        ObservedEffect::Restream {
            height,
            round,
            valid_round: Round::Nil,
            proposer,
            value_id,
        }
    );
    assert_ne!(proposer, local_address);
}

#[test]
fn late_forwarded_proposal_unlocks_decision() {
    let keys: Vec<_> = (1..=4)
        .map(|byte| PrivateKey::from_slice(&[byte; 32]).unwrap())
        .collect();
    let validator_set = ValidatorSet::new(
        keys.iter()
            .map(|key| Validator::new(key.public_key(), 1))
            .collect::<Vec<_>>(),
    );
    let height = Height::new(1);
    let round = Round::new(0);
    let ctx = EmeraldContext::new();
    let proposer = ctx.select_proposer(&validator_set, height, round).address;
    let local_key = keys[0].clone();
    let local_address = Address::from_public_key(&local_key.public_key());
    let params = Params {
        initial_height: height,
        initial_validator_set: validator_set.clone(),
        address: local_address,
        threshold_params: ThresholdParams::default(),
        value_payload: ValuePayload::PartsOnly,
        enabled: true,
    };
    let mut receiver = ConsensusState::new(ctx, params.clone(), 128);
    let mut control = ConsensusState::new(ctx, params, 128);
    let receiver_metrics = Metrics::default();
    let control_metrics = Metrics::default();
    let mut receiver_harness = ConsensusHarness {
        provider: K256Provider::new(local_key.clone()),
        observed: Vec::new(),
    };
    let mut control_harness = ConsensusHarness {
        provider: K256Provider::new(local_key),
        observed: Vec::new(),
    };
    for (state, harness, metrics) in [
        (&mut receiver, &mut receiver_harness, &receiver_metrics),
        (&mut control, &mut control_harness, &control_metrics),
    ] {
        run_consensus_input(
            state,
            Input::StartHeight(height, validator_set.clone(), false),
            harness,
            metrics,
        )
        .unwrap();
    }

    let value = Value::new(bytes::Bytes::from_static(b"late-forwarded-value"));
    let value_id = value.id();
    for key in keys.iter().take(3) {
        for (state, harness, metrics) in [
            (&mut receiver, &mut receiver_harness, &receiver_metrics),
            (&mut control, &mut control_harness, &control_metrics),
        ] {
            run_consensus_input(
                state,
                Input::Vote(signed_precommit(key, height, round, value_id)),
                harness,
                metrics,
            )
            .unwrap();
        }
    }
    assert!(!receiver_harness
        .observed
        .iter()
        .any(|effect| matches!(effect, ObservedEffect::Decide { .. })));
    assert!(!control_harness
        .observed
        .iter()
        .any(|effect| matches!(effect, ObservedEffect::Decide { .. })));

    run_consensus_input(
        &mut receiver,
        Input::ProposedValue(
            ProposedValue {
                height,
                round,
                valid_round: Round::Nil,
                proposer,
                value,
                validity: Validity::Valid,
            },
            ValueOrigin::Consensus,
        ),
        &mut receiver_harness,
        &receiver_metrics,
    )
    .unwrap();

    assert!(receiver_harness.observed.iter().any(|effect| matches!(
        effect,
        ObservedEffect::Decide {
            height: decided_height,
            round: decided_round,
            value_id: decided_value,
        } if *decided_height == height && *decided_round == round && *decided_value == value_id
    )));
    assert!(!control_harness
        .observed
        .iter()
        .any(|effect| matches!(effect, ObservedEffect::Decide { .. })));
}

#[tokio::test]
async fn hidden_lock_forwarding_decides_only_the_receiving_node() {
    let keys: Vec<_> = (1..=4)
        .map(|byte| PrivateKey::from_slice(&[byte; 32]).unwrap())
        .collect();
    let validator_set = ValidatorSet::new(
        keys.iter()
            .map(|key| Validator::new(key.public_key(), 1))
            .collect::<Vec<_>>(),
    );
    let ctx = EmeraldContext::new();
    let height = Height::new(1);
    let round = Round::new(10);
    let proposer = ctx.select_proposer(&validator_set, height, round).address;
    let proposer_key = keys
        .iter()
        .find(|key| Address::from_public_key(&key.public_key()) == proposer)
        .unwrap()
        .clone();
    let forwarding_key = keys
        .iter()
        .find(|key| Address::from_public_key(&key.public_key()) != proposer)
        .unwrap()
        .clone();
    let forwarding_address = Address::from_public_key(&forwarding_key.public_key());

    let (mut proposer_state, _proposer_dir) =
        make_app_state(proposer_key, height, validator_set.clone()).await;
    proposer_state.consensus_round = round;
    let execution_payload = fixture_execution_payload();
    let block_hash = execution_payload.payload_inner.payload_inner.block_hash;
    let payload = bytes::Bytes::from(execution_payload.as_ssz_bytes());
    let value = Value::new(payload.clone());
    let value_id = value.id();
    let original_stream = proposer_state
        .stream_proposal(
            LocallyProposedValue::new(height, round, value),
            payload.clone(),
            Round::Nil,
        )
        .await
        .unwrap();

    let (mut forwarding_state, forwarding_dir) =
        make_app_state(forwarding_key.clone(), height, validator_set.clone()).await;
    forwarding_state.consensus_round = round;
    forwarding_state
        .validated_cache_mut()
        .insert(block_hash, Validity::Valid);
    let forwarding_engine = make_test_engine(forwarding_dir.path());
    let forwarded_value = receive_stream(
        &mut forwarding_state,
        &forwarding_engine,
        original_stream.clone(),
    )
    .await;
    assert_eq!(forwarded_value.proposer, proposer);

    let mut forwarding_consensus = ConsensusState::new(
        ctx,
        Params {
            initial_height: height,
            initial_validator_set: validator_set.clone(),
            address: forwarding_address,
            threshold_params: ThresholdParams::default(),
            value_payload: ValuePayload::PartsOnly,
            enabled: true,
        },
        128,
    );
    let forwarding_metrics = Metrics::default();
    let mut forwarding_harness = ConsensusHarness {
        provider: K256Provider::new(forwarding_key),
        observed: Vec::new(),
    };
    run_consensus_input(
        &mut forwarding_consensus,
        Input::StartHeight(height, validator_set.clone(), false),
        &mut forwarding_harness,
        &forwarding_metrics,
    )
    .unwrap();
    advance_to_round(
        &mut forwarding_consensus,
        round,
        &mut forwarding_harness,
        &forwarding_metrics,
    );
    forwarding_harness.observed.clear();
    run_consensus_input(
        &mut forwarding_consensus,
        Input::ProposedValue(forwarded_value, ValueOrigin::Consensus),
        &mut forwarding_harness,
        &forwarding_metrics,
    )
    .unwrap();
    for key in keys.iter().take(3) {
        run_consensus_input(
            &mut forwarding_consensus,
            Input::Vote(signed_prevote(key, height, round, value_id)),
            &mut forwarding_harness,
            &forwarding_metrics,
        )
        .unwrap();
    }
    let restream = forwarding_harness
        .observed
        .iter()
        .find_map(|effect| match effect {
            ObservedEffect::Restream {
                height,
                round,
                valid_round,
                proposer,
                value_id,
            } => Some((*height, *round, *valid_round, *proposer, *value_id)),
            _ => None,
        })
        .expect("hidden-lock path must request forwarding");

    let (mut channels, mut network_rx) = make_network_channels();
    on_restream_proposal(
        AppMsg::RestreamProposal {
            height: restream.0,
            round: restream.1,
            valid_round: restream.2,
            address: restream.3,
            value_id: restream.4,
        },
        &mut forwarding_state,
        &mut channels,
    )
    .await
    .unwrap();
    drop(channels);
    let mut replayed_stream = Vec::new();
    while let Some(NetworkMsg::PublishProposalPart(message)) = network_rx.recv().await {
        replayed_stream.push(message);
    }
    assert_eq!(
        encode_stream_parts(&replayed_stream),
        encode_stream_parts(&original_stream)
    );

    let receiver_key = keys[3].clone();
    let receiver_address = Address::from_public_key(&receiver_key.public_key());
    let receiver_params = Params {
        initial_height: height,
        initial_validator_set: validator_set.clone(),
        address: receiver_address,
        threshold_params: ThresholdParams::default(),
        value_payload: ValuePayload::PartsOnly,
        enabled: true,
    };
    let mut receiver_consensus = ConsensusState::new(ctx, receiver_params.clone(), 128);
    let mut control_consensus = ConsensusState::new(ctx, receiver_params, 128);
    let receiver_metrics = Metrics::default();
    let control_metrics = Metrics::default();
    let mut receiver_harness = ConsensusHarness {
        provider: K256Provider::new(receiver_key.clone()),
        observed: Vec::new(),
    };
    let mut control_harness = ConsensusHarness {
        provider: K256Provider::new(receiver_key.clone()),
        observed: Vec::new(),
    };
    for (state, harness, metrics) in [
        (
            &mut receiver_consensus,
            &mut receiver_harness,
            &receiver_metrics,
        ),
        (
            &mut control_consensus,
            &mut control_harness,
            &control_metrics,
        ),
    ] {
        run_consensus_input(
            state,
            Input::StartHeight(height, validator_set.clone(), false),
            harness,
            metrics,
        )
        .unwrap();
        advance_to_round(state, round, harness, metrics);
        harness.observed.clear();
    }
    for key in keys.iter().take(3) {
        for (state, harness, metrics) in [
            (
                &mut receiver_consensus,
                &mut receiver_harness,
                &receiver_metrics,
            ),
            (
                &mut control_consensus,
                &mut control_harness,
                &control_metrics,
            ),
        ] {
            run_consensus_input(
                state,
                Input::Vote(signed_precommit(key, height, round, value_id)),
                harness,
                metrics,
            )
            .unwrap();
        }
    }
    assert!(!receiver_harness
        .observed
        .iter()
        .any(|effect| matches!(effect, ObservedEffect::Decide { .. })));
    assert!(!control_harness
        .observed
        .iter()
        .any(|effect| matches!(effect, ObservedEffect::Decide { .. })));

    let (mut receiver_state, receiver_dir) =
        make_app_state(receiver_key, height, validator_set).await;
    receiver_state.consensus_round = round;
    receiver_state
        .validated_cache_mut()
        .insert(block_hash, Validity::Valid);
    let receiver_engine = make_test_engine(receiver_dir.path());
    let received_value =
        receive_stream(&mut receiver_state, &receiver_engine, replayed_stream).await;
    run_consensus_input(
        &mut receiver_consensus,
        Input::ProposedValue(received_value, ValueOrigin::Consensus),
        &mut receiver_harness,
        &receiver_metrics,
    )
    .unwrap();

    assert!(receiver_harness.observed.iter().any(|effect| matches!(
        effect,
        ObservedEffect::Decide {
            height: decided_height,
            round: decided_round,
            value_id: decided_value,
        } if *decided_height == height && *decided_round == round && *decided_value == value_id
    )));
    assert!(!control_harness
        .observed
        .iter()
        .any(|effect| matches!(effect, ObservedEffect::Decide { .. })));
}

#[tokio::test]
async fn get_value_recovery_republishes_with_nil_pol_round() {
    let key = PrivateKey::from_slice(&[11; 32]).unwrap();
    let validator_set = ValidatorSet::new([Validator::new(key.public_key(), 1)]);
    let height = Height::new(1);
    let round = Round::new(10);
    let stale_valid_round = Round::new(7);
    let (mut state, dir) = make_app_state(key, height, validator_set).await;
    let execution_payload = fixture_execution_payload();
    let payload = bytes::Bytes::from(execution_payload.as_ssz_bytes());
    let value = Value::new(payload.clone());
    let value_id = value.id();
    state
        .store_undecided_value(
            &ProposedValue {
                height,
                round,
                valid_round: stale_valid_round,
                proposer: state.address,
                value,
                validity: Validity::Valid,
            },
            payload,
        )
        .await
        .unwrap();

    let engine = make_test_engine(dir.path());
    let config = state.emerald_config.clone();
    let (channels, mut network_rx) = make_network_channels();
    let (reply, response) = tokio::sync::oneshot::channel();
    on_get_value(
        AppMsg::GetValue {
            height,
            round,
            timeout: Duration::from_millis(1),
            reply,
        },
        &mut state,
        &channels,
        &engine,
        &config,
    )
    .await
    .unwrap();
    assert_eq!(response.await.unwrap().value.id(), value_id);
    drop(channels);

    let NetworkMsg::PublishProposalPart(first_message) = network_rx.recv().await.unwrap();
    let init = first_message
        .content
        .as_data()
        .and_then(ProposalPart::as_init)
        .unwrap();
    assert_eq!(init.pol_round, Round::Nil);

    let stored = state
        .store
        .get_undecided_record(height, round, value_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.proposal.valid_round, Round::Nil);
    assert_eq!(stored.attestation.unwrap().init.pol_round, Round::Nil);
}

#[tokio::test]
async fn received_valid_round_conflict_is_not_delivered_to_consensus() {
    let key = PrivateKey::from_slice(&[12; 32]).unwrap();
    let validator_set = ValidatorSet::new([Validator::new(key.public_key(), 1)]);
    let height = Height::new(1);
    let round = Round::new(10);
    let (mut proposer, _proposer_dir) =
        make_app_state(key.clone(), height, validator_set.clone()).await;
    proposer.consensus_round = round;
    let execution_payload = fixture_execution_payload();
    let block_hash = execution_payload.payload_inner.payload_inner.block_hash;
    let payload = bytes::Bytes::from(execution_payload.as_ssz_bytes());
    let value = Value::new(payload.clone());
    let value_id = value.id();
    let canonical_stream = proposer
        .stream_proposal(
            LocallyProposedValue::new(height, round, value),
            payload,
            Round::Nil,
        )
        .await
        .unwrap();
    let mut conflicting_stream = canonical_stream.clone();
    conflicting_stream
        .iter_mut()
        .find_map(|message| match &mut message.content {
            StreamContent::Data(ProposalPart::Init(init)) => Some(init),
            _ => None,
        })
        .unwrap()
        .pol_round = Round::new(7);

    let (mut receiver, receiver_dir) = make_app_state(key, height, validator_set).await;
    receiver.consensus_round = round;
    receiver
        .validated_cache_mut()
        .insert(block_hash, Validity::Valid);
    let receiver_engine = make_test_engine(receiver_dir.path());

    let accepted = receive_stream_result(&mut receiver, &receiver_engine, canonical_stream).await;
    assert!(accepted.is_some());
    let rejected = receive_stream_result(&mut receiver, &receiver_engine, conflicting_stream).await;
    assert!(rejected.is_none());

    let stored = receiver
        .store
        .get_undecided_record(height, round, value_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.proposal.valid_round, Round::Nil);
    assert_eq!(stored.attestation.unwrap().init.pol_round, Round::Nil);
}
