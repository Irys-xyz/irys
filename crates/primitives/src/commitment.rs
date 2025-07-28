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
// these do NOT start with 0, as RLP does not like "leading zeros"
#[derive(
    PartialEq,
    Debug,
    Default,
    Eq,
    Clone,
    Copy,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    arbitrary::Arbitrary,
)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum CommitmentType {
    #[default]
    Stake,
    Pledge {
        #[serde(rename = "pledgeCountBeforeExecuting")]
        pledge_count_before_executing: usize,
    },
    Unpledge {
        #[serde(rename = "pledgeCountBeforeExecuting")]
        pledge_count_before_executing: usize,
    },
    Unstake,
}

// TODO: custom de/serialize (or just make it a u8 field lol) impl so we can use the commitment type id integer

#[derive(thiserror::Error, Debug)]
pub enum CommitmentTypeDecodeError {
    #[error("unknown reserved Commitment type: {0}")]
    UnknownCommitmentType(u8),
}

impl Encodable for CommitmentType {
    fn encode(&self, out: &mut dyn bytes::BufMut) {
        match self {
            Self::Stake => {
                out.put_u8(1);
            }
            Self::Pledge {
                pledge_count_before_executing,
            } => {
                out.put_u8(2);
                (*pledge_count_before_executing as u64).encode(out);
            }
            Self::Unpledge {
                pledge_count_before_executing,
            } => {
                out.put_u8(3);
                (*pledge_count_before_executing as u64).encode(out);
            }
            Self::Unstake => {
                out.put_u8(4);
            }
        };
    }

    fn length(&self) -> usize {
        match self {
            Self::Stake | Self::Unstake => 1,
            Self::Pledge {
                pledge_count_before_executing,
            }
            | Self::Unpledge {
                pledge_count_before_executing,
            } => 1 + (*pledge_count_before_executing as u64).length(),
        }
    }
}

impl Decodable for CommitmentType {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        if buf.is_empty() {
            return Err(RlpError::InputTooShort);
        }

        let type_id = buf[0];
        buf.advance(1);

        match type_id {
            1 => Ok(Self::Stake),
            2 => {
                let count = u64::decode(buf)? as usize;
                Ok(Self::Pledge {
                    pledge_count_before_executing: count,
                })
            }
            3 => {
                let count = u64::decode(buf)? as usize;
                Ok(Self::Unpledge {
                    pledge_count_before_executing: count,
                })
            }
            4 => Ok(Self::Unstake),
            _ => Err(RlpError::Custom("unknown commitment type")),
        }
    }
}

// Manual implementation of Compact for CommitmentType
impl reth_codecs::Compact for CommitmentType {
    fn to_compact<B: bytes::BufMut + AsMut<[u8]>>(&self, buf: &mut B) -> usize {
        match self {
            Self::Stake => {
                buf.put_u8(1);
                1
            }
            Self::Pledge {
                pledge_count_before_executing,
            } => {
                buf.put_u8(2);
                buf.put_u64(*pledge_count_before_executing as u64);
                9
            }
            Self::Unpledge {
                pledge_count_before_executing,
            } => {
                buf.put_u8(3);
                buf.put_u64(*pledge_count_before_executing as u64);
                9
            }
            Self::Unstake => {
                buf.put_u8(4);
                1
            }
        }
    }

    fn from_compact(buf: &[u8], _len: usize) -> (Self, &[u8]) {
        match buf[0] {
            1 => (Self::Stake, &buf[1..]),
            2 => {
                let count = u64::from_le_bytes(buf[1..9].try_into().unwrap()) as usize;
                (
                    Self::Pledge {
                        pledge_count_before_executing: count,
                    },
                    &buf[9..],
                )
            }
            3 => {
                let count = u64::from_le_bytes(buf[1..9].try_into().unwrap()) as usize;
                (
                    Self::Unpledge {
                        pledge_count_before_executing: count,
                    },
                    &buf[9..],
                )
            }
            4 => (Self::Unstake, &buf[1..]),
            _ => panic!("unknown commitment type in compact encoding"),
        }
    }
}
