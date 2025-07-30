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

// Type discriminants for CommitmentType encoding
const COMMITMENT_TYPE_STAKE: u8 = 1;
const COMMITMENT_TYPE_PLEDGE: u8 = 2;
const COMMITMENT_TYPE_UNPLEDGE: u8 = 3;
const COMMITMENT_TYPE_UNSTAKE: u8 = 4;

// Size constants
const TYPE_DISCRIMINANT_SIZE: usize = 1;
const U64_SIZE: usize = 8;

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
        pledge_count_before_executing: u64,
    },
    Unpledge {
        #[serde(rename = "pledgeCountBeforeExecuting")]
        pledge_count_before_executing: u64,
    },
    Unstake,
}

impl CommitmentType {
    pub fn is_stake(&self) -> bool {
        matches!(self, &Self::Stake)
    }
    pub fn is_pledge(&self) -> bool {
        matches!(self, &Self::Pledge { .. })
    }
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
                out.put_u8(COMMITMENT_TYPE_STAKE);
            }
            Self::Pledge {
                pledge_count_before_executing,
            } => {
                out.put_u8(COMMITMENT_TYPE_PLEDGE);
                pledge_count_before_executing.encode(out);
            }
            Self::Unpledge {
                pledge_count_before_executing,
            } => {
                out.put_u8(COMMITMENT_TYPE_UNPLEDGE);
                pledge_count_before_executing.encode(out);
            }
            Self::Unstake => {
                out.put_u8(COMMITMENT_TYPE_UNSTAKE);
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
            } => 1 + pledge_count_before_executing.length(),
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
            COMMITMENT_TYPE_STAKE => Ok(Self::Stake),
            COMMITMENT_TYPE_PLEDGE => {
                let count = u64::decode(buf)?;
                Ok(Self::Pledge {
                    pledge_count_before_executing: count,
                })
            }
            COMMITMENT_TYPE_UNPLEDGE => {
                let count = u64::decode(buf)?;
                Ok(Self::Unpledge {
                    pledge_count_before_executing: count,
                })
            }
            COMMITMENT_TYPE_UNSTAKE => Ok(Self::Unstake),
            _ => Err(RlpError::Custom("unknown commitment type")),
        }
    }
}

// Manual implementation of Compact for CommitmentType
impl reth_codecs::Compact for CommitmentType {
    fn to_compact<B: bytes::BufMut + AsMut<[u8]>>(&self, buf: &mut B) -> usize {
        match self {
            Self::Stake => {
                buf.put_u8(COMMITMENT_TYPE_STAKE);
                TYPE_DISCRIMINANT_SIZE
            }
            Self::Pledge {
                pledge_count_before_executing,
            } => {
                buf.put_u8(COMMITMENT_TYPE_PLEDGE);
                buf.put_u64(*pledge_count_before_executing);
                TYPE_DISCRIMINANT_SIZE + U64_SIZE
            }
            Self::Unpledge {
                pledge_count_before_executing,
            } => {
                buf.put_u8(COMMITMENT_TYPE_UNPLEDGE);
                buf.put_u64(*pledge_count_before_executing);
                TYPE_DISCRIMINANT_SIZE + U64_SIZE
            }
            Self::Unstake => {
                buf.put_u8(COMMITMENT_TYPE_UNSTAKE);
                TYPE_DISCRIMINANT_SIZE
            }
        }
    }

    fn from_compact(buf: &[u8], _len: usize) -> (Self, &[u8]) {
        // Check minimum buffer size
        if buf.is_empty() {
            panic!("CommitmentType::from_compact: buffer too short, expected at least 1 byte for type discriminant");
        }

        let type_id = buf[0];

        match type_id {
            COMMITMENT_TYPE_STAKE => (Self::Stake, &buf[TYPE_DISCRIMINANT_SIZE..]),
            COMMITMENT_TYPE_PLEDGE => {
                let required_size = TYPE_DISCRIMINANT_SIZE + U64_SIZE;
                if buf.len() < required_size {
                    panic!(
                        "CommitmentType::from_compact: buffer too short for Pledge variant, \
                         expected at least {} bytes but got {}",
                        required_size,
                        buf.len()
                    );
                }

                let count_bytes: [u8; 8] = buf[TYPE_DISCRIMINANT_SIZE..required_size]
                    .try_into()
                    .expect("slice has correct length");
                let count = u64::from_le_bytes(count_bytes);

                (
                    Self::Pledge {
                        pledge_count_before_executing: count,
                    },
                    &buf[required_size..],
                )
            }
            COMMITMENT_TYPE_UNPLEDGE => {
                let required_size = TYPE_DISCRIMINANT_SIZE + U64_SIZE;
                if buf.len() < required_size {
                    panic!(
                        "CommitmentType::from_compact: buffer too short for Unpledge variant, \
                         expected at least {} bytes but got {}",
                        required_size,
                        buf.len()
                    );
                }

                let count_bytes: [u8; 8] = buf[TYPE_DISCRIMINANT_SIZE..required_size]
                    .try_into()
                    .expect("slice has correct length");
                let count = u64::from_le_bytes(count_bytes);

                (
                    Self::Unpledge {
                        pledge_count_before_executing: count,
                    },
                    &buf[required_size..],
                )
            }
            COMMITMENT_TYPE_UNSTAKE => (Self::Unstake, &buf[TYPE_DISCRIMINANT_SIZE..]),
            _ => panic!(
                "CommitmentType::from_compact: unknown commitment type discriminant: {}",
                type_id
            ),
        }
    }
}
