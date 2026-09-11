use bytes::Bytes;
use malachitebft_app_channel::app::types::core::{Round, Validity};
use malachitebft_app_channel::app::types::ProposedValue;
use malachitebft_eth_types::{proto, Address, EmeraldContext, Height, Value, ValueId};
use malachitebft_proto::{Error as ProtoError, Protobuf};
use prost::Message;

const VALUE_ID_LEN: usize = u64::BITS as usize / 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct StoredProposalMetadata {
    pub height: Height,
    pub round: Round,
    pub valid_round: Round,
    pub proposer: Address,
    pub value_id: ValueId,
    pub validity: Validity,
}

impl StoredProposalMetadata {
    pub fn from_proposal(proposal: &ProposedValue<EmeraldContext>) -> Self {
        Self {
            height: proposal.height,
            round: proposal.round,
            valid_round: proposal.valid_round,
            proposer: proposal.proposer,
            value_id: proposal.value.id(),
            validity: proposal.validity,
        }
    }

    pub fn encode(&self) -> Result<Bytes, ProtoError> {
        let proto = proto::ProposedValue {
            height: self.height.as_u64(),
            round: self.round.as_u32().ok_or_else(|| {
                ProtoError::Other("Cannot store a proposal with nil round".to_owned())
            })?,
            valid_round: self.valid_round.as_u32(),
            proposer: Some(self.proposer.to_proto()?),
            value: Some(proto::Value {
                value: Some(Bytes::copy_from_slice(
                    &self.value_id.as_u64().to_be_bytes(),
                )),
            }),
            validity: self.validity.to_bool(),
        };
        Ok(Bytes::from(proto.encode_to_vec()))
    }

    pub fn hydrate(self, payload: Bytes) -> ProposedValue<EmeraldContext> {
        ProposedValue {
            height: self.height,
            round: self.round,
            valid_round: self.valid_round,
            proposer: self.proposer,
            value: Value::new(payload),
            validity: self.validity,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DecodedStoredProposal {
    pub metadata: StoredProposalMetadata,
    pub embedded_payload: Option<Bytes>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum DecodedStoredValue {
    Full(Value),
    IdOnly(ValueId),
}

pub(super) fn decode_stored_value(bytes: Bytes) -> Result<DecodedStoredValue, ProtoError> {
    let stored = proto::Value::decode(bytes)?;
    let value_bytes = stored
        .value
        .ok_or_else(|| ProtoError::missing_field::<proto::Value>("value"))?;

    if value_bytes.len() == VALUE_ID_LEN {
        let id = u64::from_be_bytes(
            value_bytes
                .as_ref()
                .try_into()
                .map_err(|_| ProtoError::Other("Failed to decode stored value ID".to_owned()))?,
        );
        Ok(DecodedStoredValue::IdOnly(ValueId::new(id)))
    } else {
        Value::from_proto(proto::Value {
            value: Some(value_bytes),
        })
        .map(DecodedStoredValue::Full)
    }
}

impl DecodedStoredProposal {
    pub fn hydrate_verified(
        self,
        key: (Height, Round, ValueId),
        payload: Bytes,
    ) -> Result<ProposedValue<EmeraldContext>, &'static str> {
        let (height, round, value_id) = key;
        if self.metadata.height != height
            || self.metadata.round != round
            || self.metadata.value_id != value_id
        {
            return Err("stored proposal key does not match its metadata");
        }
        if self
            .embedded_payload
            .is_some_and(|embedded| embedded != payload)
        {
            return Err("embedded proposal payload does not match shared storage");
        }
        if Value::new(payload.clone()).id() != value_id {
            return Err("shared payload does not match the stored value ID");
        }
        Ok(self.metadata.hydrate(payload))
    }
}

pub(super) fn decode_stored_proposal(bytes: Bytes) -> Result<DecodedStoredProposal, ProtoError> {
    let stored = proto::ProposedValue::decode(bytes)?;
    let proposer = stored
        .proposer
        .ok_or_else(|| ProtoError::missing_field::<proto::ProposedValue>("proposer"))?;
    let value = stored
        .value
        .ok_or_else(|| ProtoError::missing_field::<proto::ProposedValue>("value"))?;
    let value_bytes = value
        .value
        .ok_or_else(|| ProtoError::missing_field::<proto::Value>("value"))?;

    let (value_id, embedded_payload) = if value_bytes.len() == VALUE_ID_LEN {
        let id = u64::from_be_bytes(value_bytes.as_ref().try_into().map_err(|_| {
            ProtoError::Other("Failed to decode stored proposal value ID".to_owned())
        })?);
        (ValueId::new(id), None)
    } else {
        let value = Value::from_proto(proto::Value {
            value: Some(value_bytes),
        })?;
        (value.id(), Some(value.extensions))
    };

    Ok(DecodedStoredProposal {
        metadata: StoredProposalMetadata {
            height: Height::new(stored.height),
            round: Round::new(stored.round),
            valid_round: stored.valid_round.map(Round::new).unwrap_or(Round::Nil),
            proposer: Address::from_proto(proposer)?,
            value_id,
            validity: Validity::from_bool(stored.validity),
        },
        embedded_payload,
    })
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use malachitebft_app_channel::app::types::codec::Codec;
    use malachitebft_app_channel::app::types::core::{Round, Validity};
    use malachitebft_app_channel::app::types::ProposedValue;
    use malachitebft_eth_types::codec::proto::ProtobufCodec;
    use malachitebft_eth_types::{proto, Address, EmeraldContext, Height, Value};
    use prost::Message;

    use super::*;

    fn proposal(payload: Bytes) -> ProposedValue<EmeraldContext> {
        ProposedValue {
            height: Height::new(41),
            round: Round::new(3),
            valid_round: Round::new(1),
            proposer: Address::new([7; 20]),
            value: Value::new(payload),
            validity: Validity::Valid,
        }
    }

    #[test]
    fn compact_proposal_metadata_encoding_omits_payload_and_hydrates() {
        let payload = Bytes::from(vec![0xAB; 1024 * 1024]);
        let proposal = proposal(payload.clone());

        let encoded = StoredProposalMetadata::from_proposal(&proposal)
            .encode()
            .unwrap();
        let proto = proto::ProposedValue::decode(encoded.clone()).unwrap();
        assert_eq!(
            proto.value.unwrap().value.unwrap().len(),
            u64::BITS as usize / 8
        );
        assert!(encoded.len() < 128);

        let decoded = decode_stored_proposal(encoded).unwrap();
        assert!(decoded.embedded_payload.is_none());
        assert_eq!(decoded.metadata.hydrate(payload), proposal);
    }

    #[test]
    fn proposal_metadata_decoder_distinguishes_compact_and_full_records() {
        let payload = Bytes::from_static(b"legacy-full-payload");
        let proposal = proposal(payload.clone());
        let encoded = ProtobufCodec.encode(&proposal).unwrap();

        let decoded = decode_stored_proposal(encoded).unwrap();
        assert_eq!(
            decoded.metadata,
            StoredProposalMetadata::from_proposal(&proposal)
        );
        assert_eq!(decoded.embedded_payload, Some(payload));
    }

    #[test]
    fn stored_value_decoder_distinguishes_full_and_id_only_values() {
        let value = Value::new(Bytes::from_static(b"execution-payload"));
        assert_eq!(
            decode_stored_value(value.to_bytes().unwrap()).unwrap(),
            DecodedStoredValue::Full(value.clone())
        );

        let id_only = Value {
            value: value.id().as_u64(),
            extensions: Bytes::new(),
        };
        assert_eq!(
            decode_stored_value(id_only.to_bytes().unwrap()).unwrap(),
            DecodedStoredValue::IdOnly(value.id())
        );
    }
}
