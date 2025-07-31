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
                buf.put_u64_le(*pledge_count_before_executing);
                TYPE_DISCRIMINANT_SIZE + U64_SIZE
            }
            Self::Unpledge {
                pledge_count_before_executing,
            } => {
                buf.put_u8(COMMITMENT_TYPE_UNPLEDGE);
                buf.put_u64_le(*pledge_count_before_executing);
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

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;
    use rstest::rstest;

    #[rstest]
    #[case::stake(CommitmentType::Stake)]
    #[case::pledge_zero(CommitmentType::Pledge { pledge_count_before_executing: 0 })]
    #[case::pledge_one(CommitmentType::Pledge { pledge_count_before_executing: 1 })]
    #[case::pledge_hundred(CommitmentType::Pledge { pledge_count_before_executing: 100 })]
    #[case::pledge_max(CommitmentType::Pledge { pledge_count_before_executing: u64::MAX })]
    fn test_commitment_type_rlp_roundtrip(#[case] original: CommitmentType) {
        // Encode
        let mut buf = BytesMut::new();
        original.encode(&mut buf);

        // Decode
        let mut slice = buf.as_ref();
        let decoded = CommitmentType::decode(&mut slice).unwrap();

        assert_eq!(original, decoded);
        assert!(slice.is_empty(), "Buffer should be fully consumed");
    }

    #[rstest]
    #[case::stake(CommitmentType::Stake, 1, COMMITMENT_TYPE_STAKE)]
    #[case::pledge_zero(CommitmentType::Pledge { pledge_count_before_executing: 0 }, 9, COMMITMENT_TYPE_PLEDGE)]
    #[case::pledge_one(CommitmentType::Pledge { pledge_count_before_executing: 1 }, 9, COMMITMENT_TYPE_PLEDGE)]
    #[case::pledge_hundred(CommitmentType::Pledge { pledge_count_before_executing: 100 }, 9, COMMITMENT_TYPE_PLEDGE)]
    #[case::pledge_max(CommitmentType::Pledge { pledge_count_before_executing: u64::MAX }, 9, COMMITMENT_TYPE_PLEDGE)]
    fn test_commitment_type_compact_roundtrip(
        #[case] original: CommitmentType,
        #[case] expected_len: usize,
        #[case] expected_discriminant: u8,
    ) {
        // Encode
        let mut buf = Vec::new();
        let encoded_len = original.to_compact(&mut buf);
        assert_eq!(encoded_len, expected_len);
        assert_eq!(buf.len(), expected_len);
        assert_eq!(buf[0], expected_discriminant);

        // Decode
        let (decoded, remaining) = CommitmentType::from_compact(&buf, buf.len());

        assert_eq!(original, decoded);
        assert!(remaining.is_empty(), "Buffer should be fully consumed");
    }

    #[rstest]
    #[case::empty_buffer(&[])]
    #[case::invalid_discriminant(&[99_u8])]
    #[case::pledge_buffer_too_short(&[COMMITMENT_TYPE_PLEDGE])]
    fn test_commitment_type_rlp_decode_errors(#[case] buf: &[u8]) {
        let mut slice = buf;
        assert!(CommitmentType::decode(&mut slice).is_err());
    }

    #[test]
    #[should_panic(expected = "buffer too short, expected at least 1 byte")]
    fn test_commitment_type_compact_decode_empty_buffer() {
        let empty_buf = vec![];
        CommitmentType::from_compact(&empty_buf, 0);
    }

    #[test]
    #[should_panic(expected = "unknown commitment type discriminant: 99")]
    fn test_commitment_type_compact_decode_invalid_type() {
        let invalid_buf = vec![99_u8];
        CommitmentType::from_compact(&invalid_buf, invalid_buf.len());
    }

    #[test]
    #[should_panic(expected = "buffer too short for Pledge variant")]
    fn test_commitment_type_compact_decode_pledge_buffer_too_short() {
        let short_buf = vec![COMMITMENT_TYPE_PLEDGE, 1, 2, 3]; // Only 4 bytes, need 9
        CommitmentType::from_compact(&short_buf, short_buf.len());
    }

    #[rstest]
    #[case::stake(CommitmentType::Stake, 1)]
    #[case::pledge(CommitmentType::Pledge { pledge_count_before_executing: 100 }, 1 + 100_u64.length())]
    fn test_commitment_type_rlp_length(
        #[case] commitment_type: CommitmentType,
        #[case] expected_length: usize,
    ) {
        assert_eq!(commitment_type.length(), expected_length);
    }
}
