#![allow(clippy::result_large_err)]

use core::mem::size_of;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use alloy_rpc_types_engine::ExecutionPayloadV3;
use bytes::Bytes;
use color_eyre::eyre;
use malachitebft_app_channel::app::types::codec::Codec;
use malachitebft_app_channel::app::types::core::{CommitCertificate, Round};
use malachitebft_app_channel::app::types::sync::RawDecidedValue;
use malachitebft_app_channel::app::types::ProposedValue;
use malachitebft_eth_types::codec::proto as codec;
use malachitebft_eth_types::codec::proto::ProtobufCodec;
use malachitebft_eth_types::{proto, EmeraldContext, Height, Value, ValueId};
use malachitebft_proto::{Error as ProtoError, Protobuf};
use prost::Message;
#[cfg(test)]
use redb::{ReadableTableMetadata, TableHandle};
use redb::ReadableTable;
use ssz::{Decode, Encode};
use thiserror::Error;

mod keys;
mod proposal_metadata;
use keys::{HeightKey, UndecidedBlockDataKey, UndecidedValueKey};
use proposal_metadata::{
    decode_stored_proposal, decode_stored_value, DecodedStoredValue, StoredProposalMetadata,
};

use crate::metrics::DbMetrics;
use crate::payload::extract_block_header;
use crate::store::keys::PendingValueKey;
use crate::streaming::ProposalParts;

#[derive(Clone, Debug, PartialEq, Eq)]
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

    #[error("Conflicting undecided block data at height {height}, value {value_id}")]
    ConflictingUndecidedBlockData { height: Height, value_id: ValueId },

    #[error("Conflicting decided block data at height {height}")]
    ConflictingDecidedBlockData { height: Height },

    #[error(
        "Conflicting legacy undecided block data at height {height}, value {value_id}: \
         existing round {existing_round}, incoming round {incoming_round}, \
         existing length {existing_len}, incoming length {incoming_len}"
    )]
    ConflictingLegacyUndecidedBlockData {
        height: Height,
        value_id: ValueId,
        existing_round: String,
        incoming_round: Round,
        existing_len: usize,
        incoming_len: usize,
    },

    #[error("Irrecoverable decided state at height {height}: {reason}")]
    IrrecoverableDecidedState {
        height: Height,
        reason: &'static str,
    },

    #[error(
        "Irrecoverable undecided proposal at height {height}, round {round}, value {value_id}: {reason}"
    )]
    IrrecoverableUndecidedProposal {
        height: Height,
        round: Round,
        value_id: ValueId,
        reason: &'static str,
    },

    #[error("Conflicting decided {component} at height {height}")]
    ConflictingDecidedState {
        height: Height,
        component: &'static str,
    },

    #[error("Injected decided commit failure after {boundary}")]
    InjectedDecidedCommitFailure { boundary: &'static str },
}

const CERTIFICATES_TABLE: redb::TableDefinition<'_, HeightKey, Vec<u8>> =
    redb::TableDefinition::new("certificates");

const DECIDED_VALUES_TABLE: redb::TableDefinition<'_, HeightKey, Vec<u8>> =
    redb::TableDefinition::new("decided_values");

const LEGACY_UNDECIDED_PROPOSALS_TABLE: redb::TableDefinition<'_, UndecidedValueKey, Vec<u8>> =
    redb::TableDefinition::new("undecided_values");

const UNDECIDED_PROPOSALS_TABLE: redb::TableDefinition<'_, UndecidedValueKey, Vec<u8>> =
    redb::TableDefinition::new("undecided_values_v2");

const SCHEMA_METADATA_TABLE: redb::TableDefinition<'_, &str, u64> =
    redb::TableDefinition::new("storage_schema_metadata");

const UNDECIDED_STORAGE_RECONCILIATION_KEY: &str =
    "undecided_storage_reconciliation_version";
const UNDECIDED_STORAGE_RECONCILIATION_VERSION: u64 = 1;

const DECIDED_BLOCK_DATA_TABLE: redb::TableDefinition<'_, HeightKey, Vec<u8>> =
    redb::TableDefinition::new("decided_block_data");

const LEGACY_UNDECIDED_BLOCK_DATA_TABLE: redb::TableDefinition<'_, UndecidedValueKey, Vec<u8>> =
    redb::TableDefinition::new("undecided_block_data");

const UNDECIDED_BLOCK_DATA_TABLE: redb::TableDefinition<'_, UndecidedBlockDataKey, Vec<u8>> =
    redb::TableDefinition::new("undecided_block_data_v2");

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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct UndecidedBlockDataMigrationStats {
    legacy_rows: u64,
    inserted_payloads: u64,
    duplicate_payloads: u64,
    inserted_bytes: u64,
    compatibility_rows: u64,
    compatibility_bytes: u64,
    recovered_decided_payloads: u64,
    recovered_decided_bytes: u64,
    proposal_rows: u64,
    compacted_proposals: u64,
    proposal_source_bytes: u64,
    proposal_compact_bytes: u64,
    repaired_compact_decided_values: u64,
    repaired_compact_decided_value_bytes: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SchemaInitializationStats {
    reconciliation_ran: bool,
    legacy_payload_rows_visited: u64,
    proposal_rows_visited: u64,
    targeted_decided_rows_visited: u64,
    expensive_decided_rows_validated: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DecidedCommitFailpoint {
    Payload,
    Value,
    Certificate,
    Header,
}

impl DecidedCommitFailpoint {
    #[cfg(test)]
    const ALL: [Self; 4] = [Self::Payload, Self::Value, Self::Certificate, Self::Header];

    const fn boundary(self) -> &'static str {
        match self {
            Self::Payload => "payload write",
            Self::Value => "value write",
            Self::Certificate => "certificate write",
            Self::Header => "header write",
        }
    }
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

    fn insert_decided_state(
        &self,
        decided_value: DecidedValue,
        block_header_bytes: Bytes,
        block_data: Bytes,
    ) -> Result<(), StoreError> {
        self.insert_decided_state_inner(decided_value, block_header_bytes, block_data, None)
    }

    #[cfg(test)]
    fn insert_decided_state_with_failpoint(
        &self,
        decided_value: DecidedValue,
        block_header_bytes: Bytes,
        block_data: Bytes,
        failpoint: DecidedCommitFailpoint,
    ) -> Result<(), StoreError> {
        self.insert_decided_state_inner(
            decided_value,
            block_header_bytes,
            block_data,
            Some(failpoint),
        )
    }

    fn insert_decided_state_inner(
        &self,
        decided_value: DecidedValue,
        block_header_bytes: Bytes,
        block_data: Bytes,
        failpoint: Option<DecidedCommitFailpoint>,
    ) -> Result<(), StoreError> {
        let start = Instant::now();
        let height = decided_value.certificate.height;
        if decided_value.certificate.value_id != decided_value.value.id()
            || decided_value.value.extensions.as_ref() != block_data.as_ref()
        {
            return Err(StoreError::IrrecoverableDecidedState {
                height,
                reason: "certificate, value, and execution payload do not match",
            });
        }
        let execution_payload = ExecutionPayloadV3::from_ssz_bytes(&block_data).map_err(|_| {
            StoreError::IrrecoverableDecidedState {
                height,
                reason: "execution payload cannot be decoded",
            }
        })?;
        if extract_block_header(&execution_payload).as_ssz_bytes() != block_header_bytes {
            return Err(StoreError::IrrecoverableDecidedState {
                height,
                reason: "stored header does not match the execution payload",
            });
        }

        let value_bytes = decided_value.value.to_bytes()?.to_vec();
        let certificate_bytes = encode_certificate(&decided_value.certificate)?;
        let payload_bytes = block_data.to_vec();
        let header_bytes = block_header_bytes.to_vec();
        let tx = self.db.begin_write()?;
        let mut inserted_rows = 0;
        let mut inserted_bytes = 0;

        {
            let mut payloads = tx.open_table(DECIDED_BLOCK_DATA_TABLE)?;
            let existing = payloads
                .get(&height)?
                .map(|existing| existing.value() == payload_bytes);
            match existing {
                Some(true) => {}
                Some(false) => return Err(StoreError::ConflictingDecidedBlockData { height }),
                None => {
                    let bytes = payload_bytes.len() as u64;
                    payloads.insert(height, payload_bytes)?;
                    inserted_rows += 1;
                    inserted_bytes += bytes;
                }
            }
        }
        Self::fail_decided_commit(failpoint, DecidedCommitFailpoint::Payload)?;

        {
            let mut values = tx.open_table(DECIDED_VALUES_TABLE)?;
            let existing = values
                .get(&height)?
                .map(|existing| existing.value() == value_bytes);
            match existing {
                Some(true) => {}
                Some(false) => {
                    return Err(StoreError::ConflictingDecidedState {
                        height,
                        component: "value",
                    });
                }
                None => {
                    let bytes = value_bytes.len() as u64;
                    values.insert(height, value_bytes)?;
                    inserted_rows += 1;
                    inserted_bytes += bytes;
                }
            }
        }
        Self::fail_decided_commit(failpoint, DecidedCommitFailpoint::Value)?;

        {
            let mut certificates = tx.open_table(CERTIFICATES_TABLE)?;
            let existing = certificates
                .get(&height)?
                .map(|existing| existing.value() == certificate_bytes);
            match existing {
                Some(true) => {}
                Some(false) => {
                    return Err(StoreError::ConflictingDecidedState {
                        height,
                        component: "certificate",
                    });
                }
                None => {
                    let bytes = certificate_bytes.len() as u64;
                    certificates.insert(height, certificate_bytes)?;
                    inserted_rows += 1;
                    inserted_bytes += bytes;
                }
            }
        }
        Self::fail_decided_commit(failpoint, DecidedCommitFailpoint::Certificate)?;

        {
            let mut headers = tx.open_table(DECIDED_BLOCK_HEADERS_TABLE)?;
            let existing = headers
                .get(&height)?
                .map(|existing| existing.value() == header_bytes);
            match existing {
                Some(true) => {}
                Some(false) => {
                    return Err(StoreError::ConflictingDecidedState {
                        height,
                        component: "header",
                    });
                }
                None => {
                    let bytes = header_bytes.len() as u64;
                    headers.insert(height, header_bytes)?;
                    inserted_rows += 1;
                    inserted_bytes += bytes;
                }
            }
        }
        Self::fail_decided_commit(failpoint, DecidedCommitFailpoint::Header)?;

        if inserted_rows == 0 {
            return Ok(());
        }

        tx.commit()?;

        self.metrics.observe_write_time(start.elapsed());
        self.metrics.add_writes(inserted_rows, inserted_bytes);

        Ok(())
    }

    fn fail_decided_commit(
        configured: Option<DecidedCommitFailpoint>,
        boundary: DecidedCommitFailpoint,
    ) -> Result<(), StoreError> {
        if configured == Some(boundary) {
            return Err(StoreError::InjectedDecidedCommitFailure {
                boundary: boundary.boundary(),
            });
        }
        Ok(())
    }

    #[cfg(test)]
    fn insert_legacy_decided_metadata(
        &self,
        decided_value: DecidedValue,
        block_header_bytes: Bytes,
    ) -> Result<(), StoreError> {
        let height = decided_value.certificate.height;
        let tx = self.db.begin_write()?;
        tx.open_table(DECIDED_VALUES_TABLE)?
            .insert(height, decided_value.value.to_bytes()?.to_vec())?;
        tx.open_table(CERTIFICATES_TABLE)?
            .insert(height, encode_certificate(&decided_value.certificate)?)?;
        tx.open_table(DECIDED_BLOCK_HEADERS_TABLE)?
            .insert(height, block_header_bytes.to_vec())?;
        tx.commit()?;
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn get_undecided_proposal(
        &self,
        height: Height,
        round: Round,
        value_id: ValueId,
    ) -> Result<Option<ProposedValue<EmeraldContext>>, StoreError> {
        let start = Instant::now();
        let mut read_bytes = 0;

        let key = (height, round, value_id);
        let tx = self.db.begin_read()?;
        let compact = {
            let table = tx.open_table(UNDECIDED_PROPOSALS_TABLE)?;
            table.get(&key)?.map(|value| value.value())
        };
        let value = if let Some(bytes) = compact {
            read_bytes += bytes.len() as u64;
            let proposal = self.hydrate_stored_proposal(&tx, key, Bytes::from(bytes))?;
            read_bytes += proposal.value.extensions.len() as u64;
            Some(proposal)
        } else {
            let legacy = {
                let table = tx.open_table(LEGACY_UNDECIDED_PROPOSALS_TABLE)?;
                table.get(&key)?.map(|value| value.value())
            };
            legacy
                .map(|bytes| {
                    read_bytes += bytes.len() as u64;
                    let proposal = self.decode_full_stored_proposal(key, Bytes::from(bytes))?;
                    let payload = self.resolve_undecided_payload(&tx, key)?;
                    read_bytes += payload.len() as u64;
                    if proposal.value.extensions != payload {
                        return Err(Self::irrecoverable_undecided_proposal(
                            key,
                            "full proposal payload does not match shared storage",
                        ));
                    }
                    Ok(proposal)
                })
                .transpose()?
        };

        self.metrics.observe_read_time(start.elapsed());
        self.metrics.add_read_bytes(read_bytes);
        self.metrics
            .add_key_read_bytes(size_of::<(Height, Round, ValueId)>() as u64);

        Ok(value)
    }

    fn get_undecided_proposals(
        &self,
        height: Height,
        round: Round,
    ) -> Result<Vec<ProposedValue<EmeraldContext>>, StoreError> {
        let start = Instant::now();
        let mut read_bytes = 0;

        let tx = self.db.begin_read()?;
        let mut stored = BTreeMap::new();
        {
            let table = tx.open_table(UNDECIDED_PROPOSALS_TABLE)?;
            for result in table.iter()? {
                let (key, value) = result?;
                let key = key.value();
                if key.0 == height && key.1 == round {
                    stored.insert(key, (value.value(), true));
                }
            }
        }
        {
            let table = tx.open_table(LEGACY_UNDECIDED_PROPOSALS_TABLE)?;
            for result in table.iter()? {
                let (key, value) = result?;
                let key = key.value();
                if key.0 == height && key.1 == round {
                    stored.entry(key).or_insert_with(|| (value.value(), false));
                }
            }
        }

        let mut proposals = Vec::with_capacity(stored.len());
        for (key, (bytes, compact)) in stored {
            read_bytes += bytes.len() as u64;
            let proposal = if compact {
                self.hydrate_stored_proposal(&tx, key, Bytes::from(bytes))?
            } else {
                let proposal = self.decode_full_stored_proposal(key, Bytes::from(bytes))?;
                let payload = self.resolve_undecided_payload(&tx, key)?;
                if proposal.value.extensions != payload {
                    return Err(Self::irrecoverable_undecided_proposal(
                        key,
                        "full proposal payload does not match shared storage",
                    ));
                }
                proposal
            };
            read_bytes += proposal.value.extensions.len() as u64;
            proposals.push(proposal);
        }

        self.metrics.observe_read_time(start.elapsed());
        self.metrics.add_read_bytes(read_bytes);
        self.metrics.add_key_read_bytes(
            size_of::<(Height, Round, ValueId)>() as u64 * proposals.len() as u64,
        );

        Ok(proposals)
    }

    fn insert_undecided_proposal(
        &self,
        proposal: ProposedValue<EmeraldContext>,
    ) -> Result<(), StoreError> {
        let start = Instant::now();

        let key = (proposal.height, proposal.round, proposal.value.id());

        let tx = self.db.begin_write()?;
        let existing_legacy = {
            let table = tx.open_table(LEGACY_UNDECIDED_PROPOSALS_TABLE)?;
            let value = table.get(&key)?.map(|value| value.value());
            value
        };
        let existing_compact = {
            let table = tx.open_table(UNDECIDED_PROPOSALS_TABLE)?;
            let value = table.get(&key)?.map(|value| value.value());
            value
        };

        // Keep the first accepted proposal authoritative for this key. Restream preparation may later try to
        // persist locally reconstructed metadata for the same value; overwriting the original proposer would
        // violate the proposer identity expected by consensus.
        let authoritative = if let Some(encoded) = existing_legacy.as_ref() {
            self.decode_full_stored_proposal(key, Bytes::copy_from_slice(encoded))?
        } else if let Some(encoded) = existing_compact.as_ref() {
            decode_stored_proposal(Bytes::copy_from_slice(encoded))
                .map_err(|_| {
                    Self::irrecoverable_undecided_proposal(
                        key,
                        "stored proposal metadata cannot be decoded",
                    )
                })?
                .hydrate_verified(key, proposal.value.extensions.clone())
                .map_err(|reason| Self::irrecoverable_undecided_proposal(key, reason))?
        } else {
            proposal
        };
        let legacy = ProtobufCodec.encode(&authoritative)?;
        let compact = StoredProposalMetadata::from_proposal(&authoritative).encode()?;

        if let Some(encoded) = existing_compact.as_ref() {
            let existing = decode_stored_proposal(Bytes::copy_from_slice(encoded))
                .map_err(|_| {
                    Self::irrecoverable_undecided_proposal(
                        key,
                        "stored proposal metadata cannot be decoded",
                    )
                })?
                .hydrate_verified(key, authoritative.value.extensions.clone())
                .map_err(|reason| Self::irrecoverable_undecided_proposal(key, reason))?;
            if existing != authoritative {
                return Err(Self::irrecoverable_undecided_proposal(
                    key,
                    "proposal representations disagree",
                ));
            }
        }

        let mut inserted_rows = 0;
        let mut inserted_bytes = 0;
        if existing_legacy.is_none() {
            tx.open_table(LEGACY_UNDECIDED_PROPOSALS_TABLE)?
                .insert(key, legacy.to_vec())?;
            inserted_rows += 1;
            inserted_bytes += legacy.len() as u64;
        }
        if existing_compact.is_none() {
            tx.open_table(UNDECIDED_PROPOSALS_TABLE)?
                .insert(key, compact.to_vec())?;
            inserted_rows += 1;
            inserted_bytes += compact.len() as u64;
        }

        if inserted_rows == 0 {
            return Ok(());
        }
        tx.commit()?;

        self.metrics.observe_write_time(start.elapsed());
        self.metrics.add_writes(inserted_rows, inserted_bytes);

        Ok(())
    }

    fn hydrate_stored_proposal(
        &self,
        tx: &redb::ReadTransaction,
        key: (Height, Round, ValueId),
        encoded: Bytes,
    ) -> Result<ProposedValue<EmeraldContext>, StoreError> {
        let decoded = decode_stored_proposal(encoded).map_err(|_| {
            Self::irrecoverable_undecided_proposal(
                key,
                "stored proposal metadata cannot be decoded",
            )
        })?;

        let payload = self.resolve_undecided_payload(tx, key)?;
        decoded
            .hydrate_verified(key, payload)
            .map_err(|reason| Self::irrecoverable_undecided_proposal(key, reason))
    }

    fn resolve_undecided_payload(
        &self,
        tx: &redb::ReadTransaction,
        key: (Height, Round, ValueId),
    ) -> Result<Bytes, StoreError> {
        let (height, round, value_id) = key;
        let primary_payload = {
            let table = tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)?;
            table
                .get(&(height, value_id))?
                .map(|payload| payload.value())
        };
        let payload = match primary_payload {
            Some(payload) => payload,
            None => {
                let table = tx.open_table(LEGACY_UNDECIDED_BLOCK_DATA_TABLE)?;
                table
                    .get(&(height, round, value_id))?
                    .map(|payload| payload.value())
                    .ok_or_else(|| {
                        Self::irrecoverable_undecided_proposal(key, "missing shared payload")
                    })?
            }
        };
        Ok(Bytes::from(payload))
    }

    fn decode_full_stored_proposal(
        &self,
        key: (Height, Round, ValueId),
        encoded: Bytes,
    ) -> Result<ProposedValue<EmeraldContext>, StoreError> {
        let proposal: ProposedValue<EmeraldContext> =
            ProtobufCodec.decode(encoded).map_err(|_| {
                Self::irrecoverable_undecided_proposal(
                    key,
                    "full proposal cannot be decoded",
                )
            })?;
        if (proposal.height, proposal.round, proposal.value.id()) != key {
            return Err(Self::irrecoverable_undecided_proposal(
                key,
                "full proposal key does not match its metadata",
            ));
        }
        Ok(proposal)
    }

    fn irrecoverable_undecided_proposal(
        key: (Height, Round, ValueId),
        reason: &'static str,
    ) -> StoreError {
        let (height, round, value_id) = key;
        StoreError::IrrecoverableUndecidedProposal {
            height,
            round,
            value_id,
            reason,
        }
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
            let mut legacy_undecided = tx.open_table(LEGACY_UNDECIDED_PROPOSALS_TABLE)?;
            legacy_undecided.retain(|k, _| k.0 >= block_data_retain_height)?;

            let mut undecided = tx.open_table(UNDECIDED_PROPOSALS_TABLE)?;
            undecided.retain(|k, _| k.0 >= block_data_retain_height)?;

            // Remove all undecided block data with height < retain_height
            let mut undecided_block_data = tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)?;
            undecided_block_data.retain(|k, _| k.0 >= block_data_retain_height)?;

            // Keep the temporary N-1 rollback shadow aligned with the primary v2 retention boundary.
            let mut legacy_undecided_block_data =
                tx.open_table(LEGACY_UNDECIDED_BLOCK_DATA_TABLE)?;
            legacy_undecided_block_data.retain(|k, _| k.0 >= block_data_retain_height)?;

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

    fn max_decided_value_height(&self) -> Result<Option<Height>, StoreError> {
        let tx = self.db.begin_read()?;
        let table = tx.open_table(DECIDED_VALUES_TABLE)?;
        let Some((key, _)) = table.last()? else {
            return Ok(None);
        };
        Ok(Some(key.value()))
    }

    fn initialize_schema(&self) -> Result<SchemaInitializationStats, StoreError> {
        let start = Instant::now();
        let mut stats = SchemaInitializationStats::default();
        let tx = self.db.begin_write()?;

        {
            let _ = tx.open_table(DECIDED_VALUES_TABLE)?;
            let _ = tx.open_table(CERTIFICATES_TABLE)?;
            let _ = tx.open_table(LEGACY_UNDECIDED_PROPOSALS_TABLE)?;
            let _ = tx.open_table(UNDECIDED_PROPOSALS_TABLE)?;
            let _ = tx.open_table(DECIDED_BLOCK_DATA_TABLE)?;
            let _ = tx.open_table(LEGACY_UNDECIDED_BLOCK_DATA_TABLE)?;
            let _ = tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)?;
            let _ = tx.open_table(DECIDED_BLOCK_HEADERS_TABLE)?;
            let _ = tx.open_table(PERSISTENT_METRICS_TABLE)?;
            let _ = tx.open_table(PENDING_PROPOSAL_PARTS_TABLE)?;
            let _ = tx.open_table(SCHEMA_METADATA_TABLE)?;
        }

        let version = {
            let metadata = tx.open_table(SCHEMA_METADATA_TABLE)?;
            let version = metadata
                .get(UNDECIDED_STORAGE_RECONCILIATION_KEY)?
                .map(|value| value.value())
                .unwrap_or_default();
            version
        };

        let mut migration = UndecidedBlockDataMigrationStats::default();
        if version < UNDECIDED_STORAGE_RECONCILIATION_VERSION {
            stats.reconciliation_ran = true;
            Self::reconcile_undecided_storage(&tx, &mut migration, &mut stats)?;
            tx.open_table(SCHEMA_METADATA_TABLE)?.insert(
                UNDECIDED_STORAGE_RECONCILIATION_KEY,
                UNDECIDED_STORAGE_RECONCILIATION_VERSION,
            )?;
        } else {
            Self::recover_missing_decided_payloads(&tx, &mut migration, &mut stats)?;
        }

        tx.commit()?;

        if stats.reconciliation_ran {
            self.metrics.observe_write_time(start.elapsed());
            self.metrics.add_writes(
                migration.inserted_payloads
                    + migration.compatibility_rows
                    + migration.recovered_decided_payloads
                    + migration.compacted_proposals
                    + migration.repaired_compact_decided_values,
                migration.inserted_bytes
                    + migration.compatibility_bytes
                    + migration.recovered_decided_bytes
                    + migration.proposal_compact_bytes
                    + migration.repaired_compact_decided_value_bytes,
            );
            tracing::info!(
                event = "undecided_block_data_migration",
                legacy_rows = migration.legacy_rows,
                inserted_payloads = migration.inserted_payloads,
                inserted_bytes = migration.inserted_bytes,
                duplicate_payloads = migration.duplicate_payloads,
                compatibility_rows = migration.compatibility_rows,
                compatibility_bytes = migration.compatibility_bytes,
                recovered_decided_payloads = migration.recovered_decided_payloads,
                recovered_decided_bytes = migration.recovered_decided_bytes,
                proposal_rows = migration.proposal_rows,
                compacted_proposals = migration.compacted_proposals,
                proposal_source_bytes = migration.proposal_source_bytes,
                proposal_compact_bytes = migration.proposal_compact_bytes,
                repaired_compact_decided_values = migration.repaired_compact_decided_values,
                repaired_compact_decided_value_bytes =
                    migration.repaired_compact_decided_value_bytes,
                duration_seconds = start.elapsed().as_secs_f64(),
                "Migrated legacy undecided block data"
            );
        }

        Ok(stats)
    }

    fn reconcile_undecided_storage(
        tx: &redb::WriteTransaction,
        migration: &mut UndecidedBlockDataMigrationStats,
        stats: &mut SchemaInitializationStats,
    ) -> Result<(), StoreError> {
        *migration = Self::migrate_undecided_block_data(tx)?;
        stats.legacy_payload_rows_visited = migration.legacy_rows;
        Self::reconcile_undecided_proposals(tx, migration, stats)?;
        Self::backfill_legacy_undecided_block_data(tx, migration)?;
        Self::recover_legacy_partial_commits(tx, migration)?;
        Ok(())
    }

    fn recover_missing_decided_payloads(
        tx: &redb::WriteTransaction,
        migration: &mut UndecidedBlockDataMigrationStats,
        stats: &mut SchemaInitializationStats,
    ) -> Result<(), StoreError> {
        let values = tx.open_table(DECIDED_VALUES_TABLE)?;
        for entry in values.iter()? {
            entry?;
            stats.targeted_decided_rows_visited += 1;
            stats.expensive_decided_rows_validated += 1;
        }
        drop(values);
        Self::recover_legacy_partial_commits(tx, migration)
    }

    fn recover_legacy_partial_commits(
        tx: &redb::WriteTransaction,
        stats: &mut UndecidedBlockDataMigrationStats,
    ) -> Result<(), StoreError> {
        let mut value_repairs = Vec::new();
        {
            let values = tx.open_table(DECIDED_VALUES_TABLE)?;
            let certificates = tx.open_table(CERTIFICATES_TABLE)?;
            let headers = tx.open_table(DECIDED_BLOCK_HEADERS_TABLE)?;
            let primary = tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)?;
            let legacy = tx.open_table(LEGACY_UNDECIDED_BLOCK_DATA_TABLE)?;
            let mut decided_payloads = tx.open_table(DECIDED_BLOCK_DATA_TABLE)?;

            for entry in values.iter()? {
                let (height_key, encoded_value) = entry?;
                let height = height_key.value();
                let existing_decided_payload = decided_payloads
                    .get(&height)?
                    .map(|stored_payload| stored_payload.value());
                let needs_promotion = existing_decided_payload.is_none();

                let encoded_value = encoded_value.value();
                let stored_value =
                    decode_stored_value(Bytes::from(encoded_value)).map_err(|_| {
                        StoreError::IrrecoverableDecidedState {
                            height,
                            reason: "stored value cannot be decoded",
                        }
                    })?;
                let stored_value_id = match &stored_value {
                    DecodedStoredValue::Full(value) => value.id(),
                    DecodedStoredValue::IdOnly(value_id) => *value_id,
                };
                let encoded_certificate =
                    certificates
                        .get(&height)?
                        .ok_or(StoreError::IrrecoverableDecidedState {
                            height,
                            reason: "missing certificate",
                        })?;
                let encoded_certificate = encoded_certificate.value();
                let certificate = decode_certificate(&encoded_certificate).map_err(|_| {
                    StoreError::IrrecoverableDecidedState {
                        height,
                        reason: "stored certificate cannot be decoded",
                    }
                })?;
                let stored_header =
                    headers
                        .get(&height)?
                        .ok_or(StoreError::IrrecoverableDecidedState {
                            height,
                            reason: "missing stored header",
                        })?;

                if certificate.height != height {
                    return Err(StoreError::IrrecoverableDecidedState {
                        height,
                        reason: "certificate height does not match the decided value key",
                    });
                }
                if certificate.value_id != stored_value_id {
                    return Err(StoreError::IrrecoverableDecidedState {
                        height,
                        reason: "certificate value ID does not match the stored value",
                    });
                }

                let payload = match existing_decided_payload {
                    Some(payload) => payload,
                    None => {
                        let primary_payload = primary.get(&(height, certificate.value_id))?;
                        let legacy_payload = if primary_payload.is_none() {
                            legacy.get(&(height, certificate.round, certificate.value_id))?
                        } else {
                            None
                        };
                        primary_payload
                            .as_ref()
                            .map(|value| value.value())
                            .or_else(|| legacy_payload.as_ref().map(|value| value.value()))
                            .ok_or(StoreError::IrrecoverableDecidedState {
                                height,
                                reason: "missing its execution payload",
                            })?
                    }
                };

                let recomputed = Value::new(Bytes::copy_from_slice(&payload));
                let stored_payload_matches = match &stored_value {
                    DecodedStoredValue::Full(value) => value.extensions.as_ref() == payload,
                    DecodedStoredValue::IdOnly(_) => true,
                };
                if recomputed.id() != certificate.value_id || !stored_payload_matches {
                    return Err(StoreError::IrrecoverableDecidedState {
                        height,
                        reason: "execution payload does not match the stored value ID and bytes",
                    });
                }
                let execution_payload =
                    ExecutionPayloadV3::from_ssz_bytes(&payload).map_err(|_| {
                        StoreError::IrrecoverableDecidedState {
                            height,
                            reason: "execution payload cannot be decoded",
                        }
                    })?;
                let expected_header = extract_block_header(&execution_payload).as_ssz_bytes();
                if stored_header.value() != expected_header {
                    return Err(StoreError::IrrecoverableDecidedState {
                        height,
                        reason: "stored header does not match the execution payload",
                    });
                }

                if matches!(stored_value, DecodedStoredValue::IdOnly(_)) {
                    let encoded = recomputed.to_bytes()?.to_vec();
                    stats.repaired_compact_decided_values += 1;
                    stats.repaired_compact_decided_value_bytes += encoded.len() as u64;
                    value_repairs.push((height, encoded));
                }
                if needs_promotion {
                    let payload_len = payload.len() as u64;
                    decided_payloads.insert(height, payload)?;
                    stats.recovered_decided_payloads += 1;
                    stats.recovered_decided_bytes += payload_len;
                }
            }
        }

        if !value_repairs.is_empty() {
            let mut values = tx.open_table(DECIDED_VALUES_TABLE)?;
            for (height, encoded) in value_repairs {
                values.insert(height, encoded)?;
            }
        }

        Ok(())
    }

    fn migrate_undecided_block_data(
        tx: &redb::WriteTransaction,
    ) -> Result<UndecidedBlockDataMigrationStats, StoreError> {
        let mut stats = UndecidedBlockDataMigrationStats::default();
        let mut migrated_rounds: BTreeMap<(Height, ValueId), Round> = BTreeMap::new();
        {
            let legacy = tx.open_table(LEGACY_UNDECIDED_BLOCK_DATA_TABLE)?;
            let mut target = tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)?;
            for entry in legacy.iter()? {
                let (legacy_key, legacy_value) = entry?;
                let (height, round, value_id) = legacy_key.value();
                let key = (height, value_id);
                let payload = legacy_value.value();
                let existing = match target.get(&key)? {
                    Some(value) => {
                        let existing = value.value();
                        Some((existing == payload, existing.len()))
                    }
                    None => None,
                };

                stats.legacy_rows += 1;
                match existing {
                    Some((true, _)) => {
                        stats.duplicate_payloads += 1;
                        migrated_rounds.entry(key).or_insert(round);
                    }
                    Some((false, existing_len)) => {
                        let existing_round = migrated_rounds.get(&key).copied();
                        tracing::error!(
                            event = "undecided_block_data_migration_conflict",
                            %height,
                            value = %value_id,
                            existing_round = ?existing_round,
                            incoming_round = %round,
                            existing_len,
                            incoming_len = payload.len(),
                            "Conflicting legacy undecided block data"
                        );
                        return Err(StoreError::ConflictingLegacyUndecidedBlockData {
                            height,
                            value_id,
                            existing_round: existing_round
                                .map(|round| round.to_string())
                                .unwrap_or_else(|| "unknown (pre-existing v2 row)".to_owned()),
                            incoming_round: round,
                            existing_len,
                            incoming_len: payload.len(),
                        });
                    }
                    None => {
                        stats.inserted_payloads += 1;
                        stats.inserted_bytes += payload.len() as u64;
                        target.insert(key, payload)?;
                        migrated_rounds.insert(key, round);
                    }
                }
            }
        }
        Ok(stats)
    }

    fn reconcile_undecided_proposals(
        tx: &redb::WriteTransaction,
        migration: &mut UndecidedBlockDataMigrationStats,
        stats: &mut SchemaInitializationStats,
    ) -> Result<(), StoreError> {
        let mut rows: BTreeMap<
            (Height, Round, ValueId),
            (Option<Vec<u8>>, Option<Vec<u8>>),
        > = BTreeMap::new();
        {
            let proposals = tx.open_table(LEGACY_UNDECIDED_PROPOSALS_TABLE)?;
            for entry in proposals.iter()? {
                let (proposal_key, encoded) = entry?;
                let key = proposal_key.value();
                let encoded = encoded.value();
                stats.proposal_rows_visited += 1;
                migration.proposal_rows += 1;
                migration.proposal_source_bytes += encoded.len() as u64;
                rows.entry(key).or_default().0 = Some(encoded);
            }
        }
        {
            let proposals = tx.open_table(UNDECIDED_PROPOSALS_TABLE)?;
            for entry in proposals.iter()? {
                let (proposal_key, encoded) = entry?;
                let key = proposal_key.value();
                let encoded = encoded.value();
                stats.proposal_rows_visited += 1;
                migration.proposal_rows += 1;
                migration.proposal_source_bytes += encoded.len() as u64;
                rows.entry(key).or_default().1 = Some(encoded);
            }
        }

        let primary = tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)?;
        let legacy_payloads = tx.open_table(LEGACY_UNDECIDED_BLOCK_DATA_TABLE)?;
        let mut legacy_replacements = Vec::new();
        let mut compact_replacements = Vec::new();

        for (key, (legacy_encoded, compact_encoded)) in rows {
            let primary_payload = primary.get(&(key.0, key.2))?;
            let legacy_payload = if primary_payload.is_none() {
                legacy_payloads.get(&key)?
            } else {
                None
            };
            let payload = primary_payload
                .as_ref()
                .map(|value| value.value())
                .or_else(|| legacy_payload.as_ref().map(|value| value.value()))
                .ok_or_else(|| {
                    Self::irrecoverable_undecided_proposal(key, "missing shared payload")
                })?;
            let payload = Bytes::from(payload);

            let decode = |encoded: &[u8]| {
                decode_stored_proposal(Bytes::copy_from_slice(encoded))
                    .map_err(|_| {
                        Self::irrecoverable_undecided_proposal(
                            key,
                            "stored proposal metadata cannot be decoded",
                        )
                    })?
                    .hydrate_verified(key, payload.clone())
                    .map_err(|reason| Self::irrecoverable_undecided_proposal(key, reason))
            };
            let legacy_proposal = legacy_encoded.as_deref().map(decode).transpose()?;
            let compact_proposal = compact_encoded.as_deref().map(decode).transpose()?;
            let proposal = match (legacy_proposal, compact_proposal) {
                (Some(legacy), Some(compact)) if legacy != compact => {
                    return Err(Self::irrecoverable_undecided_proposal(
                        key,
                        "proposal representations disagree",
                    ));
                }
                (Some(legacy), _) => legacy,
                (_, Some(compact)) => compact,
                (None, None) => unreachable!("proposal key must have at least one representation"),
            };

            let full = ProtobufCodec.encode(&proposal)?.to_vec();
            let compact = StoredProposalMetadata::from_proposal(&proposal)
                .encode()?
                .to_vec();
            if legacy_encoded.as_ref() != Some(&full) {
                legacy_replacements.push((key, full));
            }
            if compact_encoded.as_ref() != Some(&compact) {
                compact_replacements.push((key, compact));
            }
        }
        drop(primary);
        drop(legacy_payloads);

        if !legacy_replacements.is_empty() {
            let mut proposals = tx.open_table(LEGACY_UNDECIDED_PROPOSALS_TABLE)?;
            for (key, full) in legacy_replacements {
                migration.compacted_proposals += 1;
                migration.proposal_compact_bytes += full.len() as u64;
                proposals.insert(key, full)?;
            }
        }
        if !compact_replacements.is_empty() {
            let mut proposals = tx.open_table(UNDECIDED_PROPOSALS_TABLE)?;
            for (key, compact) in compact_replacements {
                migration.compacted_proposals += 1;
                migration.proposal_compact_bytes += compact.len() as u64;
                proposals.insert(key, compact)?;
            }
        }
        Ok(())
    }

    fn backfill_legacy_undecided_block_data(
        tx: &redb::WriteTransaction,
        stats: &mut UndecidedBlockDataMigrationStats,
    ) -> Result<(), StoreError> {
        let proposals = tx.open_table(UNDECIDED_PROPOSALS_TABLE)?;
        let primary = tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)?;
        let mut legacy = tx.open_table(LEGACY_UNDECIDED_BLOCK_DATA_TABLE)?;

        for entry in proposals.iter()? {
            let (proposal_key, _) = entry?;
            let (height, round, value_id) = proposal_key.value();
            let Some(payload) = primary.get(&(height, value_id))? else {
                continue;
            };
            let payload = payload.value();
            let legacy_key = (height, round, value_id);
            let existing = legacy
                .get(&legacy_key)?
                .map(|value| value.value() == payload);
            match existing {
                Some(true) => {}
                Some(false) => {
                    return Err(StoreError::ConflictingUndecidedBlockData { height, value_id });
                }
                None => {
                    let payload_len = payload.len() as u64;
                    legacy.insert(legacy_key, payload)?;
                    stats.compatibility_rows += 1;
                    stats.compatibility_bytes += payload_len;
                }
            }
        }

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

    fn get_decided_block_data(&self, height: Height) -> Result<Option<Bytes>, StoreError> {
        let start = Instant::now();
        let tx = self.db.begin_read()?;
        let table = tx.open_table(DECIDED_BLOCK_DATA_TABLE)?;
        let value = table
            .get(&height)?
            .map(|data| Bytes::copy_from_slice(&data.value()));
        self.metrics.observe_read_time(start.elapsed());
        self.metrics
            .add_read_bytes(value.as_ref().map_or(0, |bytes| bytes.len() as u64));
        self.metrics.add_key_read_bytes(size_of::<Height>() as u64);
        Ok(value)
    }

    fn get_undecided_block_data(
        &self,
        height: Height,
        round: Round,
        value_id: ValueId,
    ) -> Result<Option<Bytes>, StoreError> {
        let start = Instant::now();
        let tx = self.db.begin_read()?;
        let value = {
            let table = tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)?;
            table
                .get(&(height, value_id))?
                .map(|data| Bytes::copy_from_slice(&data.value()))
        };
        let value = match value {
            Some(value) => Some(value),
            None => {
                let legacy = tx.open_table(LEGACY_UNDECIDED_BLOCK_DATA_TABLE)?;
                legacy
                    .get(&(height, round, value_id))?
                    .map(|data| Bytes::copy_from_slice(&data.value()))
            }
        };

        self.metrics.observe_read_time(start.elapsed());
        self.metrics
            .add_read_bytes(value.as_ref().map_or(0, |bytes| bytes.len() as u64));
        self.metrics
            .add_key_read_bytes((size_of::<Height>() + size_of::<ValueId>()) as u64);
        Ok(value)
    }

    fn insert_undecided_block_data(
        &self,
        height: Height,
        round: Round,
        value_id: ValueId,
        data: Bytes,
    ) -> Result<(), StoreError> {
        let start = Instant::now();
        let tx = self.db.begin_write()?;
        let mut inserted_rows = 0;
        let mut inserted_bytes = 0;
        {
            let mut table = tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)?;
            let key = (height, value_id);
            let existing = table.get(&key)?.map(|value| value.value() == data.as_ref());

            match existing {
                Some(true) => {}
                Some(false) => {
                    return Err(StoreError::ConflictingUndecidedBlockData { height, value_id });
                }
                None => {
                    table.insert(key, data.to_vec())?;
                    inserted_rows += 1;
                    inserted_bytes += data.len() as u64;
                }
            }
        }

        {
            let mut legacy = tx.open_table(LEGACY_UNDECIDED_BLOCK_DATA_TABLE)?;
            let key = (height, round, value_id);
            let existing = legacy
                .get(&key)?
                .map(|value| value.value() == data.as_ref());

            match existing {
                Some(true) => {}
                Some(false) => {
                    return Err(StoreError::ConflictingUndecidedBlockData { height, value_id });
                }
                None => {
                    legacy.insert(key, data.to_vec())?;
                    inserted_rows += 1;
                    inserted_bytes += data.len() as u64;
                }
            }
        }

        if inserted_rows == 0 {
            return Ok(());
        }

        tx.commit()?;
        self.metrics.observe_write_time(start.elapsed());
        self.metrics.add_writes(inserted_rows, inserted_bytes);
        Ok(())
    }

    #[cfg(test)]
    fn undecided_block_data_len(&self) -> Result<u64, StoreError> {
        let tx = self.db.begin_read()?;
        Ok(tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)?.len()?)
    }

    #[cfg(test)]
    fn insert_decided_block_data(&self, height: Height, data: Bytes) -> Result<(), StoreError> {
        let start = Instant::now();
        let tx = self.db.begin_write()?;
        let inserted = {
            let mut table = tx.open_table(DECIDED_BLOCK_DATA_TABLE)?;
            let existing = table.get(&height)?.map(|value| value.value());
            match existing {
                Some(existing) if existing.as_slice() == data.as_ref() => false,
                Some(_) => return Err(StoreError::ConflictingDecidedBlockData { height }),
                None => {
                    table.insert(height, data.to_vec())?;
                    true
                }
            }
        };

        if !inserted {
            return Ok(());
        }

        tx.commit()?;
        self.metrics.observe_write_time(start.elapsed());
        self.metrics.add_write_bytes(data.len() as u64);
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
            db.initialize_schema()?;
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

    pub async fn max_decided_value_height(&self) -> Result<Option<Height>, StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.max_decided_value_height()).await?
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

    /// Atomically stores a decided payload, value, certificate, and block header.
    /// Called by the application when it `commit`s a value decided by consensus.
    pub async fn store_decided_state(
        &self,
        certificate: &CommitCertificate<EmeraldContext>,
        value: Value,
        block_header_bytes: Bytes,
        block_data: Bytes,
    ) -> Result<(), StoreError> {
        let decided_value = DecidedValue {
            value,
            certificate: certificate.clone(),
        };

        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || {
            db.insert_decided_state(decided_value, block_header_bytes, block_data)
        })
        .await?
    }

    /// Stores an undecided proposal.
    /// Called by the application when receiving new proposals from peers.
    pub async fn store_undecided_proposal(
        &self,
        value: ProposedValue<EmeraldContext>,
    ) -> Result<(), StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.insert_undecided_proposal(value)).await?
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

    pub async fn get_decided_block_data(
        &self,
        height: Height,
    ) -> Result<Option<Bytes>, StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.get_decided_block_data(height)).await?
    }

    pub async fn get_undecided_block_data(
        &self,
        height: Height,
        round: Round,
        value_id: ValueId,
    ) -> Result<Option<Bytes>, StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.get_undecided_block_data(height, round, value_id))
            .await?
    }

    pub async fn store_undecided_block_data(
        &self,
        height: Height,
        round: Round,
        value_id: ValueId,
        data: Bytes,
    ) -> Result<(), StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || {
            db.insert_undecided_block_data(height, round, value_id, data)
        })
        .await?
    }

    #[cfg(test)]
    pub async fn undecided_block_data_len(&self) -> Result<u64, StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.undecided_block_data_len()).await?
    }

    #[cfg(test)]
    pub(crate) async fn store_legacy_decided_block_data(
        &self,
        height: Height,
        data: Bytes,
    ) -> Result<(), StoreError> {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || db.insert_decided_block_data(height, data)).await?
    }

    #[cfg(test)]
    pub(crate) async fn store_legacy_decided_metadata(
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
            db.insert_legacy_decided_metadata(decided_value, block_header_bytes)
        })
        .await?
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
    use alloy_rpc_types_engine::{ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3};
    use malachitebft_app_channel::app::types::core::{CommitCertificate, Validity};
    use malachitebft_eth_types::Address;
    use ssz::{Decode, Encode};

    use super::*;
    use crate::payload::extract_block_header;

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
        db.initialize_schema().unwrap();
        (db, dir)
    }

    fn create_test_db_with_metrics(name: &str) -> (Db, tempfile::TempDir, DbMetrics) {
        let dir = tempfile::tempdir().unwrap();
        let metrics = DbMetrics::new();
        let db = Db::new(
            dir.path().join(format!("{name}.redb")),
            1024 * 1024,
            metrics.clone(),
        )
        .unwrap();
        db.initialize_schema().unwrap();
        (db, dir, metrics)
    }

    fn create_legacy_test_db(
        name: &str,
        rows: &[(Height, Round, ValueId, Bytes)],
    ) -> (Db, tempfile::TempDir, DbMetrics) {
        let dir = tempfile::tempdir().unwrap();
        let metrics = DbMetrics::new();
        let db = Db::new(
            dir.path().join(format!("{name}.redb")),
            1024 * 1024,
            metrics.clone(),
        )
        .unwrap();
        let tx = db.db.begin_write().unwrap();
        {
            let mut table = tx.open_table(LEGACY_UNDECIDED_BLOCK_DATA_TABLE).unwrap();
            for (height, round, value_id, bytes) in rows {
                table
                    .insert((*height, *round, *value_id), bytes.to_vec())
                    .unwrap();
            }
        }
        tx.commit().unwrap();
        (db, dir, metrics)
    }

    fn has_table(db: &Db, name: &str) -> bool {
        let tx = db.db.begin_read().unwrap();
        let has_table = tx.list_tables().unwrap().any(|table| table.name() == name);
        has_table
    }

    fn get_legacy_undecided_block_data(
        db: &Db,
        height: Height,
        round: Round,
        value_id: ValueId,
    ) -> Option<Bytes> {
        let tx = db.db.begin_read().unwrap();
        let table = tx.open_table(LEGACY_UNDECIDED_BLOCK_DATA_TABLE).unwrap();
        table
            .get(&(height, round, value_id))
            .unwrap()
            .map(|value| Bytes::copy_from_slice(&value.value()))
    }

    fn raw_legacy_proposal(db: &Db, key: (Height, Round, ValueId)) -> Option<Vec<u8>> {
        if !has_table(db, LEGACY_UNDECIDED_PROPOSALS_TABLE.name()) {
            return None;
        }
        let tx = db.db.begin_read().unwrap();
        tx.open_table(LEGACY_UNDECIDED_PROPOSALS_TABLE)
            .unwrap()
            .get(&key)
            .unwrap()
            .map(|value| value.value())
    }

    fn raw_compact_proposal(db: &Db, key: (Height, Round, ValueId)) -> Option<Vec<u8>> {
        if !has_table(db, UNDECIDED_PROPOSALS_TABLE.name()) {
            return None;
        }
        let tx = db.db.begin_read().unwrap();
        tx.open_table(UNDECIDED_PROPOSALS_TABLE)
            .unwrap()
            .get(&key)
            .unwrap()
            .map(|value| value.value())
    }

    #[test]
    fn schema_creates_separate_legacy_compact_and_metadata_tables() {
        let (db, _dir, _metrics) =
            create_test_db_with_metrics("versioned-proposal-schema");

        assert!(has_table(&db, "undecided_values"));
        assert!(has_table(&db, "undecided_values_v2"));
        assert!(has_table(&db, "storage_schema_metadata"));
    }

    fn insert_raw_proposal(db: &Db, key: (Height, Round, ValueId), encoded: Vec<u8>) {
        let tx = db.db.begin_write().unwrap();
        tx.open_table(UNDECIDED_PROPOSALS_TABLE)
            .unwrap()
            .insert(key, encoded)
            .unwrap();
        tx.commit().unwrap();
    }

    fn insert_raw_legacy_proposal(
        db: &Db,
        key: (Height, Round, ValueId),
        encoded: Vec<u8>,
    ) {
        let tx = db.db.begin_write().unwrap();
        tx.open_table(LEGACY_UNDECIDED_PROPOSALS_TABLE)
            .unwrap()
            .insert(key, encoded)
            .unwrap();
        tx.commit().unwrap();
    }

    fn reconciliation_version(db: &Db) -> Option<u64> {
        if !has_table(db, SCHEMA_METADATA_TABLE.name()) {
            return None;
        }
        let tx = db.db.begin_read().unwrap();
        tx.open_table(SCHEMA_METADATA_TABLE)
            .unwrap()
            .get(UNDECIDED_STORAGE_RECONCILIATION_KEY)
            .unwrap()
            .map(|version| version.value())
    }

    #[derive(Debug, PartialEq, Eq)]
    struct RawStorageSnapshot {
        legacy_proposal: Option<Vec<u8>>,
        compact_proposal: Option<Vec<u8>>,
        legacy_payload: Option<Vec<u8>>,
        shared_payload: Option<Vec<u8>>,
        decided_payload: Option<Vec<u8>>,
        decided_value: Option<Vec<u8>>,
        certificate: Option<Vec<u8>>,
        header: Option<Vec<u8>>,
        reconciliation_version: Option<u64>,
    }

    fn raw_height_row(
        db: &Db,
        table_definition: redb::TableDefinition<'static, HeightKey, Vec<u8>>,
        height: Height,
    ) -> Option<Vec<u8>> {
        if !has_table(db, table_definition.name()) {
            return None;
        }
        let tx = db.db.begin_read().unwrap();
        tx.open_table(table_definition)
            .unwrap()
            .get(&height)
            .unwrap()
            .map(|value| value.value())
    }

    fn raw_storage_snapshot(db: &Db, key: (Height, Round, ValueId)) -> RawStorageSnapshot {
        let legacy_payload = if has_table(db, LEGACY_UNDECIDED_BLOCK_DATA_TABLE.name()) {
            get_legacy_undecided_block_data(db, key.0, key.1, key.2)
                .map(|bytes| bytes.to_vec())
        } else {
            None
        };
        let shared_payload = if has_table(db, UNDECIDED_BLOCK_DATA_TABLE.name()) {
            let tx = db.db.begin_read().unwrap();
            tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)
                .unwrap()
                .get(&(key.0, key.2))
                .unwrap()
                .map(|value| value.value())
        } else {
            None
        };

        RawStorageSnapshot {
            legacy_proposal: raw_legacy_proposal(db, key),
            compact_proposal: raw_compact_proposal(db, key),
            legacy_payload,
            shared_payload,
            decided_payload: raw_height_row(db, DECIDED_BLOCK_DATA_TABLE, key.0),
            decided_value: raw_height_row(db, DECIDED_VALUES_TABLE, key.0),
            certificate: raw_height_row(db, CERTIFICATES_TABLE, key.0),
            header: raw_height_row(db, DECIDED_BLOCK_HEADERS_TABLE, key.0),
            reconciliation_version: reconciliation_version(db),
        }
    }

    fn insert_raw_legacy_payload(db: &Db, key: (Height, Round, ValueId), payload: &Bytes) {
        let tx = db.db.begin_write().unwrap();
        tx.open_table(LEGACY_UNDECIDED_BLOCK_DATA_TABLE)
            .unwrap()
            .insert(key, payload.to_vec())
            .unwrap();
        tx.commit().unwrap();
    }

    fn insert_raw_legacy_full_proposal_and_payload(
        db: &Db,
        proposal: &ProposedValue<EmeraldContext>,
        payload: &Bytes,
    ) {
        let key = (proposal.height, proposal.round, proposal.value.id());
        insert_raw_legacy_proposal(db, key, ProtobufCodec.encode(proposal).unwrap().to_vec());
        insert_raw_legacy_payload(db, key, payload);
    }

    fn assert_no_decided_rows(db: &Db, height: Height) {
        let tx = db.db.begin_read().unwrap();
        assert!(tx
            .open_table(DECIDED_BLOCK_DATA_TABLE)
            .unwrap()
            .get(&height)
            .unwrap()
            .is_none());
        assert!(tx
            .open_table(DECIDED_VALUES_TABLE)
            .unwrap()
            .get(&height)
            .unwrap()
            .is_none());
        assert!(tx
            .open_table(CERTIFICATES_TABLE)
            .unwrap()
            .get(&height)
            .unwrap()
            .is_none());
        assert!(tx
            .open_table(DECIDED_BLOCK_HEADERS_TABLE)
            .unwrap()
            .get(&height)
            .unwrap()
            .is_none());
    }

    fn make_execution_payload_bytes(block_number: u64) -> Bytes {
        let payload = ExecutionPayloadV3 {
            payload_inner: ExecutionPayloadV2 {
                payload_inner: ExecutionPayloadV1 {
                    parent_hash: Default::default(),
                    fee_recipient: Default::default(),
                    state_root: Default::default(),
                    receipts_root: Default::default(),
                    logs_bloom: Default::default(),
                    prev_randao: Default::default(),
                    block_number,
                    gas_limit: 30_000_000,
                    gas_used: 0,
                    timestamp: block_number,
                    extra_data: Default::default(),
                    base_fee_per_gas: Default::default(),
                    block_hash: Default::default(),
                    transactions: Vec::new(),
                },
                withdrawals: Vec::new(),
            },
            blob_gas_used: 0,
            excess_blob_gas: 0,
        };
        Bytes::from(payload.as_ssz_bytes())
    }

    fn insert_n_minus_one_partial_commit(
        db: &Db,
        decided_value: &DecidedValue,
        block_header: &Bytes,
        undecided_payload: Option<&Bytes>,
    ) {
        let height = decided_value.certificate.height;
        let tx = db.db.begin_write().unwrap();
        {
            tx.open_table(DECIDED_VALUES_TABLE)
                .unwrap()
                .insert(height, decided_value.value.to_bytes().unwrap().to_vec())
                .unwrap();
            tx.open_table(CERTIFICATES_TABLE)
                .unwrap()
                .insert(
                    height,
                    encode_certificate(&decided_value.certificate).unwrap(),
                )
                .unwrap();
            tx.open_table(DECIDED_BLOCK_HEADERS_TABLE)
                .unwrap()
                .insert(height, block_header.to_vec())
                .unwrap();
            if let Some(payload) = undecided_payload {
                tx.open_table(LEGACY_UNDECIDED_BLOCK_DATA_TABLE)
                    .unwrap()
                    .insert(
                        (
                            height,
                            decided_value.certificate.round,
                            decided_value.certificate.value_id,
                        ),
                        payload.to_vec(),
                    )
                    .unwrap();
            }
        }
        tx.commit().unwrap();
    }

    #[test]
    fn max_decided_value_height_reports_an_empty_store() {
        let (db, _dir) = create_test_db("max-decided-value-height-empty");

        assert!(matches!(db.max_decided_value_height(), Ok(None)));
    }

    #[test]
    fn legacy_undecided_block_data_migration_deduplicates_and_reopens() {
        let height = Height::new(9);
        let first_id = ValueId::new(21);
        let second_id = ValueId::new(22);
        let first = Bytes::from_static(b"first-payload");
        let second = Bytes::from_static(b"second-payload");
        let (db, dir, metrics) = create_legacy_test_db(
            "legacy_success",
            &[
                (height, Round::new(0), first_id, first.clone()),
                (height, Round::new(1), first_id, first.clone()),
                (height, Round::new(2), second_id, second.clone()),
            ],
        );

        db.initialize_schema().unwrap();
        assert!(has_table(&db, "undecided_block_data"));
        assert!(has_table(&db, "undecided_block_data_v2"));
        assert_eq!(db.undecided_block_data_len().unwrap(), 2);
        assert_eq!(metrics.write_count(), 2);
        assert_eq!(metrics.write_bytes(), (first.len() + second.len()) as u64);

        let path = dir.path().join("legacy_success.redb");
        drop(db);
        let reopened = Db::new(path, 1024 * 1024, DbMetrics::new()).unwrap();
        reopened.initialize_schema().unwrap();
        assert_eq!(
            reopened
                .get_undecided_block_data(height, Round::new(0), first_id)
                .unwrap(),
            Some(first)
        );
        assert_eq!(
            reopened
                .get_undecided_block_data(height, Round::new(2), second_id)
                .unwrap(),
            Some(second)
        );
    }

    #[test]
    fn legacy_undecided_block_data_migration_new_database_creates_rollback_table() {
        let (db, _dir, _metrics) = create_test_db_with_metrics("new_v2_only");

        assert!(has_table(&db, "undecided_block_data"));
        assert!(has_table(&db, "undecided_block_data_v2"));
    }

    #[test]
    fn legacy_undecided_block_data_migration_conflict_rolls_back() {
        let height = Height::new(9);
        let value_id = ValueId::new(21);
        let (db, _dir, metrics) = create_legacy_test_db(
            "legacy_conflict",
            &[
                (
                    height,
                    Round::new(0),
                    value_id,
                    Bytes::from_static(b"first"),
                ),
                (
                    height,
                    Round::new(1),
                    value_id,
                    Bytes::from_static(b"different"),
                ),
            ],
        );

        let error = db.initialize_schema().unwrap_err();
        let message = error.to_string();
        assert!(message.contains("existing round 0"), "{message}");
        assert!(message.contains("incoming round 1"), "{message}");
        assert!(message.contains("existing length 5"), "{message}");
        assert!(message.contains("incoming length 9"), "{message}");
        assert!(matches!(
            error,
            StoreError::ConflictingLegacyUndecidedBlockData {
                height: error_height,
                value_id: error_value_id,
                ..
            } if error_height == height && error_value_id == value_id
        ));
        assert!(has_table(&db, "undecided_block_data"));
        assert!(!has_table(&db, "undecided_block_data_v2"));
        assert_eq!(metrics.write_count(), 0);
        assert_eq!(metrics.write_bytes(), 0);
    }

    #[test]
    fn legacy_undecided_block_data_migration_merges_existing_v2_data() {
        let height = Height::new(9);
        let value_id = ValueId::new(21);
        let payload = Bytes::from_static(b"existing-payload");
        let (db, _dir, metrics) = create_legacy_test_db(
            "legacy_merge",
            &[
                (height, Round::new(0), value_id, payload.clone()),
                (height, Round::new(1), value_id, payload.clone()),
            ],
        );
        let tx = db.db.begin_write().unwrap();
        {
            let mut table = tx.open_table(UNDECIDED_BLOCK_DATA_TABLE).unwrap();
            table.insert((height, value_id), payload.to_vec()).unwrap();
        }
        tx.commit().unwrap();

        db.initialize_schema().unwrap();
        assert_eq!(db.undecided_block_data_len().unwrap(), 1);
        assert_eq!(metrics.write_count(), 0);
        assert_eq!(metrics.write_bytes(), 0);
        assert!(has_table(&db, "undecided_block_data"));
    }

    #[test]
    fn rollback_compatibility_runtime_write_is_readable_by_n_minus_one() {
        let (db, dir, _metrics) = create_test_db_with_metrics("rollback_runtime_write");
        let height = Height::new(12);
        let round = Round::new(3);
        let payload = Bytes::from_static(b"rollback-compatible-payload");
        let value = Value::new(payload.clone());
        let proposal: ProposedValue<EmeraldContext> = ProposedValue {
            height,
            round,
            valid_round: Round::Nil,
            proposer: Address::new([7; 20]),
            value: value.clone(),
            validity: Validity::Valid,
        };

        db.insert_undecided_proposal(proposal).unwrap();
        db.insert_undecided_block_data(height, round, value.id(), payload.clone())
            .unwrap();

        assert_eq!(
            get_legacy_undecided_block_data(&db, height, round, value.id()),
            Some(payload.clone())
        );

        let path = dir.path().join("rollback_runtime_write.redb");
        drop(db);

        let n_minus_one = Db::new(&path, 1024 * 1024, DbMetrics::new()).unwrap();
        assert_eq!(
            get_legacy_undecided_block_data(&n_minus_one, height, round, value.id()),
            Some(payload.clone())
        );
        drop(n_minus_one);

        let n = Db::new(path, 1024 * 1024, DbMetrics::new()).unwrap();
        n.initialize_schema().unwrap();
        assert_eq!(
            n.get_undecided_block_data(height, round, value.id())
                .unwrap(),
            Some(payload)
        );
    }

    #[test]
    fn rollback_compatibility_backfills_legacy_rows_for_an_existing_v2_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollback_v2_backfill.redb");
        let metrics = DbMetrics::new();
        let db = Db::new(&path, 1024 * 1024, metrics.clone()).unwrap();
        let height = Height::new(14);
        let round = Round::new(5);
        let payload = Bytes::from_static(b"pre-compatibility-v2-payload");
        let value = Value::new(payload.clone());
        let proposal: ProposedValue<EmeraldContext> = ProposedValue {
            height,
            round,
            valid_round: Round::Nil,
            proposer: Address::new([8; 20]),
            value: value.clone(),
            validity: Validity::Valid,
        };

        let tx = db.db.begin_write().unwrap();
        {
            let mut primary = tx.open_table(UNDECIDED_BLOCK_DATA_TABLE).unwrap();
            primary
                .insert((height, value.id()), payload.to_vec())
                .unwrap();
            let mut proposals = tx.open_table(UNDECIDED_PROPOSALS_TABLE).unwrap();
            proposals
                .insert(
                    (height, round, value.id()),
                    ProtobufCodec.encode(&proposal).unwrap().to_vec(),
                )
                .unwrap();
        }
        tx.commit().unwrap();
        assert!(!has_table(&db, "undecided_block_data"));

        db.initialize_schema().unwrap();

        assert_eq!(
            get_legacy_undecided_block_data(&db, height, round, value.id()),
            Some(payload.clone())
        );
        let key = (height, round, value.id());
        let compact_metadata = raw_compact_proposal(&db, key).unwrap();
        let full_proposal = raw_legacy_proposal(&db, key).unwrap();
        assert_eq!(metrics.write_count(), 3);
        assert_eq!(
            metrics.write_bytes(),
            (payload.len()
                + compact_metadata.len()
                + full_proposal.len()) as u64
        );
        drop(db);

        let n_minus_one = Db::new(path, 1024 * 1024, DbMetrics::new()).unwrap();
        assert_eq!(
            get_legacy_undecided_block_data(&n_minus_one, height, round, value.id()),
            Some(payload)
        );
    }

    #[test]
    fn rollback_compatibility_read_falls_back_to_the_exact_legacy_round() {
        let height = Height::new(15);
        let round = Round::new(6);
        let payload = Bytes::from_static(b"legacy-fallback-payload");
        let value_id = Value::new(payload.clone()).id();
        let (db, _dir, _metrics) = create_legacy_test_db(
            "rollback_legacy_fallback",
            &[(height, round, value_id, payload.clone())],
        );
        db.initialize_schema().unwrap();

        let tx = db.db.begin_write().unwrap();
        tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)
            .unwrap()
            .remove(&(height, value_id))
            .unwrap();
        tx.commit().unwrap();

        assert_eq!(
            db.get_undecided_block_data(height, round, value_id)
                .unwrap(),
            Some(payload)
        );
        assert!(db
            .get_undecided_block_data(height, Round::new(7), value_id)
            .unwrap()
            .is_none());
    }

    #[test]
    fn reconciliation_preserves_full_n_minus_one_rows_and_populates_compact_v2() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reconcile-full-v1.redb");
        let db = Db::new(&path, 1024 * 1024, DbMetrics::new()).unwrap();
        let payload = make_execution_payload_bytes(61);
        let proposal: ProposedValue<EmeraldContext> = ProposedValue {
            height: Height::new(61),
            round: Round::new(2),
            valid_round: Round::new(1),
            proposer: Address::new([6; 20]),
            value: Value::new(payload.clone()),
            validity: Validity::Valid,
        };
        let key = (proposal.height, proposal.round, proposal.value.id());
        insert_raw_legacy_full_proposal_and_payload(&db, &proposal, &payload);
        let full = raw_legacy_proposal(&db, key).unwrap();

        db.initialize_schema().unwrap();
        assert_eq!(raw_legacy_proposal(&db, key), Some(full));
        let compact = raw_compact_proposal(&db, key).unwrap();
        assert!(compact.len() < 128);
        assert!(decode_stored_proposal(Bytes::from(compact))
            .unwrap()
            .embedded_payload
            .is_none());
        assert_eq!(reconciliation_version(&db), Some(1));
        assert_eq!(
            db.get_undecided_proposal(proposal.height, proposal.round, proposal.value.id())
                .unwrap(),
            Some(proposal.clone()),
        );
        drop(db);

        let reopened = Db::new(path, 1024 * 1024, DbMetrics::new()).unwrap();
        reopened.initialize_schema().unwrap();
        assert_eq!(
            reopened
                .get_undecided_proposal(proposal.height, proposal.round, proposal.value.id())
                .unwrap(),
            Some(proposal),
        );
    }

    #[test]
    fn reconciliation_restores_current_pr_compact_rows_to_full_v1() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reconcile-current-pr-compact.redb");
        let db = Db::new(&path, 1024 * 1024, DbMetrics::new()).unwrap();
        let payload = make_execution_payload_bytes(62);
        let proposal: ProposedValue<EmeraldContext> = ProposedValue {
            height: Height::new(62),
            round: Round::new(3),
            valid_round: Round::new(1),
            proposer: Address::new([7; 20]),
            value: Value::new(payload.clone()),
            validity: Validity::Valid,
        };
        let key = (proposal.height, proposal.round, proposal.value.id());
        let compact = StoredProposalMetadata::from_proposal(&proposal)
            .encode()
            .unwrap()
            .to_vec();
        insert_raw_legacy_proposal(&db, key, compact.clone());
        insert_raw_proposal(&db, key, compact.clone());
        insert_raw_legacy_payload(&db, key, &payload);

        db.initialize_schema().unwrap();

        let restored = raw_legacy_proposal(&db, key).unwrap();
        let restored_proto = proto::ProposedValue::decode(restored.as_slice()).unwrap();
        assert_eq!(
            decode_value_like_n_minus_one(restored_proto.value.unwrap()).extensions,
            payload
        );
        assert_eq!(raw_compact_proposal(&db, key), Some(compact));
        assert_eq!(reconciliation_version(&db), Some(1));
    }

    #[test]
    fn reconciliation_failure_leaves_marker_and_all_tables_unchanged() {
        let cases = [
            ("missing_payload", "missing shared payload"),
            (
                "key_mismatch",
                "stored proposal key does not match its metadata",
            ),
            (
                "value_id_mismatch",
                "stored proposal metadata cannot be decoded",
            ),
            (
                "embedded_payload_conflict",
                "embedded proposal payload does not match shared storage",
            ),
            (
                "malformed_metadata",
                "stored proposal metadata cannot be decoded",
            ),
        ];

        for (index, (case, expected)) in cases.into_iter().enumerate() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(format!("proposal_metadata_{case}.redb"));
            let db = Db::new(&path, 1024 * 1024, DbMetrics::new()).unwrap();
            let payload = make_execution_payload_bytes(70 + index as u64);
            let proposal: ProposedValue<EmeraldContext> = ProposedValue {
                height: Height::new(70 + index as u64),
                round: Round::new(3),
                valid_round: Round::new(1),
                proposer: Address::new([index as u8; 20]),
                value: Value::new(payload.clone()),
                validity: Validity::Valid,
            };
            let mut key = (proposal.height, proposal.round, proposal.value.id());
            let mut encoded = ProtobufCodec.encode(&proposal).unwrap().to_vec();
            let mut stored_payload = Some(payload.clone());

            match case {
                "missing_payload" => stored_payload = None,
                "key_mismatch" => key.1 = Round::new(4),
                "value_id_mismatch" => {
                    let mut stored = proto::ProposedValue::decode(encoded.as_slice()).unwrap();
                    let value = stored.value.as_mut().unwrap();
                    let mut value_bytes = value.value.take().unwrap().to_vec();
                    value_bytes[..8].copy_from_slice(
                        &proposal.value.id().as_u64().wrapping_add(1).to_be_bytes(),
                    );
                    value.value = Some(Bytes::from(value_bytes));
                    encoded = stored.encode_to_vec();
                }
                "embedded_payload_conflict" => {
                    stored_payload = Some(make_execution_payload_bytes(170 + index as u64));
                }
                "malformed_metadata" => encoded = b"malformed-proposal".to_vec(),
                _ => unreachable!(),
            }

            insert_raw_legacy_proposal(&db, key, encoded.clone());
            if let Some(stored_payload) = stored_payload.as_ref() {
                insert_raw_legacy_payload(&db, key, stored_payload);
            }
            let before = raw_storage_snapshot(&db, key);

            let error = db.initialize_schema().expect_err(case);
            assert!(
                error.to_string().contains(expected),
                "case {case}: unexpected error: {error}"
            );
            assert_eq!(raw_storage_snapshot(&db, key), before, "case {case}");
        }
    }

    fn decode_value_like_n_minus_one(value: proto::Value) -> Value {
        let bytes = value.value.unwrap();
        Value {
            value: u64::from_be_bytes(bytes[..8].try_into().unwrap()),
            extensions: bytes.slice(8..),
        }
    }

    #[test]
    fn proposal_runtime_dual_writes_wire_complete_v1_and_compact_v2() {
        let (db, _dir) = create_test_db("proposal-dual-write");
        let payload = make_execution_payload_bytes(81);
        let proposal: ProposedValue<EmeraldContext> = ProposedValue {
            height: Height::new(81),
            round: Round::new(4),
            valid_round: Round::new(2),
            proposer: Address::new([8; 20]),
            value: Value::new(payload.clone()),
            validity: Validity::Valid,
        };
        let key = (proposal.height, proposal.round, proposal.value.id());

        db.insert_undecided_block_data(key.0, key.1, key.2, payload.clone())
            .unwrap();
        db.insert_undecided_proposal(proposal.clone()).unwrap();

        let legacy = raw_legacy_proposal(&db, key).unwrap();
        let legacy_proto = proto::ProposedValue::decode(legacy.as_slice()).unwrap();
        let legacy_value = decode_value_like_n_minus_one(legacy_proto.value.unwrap());
        assert_eq!(legacy_value.extensions, payload);

        let compact = raw_compact_proposal(&db, key).unwrap();
        assert!(compact.len() < 128);
        assert!(decode_stored_proposal(Bytes::from(compact))
            .unwrap()
            .embedded_payload
            .is_none());
        assert_eq!(
            db.get_undecided_proposal(key.0, key.1, key.2).unwrap(),
            Some(proposal)
        );
    }

    #[test]
    fn proposal_read_falls_back_to_full_v1_written_after_reconciliation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proposal-v1-fallback.redb");
        let db = Db::new(&path, 1024 * 1024, DbMetrics::new()).unwrap();
        db.initialize_schema().unwrap();

        let payload = make_execution_payload_bytes(83);
        let proposal: ProposedValue<EmeraldContext> = ProposedValue {
            height: Height::new(83),
            round: Round::new(6),
            valid_round: Round::new(4),
            proposer: Address::new([9; 20]),
            value: Value::new(payload.clone()),
            validity: Validity::Valid,
        };
        let key = (proposal.height, proposal.round, proposal.value.id());
        insert_raw_legacy_proposal(
            &db,
            key,
            ProtobufCodec.encode(&proposal).unwrap().to_vec(),
        );
        insert_raw_legacy_payload(&db, key, &payload);

        let tx = db.db.begin_write().unwrap();
        tx.open_table(SCHEMA_METADATA_TABLE)
            .unwrap()
            .insert(
                UNDECIDED_STORAGE_RECONCILIATION_KEY,
                UNDECIDED_STORAGE_RECONCILIATION_VERSION,
            )
            .unwrap();
        tx.commit().unwrap();
        drop(db);

        let reopened = Db::new(path, 1024 * 1024, DbMetrics::new()).unwrap();
        reopened.initialize_schema().unwrap();
        assert!(raw_compact_proposal(&reopened, key).is_none());
        assert_eq!(
            reopened
                .get_undecided_proposal(key.0, key.1, key.2)
                .unwrap(),
            Some(proposal)
        );
    }

    #[test]
    fn rollback_compact_proposal_n_minus_one_commit_is_repaired_on_reupgrade() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollback_compact_commit.redb");
        let metrics = DbMetrics::new();
        let db = Db::new(&path, 1024 * 1024, metrics.clone()).unwrap();
        let height = Height::new(82);
        let round = Round::new(5);
        let payload = make_execution_payload_bytes(height.as_u64());
        let value = Value::new(payload.clone());
        let execution_payload = ExecutionPayloadV3::from_ssz_bytes(&payload).unwrap();
        let header = Bytes::from(extract_block_header(&execution_payload).as_ssz_bytes());
        let certificate = CommitCertificate {
            height,
            round,
            value_id: value.id(),
            commit_signatures: Vec::new(),
        };
        let proposal = ProposedValue {
            height,
            round,
            valid_round: Round::new(3),
            proposer: Address::new([9; 20]),
            value: value.clone(),
            validity: Validity::Valid,
        };
        let compact_proposal = StoredProposalMetadata::from_proposal(&proposal)
            .encode()
            .unwrap();
        let id_only_value = Value {
            value: value.id().as_u64(),
            extensions: Bytes::new(),
        }
        .to_bytes()
        .unwrap();

        let tx = db.db.begin_write().unwrap();
        tx.open_table(LEGACY_UNDECIDED_PROPOSALS_TABLE)
            .unwrap()
            .insert((height, round, value.id()), compact_proposal.to_vec())
            .unwrap();
        tx.open_table(LEGACY_UNDECIDED_BLOCK_DATA_TABLE)
            .unwrap()
            .insert((height, round, value.id()), payload.to_vec())
            .unwrap();
        tx.open_table(UNDECIDED_BLOCK_DATA_TABLE)
            .unwrap()
            .insert((height, value.id()), payload.to_vec())
            .unwrap();
        tx.open_table(DECIDED_VALUES_TABLE)
            .unwrap()
            .insert(height, id_only_value.to_vec())
            .unwrap();
        tx.open_table(CERTIFICATES_TABLE)
            .unwrap()
            .insert(height, encode_certificate(&certificate).unwrap())
            .unwrap();
        tx.open_table(DECIDED_BLOCK_HEADERS_TABLE)
            .unwrap()
            .insert(height, header.to_vec())
            .unwrap();
        tx.open_table(DECIDED_BLOCK_DATA_TABLE)
            .unwrap()
            .insert(height, payload.to_vec())
            .unwrap();
        tx.commit().unwrap();

        db.initialize_schema().unwrap();

        assert_eq!(
            db.get_decided_value(height).unwrap(),
            Some(DecidedValue {
                value: value.clone(),
                certificate: certificate.clone(),
            })
        );
        let key = (height, round, value.id());
        let full_proposal = raw_legacy_proposal(&db, key).unwrap();
        let compact_proposal = raw_compact_proposal(&db, key).unwrap();
        assert_eq!(metrics.write_count(), 3);
        assert_eq!(
            metrics.write_bytes(),
            (value.to_bytes().unwrap().len()
                + full_proposal.len()
                + compact_proposal.len()) as u64
        );
        drop(db);

        let reopened = Db::new(path, 1024 * 1024, DbMetrics::new()).unwrap();
        reopened.initialize_schema().unwrap();
        assert_eq!(
            reopened.get_decided_value(height).unwrap(),
            Some(DecidedValue { value, certificate })
        );
    }

    #[test]
    fn legacy_partial_commit_upgrade_recovers_decided_payload_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy_partial_commit.redb");
        let db = Db::new(&path, 1024 * 1024, DbMetrics::new()).unwrap();
        let height = Height::new(21);
        let round = Round::new(2);
        let payload = make_execution_payload_bytes(height.as_u64());
        let value = Value::new(payload.clone());
        let execution_payload = ExecutionPayloadV3::from_ssz_bytes(&payload).unwrap();
        let header = Bytes::from(extract_block_header(&execution_payload).as_ssz_bytes());
        let decided = DecidedValue {
            value,
            certificate: CommitCertificate {
                height,
                round,
                value_id: Value::new(payload.clone()).id(),
                commit_signatures: Vec::new(),
            },
        };
        insert_n_minus_one_partial_commit(&db, &decided, &header, Some(&payload));

        db.initialize_schema().unwrap();

        assert_eq!(
            db.get_decided_block_data(height).unwrap(),
            Some(payload.clone())
        );
        drop(db);

        let reopened = Db::new(path, 1024 * 1024, DbMetrics::new()).unwrap();
        reopened.initialize_schema().unwrap();
        assert_eq!(
            reopened.get_decided_block_data(height).unwrap(),
            Some(payload)
        );
    }

    #[test]
    fn legacy_partial_commit_upgrade_rejects_missing_payload_without_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy_partial_commit_missing.redb");
        let db = Db::new(&path, 1024 * 1024, DbMetrics::new()).unwrap();
        let height = Height::new(22);
        let payload = make_execution_payload_bytes(height.as_u64());
        let value = Value::new(payload.clone());
        let execution_payload = ExecutionPayloadV3::from_ssz_bytes(&payload).unwrap();
        let header = Bytes::from(extract_block_header(&execution_payload).as_ssz_bytes());
        let decided = DecidedValue {
            certificate: CommitCertificate {
                height,
                round: Round::new(3),
                value_id: value.id(),
                commit_signatures: Vec::new(),
            },
            value,
        };
        insert_n_minus_one_partial_commit(&db, &decided, &header, None);

        let error = db
            .initialize_schema()
            .expect_err("missing recovery payload must fail");

        assert!(error.to_string().contains("missing its execution payload"));
        assert!(!has_table(&db, "undecided_block_data_v2"));
        assert!(db
            .get_decided_block_data(height)
            .unwrap_err()
            .to_string()
            .contains("does not exist"));
    }

    #[test]
    fn legacy_partial_commit_upgrade_rejects_header_mismatch_without_promotion() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy_partial_commit_header.redb");
        let db = Db::new(&path, 1024 * 1024, DbMetrics::new()).unwrap();
        let height = Height::new(23);
        let payload = make_execution_payload_bytes(height.as_u64());
        let value = Value::new(payload.clone());
        let decided = DecidedValue {
            certificate: CommitCertificate {
                height,
                round: Round::new(4),
                value_id: value.id(),
                commit_signatures: Vec::new(),
            },
            value,
        };
        let wrong_header = Bytes::from_static(b"wrong-header");
        insert_n_minus_one_partial_commit(&db, &decided, &wrong_header, Some(&payload));

        let error = db
            .initialize_schema()
            .expect_err("mismatched header must fail recovery");

        assert!(error.to_string().contains("stored header does not match"));
        assert!(!has_table(&db, "undecided_block_data_v2"));
        assert!(db
            .get_decided_block_data(height)
            .unwrap_err()
            .to_string()
            .contains("does not exist"));
    }

    #[test]
    fn legacy_partial_commit_upgrade_rejects_certificate_value_id_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy_partial_commit_value_id.redb");
        let db = Db::new(&path, 1024 * 1024, DbMetrics::new()).unwrap();
        let height = Height::new(24);
        let payload = make_execution_payload_bytes(height.as_u64());
        let value = Value::new(payload.clone());
        let execution_payload = ExecutionPayloadV3::from_ssz_bytes(&payload).unwrap();
        let header = Bytes::from(extract_block_header(&execution_payload).as_ssz_bytes());
        let decided = DecidedValue {
            certificate: CommitCertificate {
                height,
                round: Round::new(5),
                value_id: ValueId::new(value.id().as_u64().wrapping_add(1)),
                commit_signatures: Vec::new(),
            },
            value,
        };
        insert_n_minus_one_partial_commit(&db, &decided, &header, Some(&payload));

        let error = db
            .initialize_schema()
            .expect_err("certificate/value mismatch must fail recovery");

        assert!(error
            .to_string()
            .contains("certificate value ID does not match"));
        assert!(!has_table(&db, "undecided_block_data_v2"));
    }

    #[test]
    fn legacy_partial_commit_upgrade_rejects_malformed_execution_payload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy_partial_commit_malformed.redb");
        let db = Db::new(&path, 1024 * 1024, DbMetrics::new()).unwrap();
        let height = Height::new(25);
        let payload = Bytes::from_static(b"not-an-ssz-execution-payload");
        let value = Value::new(payload.clone());
        let decided = DecidedValue {
            certificate: CommitCertificate {
                height,
                round: Round::new(6),
                value_id: value.id(),
                commit_signatures: Vec::new(),
            },
            value,
        };
        insert_n_minus_one_partial_commit(
            &db,
            &decided,
            &Bytes::from_static(b"untrusted-header"),
            Some(&payload),
        );

        let error = db
            .initialize_schema()
            .expect_err("malformed execution payload must fail recovery");

        assert!(error
            .to_string()
            .contains("execution payload cannot be decoded"));
        assert!(!has_table(&db, "undecided_block_data_v2"));
    }

    #[test]
    fn legacy_partial_commit_upgrade_rejects_existing_decided_payload_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("legacy_partial_commit_existing_conflict.redb");
        let db = Db::new(&path, 1024 * 1024, DbMetrics::new()).unwrap();
        let height = Height::new(26);
        let payload = make_execution_payload_bytes(height.as_u64());
        let conflicting_payload = make_execution_payload_bytes(height.as_u64() + 1);
        let value = Value::new(payload.clone());
        let execution_payload = ExecutionPayloadV3::from_ssz_bytes(&payload).unwrap();
        let header = Bytes::from(extract_block_header(&execution_payload).as_ssz_bytes());
        let decided = DecidedValue {
            certificate: CommitCertificate {
                height,
                round: Round::new(7),
                value_id: value.id(),
                commit_signatures: Vec::new(),
            },
            value,
        };
        insert_n_minus_one_partial_commit(&db, &decided, &header, Some(&payload));
        let tx = db.db.begin_write().unwrap();
        tx.open_table(DECIDED_BLOCK_DATA_TABLE)
            .unwrap()
            .insert(height, conflicting_payload.to_vec())
            .unwrap();
        tx.commit().unwrap();

        let error = db
            .initialize_schema()
            .expect_err("an existing decided payload must be revalidated");

        assert!(error
            .to_string()
            .contains("execution payload does not match the stored value ID and bytes"));
        assert_eq!(
            db.get_decided_block_data(height).unwrap(),
            Some(conflicting_payload)
        );
        assert!(!has_table(&db, "undecided_block_data_v2"));
    }

    fn make_decided_state(height: u64) -> (DecidedValue, Bytes, Bytes) {
        let payload = make_execution_payload_bytes(height);
        let value = Value::new(payload.clone());
        let execution_payload = ExecutionPayloadV3::from_ssz_bytes(&payload).unwrap();
        let header = Bytes::from(extract_block_header(&execution_payload).as_ssz_bytes());
        let decided = DecidedValue {
            certificate: CommitCertificate {
                height: Height::new(height),
                round: Round::new(2),
                value_id: value.id(),
                commit_signatures: Vec::new(),
            },
            value,
        };
        (decided, header, payload)
    }

    #[test]
    fn decided_state_commit_is_atomic_and_idempotent() {
        let (db, _dir, metrics) = create_test_db_with_metrics("decided_state_atomic");
        let (decided, header, payload) = make_decided_state(31);

        db.insert_decided_state(decided.clone(), header.clone(), payload.clone())
            .unwrap();
        let writes = metrics.write_count();
        let bytes = metrics.write_bytes();
        assert_eq!(writes, 4);

        db.insert_decided_state(decided.clone(), header.clone(), payload.clone())
            .unwrap();

        assert_eq!(metrics.write_count(), writes);
        assert_eq!(metrics.write_bytes(), bytes);
        assert_eq!(
            db.get_decided_value(decided.certificate.height).unwrap(),
            Some(decided)
        );
        assert_eq!(
            db.get_decided_block_data(Height::new(31)).unwrap(),
            Some(payload)
        );
        assert_eq!(
            db.get_certificate_and_header(Height::new(31))
                .unwrap()
                .unwrap()
                .1,
            header
        );
    }

    #[test]
    fn decided_state_commit_conflicts_abort_every_component_atomically() {
        for (offset, component) in ["value", "certificate", "header"].into_iter().enumerate() {
            let (db, _dir, metrics) =
                create_test_db_with_metrics(&format!("decided_state_{component}_conflict"));
            let height = Height::new(40 + offset as u64);
            let (decided, header, payload) = make_decided_state(height.as_u64());
            let conflict = b"conflicting-existing-row".to_vec();
            let tx = db.db.begin_write().unwrap();
            match component {
                "value" => {
                    tx.open_table(DECIDED_VALUES_TABLE)
                        .unwrap()
                        .insert(height, conflict.clone())
                        .unwrap();
                }
                "certificate" => {
                    tx.open_table(CERTIFICATES_TABLE)
                        .unwrap()
                        .insert(height, conflict.clone())
                        .unwrap();
                }
                "header" => {
                    tx.open_table(DECIDED_BLOCK_HEADERS_TABLE)
                        .unwrap()
                        .insert(height, conflict.clone())
                        .unwrap();
                }
                _ => unreachable!(),
            }
            tx.commit().unwrap();

            let error = db
                .insert_decided_state(decided, header, payload)
                .expect_err("a conflicting component must abort the whole transaction");
            assert!(matches!(
                error,
                StoreError::ConflictingDecidedState {
                    height: error_height,
                    component: error_component,
                } if error_height == height && error_component == component
            ));
            assert_eq!(metrics.write_count(), 0);

            let tx = db.db.begin_read().unwrap();
            assert!(tx
                .open_table(DECIDED_BLOCK_DATA_TABLE)
                .unwrap()
                .get(&height)
                .unwrap()
                .is_none());
            let stored_value = tx
                .open_table(DECIDED_VALUES_TABLE)
                .unwrap()
                .get(&height)
                .unwrap()
                .map(|value| value.value());
            let stored_certificate = tx
                .open_table(CERTIFICATES_TABLE)
                .unwrap()
                .get(&height)
                .unwrap()
                .map(|value| value.value());
            let stored_header = tx
                .open_table(DECIDED_BLOCK_HEADERS_TABLE)
                .unwrap()
                .get(&height)
                .unwrap()
                .map(|value| value.value());
            assert_eq!(
                stored_value,
                (component == "value").then_some(conflict.clone())
            );
            assert_eq!(
                stored_certificate,
                (component == "certificate").then_some(conflict.clone())
            );
            assert_eq!(stored_header, (component == "header").then_some(conflict));
        }
    }

    #[test]
    fn decided_state_commit_failpoints_publish_no_partial_rows_after_reopen() {
        for failpoint in DecidedCommitFailpoint::ALL {
            let dir = tempfile::tempdir().unwrap();
            let path = dir
                .path()
                .join(format!("decided_state_failpoint_{failpoint:?}.redb"));
            let db = Db::new(&path, 1024 * 1024, DbMetrics::new()).unwrap();
            db.initialize_schema().unwrap();
            let (decided, header, payload) = make_decided_state(32);

            db.insert_decided_state_with_failpoint(decided, header, payload, failpoint)
                .expect_err("injected commit failure must abort the transaction");
            drop(db);

            let reopened = Db::new(&path, 1024 * 1024, DbMetrics::new()).unwrap();
            reopened.initialize_schema().unwrap();
            assert_no_decided_rows(&reopened, Height::new(32));
            assert!(reopened
                .get_decided_value(Height::new(32))
                .unwrap()
                .is_none());
            assert!(reopened
                .get_decided_block_data(Height::new(32))
                .unwrap()
                .is_none());
            assert!(reopened
                .get_certificate_and_header(Height::new(32))
                .unwrap()
                .is_none());
        }
    }

    #[test]
    fn undecided_block_data_deduplicates_identical_payloads() {
        let (db, dir, metrics) = create_test_db_with_metrics("deduplicate_payload");
        let height = Height::new(7);
        let value_id = ValueId::new(11);
        let payload = Bytes::from_static(b"one-payload");

        db.insert_undecided_block_data(height, Round::new(0), value_id, payload.clone())
            .unwrap();
        db.insert_undecided_block_data(height, Round::new(0), value_id, payload.clone())
            .unwrap();

        assert_eq!(db.undecided_block_data_len().unwrap(), 1);
        assert_eq!(metrics.write_count(), 2);
        assert_eq!(metrics.write_bytes(), (payload.len() * 2) as u64);

        let path = dir.path().join("deduplicate_payload.redb");
        drop(db);
        let reopened = Db::new(path, 1024 * 1024, DbMetrics::new()).unwrap();
        reopened.initialize_schema().unwrap();
        assert_eq!(
            reopened
                .get_undecided_block_data(height, Round::new(0), value_id)
                .unwrap(),
            Some(payload)
        );
    }

    #[test]
    fn undecided_block_data_large_duplicate_does_not_add_physical_writes() {
        let (db, _dir, metrics) = create_test_db_with_metrics("large_duplicate_payload");
        let height = Height::new(8);
        let round = Round::new(1);
        let payload = Bytes::from(vec![0xAB; 4 * 1024 * 1024]);
        let value_id = Value::new(payload.clone()).id();

        db.insert_undecided_block_data(height, round, value_id, payload.clone())
            .unwrap();
        let writes = metrics.write_count();
        let bytes = metrics.write_bytes();

        db.insert_undecided_block_data(height, round, value_id, payload)
            .unwrap();

        assert_eq!(db.undecided_block_data_len().unwrap(), 1);
        assert_eq!(metrics.write_count(), writes);
        assert_eq!(metrics.write_bytes(), bytes);
    }

    #[test]
    fn undecided_block_data_keeps_distinct_values_at_one_height() {
        let (db, _dir, _metrics) = create_test_db_with_metrics("distinct_payloads");
        let height = Height::new(7);

        db.insert_undecided_block_data(
            height,
            Round::new(0),
            ValueId::new(11),
            Bytes::from_static(b"first"),
        )
        .unwrap();
        db.insert_undecided_block_data(
            height,
            Round::new(0),
            ValueId::new(12),
            Bytes::from_static(b"second"),
        )
        .unwrap();

        assert_eq!(db.undecided_block_data_len().unwrap(), 2);
    }

    #[test]
    fn undecided_block_data_rejects_conflicting_bytes_without_overwrite() {
        let (db, _dir, metrics) = create_test_db_with_metrics("conflicting_payload");
        let height = Height::new(7);
        let value_id = ValueId::new(11);
        let original = Bytes::from_static(b"original");

        db.insert_undecided_block_data(height, Round::new(0), value_id, original.clone())
            .unwrap();
        let error = db
            .insert_undecided_block_data(
                height,
                Round::new(0),
                value_id,
                Bytes::from_static(b"conflict"),
            )
            .unwrap_err();

        assert!(matches!(
            error,
            StoreError::ConflictingUndecidedBlockData {
                height: error_height,
                value_id: error_value_id,
            } if error_height == height && error_value_id == value_id
        ));
        assert_eq!(
            db.get_undecided_block_data(height, Round::new(0), value_id)
                .unwrap(),
            Some(original.clone())
        );
        assert_eq!(metrics.write_count(), 2);
        assert_eq!(metrics.write_bytes(), (original.len() * 2) as u64);
    }

    #[test]
    fn undecided_proposal_duplicate_does_not_increment_write_metrics() {
        let (db, _dir, metrics) = create_test_db_with_metrics("duplicate_proposal_metrics");
        let proposal = make_proposed_value(7);

        db.insert_undecided_proposal(proposal.clone()).unwrap();
        let writes = metrics.write_count();
        let bytes = metrics.write_bytes();
        db.insert_undecided_proposal(proposal).unwrap();

        assert_eq!(metrics.write_count(), writes);
        assert_eq!(metrics.write_bytes(), bytes);
    }

    #[test]
    fn compact_proposal_metadata_runtime_stores_one_primary_payload_copy_across_rounds() {
        let (db, _dir, _metrics) = create_test_db_with_metrics("compact_runtime_rounds");
        let payload = Bytes::from(vec![0xCD; 4 * 1024 * 1024]);
        let value = Value::new(payload.clone());

        for round in 0..4 {
            let round = Round::new(round);
            db.insert_undecided_block_data(Height::new(51), round, value.id(), payload.clone())
                .unwrap();
            let proposal = ProposedValue {
                height: Height::new(51),
                round,
                valid_round: if round == Round::new(0) {
                    Round::Nil
                } else {
                    Round::new(round.as_u32().unwrap() - 1)
                },
                proposer: Address::new([round.as_u32().unwrap() as u8; 20]),
                value: value.clone(),
                validity: Validity::Valid,
            };
            db.insert_undecided_proposal(proposal.clone()).unwrap();

            let raw = raw_compact_proposal(
                &db,
                (proposal.height, proposal.round, proposal.value.id()),
            )
            .unwrap();
            assert!(raw.len() < 128);
            assert!(!raw
                .windows(payload.len())
                .any(|window| window == payload.as_ref()));
            assert_eq!(
                db.get_undecided_proposal(proposal.height, proposal.round, value.id())
                    .unwrap(),
                Some(proposal),
            );
        }

        assert_eq!(db.undecided_block_data_len().unwrap(), 1);
    }

    #[test]
    fn compact_proposal_metadata_read_rejects_missing_shared_payload() {
        let (db, _dir) = create_test_db("compact_missing_payload");
        let proposal = make_proposed_value(52);
        db.insert_undecided_proposal(proposal.clone()).unwrap();

        let error = db
            .get_undecided_proposal(proposal.height, proposal.round, proposal.value.id())
            .unwrap_err();
        assert!(error.to_string().contains("missing shared payload"));
    }

    #[test]
    fn decided_block_data_duplicate_does_not_increment_write_metrics() {
        let (db, _dir, metrics) = create_test_db_with_metrics("duplicate_decided_metrics");
        let height = Height::new(7);
        let payload = Bytes::from_static(b"decided-payload");

        db.insert_decided_block_data(height, payload.clone())
            .unwrap();
        let writes = metrics.write_count();
        let bytes = metrics.write_bytes();
        db.insert_decided_block_data(height, payload).unwrap();

        assert_eq!(metrics.write_count(), writes);
        assert_eq!(metrics.write_bytes(), bytes);
    }

    #[test]
    fn decided_block_data_rejects_conflicting_bytes_without_overwrite() {
        let (db, _dir, metrics) = create_test_db_with_metrics("conflicting_decided_payload");
        let height = Height::new(7);
        let original = Bytes::from_static(b"original");

        db.insert_decided_block_data(height, original.clone())
            .unwrap();
        let error = db
            .insert_decided_block_data(height, Bytes::from_static(b"conflict"))
            .unwrap_err();

        assert!(
            error.to_string().contains("Conflicting decided block data"),
            "unexpected error: {error}"
        );
        assert_eq!(
            db.get_decided_block_data(height).unwrap(),
            Some(original.clone())
        );
        assert_eq!(metrics.write_count(), 1);
        assert_eq!(metrics.write_bytes(), original.len() as u64);
    }

    /// Build a minimal ProposedValue for a given height.
    fn make_proposed_value(height: u64) -> ProposedValue<EmeraldContext> {
        let value = Value::new(Bytes::from(vec![height as u8; 10]));
        ProposedValue {
            height: Height::new(height),
            round: Round::new(0),
            valid_round: Round::Nil,
            proposer: Address::new([height as u8; 20]),
            value,
            validity: Validity::Valid,
        }
    }

    #[test]
    fn test_prune() {
        let (db, _dir) = create_test_db("prune_test");

        // --- Populate all tables at heights 1, 2, 3 ---
        for h in 1..=4u64 {
            // Decided values table + certificates table + block headers table
            let (decided, header, payload) = make_decided_state(h);
            db.insert_decided_state(decided, header, payload).unwrap();

            // Undecided proposals table
            let proposal = make_proposed_value(h);
            db.insert_undecided_block_data(
                proposal.height,
                proposal.round,
                proposal.value.id(),
                proposal.value.extensions.clone(),
            )
            .unwrap();
            db.insert_undecided_proposal(proposal).unwrap();

            // Undecided block data table
            db.insert_undecided_block_data(
                Height::new(h),
                Round::new(0),
                ValueId::new(h),
                Bytes::from(vec![h as u8; 40]),
            )
            .unwrap();
        }

        db.insert_undecided_block_data(
            Height::new(3),
            Round::new(0),
            ValueId::new(33),
            Bytes::from_static(b"second-surviving-payload"),
        )
        .unwrap();

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
                db.get_undecided_block_data(Height::new(h), Round::new(0), ValueId::new(h),)
                    .unwrap()
                    .is_some(),
                "block data at height {h} should exist before pruning"
            );
            let proposal = make_proposed_value(h);
            let key = (proposal.height, proposal.round, proposal.value.id());
            assert!(raw_legacy_proposal(&db, key).is_some());
            assert!(raw_compact_proposal(&db, key).is_some());
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
        assert!(
            db.get_decided_block_data(Height::new(3)).unwrap().is_some(),
            "decided block data at height 3 should survive"
        );
        assert!(
            db.get_decided_block_data(Height::new(2)).unwrap().is_none(),
            "decided block data at height 2 should not survive (retain height = 3)"
        );
        assert!(
            db.get_decided_block_data(Height::new(1)).unwrap().is_none(),
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
            db.get_undecided_block_data(Height::new(3), Round::new(0), ValueId::new(3))
                .unwrap()
                .is_some(),
            "undecided block data at height 3 should survive"
        );
        assert!(
            db.get_undecided_block_data(Height::new(2), Round::new(0), ValueId::new(2))
                .unwrap()
                .is_none(),
            "undecided block data at height 2 should be pruned"
        );
        assert!(get_legacy_undecided_block_data(
            &db,
            Height::new(2),
            Round::new(0),
            ValueId::new(2),
        )
        .is_none());
        assert!(
            db.get_undecided_block_data(Height::new(1), Round::new(0), ValueId::new(1))
                .unwrap()
                .is_none(),
            "undecided block data at height 1 should be pruned"
        );
        assert!(
            db.get_undecided_block_data(Height::new(3), Round::new(0), ValueId::new(33))
                .unwrap()
                .is_some(),
            "distinct undecided block data at height 3 should survive"
        );
        assert!(get_legacy_undecided_block_data(
            &db,
            Height::new(3),
            Round::new(0),
            ValueId::new(33),
        )
        .is_some());

        // === Undecided proposals (retain height = 3, heights > 2 survive) ===
        assert!(
            !db.get_undecided_proposals(Height::new(3), Round::new(0))
                .unwrap()
                .is_empty(),
            "undecided proposals at height 3 should survive"
        );
        let retained = make_proposed_value(3);
        let retained_key = (
            retained.height,
            retained.round,
            retained.value.id(),
        );
        assert!(raw_legacy_proposal(&db, retained_key).is_some());
        assert!(raw_compact_proposal(&db, retained_key).is_some());
        assert_eq!(
            db.get_undecided_proposals(Height::new(3), Round::new(0))
                .unwrap(),
            vec![retained]
        );
        assert!(
            db.get_undecided_proposals(Height::new(2), Round::new(0))
                .unwrap()
                .is_empty(),
            "undecided proposals at height 2 should be pruned"
        );
        let pruned = make_proposed_value(2);
        let pruned_key = (pruned.height, pruned.round, pruned.value.id());
        assert!(raw_legacy_proposal(&db, pruned_key).is_none());
        assert!(raw_compact_proposal(&db, pruned_key).is_none());
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
            let (decided, header, payload) = make_decided_state(h);
            db.insert_decided_state(decided, header, payload).unwrap();
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
        let (decided, header, payload) = make_decided_state(1);
        db.insert_decided_state(decided, header, payload).unwrap();

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
        let (decided, header, payload) = make_decided_state(1);
        db.insert_decided_state(decided, header, payload).unwrap();

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
