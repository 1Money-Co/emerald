#![allow(clippy::result_large_err)]

use core::mem::size_of;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use color_eyre::eyre;
use malachitebft_app_channel::app::types::codec::Codec;
use malachitebft_app_channel::app::types::core::{CommitCertificate, Round};
use malachitebft_app_channel::app::types::sync::RawDecidedValue;
use malachitebft_app_channel::app::types::ProposedValue;
use malachitebft_eth_types::codec::proto as codec;
use malachitebft_eth_types::codec::proto::ProtobufCodec;
use malachitebft_eth_types::{proto, EmeraldContext, Height, ProposalAttestation, Value, ValueId};
use malachitebft_proto::{Error as ProtoError, Protobuf};
use prost::Message;
use redb::ReadableTable;
use thiserror::Error;

mod keys;
use keys::{HeightKey, UndecidedValueKey};

use crate::metrics::DbMetrics;
use crate::store::keys::PendingValueKey;
use crate::streaming::ProposalParts;

#[derive(Clone, Debug)]
pub struct DecidedValue {
    pub value: Value,
    pub certificate: CommitCertificate<EmeraldContext>,
}

/// Result of a prune operation containing the new earliest heights.
#[derive(Clone, Copy, Debug, Default)]
pub struct PruneResult {
    /// New earliest height with a certificate, if certificates were pruned.
    pub earliest_certificate_height: Option<Height>,
    /// New earliest height with decided value data, if values were pruned.
    pub earliest_value_height: Option<Height>,
}

fn decode_certificate(bytes: &[u8]) -> Result<CommitCertificate<EmeraldContext>, ProtoError> {
    let proto = proto::CommitCertificate::decode(bytes)?;
    codec::decode_certificate(proto)
}

fn encode_certificate(
    certificate: &CommitCertificate<EmeraldContext>,
) -> Result<Vec<u8>, ProtoError> {
    let proto = codec::encode_certificate(certificate)?;
    Ok(proto.encode_to_vec())
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("Database error: {0}")]
    Database(#[from] redb::DatabaseError),

    #[error("Storage error: {0}")]
    Storage(#[from] redb::StorageError),

    #[error("Table error: {0}")]
    Table(#[from] redb::TableError),

    #[error("Commit error: {0}")]
    Commit(#[from] redb::CommitError),

    #[error("Transaction error: {0}")]
    Transaction(#[from] redb::TransactionError),

    #[error("Failed to encode/decode Protobuf: {0}")]
    Protobuf(#[from] ProtoError),

    #[error("Failed to join on task: {0}")]
    TaskJoin(#[from] tokio::task::JoinError),

    #[error("Failed to serialize/deserialize JSON: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Undecided proposal integrity error: {0}")]
    Integrity(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UndecidedWriteSource {
    Proposal,
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

const CERTIFICATES_TABLE: redb::TableDefinition<'_, HeightKey, Vec<u8>> =
    redb::TableDefinition::new("certificates");

const DECIDED_VALUES_TABLE: redb::TableDefinition<'_, HeightKey, Vec<u8>> =
    redb::TableDefinition::new("decided_values");

const UNDECIDED_PROPOSALS_TABLE: redb::TableDefinition<'_, UndecidedValueKey, Vec<u8>> =
    redb::TableDefinition::new("undecided_values");

const DECIDED_BLOCK_DATA_TABLE: redb::TableDefinition<'_, HeightKey, Vec<u8>> =
    redb::TableDefinition::new("decided_block_data");

const UNDECIDED_BLOCK_DATA_TABLE: redb::TableDefinition<'_, UndecidedValueKey, Vec<u8>> =
    redb::TableDefinition::new("undecided_block_data");

const UNDECIDED_PROPOSAL_ATTESTATIONS_TABLE: redb::TableDefinition<'_, UndecidedValueKey, Vec<u8>> =
    redb::TableDefinition::new("undecided_proposal_attestations");

const DECIDED_BLOCK_HEADERS_TABLE: redb::TableDefinition<'_, HeightKey, Vec<u8>> =
    redb::TableDefinition::new("decided_block_headers");

const PERSISTENT_METRICS_TABLE: redb::TableDefinition<'_, &str, u64> =
    redb::TableDefinition::new("persistent_metrics");

const PENDING_PROPOSAL_PARTS_TABLE: redb::TableDefinition<'_, PendingValueKey, Vec<u8>> =
    redb::TableDefinition::new("pending_proposal_parts");

struct Db {
    db: redb::Database,
    metrics: DbMetrics,
}

impl Db {
    fn new(
        path: impl AsRef<Path>,
        cache_size_bytes: usize,
        metrics: DbMetrics,
    ) -> Result<Self, StoreError> {
        Ok(Self {
            db: redb::Database::builder()
                .set_cache_size(cache_size_bytes)
                .create(path)
                .map_err(StoreError::Database)?,
            metrics,
        })
    }

    fn get_decided_value(&self, height: Height) -> Result<Option<DecidedValue>, StoreError> {
        let start = Instant::now();
        let mut read_bytes = 0;

        let tx = self.db.begin_read()?;

        let value = {
            let table = tx.open_table(DECIDED_VALUES_TABLE)?;
            let value = table.get(&height)?;
            value.and_then(|value| {
                let bytes = value.value();
                read_bytes = bytes.len() as u64;
                Value::from_bytes(&bytes).ok()
            })
        };

        let certificate = {
            let table = tx.open_table(CERTIFICATES_TABLE)?;
            let value = table.get(&height)?;
            value.and_then(|value| {
                let bytes = value.value();
                read_bytes += bytes.len() as u64;
                decode_certificate(&bytes).ok()
            })
        };

        self.metrics.observe_read_time(start.elapsed());
        self.metrics.add_read_bytes(read_bytes);
        self.metrics.add_key_read_bytes(size_of::<Height>() as u64);

        let decided_value = value
            .zip(certificate)
            .map(|(value, certificate)| DecidedValue { value, certificate });

        Ok(decided_value)
    }

    fn insert_decided_value(
        &self,
        decided_value: DecidedValue,
        block_header_bytes: Bytes,
    ) -> Result<(), StoreError> {
        let start = Instant::now();
        let mut write_bytes = 0;

        let height = decided_value.certificate.height;
        let tx = self.db.begin_write()?;

        {
            let mut values = tx.open_table(DECIDED_VALUES_TABLE)?;
            let values_bytes = decided_value.value.to_bytes()?.to_vec();
            write_bytes += values_bytes.len() as u64;
            values.insert(height, values_bytes)?;
        }

        {
            let mut certificates = tx.open_table(CERTIFICATES_TABLE)?;
            let encoded_certificate = encode_certificate(&decided_value.certificate)?;
            write_bytes += encoded_certificate.len() as u64;
            certificates.insert(height, encoded_certificate)?;
        }

        {
            let mut headers = tx.open_table(DECIDED_BLOCK_HEADERS_TABLE)?;
            write_bytes += block_header_bytes.len() as u64;
            headers.insert(height, block_header_bytes.to_vec())?;
        }

        tx.commit()?;

        self.metrics.observe_write_time(start.elapsed());
        self.metrics.add_write_bytes(write_bytes);

        Ok(())
    }

    fn proposal_key(proposal: &ProposedValue<EmeraldContext>) -> (Height, Round, ValueId) {
        (proposal.height, proposal.round, proposal.value.id())
    }

    fn validate_write(
        write: &UndecidedProposalWrite,
    ) -> Result<(Height, Round, ValueId), StoreError> {
        let key = Self::proposal_key(&write.proposal);
        if write.proposal.value.extensions != write.payload {
            return Err(StoreError::Integrity(format!(
                "incoming proposal payload disagrees with embedded value at {key:?}"
            )));
        }

        if let Some(attestation) = &write.attestation {
            let init = &attestation.init;
            if init.height != write.proposal.height
                || init.round != write.proposal.round
                || init.pol_round != write.proposal.valid_round
                || init.proposer != write.proposal.proposer
            {
                return Err(StoreError::Integrity(format!(
                    "incoming proposal attestation disagrees with metadata at {key:?}"
                )));
            }
        }

        Ok(key)
    }

    fn validate_stored_record(
        key: (Height, Round, ValueId),
        proposal: ProposedValue<EmeraldContext>,
        payload: Bytes,
        attestation: Option<ProposalAttestation>,
    ) -> Result<StoredUndecidedProposal, StoreError> {
        if Self::proposal_key(&proposal) != key {
            return Err(StoreError::Integrity(format!(
                "stored proposal key disagrees with metadata at {key:?}"
            )));
        }

        if proposal.value.extensions != payload {
            return Err(StoreError::Integrity(format!(
                "stored proposal payload disagrees with embedded value at {key:?}"
            )));
        }

        if let Some(attestation) = &attestation {
            let init = &attestation.init;
            if init.height != proposal.height
                || init.round != proposal.round
                || init.pol_round != proposal.valid_round
                || init.proposer != proposal.proposer
            {
                return Err(StoreError::Integrity(format!(
                    "stored proposal attestation disagrees with metadata at {key:?}"
                )));
            }
        }

        Ok(StoredUndecidedProposal {
            proposal,
            payload,
            attestation,
        })
    }

    fn compare_write(
        stored: &StoredUndecidedProposal,
        write: &UndecidedProposalWrite,
    ) -> Result<(), UndecidedConflict> {
        if stored.proposal.proposer != write.proposal.proposer {
            return Err(UndecidedConflict {
                field: UndecidedConflictField::Proposer,
                stored: stored.proposal.clone(),
            });
        }

        if stored.proposal.value != write.proposal.value {
            return Err(UndecidedConflict {
                field: UndecidedConflictField::Value,
                stored: stored.proposal.clone(),
            });
        }

        if stored.payload != write.payload {
            return Err(UndecidedConflict {
                field: UndecidedConflictField::Payload,
                stored: stored.proposal.clone(),
            });
        }

        if write.source != UndecidedWriteSource::Sync
            && stored.proposal.valid_round != write.proposal.valid_round
        {
            return Err(UndecidedConflict {
                field: UndecidedConflictField::ValidRound,
                stored: stored.proposal.clone(),
            });
        }

        if let (Some(stored_attestation), Some(incoming_attestation)) =
            (&stored.attestation, &write.attestation)
        {
            if stored_attestation != incoming_attestation {
                return Err(UndecidedConflict {
                    field: UndecidedConflictField::Attestation,
                    stored: stored.proposal.clone(),
                });
            }
        }

        Ok(())
    }

    fn decode_proposal(bytes: &[u8]) -> Result<ProposedValue<EmeraldContext>, StoreError> {
        ProtobufCodec
            .decode(Bytes::copy_from_slice(bytes))
            .map_err(StoreError::Protobuf)
    }

    fn decode_attestation(bytes: &[u8]) -> Result<ProposalAttestation, StoreError> {
        ProtobufCodec
            .decode(Bytes::copy_from_slice(bytes))
            .map_err(StoreError::Protobuf)
    }

    #[tracing::instrument(skip(self))]
    fn get_undecided_record(
        &self,
        height: Height,
        round: Round,
        value_id: ValueId,
    ) -> Result<Option<StoredUndecidedProposal>, StoreError> {
        let start = Instant::now();
        let key = (height, round, value_id);
        let tx = self.db.begin_read()?;

        let payload = {
            let table = tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)?;
            let payload = table
                .get(&key)?
                .map(|value| Bytes::copy_from_slice(&value.value()));
            payload
        };
        let proposal = {
            let table = tx.open_table(UNDECIDED_PROPOSALS_TABLE)?;
            let proposal = table
                .get(&key)?
                .map(|value| Self::decode_proposal(&value.value()))
                .transpose()?;
            proposal
        };
        let attestation = {
            let table = tx.open_table(UNDECIDED_PROPOSAL_ATTESTATIONS_TABLE)?;
            let attestation = table
                .get(&key)?
                .map(|value| Self::decode_attestation(&value.value()))
                .transpose()?;
            attestation
        };

        let record = match (payload, proposal, attestation) {
            (None, None, None) | (Some(_), None, None) => None,
            (Some(payload), Some(proposal), attestation) => Some(Self::validate_stored_record(
                key,
                proposal,
                payload,
                attestation,
            )?),
            _ => {
                return Err(StoreError::Integrity(format!(
                    "invalid undecided proposal record shape for {key:?}"
                )));
            }
        };

        self.metrics.observe_read_time(start.elapsed());
        self.metrics
            .add_key_read_bytes(size_of::<(Height, Round, ValueId)>() as u64);
        Ok(record)
    }

    #[tracing::instrument(skip(self))]
    pub fn get_undecided_proposal(
        &self,
        height: Height,
        round: Round,
        value_id: ValueId,
    ) -> Result<Option<ProposedValue<EmeraldContext>>, StoreError> {
        Ok(self
            .get_undecided_record(height, round, value_id)?
            .map(|record| record.proposal))
    }

    fn get_undecided_proposals(
        &self,
        height: Height,
        round: Round,
    ) -> Result<Vec<ProposedValue<EmeraldContext>>, StoreError> {
        let keys = {
            let tx = self.db.begin_read()?;
            let mut keys = BTreeSet::new();

            for table_definition in [
                UNDECIDED_PROPOSALS_TABLE,
                UNDECIDED_PROPOSAL_ATTESTATIONS_TABLE,
            ] {
                let table = tx.open_table(table_definition)?;
                for result in table.iter()? {
                    let (key, _) = result?;
                    let (stored_height, stored_round, value_id) = key.value();
                    if stored_height == height && stored_round == round {
                        keys.insert((stored_height, stored_round, value_id));
                    }
                }
            }

            keys
        };

        keys.into_iter()
            .map(|(stored_height, stored_round, value_id)| {
                self.get_undecided_record(stored_height, stored_round, value_id)?
                    .ok_or_else(|| {
                        StoreError::Integrity(format!(
                            "undecided proposal metadata has no coherent record at ({stored_height:?}, {stored_round:?}, {value_id:?})"
                        ))
                    })
                    .map(|record| record.proposal)
            })
            .collect()
    }

    fn write_undecided_proposal(
        &self,
        write: UndecidedProposalWrite,
    ) -> Result<UndecidedWriteOutcome, StoreError> {
        let start = Instant::now();
        let key = Self::validate_write(&write)?;
        let proposal_bytes = ProtobufCodec.encode(&write.proposal)?;
        let attestation_bytes = write
            .attestation
            .as_ref()
            .map(|attestation| ProtobufCodec.encode(attestation))
            .transpose()?;
        let tx = self.db.begin_write()?;
        let mut write_bytes = 0;

        let payload = {
            let table = tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)?;
            let payload = table
                .get(&key)?
                .map(|value| Bytes::copy_from_slice(&value.value()));
            payload
        };
        let proposal = {
            let table = tx.open_table(UNDECIDED_PROPOSALS_TABLE)?;
            let proposal = table
                .get(&key)?
                .map(|value| Self::decode_proposal(&value.value()))
                .transpose()?;
            proposal
        };
        let attestation = {
            let table = tx.open_table(UNDECIDED_PROPOSAL_ATTESTATIONS_TABLE)?;
            let attestation = table
                .get(&key)?
                .map(|value| Self::decode_attestation(&value.value()))
                .transpose()?;
            attestation
        };

        let outcome = match (payload, proposal, attestation) {
            (None, None, None) => {
                {
                    let mut table = tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)?;
                    table.insert(key, write.payload.to_vec())?;
                    write_bytes += write.payload.len() as u64;
                }
                {
                    let mut table = tx.open_table(UNDECIDED_PROPOSALS_TABLE)?;
                    table.insert(key, proposal_bytes.to_vec())?;
                    write_bytes += proposal_bytes.len() as u64;
                }
                if let Some(attestation_bytes) = &attestation_bytes {
                    let mut table = tx.open_table(UNDECIDED_PROPOSAL_ATTESTATIONS_TABLE)?;
                    table.insert(key, attestation_bytes.to_vec())?;
                    write_bytes += attestation_bytes.len() as u64;
                }
                UndecidedWriteOutcome::Canonical(write.proposal)
            }
            (Some(orphan), None, None) => {
                if orphan != write.payload {
                    let mut table = tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)?;
                    table.insert(key, write.payload.to_vec())?;
                    write_bytes += write.payload.len() as u64;
                }
                {
                    let mut table = tx.open_table(UNDECIDED_PROPOSALS_TABLE)?;
                    table.insert(key, proposal_bytes.to_vec())?;
                    write_bytes += proposal_bytes.len() as u64;
                }
                if let Some(attestation_bytes) = &attestation_bytes {
                    let mut table = tx.open_table(UNDECIDED_PROPOSAL_ATTESTATIONS_TABLE)?;
                    table.insert(key, attestation_bytes.to_vec())?;
                    write_bytes += attestation_bytes.len() as u64;
                }
                UndecidedWriteOutcome::Canonical(write.proposal)
            }
            (Some(payload), Some(proposal), attestation) => {
                let stored = Self::validate_stored_record(key, proposal, payload, attestation)?;
                if let Err(conflict) = Self::compare_write(&stored, &write) {
                    UndecidedWriteOutcome::Conflict(conflict)
                } else {
                    if stored.attestation.is_none() {
                        if let Some(attestation_bytes) = &attestation_bytes {
                            let mut table = tx.open_table(UNDECIDED_PROPOSAL_ATTESTATIONS_TABLE)?;
                            table.insert(key, attestation_bytes.to_vec())?;
                            write_bytes += attestation_bytes.len() as u64;
                        }
                    }
                    UndecidedWriteOutcome::Canonical(stored.proposal)
                }
            }
            _ => {
                return Err(StoreError::Integrity(format!(
                    "invalid undecided proposal record shape for {key:?}"
                )));
            }
        };

        tx.commit()?;
        self.metrics.observe_write_time(start.elapsed());
        self.metrics.add_write_bytes(write_bytes);
        Ok(outcome)
    }

    #[cfg(test)]
    fn insert_undecided_proposal(
        &self,
        proposal: ProposedValue<EmeraldContext>,
    ) -> Result<(), StoreError> {
        let key = Self::proposal_key(&proposal);
        let value = ProtobufCodec.encode(&proposal)?;
        let tx = self.db.begin_write()?;
        let mut table = tx.open_table(UNDECIDED_PROPOSALS_TABLE)?;
        if table.get(&key)?.is_none() {
            table.insert(key, value.to_vec())?;
        }
        drop(table);
        tx.commit()?;
        Ok(())
    }

    fn get_pending_proposal_parts(
        &self,
        height: Height,
        round: Round,
    ) -> Result<Vec<ProposalParts>, StoreError> {
        let start = Instant::now();
        let mut read_bytes = 0;

        let tx = self.db.begin_read()?;
        let table = tx.open_table(PENDING_PROPOSAL_PARTS_TABLE)?;

        let mut proposals = Vec::new();
        for result in table.iter()? {
            let (key, value) = result?;
            let (h, r, _) = key.value();

            if h == height && r == round {
                let bytes = value.value();
                read_bytes += bytes.len() as u64;

                let parts: ProposalParts = serde_json::from_slice(&bytes)?;

                proposals.push(parts);
            }
        }

        self.metrics.observe_read_time(start.elapsed());
        self.metrics.add_read_bytes(read_bytes);
        self.metrics.add_key_read_bytes(
            size_of::<(Height, Round, ValueId)>() as u64 * proposals.len() as u64,
        );

        Ok(proposals)
    }

    fn remove_pending_proposal_parts(&self, parts: ProposalParts) -> Result<(), StoreError> {
        let key = (
            parts.height,
            parts.round,
            Self::generate_value_id_from_parts(&parts),
        );
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(PENDING_PROPOSAL_PARTS_TABLE)?;
            table.remove(key)?;
        }
        tx.commit()?;
        Ok(())
    }

    fn insert_pending_proposal_parts(&self, parts: ProposalParts) -> Result<(), StoreError> {
        let start = Instant::now();
        let key = (
            parts.height,
            parts.round,
            Self::generate_value_id_from_parts(&parts),
        );
        let value = serde_json::to_vec(&parts)?;

        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(PENDING_PROPOSAL_PARTS_TABLE)?;
            table.insert(key, value.clone())?;
        }
        tx.commit()?;

        self.metrics.observe_write_time(start.elapsed());
        self.metrics.add_write_bytes(value.len() as u64);

        Ok(())
    }

    // fn height_range<Table>(
    //     &self,
    //     table: &Table,
    //     range: impl RangeBounds<Height>,
    // ) -> Result<Vec<Height>, StoreError>
    // where
    //     Table: redb::ReadableTable<HeightKey, Vec<u8>>,
    // {
    //     Ok(table
    //         .range(range)?
    //         .flatten()
    //         .map(|(key, _)| key.value())
    //         .collect::<Vec<_>>())
    // }

    // Helper method to generate a unique ValueId from proposal parts
    pub fn generate_value_id_from_parts(parts: &ProposalParts) -> ValueId {
        use sha3::{Digest, Keccak256};

        let mut hasher = Keccak256::new();

        // Hash height, round, and proposer
        hasher.update(parts.height.as_u64().to_be_bytes());
        hasher.update(parts.round.as_i64().to_be_bytes());
        hasher.update(parts.proposer.into_inner());

        // Hash all the proposal parts content
        for part in &parts.parts {
            if let Some(data) = part.as_data() {
                hasher.update(data.bytes.as_ref());
            }
        }

        // In the generate_value_id_from_parts method:
        let hash = hasher.finalize();

        // Use first 8 bytes of hash to create ValueId
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&hash[..8]);
        ValueId::new(u64::from_be_bytes(bytes))
    }

    // All values except certificates can be retrieved from Reth (if the node has not been pruned)
    // But if we prune certificates, other nodes will not be able to catchup.
    // Returns the new earliest heights after pruning.
    fn prune(
        &self,
        num_certificates_to_retain: u64,
        num_temp_blocks_retained: u64,
        curr_height: Height,
        prune_certificates: bool,
    ) -> Result<PruneResult, StoreError> {
        let start = Instant::now();

        let tx = self.db.begin_write().unwrap();

        let mut result = PruneResult::default();

        if curr_height > Height::new(num_temp_blocks_retained) {
            // Compute actual height until which we will retain temporary data
            let block_data_retain_height = Height::new(
                curr_height
                    .as_u64()
                    .saturating_sub(num_temp_blocks_retained),
            );

            // Remove all undecided proposals with height < retain_height
            let mut undecided = tx.open_table(UNDECIDED_PROPOSALS_TABLE)?;
            undecided.retain(|k, _| k.0 >= block_data_retain_height)?;

            // Remove all undecided block data with height < retain_height
            let mut undecided_block_data = tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)?;
            undecided_block_data.retain(|k, _| k.0 >= block_data_retain_height)?;

            let mut undecided_attestations =
                tx.open_table(UNDECIDED_PROPOSAL_ATTESTATIONS_TABLE)?;
            undecided_attestations.retain(|k, _| k.0 >= block_data_retain_height)?;

            // Remove all pending proposal parts with height < retain_height
            let mut pending = tx.open_table(PENDING_PROPOSAL_PARTS_TABLE)?;
            pending.retain(|k, _| k.0 >= block_data_retain_height)?;

            // Remove all decided values with height < retain_height
            let mut decided = tx.open_table(DECIDED_VALUES_TABLE)?;
            decided.retain(|k, _| k >= block_data_retain_height)?;

            // Remove all decided block data with height < retain_height
            let mut decided_block_data = tx.open_table(DECIDED_BLOCK_DATA_TABLE)?;
            decided_block_data.retain(|k, _| k >= block_data_retain_height)?;

            result.earliest_value_height = Some(block_data_retain_height);
        }

        if prune_certificates {
            // This will compute the retain height for the certificates which is based on the
            // retain height set in the config.
            // The intermediary block data stored for Consensus is pruned at every height after
            // num_temp_blocks_retained
            let certificate_retain_height = Height::new(
                curr_height
                    .as_u64()
                    .saturating_sub(num_certificates_to_retain),
            );
            // We prune certificates only if pruning is set.
            let mut certificate_data = tx.open_table(CERTIFICATES_TABLE)?;
            certificate_data.retain(|k, _| k >= certificate_retain_height)?;

            // Prune block headers along with certificates since they are retrieved together
            let mut block_headers = tx.open_table(DECIDED_BLOCK_HEADERS_TABLE)?;
            block_headers.retain(|k, _| k >= certificate_retain_height)?;

            result.earliest_certificate_height = Some(certificate_retain_height);
        }

        tx.commit()?;

        self.metrics.observe_delete_time(start.elapsed());

        Ok(result)
    }

    fn min_decided_value_height(&self) -> Option<Height> {
        let start = Instant::now();

        let tx = self.db.begin_read().unwrap();
        let table = tx.open_table(CERTIFICATES_TABLE).unwrap();
        let (key, value) = table.first().ok()??;

        self.metrics.observe_read_time(start.elapsed());
        self.metrics.add_read_bytes(value.value().len() as u64);
        self.metrics.add_key_read_bytes(size_of::<Height>() as u64);

        Some(key.value())
    }

    fn min_unpruned_decided_value_height(&self) -> Option<Height> {
        let start = Instant::now();

        let tx = self.db.begin_read().expect("failed to open db for reading");
        let table = tx
            .open_table(DECIDED_VALUES_TABLE)
            .expect("failed to open DECIDED_VALUES_TABLE");
        let (key, value) = table.first().ok()??;

        self.metrics.observe_read_time(start.elapsed());
        self.metrics.add_read_bytes(value.value().len() as u64);
        self.metrics.add_key_read_bytes(size_of::<Height>() as u64);

        Some(key.value())
    }

    fn max_decided_value_height(&self) -> Option<Height> {
        let tx = self
            .db
            .begin_read()
            .expect("failed for open db for reading");
        let table = tx
            .open_table(DECIDED_VALUES_TABLE)
            .expect("failed to open DECIDED_VALUES_TABLE");
        let (key, _) = table.last().ok()??;
        Some(key.value())
    }

    fn create_tables(&self) -> Result<(), StoreError> {
        let tx = self.db.begin_write()?;

        // Implicitly creates the tables if they do not exist yet
        let _ = tx.open_table(DECIDED_VALUES_TABLE)?;
        let _ = tx.open_table(CERTIFICATES_TABLE)?;
        let _ = tx.open_table(UNDECIDED_PROPOSALS_TABLE)?;
        let _ = tx.open_table(DECIDED_BLOCK_DATA_TABLE)?;
        let _ = tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)?;
        let _ = tx.open_table(UNDECIDED_PROPOSAL_ATTESTATIONS_TABLE)?;
        let _ = tx.open_table(DECIDED_BLOCK_HEADERS_TABLE)?;
        let _ = tx.open_table(PERSISTENT_METRICS_TABLE)?;
        let _ = tx.open_table(PENDING_PROPOSAL_PARTS_TABLE)?;

        tx.commit()?;

        Ok(())
    }

    fn insert_cumulative_metrics(
        &self,
        txs_count: u64,
        chain_bytes: u64,
        elapsed_seconds: u64,
    ) -> Result<(), StoreError> {
        let start = Instant::now();
        let write_bytes = (size_of::<u64>() * 3) as u64;

        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(PERSISTENT_METRICS_TABLE)?;
            table.insert("txs_count", txs_count)?;
            table.insert("chain_bytes", chain_bytes)?;
            table.insert("elapsed_seconds", elapsed_seconds)?;
        }
        tx.commit()?;

        self.metrics.observe_write_time(start.elapsed());
        self.metrics.add_write_bytes(write_bytes);

        Ok(())
    }

    fn get_cumulative_metrics(&self) -> Result<Option<(u64, u64, u64)>, StoreError> {
        let start = Instant::now();
        let mut read_bytes = 0;

        let tx = self.db.begin_read()?;
        let table = tx.open_table(PERSISTENT_METRICS_TABLE)?;

        let txs_count = table.get("txs_count")?.map(|v| {
            read_bytes += size_of::<u64>() as u64;
            v.value()
        });

        let chain_bytes = table.get("chain_bytes")?.map(|v| {
            read_bytes += size_of::<u64>() as u64;
            v.value()
        });

        let elapsed_seconds = table.get("elapsed_seconds")?.map(|v| {
            read_bytes += size_of::<u64>() as u64;
            v.value()
        });

        self.metrics.observe_read_time(start.elapsed());
        self.metrics.add_read_bytes(read_bytes);
        self.metrics.add_key_read_bytes(
            ("txs_count".len() + "chain_bytes".len() + "elapsed_seconds".len()) as u64,
        );

        Ok(txs_count
            .zip(chain_bytes)
            .and_then(|(t, c)| elapsed_seconds.map(|e| (t, c, e))))
    }

    fn get_block_data(
        &self,
        height: Height,
        round: Round,
        value_id: ValueId,
    ) -> Result<Option<Bytes>, StoreError> {
        let start = Instant::now();

        let tx = self.db.begin_read()?;

        // Try undecided block data first
        let undecided_table = tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)?;
        if let Some(data) = undecided_table.get(&(height, round, value_id))? {
            let bytes = data.value();
            let read_bytes = bytes.len() as u64;
            self.metrics.observe_read_time(start.elapsed());
            self.metrics.add_read_bytes(read_bytes);
            self.metrics.add_key_read_bytes(
                (size_of::<Height>() + size_of::<Round>() + size_of::<ValueId>()) as u64,
            );
            return Ok(Some(Bytes::copy_from_slice(&bytes)));
        }

        // Then try decided block data
        let decided_table = tx.open_table(DECIDED_BLOCK_DATA_TABLE)?;
        if let Some(data) = decided_table.get(&height)? {
            let bytes = data.value();
            let read_bytes = bytes.len() as u64;
            self.metrics.observe_read_time(start.elapsed());
            self.metrics.add_read_bytes(read_bytes);
            self.metrics.add_key_read_bytes(size_of::<Height>() as u64);
            return Ok(Some(Bytes::copy_from_slice(&bytes)));
        }

        self.metrics.observe_read_time(start.elapsed());
        Ok(None)
    }

    fn insert_decided_block_data(&self, height: Height, data: Bytes) -> Result<(), StoreError> {
        let start = Instant::now();
        let write_bytes = data.len() as u64;

        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(DECIDED_BLOCK_DATA_TABLE)?;
            // Only insert if no value exists at this key
            if table.get(&height)?.is_none() {
                table.insert(height, data.to_vec())?;
            }
        }
        tx.commit()?;

        self.metrics.observe_write_time(start.elapsed());
        self.metrics.add_write_bytes(write_bytes);

        Ok(())
    }

    fn get_certificate_and_header(
        &self,
        height: Height,
    ) -> Result<Option<(CommitCertificate<EmeraldContext>, Bytes)>, StoreError> {
        let start = Instant::now();
        let mut read_bytes = 0;

        let tx = self.db.begin_read()?;

        let certificate = {
            let table = tx.open_table(CERTIFICATES_TABLE)?;
            table.get(&height)?.and_then(|v| {
                let bytes = v.value();
                read_bytes += bytes.len() as u64;
                decode_certificate(&bytes).ok()
            })
        };

        let header = {
            let table = tx.open_table(DECIDED_BLOCK_HEADERS_TABLE)?;
            table.get(&height)?.map(|v| {
                let bytes = v.value();
                read_bytes += bytes.len() as u64;
                Bytes::copy_from_slice(&bytes)
            })
        };

        self.metrics.observe_read_time(start.elapsed());
        self.metrics.add_read_bytes(read_bytes);
        self.metrics.add_key_read_bytes(size_of::<Height>() as u64);

        Ok(certificate.zip(header))
    }
}

#[derive(Clone)]
pub struct Store {
    db: Arc<Db>,
}

impl Store {
    /// Opens a new store at the given path with the provided metrics.
    /// Called by the application when initializing the store.
    pub async fn open(
        path: impl AsRef<Path>,
        cache_size_bytes: usize,
        metrics: DbMetrics,
    ) -> Result<Self, StoreError> {
        let path = path.as_ref().to_owned();

        tokio::task::spawn_blocking(move || {
            let db = Db::new(path, cache_size_bytes, metrics)?;
            db.create_tables()?;
            Ok(Self { db: Arc::new(db) })
        })
        .await?
    }

    /// Returns the minimum height of decided values in the store.
    /// Called by the application to determine the earliest available height.
    pub async fn min_decided_value_height(&self) -> Option<Height> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.min_decided_value_height())
            .await
            .ok()
            .flatten()
    }

    pub async fn min_unpruned_decided_value_height(&self) -> Option<Height> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.min_unpruned_decided_value_height())
            .await
            .ok()
            .flatten()
    }

    pub async fn max_decided_value_height(&self) -> Option<Height> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.max_decided_value_height())
            .await
            .ok()
            .flatten()
    }

    /// Retrieves a decided value for the given height.
    /// Called by the application when a syncing peer is asking for a decided value.
    pub async fn get_decided_value(
        &self,
        height: Height,
    ) -> Result<Option<DecidedValue>, StoreError> {
        let db = Arc::clone(&self.db);

        tokio::task::spawn_blocking(move || db.get_decided_value(height)).await?
    }

    /// Stores a decided value with its certificate.
    /// Called by the application when it `commit`s a value decided by consensus.
    pub async fn store_decided_value(
        &self,
        certificate: &CommitCertificate<EmeraldContext>,
        value: Value,
        block_header_bytes: Bytes,
    ) -> Result<(), StoreError> {
        let decided_value = DecidedValue {
            value,
            certificate: certificate.clone(),
        };

        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || {
            db.insert_decided_value(decided_value, block_header_bytes)
        })
        .await?
    }

    /// Stores an undecided proposal.
    /// Called by the application when receiving new proposals from peers.
    #[cfg(test)]
    pub(crate) async fn store_undecided_proposal(
        &self,
        value: ProposedValue<EmeraldContext>,
    ) -> Result<(), StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.insert_undecided_proposal(value)).await?
    }

    pub async fn write_undecided_proposal(
        &self,
        write: UndecidedProposalWrite,
    ) -> Result<UndecidedWriteOutcome, StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.write_undecided_proposal(write)).await?
    }

    pub async fn get_undecided_record(
        &self,
        height: Height,
        round: Round,
        value_id: ValueId,
    ) -> Result<Option<StoredUndecidedProposal>, StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.get_undecided_record(height, round, value_id))
            .await?
    }

    /// Retrieves a specific undecided proposal by height, round, and value ID.
    /// Called by the application when consensus asks for a specific proposal to restream.
    pub async fn get_undecided_proposal(
        &self,
        height: Height,
        round: Round,
        value_id: ValueId,
    ) -> Result<Option<ProposedValue<EmeraldContext>>, StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.get_undecided_proposal(height, round, value_id))
            .await?
    }

    /// Retrieves all undecided proposals for a given height and round.
    /// Called by the application when starting a new round and existing proposals need to be replayed.
    pub async fn get_undecided_proposals(
        &self,
        height: Height,
        round: Round,
    ) -> Result<Vec<ProposedValue<EmeraldContext>>, StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.get_undecided_proposals(height, round)).await?
    }

    /// Stores a pending proposal parts.
    /// Called by the application when receiving new proposals from peers.
    pub async fn store_pending_proposal_parts(
        &self,
        value: ProposalParts,
    ) -> Result<(), StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.insert_pending_proposal_parts(value)).await?
    }

    /// Retrieves all pendingproposal parts for a given height and round.
    /// Called by the application when starting a new round and existing proposals need to be replayed.
    pub async fn get_pending_proposal_parts(
        &self,
        height: Height,
        round: Round,
    ) -> Result<Vec<ProposalParts>, StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.get_pending_proposal_parts(height, round)).await?
    }

    /// Removes a pending proposal parts.
    /// Called by the application when a proposal is no longer valid.
    pub async fn remove_pending_proposal_parts(
        &self,
        value: ProposalParts,
    ) -> Result<(), StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.remove_pending_proposal_parts(value)).await?
    }

    /// Prunes the store by removing all undecided proposals and decided values up to the retain height.
    /// Called by the application to clean up old data and free up space. This is done when a new value is committed.
    /// If state.max_retain_height is set to something else than u64::MAX, this function also prunes certificates.
    /// Pruned certificates cannot be retrieved later on.
    /// Returns the new earliest heights after pruning.
    pub async fn prune(
        &self,
        num_certificates_to_retain: u64,
        num_temp_blocks_retained: u64,
        curr_height: Height,
        prune_certificates: bool,
    ) -> Result<PruneResult, StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || {
            db.prune(
                num_certificates_to_retain,
                num_temp_blocks_retained,
                curr_height,
                prune_certificates,
            )
        })
        .await?
    }

    pub async fn get_block_data(
        &self,
        height: Height,
        round: Round,
        value_id: ValueId,
    ) -> Result<Option<Bytes>, StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.get_block_data(height, round, value_id)).await?
    }

    pub async fn store_decided_block_data(
        &self,
        height: Height,
        data: Bytes,
    ) -> Result<(), StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.insert_decided_block_data(height, data)).await?
    }

    pub async fn get_certificate_and_header(
        &self,
        height: Height,
    ) -> Result<Option<(CommitCertificate<EmeraldContext>, Bytes)>, StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.get_certificate_and_header(height)).await?
    }

    pub async fn store_cumulative_metrics(
        &self,
        txs_count: u64,
        chain_bytes: u64,
        elapsed_seconds: u64,
    ) -> Result<(), StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || {
            db.insert_cumulative_metrics(txs_count, chain_bytes, elapsed_seconds)
        })
        .await?
    }

    pub async fn load_cumulative_metrics(&self) -> Result<Option<(u64, u64, u64)>, StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.get_cumulative_metrics()).await?
    }

    /// Retrieves a decided value encoded as a RawDecidedValue for the given height.
    /// Returns None if no decided value exists at the given height.
    pub async fn get_raw_decided_value(
        &self,
        height: Height,
    ) -> eyre::Result<Option<RawDecidedValue<EmeraldContext>>> {
        self.get_decided_value(height)
            .await?
            .map(|decided_value| {
                Ok(RawDecidedValue {
                    certificate: decided_value.certificate,
                    value_bytes: ProtobufCodec.encode(&decided_value.value)?,
                })
            })
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use malachitebft_app_channel::app::types::codec::Codec;
    use malachitebft_app_channel::app::types::core::{CommitCertificate, Validity};
    use malachitebft_eth_types::secp256k1::PrivateKey;
    use malachitebft_eth_types::{Address, ProposalAttestation, ProposalFin, ProposalInit};

    use super::*;

    /// Create a test database backed by a temporary directory.
    /// Returns both the Db and the TempDir (must be kept alive for the DB to remain valid).
    fn create_test_db(name: &str) -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::new(
            dir.path().join(format!("{name}.redb")),
            1024 * 1024,
            DbMetrics::new(),
        )
        .unwrap();
        db.create_tables().unwrap();
        (db, dir)
    }

    /// Build a DecidedValue (Value + CommitCertificate) and a block header for a given height.
    fn make_decided_value(height: u64) -> (DecidedValue, Bytes) {
        let value = Value::new(Bytes::from(vec![height as u8; 10]));
        let certificate = CommitCertificate {
            height: Height::new(height),
            round: Round::new(0),
            value_id: value.id(),
            commit_signatures: vec![],
        };
        let block_header = Bytes::from(vec![height as u8; 20]);
        (DecidedValue { value, certificate }, block_header)
    }

    fn make_attested_write(height: u64) -> UndecidedProposalWrite {
        let key = PrivateKey::from_slice(&[height as u8; 32]).unwrap();
        let payload = Bytes::from(vec![height as u8; 10]);
        let proposal = ProposedValue {
            height: Height::new(height),
            round: Round::new(0),
            valid_round: Round::Nil,
            proposer: Address::from_public_key(&key.public_key()),
            value: Value::new(payload.clone()),
            validity: Validity::Valid,
        };

        UndecidedProposalWrite {
            payload,
            attestation: Some(ProposalAttestation::new(
                ProposalInit::new(
                    proposal.height,
                    proposal.round,
                    proposal.valid_round,
                    proposal.proposer,
                ),
                ProposalFin::new(key.sign(b"store-attestation")),
            )),
            proposal,
            source: UndecidedWriteSource::Proposal,
        }
    }

    fn with_valid_round(
        mut write: UndecidedProposalWrite,
        valid_round: Round,
    ) -> UndecidedProposalWrite {
        write.proposal.valid_round = valid_round;
        if let Some(attestation) = &mut write.attestation {
            attestation.init.pol_round = valid_round;
        }
        write
    }

    fn as_unattested(
        mut write: UndecidedProposalWrite,
        source: UndecidedWriteSource,
    ) -> UndecidedProposalWrite {
        write.attestation = None;
        write.source = source;
        write
    }

    #[test]
    fn undecided_write_empty_attested_stores_coherent_aggregate() {
        let (db, _dir) = create_test_db("empty_attested");
        let write = make_attested_write(42);
        let expected = write.proposal.clone();

        let outcome = db.write_undecided_proposal(write).unwrap();
        assert_eq!(outcome, UndecidedWriteOutcome::Canonical(expected.clone()));

        let stored = db
            .get_undecided_record(expected.height, expected.round, expected.value.id())
            .unwrap()
            .expect("attested write must create a record");
        assert_eq!(stored.proposal, expected);
        assert_eq!(stored.payload, stored.proposal.value.extensions);
        assert!(stored.attestation.is_some());
    }

    #[test]
    fn undecided_proposal_list_rejects_attestation_without_metadata() {
        let (db, _dir) = create_test_db("attestation_without_metadata");
        let write = make_attested_write(43);
        let key = Db::proposal_key(&write.proposal);
        let attestation = ProtobufCodec
            .encode(write.attestation.as_ref().unwrap())
            .unwrap();

        let tx = db.db.begin_write().unwrap();
        {
            let mut table = tx
                .open_table(UNDECIDED_PROPOSAL_ATTESTATIONS_TABLE)
                .unwrap();
            table.insert(key, attestation.to_vec()).unwrap();
        }
        tx.commit().unwrap();

        let error = db
            .get_undecided_proposals(write.proposal.height, write.proposal.round)
            .unwrap_err();
        assert!(matches!(error, StoreError::Integrity(_)));
    }

    #[test]
    fn undecided_write_backfills_matching_attestation() {
        let (db, _dir) = create_test_db("backfill_attestation");
        let attested = make_attested_write(44);
        let proposal = attested.proposal.clone();

        assert_eq!(
            db.write_undecided_proposal(as_unattested(
                attested.clone(),
                UndecidedWriteSource::Proposal,
            ))
            .unwrap(),
            UndecidedWriteOutcome::Canonical(proposal.clone())
        );
        assert_eq!(
            db.write_undecided_proposal(attested).unwrap(),
            UndecidedWriteOutcome::Canonical(proposal.clone())
        );

        let stored = db
            .get_undecided_record(proposal.height, proposal.round, proposal.value.id())
            .unwrap()
            .unwrap();
        assert!(stored.attestation.is_some());
    }

    #[test]
    fn undecided_sync_preserves_stored_valid_round() {
        let (db, _dir) = create_test_db("sync_preserves_valid_round");
        let stored_write = with_valid_round(make_attested_write(45), Round::new(3));
        let stored_proposal = stored_write.proposal.clone();
        db.write_undecided_proposal(stored_write).unwrap();

        let sync_write = as_unattested(
            with_valid_round(make_attested_write(45), Round::Nil),
            UndecidedWriteSource::Sync,
        );
        assert_eq!(
            db.write_undecided_proposal(sync_write).unwrap(),
            UndecidedWriteOutcome::Canonical(stored_proposal)
        );
    }

    #[test]
    fn undecided_write_replaces_colliding_orphan_payload() {
        let (db, _dir) = create_test_db("replace_orphan");
        let write = make_attested_write(46);
        let key = Db::proposal_key(&write.proposal);
        let orphan = Bytes::from_static(b"orphan-payload");

        let tx = db.db.begin_write().unwrap();
        {
            let mut table = tx.open_table(UNDECIDED_BLOCK_DATA_TABLE).unwrap();
            table.insert(key, orphan.to_vec()).unwrap();
        }
        tx.commit().unwrap();

        db.write_undecided_proposal(write.clone()).unwrap();
        let stored = db
            .get_undecided_record(key.0, key.1, key.2)
            .unwrap()
            .unwrap();
        assert_eq!(stored.payload, write.payload);
    }

    #[test]
    fn undecided_write_rejects_incoherent_input_without_writing() {
        let (db, _dir) = create_test_db("incoherent_input");
        let mut write = make_attested_write(47);
        write.payload = Bytes::from_static(b"different-payload");
        let key = Db::proposal_key(&write.proposal);

        assert!(matches!(
            db.write_undecided_proposal(write),
            Err(StoreError::Integrity(_))
        ));
        assert!(db
            .get_undecided_record(key.0, key.1, key.2)
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_prune() {
        let (db, _dir) = create_test_db("prune_test");

        // --- Populate all tables at heights 1, 2, 3 ---
        for h in 1..=4u64 {
            // Decided values table + certificates table + block headers table
            let (decided, header) = make_decided_value(h);
            db.insert_decided_value(decided, header).unwrap();

            // Decided block data table
            db.insert_decided_block_data(Height::new(h), Bytes::from(vec![h as u8; 30]))
                .unwrap();

            let proposal = make_attested_write(h);
            db.write_undecided_proposal(proposal).unwrap();
        }

        // Verify all data is present before pruning
        for h in 1..=4u64 {
            assert!(
                db.get_decided_value(Height::new(h)).unwrap().is_some(),
                "decided value at height {h} should exist before pruning"
            );
            assert!(
                db.get_certificate_and_header(Height::new(h))
                    .unwrap()
                    .is_some(),
                "certificate at height {h} should exist before pruning"
            );
            assert!(
                db.get_block_data(
                    Height::new(h),
                    Round::new(0),
                    Value::new(Bytes::from(vec![h as u8; 10])).id(),
                )
                .unwrap()
                .is_some(),
                "block data at height {h} should exist before pruning"
            );
        }

        // --- Prune ---
        //
        // Parameters:
        //   num_certificates_to_retain = 2
        //   num_temp_blocks_retained   = 1
        //   curr_height                = 4
        //   prune_certificates         = true
        //
        // Computed retain heights:
        //   block_data_retain_height    = 4 - 1 = 3  →  keep heights >= 3
        //   certificate_retain_height   = 4 - 2 = 2  →  keep heights >= 2
        let result = db.prune(2, 1, Height::new(4), true).unwrap();

        // Verify returned PruneResult
        assert_eq!(
            result.earliest_certificate_height,
            Some(Height::new(2)),
            "earliest_certificate_height should be 4 - 2 = 2"
        );
        assert_eq!(
            result.earliest_value_height,
            Some(Height::new(3)),
            "earliest_value_height should be 4 - 1 = 3"
        );

        // === Certificates and block headers (certificate_retain_height = 2) ===
        // Block headers are pruned at the same height as certificates.
        // get_certificate_and_header returns Some only if BOTH exist.
        assert!(
            db.get_certificate_and_header(Height::new(4))
                .unwrap()
                .is_some(),
            "certificate and header at height 4 should survive"
        );
        assert!(
            db.get_certificate_and_header(Height::new(3))
                .unwrap()
                .is_some(),
            "certificate and header at height 3 should survive"
        );
        assert!(
            db.get_certificate_and_header(Height::new(2))
                .unwrap()
                .is_some(),
            "certificate and header at height 2 should survive (>= retain height)"
        );
        // Height 1 < retain height 2, so both certificate and header are pruned
        assert!(
            db.get_certificate_and_header(Height::new(1))
                .unwrap()
                .is_none(),
            "certificate and header at height 1 should be pruned (< retain height 2)"
        );

        // === Decided block data (retain height = 3, heights > 2 survive) ===
        // Use a dummy round/value_id — decided block data is keyed by height only
        let r = Round::new(0);
        let vid = ValueId::new(0);
        assert!(
            db.get_block_data(Height::new(3), r, vid).unwrap().is_some(),
            "decided block data at height 3 should survive"
        );
        assert!(
            db.get_block_data(Height::new(2), r, vid).unwrap().is_none(),
            "decided block data at height 2 should not survive (retain height = 3)"
        );
        assert!(
            db.get_block_data(Height::new(1), r, vid).unwrap().is_none(),
            "decided block data at height 1 should be pruned"
        );

        // === Decided values (retain height = 3, heights > 2 survive) ===
        assert!(
            db.get_decided_value(Height::new(3)).unwrap().is_some(),
            "decided value at height 3 should survive"
        );
        assert!(
            db.get_decided_value(Height::new(2)).unwrap().is_none(),
            "decided value at height 2 should not survive"
        );
        // Decided value at height 1: the value is pruned from DECIDED_VALUES_TABLE,
        // but the certificate still exists. get_decided_value zips both, so returns None.
        assert!(
            db.get_decided_value(Height::new(1)).unwrap().is_none(),
            "decided value at height 1 should be pruned"
        );

        // === Undecided block data (retain height = 3, heights > 2 survive) ===
        assert!(
            db.get_block_data(
                Height::new(3),
                Round::new(0),
                Value::new(Bytes::from(vec![3_u8; 10])).id(),
            )
            .unwrap()
            .is_some(),
            "undecided block data at height 3 should survive"
        );
        assert!(
            db.get_block_data(
                Height::new(2),
                Round::new(0),
                Value::new(Bytes::from(vec![2_u8; 10])).id(),
            )
            .unwrap()
            .is_none(),
            "undecided block data at height 2 should be pruned"
        );
        assert!(
            db.get_block_data(
                Height::new(1),
                Round::new(0),
                Value::new(Bytes::from(vec![1_u8; 10])).id(),
            )
            .unwrap()
            .is_none(),
            "undecided block data at height 1 should be pruned"
        );

        // === Undecided proposals (retain height = 3, heights > 2 survive) ===
        assert!(
            !db.get_undecided_proposals(Height::new(3), Round::new(0))
                .unwrap()
                .is_empty(),
            "undecided proposals at height 3 should survive"
        );
        assert!(
            db.get_undecided_proposals(Height::new(2), Round::new(0))
                .unwrap()
                .is_empty(),
            "undecided proposals at height 2 should be pruned"
        );
        assert!(
            db.get_undecided_proposals(Height::new(1), Round::new(0))
                .unwrap()
                .is_empty(),
            "undecided proposals at height 1 should be pruned"
        );
    }

    #[test]
    fn test_prune_without_certificates() {
        let (db, _dir) = create_test_db("prune_no_certs_test");

        // Populate data at heights 1-4
        for h in 1..=4u64 {
            let (decided, header) = make_decided_value(h);
            db.insert_decided_value(decided, header).unwrap();
            db.insert_decided_block_data(Height::new(h), Bytes::from(vec![h as u8; 30]))
                .unwrap();
        }

        // Prune with prune_certificates = false
        let result = db.prune(2, 1, Height::new(4), false).unwrap();

        // Should not return certificate height when not pruning certificates
        assert_eq!(
            result.earliest_certificate_height, None,
            "earliest_certificate_height should be None when prune_certificates is false"
        );
        // Should still return value height
        assert_eq!(
            result.earliest_value_height,
            Some(Height::new(3)),
            "earliest_value_height should be 4 - 1 = 3"
        );

        // Certificates and block headers should all still exist.
        // Block headers are only pruned when prune_certificates = true.
        // get_certificate_and_header returns Some only if BOTH exist.
        for h in 1..=4u64 {
            assert!(
                db.get_certificate_and_header(Height::new(h))
                    .unwrap()
                    .is_some(),
                "certificate and header at height {h} should still exist when prune_certificates = false"
            );
        }
    }

    #[test]
    fn test_prune_no_op_low_height() {
        let (db, _dir) = create_test_db("prune_no_op_test");

        // Populate data at height 1
        let (decided, header) = make_decided_value(1);
        db.insert_decided_value(decided, header).unwrap();

        // Prune with curr_height <= num_temp_blocks_retained (no value pruning)
        // and prune_certificates = false (no cert pruning)
        let result = db.prune(10, 10, Height::new(1), false).unwrap();

        // Neither height should be returned
        assert_eq!(
            result.earliest_certificate_height, None,
            "earliest_certificate_height should be None"
        );
        assert_eq!(
            result.earliest_value_height, None,
            "earliest_value_height should be None when curr_height <= num_temp_blocks_retained"
        );

        // Certificate and header should still exist (no pruning occurred)
        assert!(
            db.get_certificate_and_header(Height::new(1))
                .unwrap()
                .is_some(),
            "certificate and header at height 1 should still exist"
        );
    }

    #[test]
    fn test_prune_result_saturating_sub() {
        let (db, _dir) = create_test_db("prune_saturating_test");

        // Populate data at height 1
        let (decided, header) = make_decided_value(1);
        db.insert_decided_value(decided, header).unwrap();

        // Prune with values that would underflow without saturating_sub
        // curr_height = 2, num_certificates_to_retain = 100
        // certificate_retain_height = 2 - 100 = 0 (saturated)
        let result = db.prune(100, 1, Height::new(2), true).unwrap();

        assert_eq!(
            result.earliest_certificate_height,
            Some(Height::new(0)),
            "earliest_certificate_height should saturate to 0"
        );
        assert_eq!(
            result.earliest_value_height,
            Some(Height::new(1)),
            "earliest_value_height should be 2 - 1 = 1"
        );

        // Certificate and header at height 1 should survive (retain_height = 0, so height >= 0 survives)
        assert!(
            db.get_certificate_and_header(Height::new(1))
                .unwrap()
                .is_some(),
            "certificate and header at height 1 should survive with retain_height = 0"
        );
    }
}
