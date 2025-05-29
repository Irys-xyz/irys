use alloy_primitives::wrap_fixed_bytes;
use alloy_rlp::{Decodable, Encodable, Error as RlpError};
use bytes::Buf;
use reth_codecs::Compact;

#[derive(
    PartialEq,
    Debug,
    Default,
    Eq,
    Clone,
    Copy,
    Hash,
    Compact,
    serde::Serialize,
    serde::Deserialize,
    arbitrary::Arbitrary,
)]

// these do NOT start with 0, as RLP does not like "leading zeros"

pub enum CommitmentStatus {
    #[default]
    /// Stake is pending epoch activation
    Pending = 1,
    /// Stake is active
    Active = 2,
    /// Stake is pending epoch removal
    Inactive = 3,
    /// Stake is pending slash epoch removal
    Slashed = 4,
}

#[derive(thiserror::Error, Debug)]
pub enum CommitmentStatusDecodeError {
    #[error("unknown reserved Commitment status: {0}")]
    UnknownCommitmentStatus(u8),
}

impl TryFrom<u8> for CommitmentStatus {
    type Error = CommitmentStatusDecodeError;
    fn try_from(id: u8) -> Result<Self, Self::Error> {
        match id {
            1 => Ok(CommitmentStatus::Pending),
            2 => Ok(CommitmentStatus::Active),
            3 => Ok(CommitmentStatus::Inactive),
            4 => Ok(CommitmentStatus::Slashed),
            _ => Err(CommitmentStatusDecodeError::UnknownCommitmentStatus(id)),
        }
    }
}

impl Encodable for CommitmentStatus {
    fn encode(&self, out: &mut dyn bytes::BufMut) {
        match self {
            CommitmentStatus::Pending => out.put_u8(CommitmentStatus::Pending as u8),
            CommitmentStatus::Active => out.put_u8(CommitmentStatus::Active as u8),
            CommitmentStatus::Inactive => out.put_u8(CommitmentStatus::Inactive as u8),
            CommitmentStatus::Slashed => out.put_u8(CommitmentStatus::Slashed as u8),
        };
    }
    fn length(&self) -> usize {
        1
    }
}

impl Decodable for CommitmentStatus {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let _v = buf.to_vec();
        let enc_stake_status = u8::decode(&mut &buf[..])?;
        buf.advance(1);
        let id = CommitmentStatus::try_from(enc_stake_status)
            .or(Err(RlpError::Custom("unknown stake status id")))?;
        let _v2 = buf.to_vec();
        Ok(id)
    }
}

// TODO: these need to be redone!
#[derive(
    PartialEq,
    Debug,
    Default,
    Eq,
    Clone,
    Copy,
    Hash,
    Compact,
    serde::Serialize,
    serde::Deserialize,
    arbitrary::Arbitrary,
)]

// these do NOT start with 0, as RLP does not like "leading zeros"
pub enum CommitmentType {
    #[default]
    Stake = 1,
    Pledge = 2,
    Unpledge = 3,
    Unstake = 4,
}

// TODO: custom de/serialize (or just make it a u8 field lol) impl so we can use the commitment type id integer

#[derive(thiserror::Error, Debug)]
pub enum CommitmentTypeDecodeError {
    #[error("unknown reserved Commitment type: {0}")]
    UnknownCommitmentType(u8),
}

impl TryFrom<u8> for CommitmentType {
    type Error = CommitmentTypeDecodeError;
    fn try_from(id: u8) -> Result<Self, Self::Error> {
        match id {
            1 => Ok(CommitmentType::Stake),
            2 => Ok(CommitmentType::Pledge),
            3 => Ok(CommitmentType::Unpledge),
            4 => Ok(CommitmentType::Unstake),
            _ => Err(CommitmentTypeDecodeError::UnknownCommitmentType(id)),
        }
    }
}

impl Encodable for CommitmentType {
    fn encode(&self, out: &mut dyn bytes::BufMut) {
        match self {
            CommitmentType::Stake => out.put_u8(CommitmentType::Stake as u8),
            CommitmentType::Pledge => out.put_u8(CommitmentType::Pledge as u8),
            CommitmentType::Unpledge => out.put_u8(CommitmentType::Unpledge as u8),
            CommitmentType::Unstake => out.put_u8(CommitmentType::Unstake as u8),
        };
    }
    fn length(&self) -> usize {
        1
    }
}

impl Decodable for CommitmentType {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let _v = buf.to_vec();
        let enc_commitment_type = u8::decode(&mut &buf[..])?;
        let commitment_type = CommitmentType::try_from(enc_commitment_type)
            .or(Err(RlpError::Custom("unknown commitment status")))?;

        buf.advance(1);
        Ok(commitment_type)
    }
}

wrap_fixed_bytes!(
    extra_derives: [],
    pub struct IrysTxId<32>;
);

// from storage/codecs/src/lib.rs, line 328, impl_compact_for_wrapped_bytes! macro
// this is done "manually" here to prevent acyclic deps, which is why these structs are defined here in the first place...
impl Compact for IrysTxId {
    #[inline]
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        self.0.to_compact(buf)
    }

    #[inline]
    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        let (v, buf) = <[u8; core::mem::size_of::<IrysTxId>()]>::from_compact(buf, len);
        (Self::from(v), buf)
    }
}
