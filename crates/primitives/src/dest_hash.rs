use alloy_primitives::Address;
use alloy_rlp::{Decodable, Encodable, Error as RlpError};
use bytes::Buf;
use reth_codecs::Compact;

use crate::IrysTxId;

#[derive(PartialEq, Debug, Eq, Clone, Copy, Hash)]
// #[main_codec(no_arbitrary)]
// #[derive(PledgeArbitrary, PledgePropTestArbitrary)]
// #[derive(
//     Compact,
//     serde::Serialize,
//     serde::Deserialize,
//     RlpEncodable,
//     RlpDecodable
// )]
#[derive(Compact, serde::Serialize, serde::Deserialize, arbitrary::Arbitrary)]

pub enum DestHash {
    Address(Address),
    // txId has the same length as part_hash
    PartitionHash(IrysTxId),
}

#[derive(PartialEq, Debug, Eq, Clone, Copy)]

pub enum DestHashId {
    Address = 0,
    PartitionHash = 1,
}

#[derive(thiserror::Error, Debug)]
pub enum DestHashIdDecodeError {
    #[error("unknown reserved Stake type ID: {0}")]
    UnknownStakeTypeId(u8),
}

impl TryFrom<u8> for DestHashId {
    type Error = DestHashIdDecodeError;
    fn try_from(id: u8) -> Result<Self, Self::Error> {
        match id {
            0 => Ok(DestHashId::Address),
            1 => Ok(DestHashId::PartitionHash),
            _ => Err(DestHashIdDecodeError::UnknownStakeTypeId(id)),
        }
    }
}

impl DestHash {
    pub fn type_id(&self) -> DestHashId {
        match self {
            DestHash::Address(_) => DestHashId::Address,
            DestHash::PartitionHash(_) => DestHashId::PartitionHash,
        }
    }
    pub fn is_address(&self) -> bool {
        matches!(self, DestHash::Address(_))
    }
    pub fn is_part_hash(&self) -> bool {
        matches!(self, DestHash::PartitionHash(_))
    }
}

impl Encodable for DestHash {
    fn encode(&self, out: &mut dyn bytes::BufMut) {
        (self.type_id() as u8).encode(out);
        match self {
            DestHash::Address(msg) => msg.encode(out),
            DestHash::PartitionHash(msg) => msg.encode(out),
        };
    }
    fn length(&self) -> usize {
        let payload_len = match self {
            DestHash::Address(msg) => msg.length(),
            DestHash::PartitionHash(msg) => msg.length(),
        };
        payload_len + 1 // +1 for type ID
    }
}

impl Decodable for DestHash {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let enc_stake_status_id = u8::decode(&mut &buf[..])?;
        let id = DestHashId::try_from(enc_stake_status_id)
            .or(Err(RlpError::Custom("unknown last tx id")))?;
        buf.advance(1);
        match id {
            DestHashId::Address => Ok(DestHash::Address(Address::decode(buf)?)),
            DestHashId::PartitionHash => Ok(DestHash::PartitionHash(IrysTxId::decode(buf)?)),
        }
    }
}
impl Default for DestHash {
    fn default() -> Self {
        Self::Address(Address::default())
    }
}
