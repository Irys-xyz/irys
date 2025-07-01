use alloy_rlp::{Decodable, Encodable, Error as RlpError};
use bytes::Buf as _;
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
            1 => Ok(Self::Pending),
            2 => Ok(Self::Active),
            3 => Ok(Self::Inactive),
            4 => Ok(Self::Slashed),
            _ => Err(CommitmentStatusDecodeError::UnknownCommitmentStatus(id)),
        }
    }
}

impl Encodable for CommitmentStatus {
    fn encode(&self, out: &mut dyn bytes::BufMut) {
        match self {
            Self::Pending => out.put_u8(Self::Pending as u8),
            Self::Active => out.put_u8(Self::Active as u8),
            Self::Inactive => out.put_u8(Self::Inactive as u8),
            Self::Slashed => out.put_u8(Self::Slashed as u8),
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
        let id = Self::try_from(enc_stake_status)
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
            1 => Ok(Self::Stake),
            2 => Ok(Self::Pledge),
            3 => Ok(Self::Unpledge),
            4 => Ok(Self::Unstake),
            _ => Err(CommitmentTypeDecodeError::UnknownCommitmentType(id)),
        }
    }
}

impl Encodable for CommitmentType {
    fn encode(&self, out: &mut dyn bytes::BufMut) {
        match self {
            Self::Stake => out.put_u8(Self::Stake as u8),
            Self::Pledge => out.put_u8(Self::Pledge as u8),
            Self::Unpledge => out.put_u8(Self::Unpledge as u8),
            Self::Unstake => out.put_u8(Self::Unstake as u8),
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
        let commitment_type = Self::try_from(enc_commitment_type)
            .or(Err(RlpError::Custom("unknown commitment status")))?;

        buf.advance(1);
        Ok(commitment_type)
    }
}
