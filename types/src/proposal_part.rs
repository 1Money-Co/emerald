use core::fmt;

use bytes::Bytes;
use malachitebft_core_types::Round;
use malachitebft_proto::{self as proto, Error as ProtoError, Protobuf};
use serde::{Deserialize, Serialize};

use crate::codec::proto::{decode_signature, encode_signature};
use crate::secp256k1::Signature;
use crate::{Address, EmeraldContext, Height};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalData {
    pub bytes: Bytes,
}

impl ProposalData {
    pub fn new(bytes: Bytes) -> Self {
        Self { bytes }
    }

    pub fn size_bytes(&self) -> usize {
        core::mem::size_of::<u64>()
    }
}

impl fmt::Debug for ProposalData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProposalData")
            .field("bytes", &"<...>")
            .field("len", &self.bytes.len())
            .finish()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "Round")]
enum RoundDef {
    Nil,
    Some(u32),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProposalPart {
    Init(ProposalInit),
    Data(ProposalData),
    Fin(ProposalFin),
}

impl ProposalPart {
    pub fn get_type(&self) -> &'static str {
        match self {
            Self::Init(_) => "init",
            Self::Data(_) => "data",
            Self::Fin(_) => "fin",
        }
    }

    pub fn as_init(&self) -> Option<&ProposalInit> {
        match self {
            Self::Init(init) => Some(init),
            _ => None,
        }
    }

    pub fn as_data(&self) -> Option<&ProposalData> {
        match self {
            Self::Data(data) => Some(data),
            _ => None,
        }
    }

    pub fn as_fin(&self) -> Option<&ProposalFin> {
        match self {
            Self::Fin(fin) => Some(fin),
            _ => None,
        }
    }

    pub fn to_sign_bytes(&self) -> Bytes {
        proto::Protobuf::to_bytes(self).unwrap()
    }

    pub fn size_bytes(&self) -> usize {
        self.to_sign_bytes().len() // FIXME: This is dumb
    }
}

/// A part of a value for a height, round. Identified in this scope by the sequence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalInit {
    pub height: Height,
    #[serde(with = "RoundDef")]
    pub round: Round,
    #[serde(with = "RoundDef")]
    pub pol_round: Round,
    pub proposer: Address,
}

impl ProposalInit {
    pub fn new(height: Height, round: Round, pol_round: Round, proposer: Address) -> Self {
        Self {
            height,
            round,
            pol_round,
            proposer,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalFin {
    pub signature: Signature,
}

impl ProposalFin {
    pub fn new(signature: Signature) -> Self {
        Self { signature }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalAttestation {
    pub init: ProposalInit,
    pub fin: ProposalFin,
}

impl ProposalAttestation {
    pub fn new(init: ProposalInit, fin: ProposalFin) -> Self {
        Self { init, fin }
    }
}

impl malachitebft_core_types::ProposalPart<EmeraldContext> for ProposalPart {
    fn is_first(&self) -> bool {
        matches!(self, Self::Init(_))
    }

    fn is_last(&self) -> bool {
        matches!(self, Self::Fin(_))
    }
}

impl Protobuf for ProposalInit {
    type Proto = crate::proto::ProposalInit;

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn from_proto(proto: Self::Proto) -> Result<Self, ProtoError> {
        Ok(Self {
            height: Height::new(proto.height),
            round: Round::new(proto.round),
            pol_round: Round::from(proto.pol_round),
            proposer: proto
                .proposer
                .ok_or_else(|| ProtoError::missing_field::<Self::Proto>("proposer"))
                .and_then(Address::from_proto)?,
        })
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn to_proto(&self) -> Result<Self::Proto, ProtoError> {
        Ok(Self::Proto {
            height: self.height.as_u64(),
            round: self.round.as_u32().unwrap(),
            pol_round: self.pol_round.as_u32(),
            proposer: Some(self.proposer.to_proto()?),
        })
    }
}

impl Protobuf for ProposalFin {
    type Proto = crate::proto::ProposalFin;

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn from_proto(proto: Self::Proto) -> Result<Self, ProtoError> {
        Ok(Self {
            signature: proto
                .signature
                .ok_or_else(|| ProtoError::missing_field::<Self::Proto>("signature"))
                .and_then(decode_signature)?,
        })
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn to_proto(&self) -> Result<Self::Proto, ProtoError> {
        Ok(Self::Proto {
            signature: Some(encode_signature(&self.signature)),
        })
    }
}

impl Protobuf for ProposalAttestation {
    type Proto = crate::proto::ProposalAttestation;

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn from_proto(proto: Self::Proto) -> Result<Self, ProtoError> {
        Ok(Self {
            init: proto
                .init
                .ok_or_else(|| ProtoError::missing_field::<Self::Proto>("init"))
                .and_then(ProposalInit::from_proto)?,
            fin: proto
                .fin
                .ok_or_else(|| ProtoError::missing_field::<Self::Proto>("fin"))
                .and_then(ProposalFin::from_proto)?,
        })
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn to_proto(&self) -> Result<Self::Proto, ProtoError> {
        Ok(Self::Proto {
            init: Some(self.init.to_proto()?),
            fin: Some(self.fin.to_proto()?),
        })
    }
}

impl Protobuf for ProposalPart {
    type Proto = crate::proto::ProposalPart;

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn from_proto(proto: Self::Proto) -> Result<Self, ProtoError> {
        use crate::proto::proposal_part::Part;

        let part = proto
            .part
            .ok_or_else(|| ProtoError::missing_field::<Self::Proto>("part"))?;

        match part {
            Part::Init(init) => Ok(Self::Init(ProposalInit::from_proto(init)?)),
            Part::Data(data) => Ok(Self::Data(ProposalData::new(data.bytes))),
            Part::Fin(fin) => Ok(Self::Fin(ProposalFin::from_proto(fin)?)),
        }
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn to_proto(&self) -> Result<Self::Proto, ProtoError> {
        use crate::proto;
        use crate::proto::proposal_part::Part;

        match self {
            Self::Init(init) => Ok(Self::Proto {
                part: Some(Part::Init(init.to_proto()?)),
            }),
            Self::Data(data) => Ok(Self::Proto {
                part: Some(Part::Data(proto::ProposalData {
                    bytes: data.bytes.clone(),
                })),
            }),
            Self::Fin(fin) => Ok(Self::Proto {
                part: Some(Part::Fin(fin.to_proto()?)),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use malachitebft_codec::Codec;

    use super::*;
    use crate::codec::proto::ProtobufCodec;
    use crate::secp256k1::PrivateKey;

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
}
