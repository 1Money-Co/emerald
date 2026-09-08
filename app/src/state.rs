//! Internal state of the application. This is a simplified abstract to keep it simple.
//! A regular application would have mempool implemented, a proper database and input methods like RPC.

use core::str::FromStr;
use std::path::PathBuf;
use std::{fmt, fs};

use alloy_genesis::{ChainConfig, Genesis as EvmGenesis};
use alloy_rpc_types_engine::ExecutionPayloadV3;
use bytes::Bytes;
use color_eyre::eyre;
use malachitebft_app_channel::app::streaming::{StreamContent, StreamId, StreamMessage};
use malachitebft_app_channel::app::types::codec::Codec;
use malachitebft_app_channel::app::types::core::{CommitCertificate, Context, Round, Validity};
use malachitebft_app_channel::app::types::{LocallyProposedValue, PeerId, ProposedValue};
use malachitebft_eth_cli::config::EmeraldConfig;
use malachitebft_eth_engine::engine::Engine;
use malachitebft_eth_engine::engine_rpc::Fork;
use malachitebft_eth_engine::json_structures::ExecutionBlock;
use malachitebft_eth_types::codec::proto::ProtobufCodec;
use malachitebft_eth_types::secp256k1::K256Provider;
use malachitebft_eth_types::{
    Address, BlockTimestamp, EmeraldContext, Genesis, Height, ProposalAttestation, ProposalData,
    ProposalFin, ProposalInit, ProposalPart, RetryConfig, ValidatorSet, Value, ValueId,
};
use malachitebft_proto::Error as ProtoError;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use sha3::Digest;
use ssz::{Decode, Encode};
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

use crate::metrics::Metrics;
use crate::payload::{extract_block_header, validate_execution_payload, ValidatedPayloadCache};
use crate::store::{Store, UndecidedProposalWrite, UndecidedWriteOutcome, UndecidedWriteSource};
use crate::streaming::{PartStreamsMap, ProposalParts};

pub struct StateMetrics {
    pub txs_count: u64,
    pub chain_bytes: u64,
    pub elapsed_seconds: u64,
    pub metrics: Metrics,
}

/// The safe result of looking up material requested by a `RestreamProposal` effect.
pub enum AttestedReplay {
    /// Re-publish the exact authenticated proposal under a fresh transport stream ID.
    Ready(Vec<ProposalPart>),
    /// No authenticated proposal matches the effect, so nothing may be published.
    Absent,
    /// An attestation exists at the requested key but does not describe the requested effect.
    IdentityMismatch { stored_init: ProposalInit },
}

/// Size of randomly generated blocks in bytes
#[allow(dead_code)]
const BLOCK_SIZE: usize = 10 * 1024 * 1024; // 10 MiB

/// Size of chunks in which the data is split for streaming
const CHUNK_SIZE: usize = 128 * 1024; // 128 KiB

/// Represents the internal state of the application node
/// Contains information about current height, round, proposals and blocks
pub struct State {
    #[allow(dead_code)]
    ctx: EmeraldContext,
    pub signing_provider: K256Provider,
    pub address: Address,
    pub store: Store,
    stream_nonce: u32,
    streams_map: PartStreamsMap,
    #[allow(dead_code)]
    rng: StdRng,

    // ------------ Config import
    /// EmeraldConfig is used to extract the following config parameters:
    /// num_certificates_to_retain
    /// prune_at_block_interval
    /// num_temp_blocks_retained
    /// min_block_time
    /// ethereum_config : EthereumConfig (path to eth genesis and EL relevant information)
    pub emerald_config: EmeraldConfig,

    /// Needed to extract chain configuration contained in the ethereum genesis file.
    /// Currently used to read information on the fork supported by the chain.
    pub eth_chain_config: ChainConfig,
    // ------------

    // ------------- Internal temporary state
    /// The height where consensus is working (the tip of the blockchain).
    /// After deciding on height H, this is set to H+1.
    /// This represents the next height where consensus will propose, vote, and commit.
    pub consensus_height: Height,
    /// The round of consensus at consensus_height.
    /// Reset to 0 when advancing to a new height.
    pub consensus_round: Round,

    pub latest_block: Option<ExecutionBlock>,

    validator_set: Option<(Height, ValidatorSet)>,

    // Cache for tracking recently validated payloads to avoid duplicate validation
    validated_payload_cache: ValidatedPayloadCache,

    /// Cached earliest height with a certificate in the store.
    /// Lazily loaded on first access and updated after pruning.
    earliest_certificate_height: Option<Height>,

    /// Cached earliest height with decided value data in the store.
    /// Lazily loaded on first access and updated after pruning.
    earliest_value_height: Option<Height>,

    /// Time it took to execute last block.
    /// Used to decide on whether we should sleep in case min_block_time
    /// is set.
    pub last_block_time: Instant,

    /// Tracks when the previous block was committed (for per-block TPS calculation)
    pub previous_block_commit_time: Instant,

    // --------------

    // -------------- Stat collection - persisted to DB
    pub txs_count: u64,
    pub chain_bytes: u64,
    pub start_time: Instant,
    pub metrics: Metrics,
    // --------------
}

/// Represents errors that can occur during the verification of a proposal's signature.
#[derive(Debug)]
pub enum SignatureVerificationError {
    /// Indicates that the `Init` part of the proposal is unexpectedly missing.
    MissingInitPart,
    /// Indicates that the `Fin` part of the proposal is unexpectedly missing.
    MissingFinPart,
    /// Indicates that the proposer was not found in the validator set.
    ProposerNotFound,
    /// Indicates that the signature in the `Fin` part is invalid.
    InvalidSignature,
    /// Validator set not found for the given height
    ValidatorSetNotFound { _height: Height },
}

/// Represents errors that can occur during proposal validation.
#[derive(Debug)]
pub enum ProposalValidationError {
    /// Proposer doesn't match the expected proposer for the given round
    WrongProposer { actual: Address, expected: Address },
    /// Signature verification errors
    Signature(SignatureVerificationError),
    /// Validator set not found for the given height
    ValidatorSetNotFound { height: Height },
}

impl fmt::Display for ProposalValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongProposer { actual, expected } => {
                write!(f, "Wrong proposer: got {actual}, expected {expected}")
            }
            Self::Signature(err) => {
                write!(f, "Signature verification failed: {err:?}")
            }
            Self::ValidatorSetNotFound { height } => {
                write!(f, "Validator set not found for height {height}")
            }
        }
    }
}

// Make up a seed for the rng based on our address in
// order for each node to likely propose different values at
// each round.
fn seed_from_address(address: &Address) -> u64 {
    address.into_inner().chunks(8).fold(0u64, |acc, chunk| {
        let term = chunk.iter().fold(0u64, |acc, &x| {
            acc.wrapping_shl(8).wrapping_add(u64::from(x))
        });
        acc.wrapping_add(term)
    })
}

fn build_execution_block_from_bytes(raw_block_data: Bytes) -> ExecutionBlock {
    let execution_payload: ExecutionPayloadV3 = ExecutionPayloadV3::from_ssz_bytes(&raw_block_data)
        .expect("failed to convert block bytes into executon payload");
    ExecutionBlock {
        block_hash: execution_payload.payload_inner.payload_inner.block_hash,
        block_number: execution_payload.payload_inner.payload_inner.block_number,
        parent_hash: execution_payload.payload_inner.payload_inner.parent_hash,
        timestamp: execution_payload.payload_inner.payload_inner.timestamp,
        prev_randao: execution_payload.payload_inner.payload_inner.prev_randao,
    }
}

impl State {
    /// Creates a new State instance with the given validator address and starting height
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        _genesis: Genesis, // all genesis data is in EVM via genesis.json
        ctx: EmeraldContext,
        signing_provider: K256Provider,
        address: Address,
        height: Height,
        store: Store,
        state_metrics: StateMetrics,
        emerald_config: EmeraldConfig,
    ) -> Self {
        // Calculate start_time by subtracting elapsed_seconds from now.
        // It represents the start time of measuring metrics, not the actual node start time.
        // This allows us to continue accumulating time correctly after a restart
        let start_time =
            Instant::now() - core::time::Duration::from_secs(state_metrics.elapsed_seconds);

        let eth_genesis_path = PathBuf::from_str(&emerald_config.ethereum_config.eth_genesis_path)
            .unwrap_or_else(|_| panic!("failed to read evm genesis file path from config"));

        let eth_genesis_path_str = &fs::read_to_string(eth_genesis_path)
            .unwrap_or_else(|_| panic!("failed to read evm genesis path"));
        let eth_genesis: EvmGenesis = serde_json::from_str(eth_genesis_path_str)
            .unwrap_or_else(|_| panic!("failed to read evm genesis file"));

        Self {
            ctx,
            signing_provider,
            consensus_height: height,
            consensus_round: Round::new(0),
            address,
            store,
            stream_nonce: 0,
            streams_map: PartStreamsMap::new(),
            rng: StdRng::seed_from_u64(seed_from_address(&address)),

            latest_block: None,
            validator_set: None,

            validated_payload_cache: ValidatedPayloadCache::new(10),
            earliest_certificate_height: None,
            earliest_value_height: None,

            txs_count: state_metrics.txs_count,
            chain_bytes: state_metrics.chain_bytes,
            start_time,
            metrics: state_metrics.metrics,
            last_block_time: Instant::now(),
            previous_block_commit_time: Instant::now(),
            eth_chain_config: eth_genesis.config,
            emerald_config,
        }
    }

    pub fn get_fork(&self, block_timestamp: BlockTimestamp) -> Fork {
        let is_osaka = self
            .eth_chain_config
            .osaka_time
            .is_some_and(|time| time <= block_timestamp);
        if is_osaka {
            return Fork::Osaka;
        }
        let is_prague = self
            .eth_chain_config
            .prague_time
            .is_some_and(|time| time <= block_timestamp);
        if is_prague {
            return Fork::Prague;
        }
        Fork::Unsupported
    }

    pub fn validated_cache_mut(&mut self) -> &mut ValidatedPayloadCache {
        &mut self.validated_payload_cache
    }

    pub async fn get_latest_block_candidate(&self, height: Height) -> Option<ExecutionBlock> {
        let decided_value = self.store.get_decided_value(height).await.ok().flatten()?;

        let certificate = decided_value.certificate;

        let raw_block_data = self
            .get_block_data(certificate.height, certificate.round, certificate.value_id)
            .await
            .expect("state: certificate should have associated block data");
        debug!(
            "🎁 block size: {:?}, height: {}",
            raw_block_data.iter().len(),
            height
        );
        Some(build_execution_block_from_bytes(raw_block_data))
    }

    /// Returns the earliest height with a certificate in the store.
    /// The value is cached and updated after pruning.
    pub async fn get_earliest_certificate_height(&mut self) -> Height {
        if let Some(height) = self.earliest_certificate_height {
            return height;
        }

        let height = self
            .store
            .min_decided_value_height()
            .await
            .unwrap_or_default();
        self.earliest_certificate_height = Some(height);
        height
    }

    /// Returns the earliest height with decided value data in the store.
    /// The value is cached and updated after pruning.
    pub async fn get_earliest_value_height(&mut self) -> Height {
        if let Some(height) = self.earliest_value_height {
            return height;
        }

        let height = self
            .store
            .min_unpruned_decided_value_height()
            .await
            .unwrap_or_default();
        self.earliest_value_height = Some(height);
        height
    }

    /// Validates a proposal by checking both proposer and signature
    pub fn validate_proposal_parts(
        &self,
        parts: &ProposalParts,
    ) -> Result<(), ProposalValidationError> {
        let height = parts.height;
        let round = parts.round;

        // Get the expected proposer for this height and round
        let validator_set = self
            .get_validator_set(height)
            .ok_or(ProposalValidationError::ValidatorSetNotFound { height })?;
        let expected_proposer = self
            .ctx
            .select_proposer(validator_set, height, round)
            .address;

        // Check if the proposer matches the expected proposer
        if parts.proposer != expected_proposer {
            return Err(ProposalValidationError::WrongProposer {
                actual: parts.proposer,
                expected: expected_proposer,
            });
        }

        // If proposer is correct, verify the signature
        self.verify_proposal_parts_signature(parts)
            .map_err(ProposalValidationError::Signature)?;

        Ok(())
    }

    /// Verify proposal signature
    fn verify_proposal_parts_signature(
        &self,
        parts: &ProposalParts,
    ) -> Result<(), SignatureVerificationError> {
        let mut hasher = sha3::Keccak256::new();

        let init = parts
            .init()
            .ok_or(SignatureVerificationError::MissingInitPart)?;

        let fin = parts
            .fin()
            .ok_or(SignatureVerificationError::MissingFinPart)?;

        let hash = {
            hasher.update(init.height.as_u64().to_be_bytes());
            hasher.update(init.round.as_i64().to_be_bytes());

            // The correctness of the hash computation relies on the parts being ordered by sequence
            // number, which is guaranteed by the `PartStreamsMap`.
            for part in parts.parts.iter().filter_map(|part| part.as_data()) {
                hasher.update(part.bytes.as_ref());
            }

            hasher.finalize()
        };

        // Retrieve the proposer from the validator set for the given height
        let validator_set = self.get_validator_set(parts.height).ok_or(
            SignatureVerificationError::ValidatorSetNotFound {
                _height: parts.height,
            },
        )?;
        let proposer = validator_set
            .get_by_address(&parts.proposer)
            .ok_or(SignatureVerificationError::ProposerNotFound)?;

        // Verify the signature
        if !self
            .signing_provider
            .verify(&hash, &fin.signature, &proposer.public_key)
        {
            return Err(SignatureVerificationError::InvalidSignature);
        }

        Ok(())
    }

    /// Processes complete proposal parts: validates, stores, and returns the proposed value.
    ///
    /// Returns `Ok(Some(ProposedValue))` if the proposal is valid and stored,
    /// `Ok(None)` if validation fails, or an error for storage/engine failures.
    pub async fn process_complete_proposal_parts(
        &mut self,
        parts: &ProposalParts,
        engine: &Engine,
        retry_config: &RetryConfig,
    ) -> eyre::Result<Option<ProposedValue<EmeraldContext>>> {
        // Validate proposal (proposer + signature)
        if let Err(error) = self.validate_proposal_parts(parts) {
            error!(
                height = %parts.height,
                round = %parts.round,
                proposer = %parts.proposer,
                error = ?error,
                "Rejecting invalid proposal"
            );
            return Ok(None);
        }

        // Assemble the proposal from its parts
        let (value, data) = assemble_value_from_parts(parts.clone());

        // Log first 32 bytes of proposal data and total size
        info!(
            data = %hex::encode(&data[..data.len().min(32)]),
            total_size = %data.len(),
            id = %value.value.id().as_u64(),
            "Proposal data"
        );

        // Validate the execution payload with the execution engine
        let validity = validate_execution_payload(
            &mut self.validated_payload_cache,
            &data,
            value.height,
            value.round,
            engine,
            retry_config,
        )
        .await?;

        if validity == Validity::Invalid {
            warn!(
                height = %parts.height,
                round = %parts.round,
                "Proposal has invalid execution payload, rejecting"
            );
            return Ok(None);
        }

        // Store the verified proposal and the authenticated wire evidence together. A conflicting
        // aggregate is not safe to deliver to consensus as though it were canonical.
        info!(%value.height, %value.round, %value.proposer, "Storing validated proposal as undecided");
        let attestation = ProposalAttestation {
            init: parts
                .init()
                .cloned()
                .expect("complete proposal has init part"),
            fin: parts
                .fin()
                .cloned()
                .expect("complete proposal has fin part"),
        };
        let outcome = self
            .store
            .write_undecided_proposal(UndecidedProposalWrite {
                proposal: value.clone(),
                payload: data,
                attestation: Some(attestation),
                source: UndecidedWriteSource::Proposal,
            })
            .await?;
        if let UndecidedWriteOutcome::Conflict(conflict) = &outcome {
            warn!(
                height = %value.height,
                round = %value.round,
                field = ?conflict.field,
                "Received conflicting complete proposal"
            );
        }

        Ok(match outcome {
            UndecidedWriteOutcome::Canonical(value) => Some(value),
            UndecidedWriteOutcome::Conflict(_) => None,
        })
    }

    /// Reassembles proposal parts from streamed messages.
    ///
    /// Handles height filtering:
    /// - Outdated proposals (height < current) are dropped
    /// - Future proposals (height > current) are stored as pending
    /// - Current height proposals are returned for validation
    ///
    /// Returns `Some(ProposalParts)` when a complete proposal is ready for validation,
    /// `None` if the proposal is incomplete, outdated, or stored for later.
    pub async fn reassemble_proposal(
        &mut self,
        from: PeerId,
        part: StreamMessage<ProposalPart>,
    ) -> eyre::Result<Option<ProposalParts>> {
        let sequence = part.sequence;

        // Check if we have a full proposal
        let Some(parts) = self.streams_map.insert(from, part) else {
            return Ok(None);
        };

        // Check if the proposal is outdated
        if parts.height < self.consensus_height {
            debug!(
                height = %self.consensus_height,
                round = %self.consensus_round,
                part.height = %parts.height,
                part.round = %parts.round,
                part.sequence = %sequence,
                "Received outdated proposal part, ignoring"
            );

            return Ok(None);
        }

        // Store future proposals parts in pending without validation
        if parts.height > self.consensus_height {
            info!(%parts.height, %parts.round, "Storing proposal parts for a future height in pending");
            self.store.store_pending_proposal_parts(parts).await?;
            return Ok(None);
        }

        // For current height, return parts for validation
        Ok(Some(parts))
    }

    /// Retrieves a decided block data at the given height
    pub async fn get_block_data(
        &self,
        height: Height,
        round: Round,
        value_id: ValueId,
    ) -> Option<Bytes> {
        self.store
            .get_block_data(height, round, value_id)
            .await
            .ok()
            .flatten()
    }

    /// Stores an unattested undecided proposal through the atomic aggregate boundary.
    ///
    /// This is used while a local proposal is being built. `stream_proposal` upgrades the
    /// matching record with its authenticated Init/Fin envelope before publication.
    pub async fn store_undecided_value(
        &self,
        value: &ProposedValue<EmeraldContext>,
        data: Bytes,
    ) -> eyre::Result<()> {
        match self
            .store
            .write_undecided_proposal(UndecidedProposalWrite {
                proposal: value.clone(),
                payload: data,
                attestation: None,
                source: UndecidedWriteSource::Proposal,
            })
            .await?
        {
            UndecidedWriteOutcome::Canonical(_) => Ok(()),
            UndecidedWriteOutcome::Conflict(conflict) => Err(eyre::eyre!(
                "undecided proposal conflict at height {}, round {}, field {:?}",
                value.height,
                value.round,
                conflict.field,
            )),
        }
    }

    /// Commits a value with the given certificate, updating internal state
    /// and moving to the next height
    pub async fn commit(
        &mut self,
        certificate: CommitCertificate<EmeraldContext>,
    ) -> eyre::Result<()> {
        info!(
            height = %certificate.height,
            round = %certificate.round,
            "Looking for certificate"
        );

        let proposal = self
            .store
            .get_undecided_proposal(certificate.height, certificate.round, certificate.value_id)
            .await;

        let proposal = match proposal {
            Ok(Some(proposal)) => proposal,
            Ok(None) => {
                error!(
                    height = %certificate.height,
                    round = %certificate.round,
                    "Trying to commit a value that is not decided"
                );

                return Ok(()); // FIXME: Return an actual error and handle in caller
            }
            Err(e) => return Err(e.into()),
        };

        // Get block data for decided value
        let block_data = self
            .store
            .get_block_data(certificate.height, certificate.round, certificate.value_id)
            .await?;

        // Log first 32 bytes of block data with JNT prefix
        if let Some(data) = &block_data {
            if data.len() >= 32 {
                info!("Committed block_data[0..32]: {}", hex::encode(&data[..32]));
            }
        }

        if let Some(data) = block_data {
            // Store decided value and the block header
            let execution_payload = ExecutionPayloadV3::from_ssz_bytes(&data).unwrap();
            let block_header = extract_block_header(&execution_payload);
            let block_header_bytes = Bytes::from(block_header.as_ssz_bytes());
            self.store
                .store_decided_value(&certificate, proposal.value, block_header_bytes)
                .await?;

            // Store decided block data
            self.store
                .store_decided_block_data(certificate.height, data)
                .await?;
        }

        let prune_certificates = self.emerald_config.num_certificates_to_retain != u64::MAX
            && certificate.height.as_u64() % self.emerald_config.prune_at_block_interval == 0;

        // If storege becomes a bottleneck, consider optimizing this by pruning every INTERVAL heights
        let prune_result = self
            .store
            .prune(
                self.emerald_config.num_certificates_to_retain,
                self.emerald_config.num_temp_blocks_retained,
                certificate.height,
                prune_certificates,
            )
            .await?;

        if let Some(height) = prune_result.earliest_certificate_height {
            self.earliest_certificate_height = Some(height);
        }
        if let Some(height) = prune_result.earliest_value_height {
            self.earliest_value_height = Some(height);
        }

        // Sleep to reduce the block speed, if set via config.
        debug!(timeout_commit = ?self.emerald_config.min_block_time);
        let elapsed_height_time = self.last_block_time.elapsed();

        info!(
            "👉 stats at {:?}: block_time {:?}",
            certificate.height, elapsed_height_time
        );

        if elapsed_height_time < self.emerald_config.min_block_time {
            tokio::time::sleep(self.emerald_config.min_block_time - elapsed_height_time).await;
        }

        Ok(())
    }

    /// Retrieves a previously built proposal value for the given height and round.
    /// Called by the consensus engine to re-use a previously built value.
    /// There should be at most one proposal for a given height and round when the proposer is not byzantine.
    /// We assume this implementation is not byzantine and we are the proposer for the given height and round.
    /// Therefore there must be a single proposal for the rounds where we are the proposer, with the proposer address
    /// matching our own.
    pub async fn get_previously_built_value(
        &self,
        height: Height,
        round: Round,
    ) -> eyre::Result<Option<(LocallyProposedValue<EmeraldContext>, Round)>> {
        let proposals: Vec<ProposedValue<EmeraldContext>> =
            self.store.get_undecided_proposals(height, round).await?;

        let Some(proposal) = proposals.first() else {
            return Ok(None);
        };
        if proposals.len() > 1 {
            return Err(eyre::eyre!(
                "multiple proposals stored at height {height}, round {round}"
            ));
        }
        if proposal.proposer != self.address {
            return Err(eyre::eyre!(
                "proposal stored for non-local proposer {} at height {height}, round {round}",
                proposal.proposer
            ));
        }

        Ok(Some((
            LocallyProposedValue::new(proposal.height, proposal.round, proposal.value.clone()),
            proposal.valid_round,
        )))
    }

    /// Prepares a stored proposal for restreaming and records the re-proposal at the current round.
    pub async fn prepare_restream_proposal(
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

        if proposal_round != current_round {
            // A conflicting aggregate at the current-round key aborts without mutation.
            let current_round_proposal = ProposedValue {
                height: proposal.height,
                round: current_round,
                valid_round: proposal_round,
                proposer: self.address,
                value: proposal.value.clone(),
                validity: Validity::Valid,
            };
            self.store_undecided_value(&current_round_proposal, bytes.clone())
                .await?;
        }

        Ok(Some((
            LocallyProposedValue::new(proposal.height, current_round, proposal.value),
            bytes,
        )))
    }

    /// Loads and verifies the exact authenticated proposal requested for a restream effect.
    ///
    /// The stored proposer must match the effect address and the original fin signature is
    /// verified again before any wire part is returned for publication.
    pub async fn prepare_attested_replay(
        &self,
        height: Height,
        round: Round,
        valid_round: Round,
        address: Address,
        value_id: ValueId,
    ) -> eyre::Result<AttestedReplay> {
        let Some(record) = self
            .store
            .get_undecided_record(height, round, value_id)
            .await?
        else {
            return Ok(AttestedReplay::Absent);
        };
        let Some(attestation) = record.attestation else {
            return Ok(AttestedReplay::Absent);
        };

        let identity_matches = attestation.init.height == height
            && attestation.init.round == round
            && attestation.init.pol_round == valid_round
            && attestation.init.proposer == address
            && record.proposal.value.id() == value_id;
        if !identity_matches {
            return Ok(AttestedReplay::IdentityMismatch {
                stored_init: attestation.init,
            });
        }

        let mut parts = Vec::with_capacity(record.payload.chunks(CHUNK_SIZE).len() + 2);
        parts.push(ProposalPart::Init(attestation.init));
        for chunk in record.payload.chunks(CHUNK_SIZE) {
            parts.push(ProposalPart::Data(ProposalData::new(
                Bytes::copy_from_slice(chunk),
            )));
        }
        parts.push(ProposalPart::Fin(attestation.fin));

        let proposal_parts = ProposalParts {
            height,
            round,
            proposer: address,
            parts,
        };
        self.verify_proposal_parts_signature(&proposal_parts)
            .map_err(|error| eyre::eyre!(
                "invalid stored proposal attestation at height {height}, round {round}, value {value_id}: {error:?}"
            ))?;

        Ok(AttestedReplay::Ready(proposal_parts.parts))
    }

    // /// Make up a new value to propose
    // /// A real application would have a more complex logic here,
    // /// typically reaping transactions from a mempool and executing them against its state,
    // /// before computing the merkle root of the new app state.
    // fn make_value(&mut self) -> Value {
    //     let value = self.rng.gen_range(100..=100000);
    //     Value::new(value)
    // }

    #[allow(dead_code)]
    pub fn make_block(&mut self) -> Bytes {
        let mut random_bytes = vec![0u8; BLOCK_SIZE];
        self.rng.fill(&mut random_bytes[..]);
        Bytes::from(random_bytes)
    }

    /// Creates a new proposal value for the given height
    /// Returns either a previously built proposal or creates a new one
    pub async fn propose_value(
        &mut self,
        height: Height,
        round: Round,
        data: Bytes,
    ) -> eyre::Result<LocallyProposedValue<EmeraldContext>> {
        assert_eq!(height, self.consensus_height);
        assert_eq!(round, self.consensus_round);

        // We create a new value.
        let value = Value::new(data.clone());

        let proposal: ProposedValue<EmeraldContext> = ProposedValue {
            height,
            round,
            valid_round: Round::Nil,
            proposer: self.address, // We are the proposer
            value,
            validity: Validity::Valid, // Our proposals are de facto valid
        };

        // Store the proposal and its block data
        self.store_undecided_value(&proposal, data).await?;

        Ok(LocallyProposedValue::new(
            proposal.height,
            proposal.round,
            proposal.value,
        ))
    }

    fn stream_id(&mut self, height: Height, round: Round) -> StreamId {
        let mut bytes = Vec::with_capacity(size_of::<u64>() + size_of::<u32>());
        bytes.extend_from_slice(&height.as_u64().to_be_bytes());
        bytes.extend_from_slice(&round.as_u32().unwrap().to_be_bytes());
        bytes.extend_from_slice(&self.stream_nonce.to_be_bytes());
        self.stream_nonce += 1;
        StreamId::new(bytes.into())
    }

    /// Creates a stream message containing a proposal part.
    /// Updates internal sequence number and current proposal.
    pub async fn stream_proposal(
        &mut self,
        value: LocallyProposedValue<EmeraldContext>,
        data: Bytes,
        pol_round: Round,
    ) -> eyre::Result<Vec<StreamMessage<ProposalPart>>> {
        let parts = self.make_proposal_parts(value.clone(), data.clone(), pol_round);
        let init = parts
            .first()
            .and_then(ProposalPart::as_init)
            .expect("locally built proposal has init")
            .clone();
        let fin = parts
            .last()
            .and_then(ProposalPart::as_fin)
            .expect("locally built proposal has fin")
            .clone();
        let proposal = ProposedValue {
            height: value.height,
            round: value.round,
            valid_round: pol_round,
            proposer: self.address,
            value: value.value.clone(),
            validity: Validity::Valid,
        };

        match self
            .store
            .write_undecided_proposal(UndecidedProposalWrite {
                proposal,
                payload: data,
                attestation: Some(ProposalAttestation::new(init, fin)),
                source: UndecidedWriteSource::Proposal,
            })
            .await?
        {
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

    pub(crate) fn make_stream_messages(
        &mut self,
        height: Height,
        round: Round,
        parts: Vec<ProposalPart>,
    ) -> Vec<StreamMessage<ProposalPart>> {
        let stream_id = self.stream_id(height, round);

        let mut msgs = Vec::with_capacity(parts.len() + 1);
        let mut sequence = 0;

        for part in parts {
            let msg = StreamMessage::new(stream_id.clone(), sequence, StreamContent::Data(part));
            sequence += 1;
            msgs.push(msg);
        }

        msgs.push(StreamMessage::new(stream_id, sequence, StreamContent::Fin));
        msgs
    }

    fn make_proposal_parts(
        &self,
        value: LocallyProposedValue<EmeraldContext>,
        data: Bytes,
        pol_round: Round,
    ) -> Vec<ProposalPart> {
        let mut hasher = sha3::Keccak256::new();
        let mut parts = Vec::new();

        // Init
        {
            parts.push(ProposalPart::Init(ProposalInit::new(
                value.height,
                value.round,
                pol_round,
                self.address,
            )));

            hasher.update(value.height.as_u64().to_be_bytes().as_slice());
            hasher.update(value.round.as_i64().to_be_bytes().as_slice());
        }

        // Data
        {
            for chunk in data.chunks(CHUNK_SIZE) {
                let chunk_data = ProposalData::new(Bytes::copy_from_slice(chunk));
                parts.push(ProposalPart::Data(chunk_data));
                hasher.update(chunk);
            }
        }

        {
            let hash = hasher.finalize().to_vec();
            let signature = self.signing_provider.sign(&hash);
            parts.push(ProposalPart::Fin(ProposalFin::new(signature)));
        }

        parts
    }

    /// Returns the set of validators for the given consensus height.
    /// Returns None if the height doesn't match the stored validator set height.
    pub fn get_validator_set(&self, height: Height) -> Option<&ValidatorSet> {
        self.validator_set
            .as_ref()
            .and_then(|(h, vs)| if *h == height { Some(vs) } else { None })
    }

    /// Sets the validator set for the given consensus height.
    pub fn set_validator_set(&mut self, height: Height, validator_set: ValidatorSet) {
        self.validator_set = Some((height, validator_set));
    }

    /// Update and log per-block statistics
    pub async fn log_block_stats(
        &mut self,
        height: Height,
        tx_count: usize,
        block_bytes_len: usize,
        block_time_secs: f64,
    ) -> eyre::Result<()> {
        // Calculate per-block metrics
        let txs_per_second = if block_time_secs > 0.0 {
            tx_count as f64 / block_time_secs
        } else {
            0.0
        };
        let bytes_per_second = if block_time_secs > 0.0 {
            block_bytes_len as f64 / block_time_secs
        } else {
            0.0
        };

        // Update cumulative counters
        self.txs_count += tx_count as u64;
        self.chain_bytes += block_bytes_len as u64;
        let elapsed_time = self.start_time.elapsed();

        // Update metrics
        self.metrics.tx_stats.add_txs(tx_count as u64);
        self.metrics
            .tx_stats
            .add_chain_bytes(block_bytes_len as u64);
        self.metrics.tx_stats.set_txs_per_second(txs_per_second);
        self.metrics.tx_stats.set_bytes_per_second(bytes_per_second);
        self.metrics.tx_stats.set_block_tx_count(tx_count as u64);
        self.metrics.tx_stats.set_block_size(block_bytes_len as u64);

        // Persist cumulative metrics to database for crash recovery
        self.store
            .store_cumulative_metrics(self.txs_count, self.chain_bytes, elapsed_time.as_secs())
            .await?;

        info!(
            "👉 stats at height {}: block_time={:.3}s, #txs={}, txs/s={:.2}, block_bytes={}, bytes/s={:.2}, total_txs={}, total_bytes={}",
            height,
            block_time_secs,
            tx_count,
            txs_per_second,
            block_bytes_len,
            bytes_per_second,
            self.txs_count,
            self.chain_bytes,
        );

        Ok(())
    }
}

/// Re-assemble a [`ProposedValue`] from its [`ProposalParts`].
///
/// This is done by multiplying all the factors in the parts.
pub fn assemble_value_from_parts(parts: ProposalParts) -> (ProposedValue<EmeraldContext>, Bytes) {
    // Get the init part to extract pol_round
    let init = parts
        .parts
        .iter()
        .find_map(|part| part.as_init())
        .expect("ProposalParts should have an init part");

    // Calculate total size and allocate buffer
    let total_size: usize = parts
        .parts
        .iter()
        .filter_map(|part| part.as_data())
        .map(|data| data.bytes.len())
        .sum();

    let mut data = Vec::with_capacity(total_size);
    // Concatenate all chunks
    for part in parts.parts.iter().filter_map(|part| part.as_data()) {
        data.extend_from_slice(&part.bytes);
    }

    // Convert the concatenated data vector into Bytes
    let data = Bytes::from(data);

    let proposed_value = ProposedValue {
        height: parts.height,
        round: parts.round,
        valid_round: init.pol_round,
        proposer: parts.proposer,
        value: Value::new(data.clone()),
        validity: Validity::Valid,
    };

    (proposed_value, data)
}

/// Decodes a Value from its byte representation using ProtobufCodec
pub fn decode_value(bytes: Bytes) -> Result<Value, ProtoError> {
    ProtobufCodec.decode(bytes)
}

#[cfg(test)]
mod tests {
    use malachitebft_app_channel::{AppMsg, Channels, NetworkMsg};
    use malachitebft_eth_types::secp256k1::PrivateKey;
    use malachitebft_eth_types::Validator;
    use tokio::sync::mpsc;

    use super::*;
    use crate::app::on_restream_proposal;
    use crate::metrics::{DbMetrics, Metrics};

    async fn make_test_state() -> (State, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("store.redb"), 1024 * 1024, DbMetrics::new())
            .await
            .unwrap();

        let private_key = PrivateKey::from_slice(&[1_u8; 32]).unwrap();
        let public_key = private_key.public_key();
        let address = Address::from_public_key(&public_key);
        let genesis = Genesis {
            validator_set: ValidatorSet::new([Validator::new(public_key.clone(), 1)]),
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
        let eth_genesis_path = dir.path().join("genesis.json");
        std::fs::write(&eth_genesis_path, "{}").unwrap();
        emerald_config.ethereum_config.eth_genesis_path = eth_genesis_path.display().to_string();

        let mut state = State::new(
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
        state.set_validator_set(
            Height::new(1426),
            ValidatorSet::new([Validator::new(public_key, 1)]),
        );

        (state, dir)
    }

    fn make_test_channels() -> (
        Channels<EmeraldContext>,
        tokio::task::JoinHandle<ProposalInit>,
    ) {
        let (_consensus_tx, consensus_rx) = mpsc::channel(1);
        let (network_tx, mut network_rx) = mpsc::channel::<NetworkMsg<EmeraldContext>>(1);
        let (requests_tx, _requests_rx) = mpsc::channel(1);

        let proposal_init = tokio::spawn(async move {
            let mut proposal_init = None;
            while let Some(NetworkMsg::PublishProposalPart(message)) = network_rx.recv().await {
                if proposal_init.is_none() {
                    proposal_init = message
                        .content
                        .into_data()
                        .and_then(|part| part.as_init().cloned());
                }
            }

            proposal_init.expect("streamed proposal must contain ProposalInit")
        });

        (
            Channels {
                consensus: consensus_rx,
                network: network_tx,
                events: Default::default(),
                requests: requests_tx,
            },
            proposal_init,
        )
    }

    fn make_collecting_channels() -> (
        Channels<EmeraldContext>,
        tokio::task::JoinHandle<Vec<ProposalPart>>,
    ) {
        let (_consensus_tx, consensus_rx) = mpsc::channel(1);
        let (network_tx, mut network_rx) = mpsc::channel::<NetworkMsg<EmeraldContext>>(16);
        let (requests_tx, _requests_rx) = mpsc::channel(1);
        let collector = tokio::spawn(async move {
            let mut parts = Vec::new();
            while let Some(NetworkMsg::PublishProposalPart(message)) = network_rx.recv().await {
                if let Some(part) = message.content.into_data() {
                    parts.push(part);
                }
            }
            parts
        });

        (
            Channels {
                consensus: consensus_rx,
                network: network_tx,
                events: Default::default(),
                requests: requests_tx,
            },
            collector,
        )
    }

    fn make_message_collecting_channels() -> (
        Channels<EmeraldContext>,
        tokio::task::JoinHandle<Vec<StreamMessage<ProposalPart>>>,
    ) {
        let (_consensus_tx, consensus_rx) = mpsc::channel(1);
        let (network_tx, mut network_rx) = mpsc::channel::<NetworkMsg<EmeraldContext>>(16);
        let (requests_tx, _requests_rx) = mpsc::channel(1);
        let collector = tokio::spawn(async move {
            let mut messages = Vec::new();
            while let Some(NetworkMsg::PublishProposalPart(message)) = network_rx.recv().await {
                messages.push(message);
            }
            messages
        });

        (
            Channels {
                consensus: consensus_rx,
                network: network_tx,
                events: Default::default(),
                requests: requests_tx,
            },
            collector,
        )
    }

    async fn store_foreign_attested_proposal(
        state: &mut State,
        valid_signature: bool,
    ) -> (ProposedValue<EmeraldContext>, Vec<ProposalPart>) {
        let foreign_key = PrivateKey::from_slice(&[2_u8; 32]).unwrap();
        let foreign_public_key = foreign_key.public_key();
        let foreign_address = Address::from_public_key(&foreign_public_key);
        let height = Height::new(1426);
        let round = Round::new(10);
        let valid_round = Round::new(7);
        let payload = Bytes::from(vec![0xA5; CHUNK_SIZE + 17]);
        state.set_validator_set(
            height,
            ValidatorSet::new([Validator::new(foreign_public_key, 1)]),
        );

        let init = ProposalInit::new(height, round, valid_round, foreign_address);
        let mut hasher = sha3::Keccak256::new();
        hasher.update(height.as_u64().to_be_bytes());
        hasher.update(round.as_i64().to_be_bytes());
        hasher.update(&payload);
        let digest = hasher.finalize();
        let signature = if valid_signature {
            K256Provider::new(foreign_key).sign(&digest)
        } else {
            state.signing_provider.sign(&digest)
        };
        let fin = ProposalFin::new(signature);
        let proposal = ProposedValue {
            height,
            round,
            valid_round,
            proposer: foreign_address,
            value: Value::new(payload.clone()),
            validity: Validity::Valid,
        };
        state
            .store
            .write_undecided_proposal(UndecidedProposalWrite {
                proposal: proposal.clone(),
                payload: payload.clone(),
                attestation: Some(ProposalAttestation::new(init.clone(), fin.clone())),
                source: UndecidedWriteSource::Proposal,
            })
            .await
            .unwrap();

        let mut parts = Vec::with_capacity(payload.chunks(CHUNK_SIZE).len() + 2);
        parts.push(ProposalPart::Init(init));
        parts.extend(
            payload
                .chunks(CHUNK_SIZE)
                .map(|chunk| ProposalPart::Data(ProposalData::new(Bytes::copy_from_slice(chunk)))),
        );
        parts.push(ProposalPart::Fin(fin));

        (proposal, parts)
    }

    #[tokio::test]
    async fn restream_proposal_does_not_rebuild_a_foreign_proposal() {
        let (mut state, _dir) = make_test_state().await;
        let height = Height::new(1426);
        let proposal_round = Round::new(0);
        let current_round = Round::new(1);
        let bytes = Bytes::from(vec![0xAB; CHUNK_SIZE * 17]);
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

        let (mut channels, proposal_init) = make_test_channels();

        on_restream_proposal(
            AppMsg::RestreamProposal {
                height,
                round: current_round,
                valid_round: proposal_round,
                address: original_proposer,
                value_id: value.id(),
            },
            &mut state,
            &mut channels,
        )
        .await
        .unwrap();

        let current_round_proposal = state
            .store
            .get_undecided_proposal(height, current_round, value.id())
            .await
            .unwrap();
        assert!(current_round_proposal.is_none());

        drop(channels);
        proposal_init.abort();
    }

    #[tokio::test]
    async fn restream_proposal_returns_none_when_proposal_is_missing() {
        let (state, _dir) = make_test_state().await;

        let result = state
            .prepare_restream_proposal(
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
    async fn restream_proposal_errors_when_storage_record_is_incomplete() {
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
            .prepare_restream_proposal(height, proposal_round, Round::new(1), value.id())
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("missing block data must be an error"),
        };

        assert!(error
            .to_string()
            .contains("invalid undecided proposal record shape"));
        assert!(error.to_string().contains("1426"));
    }

    #[tokio::test]
    async fn restream_proposal_preserves_nil_valid_round_in_proposal_init() {
        let (mut state, _dir) = make_test_state().await;
        let height = Height::new(1426);
        let round = Round::new(10);
        let bytes = Bytes::from_static(b"hidden-lock-block");
        let value = Value::new(bytes.clone());
        let stored_proposal = ProposedValue {
            height,
            round,
            valid_round: Round::Nil,
            proposer: state.address,
            value: value.clone(),
            validity: Validity::Valid,
        };
        state
            .store_undecided_value(&stored_proposal, bytes)
            .await
            .unwrap();

        let (mut channels, proposal_init) = make_test_channels();

        on_restream_proposal(
            AppMsg::RestreamProposal {
                height,
                round,
                valid_round: Round::Nil,
                address: state.address,
                value_id: value.id(),
            },
            &mut state,
            &mut channels,
        )
        .await
        .unwrap();

        drop(channels);
        let init = proposal_init.await.unwrap();

        assert_eq!(init.height, height);
        assert_eq!(init.round, round);
        assert_eq!(init.pol_round, Round::Nil);

        let current_round_proposal = state
            .store
            .get_undecided_proposal(height, round, value.id())
            .await
            .unwrap()
            .expect("current-round proposal must remain stored");
        assert_eq!(current_round_proposal.valid_round, Round::Nil);
    }

    #[tokio::test]
    async fn local_stream_persists_attestation_before_returning_messages() {
        let (mut state, _dir) = make_test_state().await;
        let height = Height::new(1426);
        let round = Round::new(0);
        let payload = Bytes::from_static(b"local-attested-proposal");
        let proposal = state
            .propose_value(height, round, payload.clone())
            .await
            .unwrap();

        let messages = state
            .stream_proposal(proposal.clone(), payload, Round::Nil)
            .await
            .unwrap();
        assert!(!messages.is_empty());

        let stored = state
            .store
            .get_undecided_record(height, round, proposal.value.id())
            .await
            .unwrap()
            .expect("streamed proposal must be stored");
        assert!(stored.attestation.is_some());
    }

    #[tokio::test]
    async fn previously_built_value_rejects_a_non_local_proposer() {
        let (state, _dir) = make_test_state().await;
        let height = Height::new(1426);
        let round = Round::new(0);
        let payload = Bytes::from_static(b"foreign-proposal");
        let foreign = ProposedValue {
            height,
            round,
            valid_round: Round::Nil,
            proposer: Address::new([9; 20]),
            value: Value::new(payload.clone()),
            validity: Validity::Valid,
        };
        state
            .store
            .write_undecided_proposal(UndecidedProposalWrite {
                proposal: foreign,
                payload,
                attestation: None,
                source: UndecidedWriteSource::Proposal,
            })
            .await
            .unwrap();

        let error = state
            .get_previously_built_value(height, round)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("non-local proposer"));
    }

    #[tokio::test]
    async fn previously_built_value_rejects_multiple_candidates() {
        let (state, _dir) = make_test_state().await;
        let height = Height::new(1426);
        let round = Round::new(0);

        for payload in [
            Bytes::from_static(b"first-local-proposal"),
            Bytes::from_static(b"second-local-proposal"),
        ] {
            let proposal = ProposedValue {
                height,
                round,
                valid_round: Round::Nil,
                proposer: state.address,
                value: Value::new(payload.clone()),
                validity: Validity::Valid,
            };
            state
                .store
                .write_undecided_proposal(UndecidedProposalWrite {
                    proposal,
                    payload,
                    attestation: None,
                    source: UndecidedWriteSource::Proposal,
                })
                .await
                .unwrap();
        }

        let error = state
            .get_previously_built_value(height, round)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("multiple proposals"));
    }

    #[tokio::test]
    async fn attested_replay_returns_the_original_authenticated_parts() {
        let (mut state, _dir) = make_test_state().await;
        let height = Height::new(1426);
        let round = Round::new(0);
        let payload = Bytes::from_static(b"authenticated-replay");
        let proposal = state
            .propose_value(height, round, payload.clone())
            .await
            .unwrap();
        let streamed = state
            .stream_proposal(proposal.clone(), payload, Round::Nil)
            .await
            .unwrap();

        let AttestedReplay::Ready(parts) = state
            .prepare_attested_replay(
                height,
                round,
                Round::Nil,
                state.address,
                proposal.value.id(),
            )
            .await
            .unwrap()
        else {
            panic!("locally streamed proposal must be replayable");
        };

        assert_eq!(
            parts.first().and_then(ProposalPart::as_init),
            streamed[0]
                .content
                .as_data()
                .and_then(ProposalPart::as_init)
        );
        assert_eq!(
            parts.last().and_then(ProposalPart::as_fin),
            streamed[streamed.len() - 2]
                .content
                .as_data()
                .and_then(ProposalPart::as_fin)
        );
    }

    #[tokio::test]
    async fn attested_replay_rejects_an_effect_with_a_different_pol_round() {
        let (mut state, _dir) = make_test_state().await;
        let height = Height::new(1426);
        let round = Round::new(0);
        let payload = Bytes::from_static(b"attested-replay-identity");
        let proposal = state
            .propose_value(height, round, payload.clone())
            .await
            .unwrap();
        state
            .stream_proposal(proposal.clone(), payload, Round::Nil)
            .await
            .unwrap();

        let AttestedReplay::IdentityMismatch { stored_init } = state
            .prepare_attested_replay(
                height,
                round,
                Round::new(1),
                state.address,
                proposal.value.id(),
            )
            .await
            .unwrap()
        else {
            panic!("a different POL round must not be replayed");
        };

        assert_eq!(stored_init.pol_round, Round::Nil);
    }

    #[tokio::test]
    async fn foreign_restream_replays_original_parts_byte_for_byte() {
        let (mut state, _dir) = make_test_state().await;
        let (proposal, original_parts) = store_foreign_attested_proposal(&mut state, true).await;
        assert_ne!(proposal.proposer, state.address);
        let (mut channels, collector) = make_collecting_channels();

        on_restream_proposal(
            AppMsg::RestreamProposal {
                height: proposal.height,
                round: proposal.round,
                valid_round: proposal.valid_round,
                address: proposal.proposer,
                value_id: proposal.value.id(),
            },
            &mut state,
            &mut channels,
        )
        .await
        .unwrap();
        drop(channels);
        let replayed_parts = collector.await.unwrap();

        let encoded_original: Vec<_> = original_parts
            .iter()
            .map(|part| ProtobufCodec.encode(part).unwrap())
            .collect();
        let encoded_replayed: Vec<_> = replayed_parts
            .iter()
            .map(|part| ProtobufCodec.encode(part).unwrap())
            .collect();
        assert_eq!(
            replayed_parts
                .iter()
                .filter(|part| matches!(part, ProposalPart::Data(_)))
                .count(),
            2
        );
        assert_eq!(encoded_replayed, encoded_original);
    }

    #[tokio::test]
    async fn repeated_foreign_restreams_use_fresh_stream_ids() {
        let (mut state, _dir) = make_test_state().await;
        let (proposal, _) = store_foreign_attested_proposal(&mut state, true).await;
        let mut stream_ids = Vec::new();

        for _ in 0..2 {
            let (mut channels, collector) = make_message_collecting_channels();
            on_restream_proposal(
                AppMsg::RestreamProposal {
                    height: proposal.height,
                    round: proposal.round,
                    valid_round: proposal.valid_round,
                    address: proposal.proposer,
                    value_id: proposal.value.id(),
                },
                &mut state,
                &mut channels,
            )
            .await
            .unwrap();
            drop(channels);
            let messages = collector.await.unwrap();
            let stream_id = messages
                .first()
                .expect("restream must publish proposal parts")
                .stream_id
                .clone();
            assert!(messages
                .iter()
                .all(|message| message.stream_id == stream_id));
            stream_ids.push(stream_id);
        }

        assert_ne!(stream_ids[0], stream_ids[1]);
    }

    #[tokio::test]
    async fn forwarded_parts_validate_on_an_independent_receiver() {
        let (mut forwarder, _forwarder_dir) = make_test_state().await;
        let (proposal, _) = store_foreign_attested_proposal(&mut forwarder, true).await;
        let AttestedReplay::Ready(parts) = forwarder
            .prepare_attested_replay(
                proposal.height,
                proposal.round,
                proposal.valid_round,
                proposal.proposer,
                proposal.value.id(),
            )
            .await
            .unwrap()
        else {
            panic!("foreign proposal must be replayable");
        };

        let (mut receiver, _receiver_dir) = make_test_state().await;
        let foreign_key = PrivateKey::from_slice(&[2_u8; 32]).unwrap();
        receiver.set_validator_set(
            proposal.height,
            ValidatorSet::new([Validator::new(foreign_key.public_key(), 1)]),
        );
        let forwarded = ProposalParts {
            height: proposal.height,
            round: proposal.round,
            proposer: proposal.proposer,
            parts,
        };

        receiver.validate_proposal_parts(&forwarded).unwrap();
    }

    #[tokio::test]
    async fn independent_receiver_rejects_mutated_forwarded_payload() {
        let (mut forwarder, _forwarder_dir) = make_test_state().await;
        let (proposal, _) = store_foreign_attested_proposal(&mut forwarder, true).await;
        let AttestedReplay::Ready(mut parts) = forwarder
            .prepare_attested_replay(
                proposal.height,
                proposal.round,
                proposal.valid_round,
                proposal.proposer,
                proposal.value.id(),
            )
            .await
            .unwrap()
        else {
            panic!("foreign proposal must be replayable");
        };
        let data = parts
            .iter_mut()
            .find_map(|part| match part {
                ProposalPart::Data(data) => Some(data),
                _ => None,
            })
            .unwrap();
        let mut mutated = data.bytes.to_vec();
        mutated[0] ^= 0xFF;
        data.bytes = Bytes::from(mutated);

        let (mut receiver, _receiver_dir) = make_test_state().await;
        let foreign_key = PrivateKey::from_slice(&[2_u8; 32]).unwrap();
        receiver.set_validator_set(
            proposal.height,
            ValidatorSet::new([Validator::new(foreign_key.public_key(), 1)]),
        );
        let error = receiver
            .validate_proposal_parts(&ProposalParts {
                height: proposal.height,
                round: proposal.round,
                proposer: proposal.proposer,
                parts,
            })
            .unwrap_err();

        assert!(matches!(
            error,
            ProposalValidationError::Signature(SignatureVerificationError::InvalidSignature)
        ));
    }

    #[tokio::test]
    async fn independent_receiver_rejects_substituted_forwarder_as_proposer() {
        let (mut forwarder, _forwarder_dir) = make_test_state().await;
        let (proposal, _) = store_foreign_attested_proposal(&mut forwarder, true).await;
        let AttestedReplay::Ready(mut parts) = forwarder
            .prepare_attested_replay(
                proposal.height,
                proposal.round,
                proposal.valid_round,
                proposal.proposer,
                proposal.value.id(),
            )
            .await
            .unwrap()
        else {
            panic!("foreign proposal must be replayable");
        };
        let substituted = forwarder.address;
        parts
            .iter_mut()
            .find_map(|part| match part {
                ProposalPart::Init(init) => Some(init),
                _ => None,
            })
            .unwrap()
            .proposer = substituted;

        let (mut receiver, _receiver_dir) = make_test_state().await;
        let foreign_key = PrivateKey::from_slice(&[2_u8; 32]).unwrap();
        receiver.set_validator_set(
            proposal.height,
            ValidatorSet::new([Validator::new(foreign_key.public_key(), 1)]),
        );
        let error = receiver
            .validate_proposal_parts(&ProposalParts {
                height: proposal.height,
                round: proposal.round,
                proposer: substituted,
                parts,
            })
            .unwrap_err();

        assert!(matches!(
            error,
            ProposalValidationError::WrongProposer {
                actual,
                expected
            } if actual == substituted && expected == proposal.proposer
        ));
    }

    #[tokio::test]
    async fn invalid_stored_signature_does_not_stop_the_application() {
        let (mut state, _dir) = make_test_state().await;
        let (proposal, _) = store_foreign_attested_proposal(&mut state, false).await;
        let (mut channels, collector) = make_collecting_channels();

        let result = on_restream_proposal(
            AppMsg::RestreamProposal {
                height: proposal.height,
                round: proposal.round,
                valid_round: proposal.valid_round,
                address: proposal.proposer,
                value_id: proposal.value.id(),
            },
            &mut state,
            &mut channels,
        )
        .await;
        drop(channels);

        assert!(result.is_ok());
        assert!(collector.await.unwrap().is_empty());
    }
}
