use crate::Versioned;
pub use crate::{
    address_base58_stringify, compare_commitment_transactions, ingress::IngressProof,
    optional_string_u64, string_u64, Arbitrary, Base64, CommitmentValidationError, ConsensusConfig,
    IrysAddress, IrysSignature, IrysTransactionId, Node, PledgeDataProvider, Proof, Signature,
    H256, U256,
};
use alloy_rlp::{Decodable, Encodable, Error as RlpError};
use alloy_rlp::{RlpDecodable, RlpEncodable};
use bytes::Buf as _;
use reth_codecs::Compact;
use serde::{Deserialize, Serialize};

#[derive(
    Clone,
    Debug,
    Default,
    Eq,
    Serialize,
    Deserialize,
    PartialEq,
    Arbitrary,
    Compact,
    RlpEncodable,
    RlpDecodable,
    Hash,
)]
#[rlp(trailing)]
/// Stores deserialized fields from a JSON formatted commitment transaction.
/// NOTE: be CAREFUL with using serde(default) it should ONLY be for `Option`al fields.
#[serde(rename_all = "camelCase")]
pub struct CommitmentTransactionV1 {
    // NOTE: both rlp skip AND rlp default must be present in order for field skipping to work
    #[rlp(skip)]
    #[rlp(default)]
    /// A SHA-256 hash of the transaction signature.
    pub id: H256,

    /// block_hash of a recent (last 50) blocks or the a recent transaction id
    /// from the signer. Multiple transactions can share the same anchor.
    pub anchor: H256,

    /// The ecdsa/secp256k1 public key of the transaction signer
    // #[serde(with = "address_base58_stringify")]
    pub signer: IrysAddress,

    /// The type of commitment Stake/UnStake Pledge/UnPledge
    pub commitment_type: CommitmentTypeV1,

    /// EVM chain ID - used to prevent cross-chain replays
    #[serde(with = "string_u64")]
    pub chain_id: u64,

    /// Pay the fee required to mitigate tx spam
    #[serde(with = "string_u64")]
    pub fee: u64,

    /// The value being staked, pledged, unstaked or unpledged
    pub value: U256,

    /// Transaction signature bytes
    #[rlp(skip)]
    #[rlp(default)]
    pub signature: IrysSignature,
}

impl Ord for CommitmentTransactionV1 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        compare_commitment_transactions(
            &self.commitment_type.into(),
            self.user_fee(),
            self.id,
            &other.commitment_type.into(),
            other.user_fee(),
            other.id,
        )
    }
}

impl PartialOrd for CommitmentTransactionV1 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl CommitmentTransactionV1 {
    /// Create a new CommitmentTransaction with default values from config
    pub fn new(config: &ConsensusConfig) -> Self {
        Self {
            id: H256::zero(),
            anchor: H256::zero(),
            signer: IrysAddress::default(),
            commitment_type: CommitmentTypeV1::default(),
            chain_id: config.chain_id,
            fee: 0,
            value: U256::zero(),
            signature: IrysSignature::new(Signature::test_signature()),
        }
    }

    /// Calculate the value for a pledge at the given count
    /// For pledge N, use count = N
    /// For unpledge refund, use count = N - 1 (to get the value of the most recent pledge)
    pub fn calculate_pledge_value_at_count(config: &ConsensusConfig, pledge_count: u64) -> U256 {
        config
            .pledge_base_value
            .apply_pledge_decay(pledge_count, config.pledge_decay)
            .map(|a| a.amount)
            .unwrap_or(config.pledge_base_value.amount)
    }

    /// Create a new stake transaction with the configured stake fee as value
    pub fn new_stake(config: &ConsensusConfig, anchor: H256) -> Self {
        Self {
            commitment_type: CommitmentTypeV1::Stake,
            anchor,
            fee: config.mempool.commitment_fee,
            value: config.stake_value.amount,
            ..Self::new(config)
        }
    }

    /// Create a new unstake transaction with the configured stake fee as value
    pub fn new_unstake(config: &ConsensusConfig, anchor: H256) -> Self {
        Self {
            commitment_type: CommitmentTypeV1::Unstake,
            anchor,
            fee: config.mempool.commitment_fee,
            value: config.stake_value.amount,
            ..Self::new(config)
        }
    }

    /// Create a new pledge transaction with decreasing cost per pledge.
    /// Cost = pledge_base_fee / ((existing_pledges + 1) ^ pledge_decay)
    /// The calculated cost is stored in the transaction's `value` field.
    pub async fn new_pledge(
        config: &ConsensusConfig,
        anchor: H256,
        provider: &impl PledgeDataProvider,
        signer_address: IrysAddress,
    ) -> Self {
        let count = provider.pledge_count(signer_address).await;
        let value = Self::calculate_pledge_value_at_count(config, count);

        Self {
            commitment_type: CommitmentTypeV1::Pledge {
                pledge_count_before_executing: count,
            },
            anchor,
            fee: config.mempool.commitment_fee,
            value,
            ..Self::new(config)
        }
    }

    /// Create a new unpledge transaction that refunds the most recent pledge's cost.
    /// Refund = cost of the last pledge made (existing_pledges - 1)
    /// Returns 0 if user has no pledges. The refund is in the `value` field.
    pub async fn new_unpledge(
        config: &ConsensusConfig,
        anchor: H256,
        provider: &impl PledgeDataProvider,
        signer_address: IrysAddress,
        partition_hash: H256,
    ) -> Self {
        let count = provider.pledge_count(signer_address).await;
        let value = Self::calculate_pledge_value_at_count(config, count.saturating_sub(1));

        Self {
            commitment_type: CommitmentTypeV1::Unpledge {
                pledge_count_before_executing: count,
                partition_hash,
            },
            anchor,
            fee: config.mempool.commitment_fee,
            value,
            ..Self::new(config)
        }
    }

    /// Rely on RLP encoding for signing
    pub fn encode_for_signing(&self, out: &mut dyn alloy_rlp::BufMut) {
        self.encode(out)
    }

    /// Returns the value stored in the transaction
    pub fn commitment_value(&self) -> U256 {
        self.value
    }

    /// Returns the user fee for prioritization
    pub fn user_fee(&self) -> U256 {
        U256::from(self.fee)
    }

    /// Returns the total cost including value
    pub fn total_cost(&self) -> U256 {
        let additional_fee = match &self.commitment_type {
            CommitmentTypeV1::Stake => self.value,
            CommitmentTypeV1::Pledge { .. } => self.value,
            CommitmentTypeV1::Unpledge { .. } => U256::zero(),
            CommitmentTypeV1::Unstake => U256::zero(),
        };
        U256::from(self.fee).saturating_add(additional_fee)
    }

    /// Simple getter methods for IrysTransaction compatibility
    pub fn id(&self) -> IrysTransactionId {
        self.id
    }

    pub fn signer(&self) -> IrysAddress {
        self.signer
    }

    pub fn signature(&self) -> &IrysSignature {
        &self.signature
    }

    pub fn anchor(&self) -> H256 {
        self.anchor
    }

    /// Validates that the commitment transaction has a sufficient fee
    pub fn validate_fee(&self, config: &ConsensusConfig) -> Result<(), CommitmentValidationError> {
        let required_fee = config.mempool.commitment_fee;

        if self.fee < required_fee {
            return Err(CommitmentValidationError::InsufficientFee {
                provided: self.fee,
                required: required_fee,
            });
        }

        Ok(())
    }

    /// Validates the value field based on commitment type
    pub fn validate_value(
        &self,
        config: &ConsensusConfig,
    ) -> Result<(), CommitmentValidationError> {
        match &self.commitment_type {
            CommitmentTypeV1::Stake | CommitmentTypeV1::Unstake => {
                // For stake/unstake, value must match configured stake value
                let expected_value = config.stake_value.amount;
                if self.value != expected_value {
                    return Err(CommitmentValidationError::InvalidStakeValue {
                        provided: self.value,
                        expected: expected_value,
                    });
                }
            }
            CommitmentTypeV1::Pledge {
                pledge_count_before_executing,
            } => {
                // For pledge, validate using the embedded pledge count
                let expected_value =
                    Self::calculate_pledge_value_at_count(config, *pledge_count_before_executing);

                if self.value != expected_value {
                    return Err(CommitmentValidationError::InvalidPledgeValue {
                        provided: self.value,
                        expected: expected_value,
                        pledge_count: *pledge_count_before_executing,
                    });
                }
            }
            CommitmentTypeV1::Unpledge {
                pledge_count_before_executing,
                ..
            } => {
                // Unpledge must reference an existing pledge (count > 0)
                if *pledge_count_before_executing == 0 {
                    return Err(CommitmentValidationError::InvalidUnpledgeCountZero);
                }

                // Calculate expected refund value: value of the most recent pledge (count-1)
                let expected_value = Self::calculate_pledge_value_at_count(
                    config,
                    *pledge_count_before_executing - 1,
                );

                if self.value != expected_value {
                    return Err(CommitmentValidationError::InvalidUnpledgeValue {
                        provided: self.value,
                        expected: expected_value,
                        pledge_count: *pledge_count_before_executing,
                    });
                }
            }
        }

        Ok(())
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
const PARTITION_HASH_SIZE: usize = 32;

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
pub enum CommitmentTypeV1 {
    #[default]
    Stake,
    Pledge {
        #[serde(rename = "pledgeCountBeforeExecuting", with = "string_u64")]
        pledge_count_before_executing: u64,
    },
    Unpledge {
        #[serde(rename = "pledgeCountBeforeExecuting", with = "string_u64")]
        pledge_count_before_executing: u64,
        #[serde(rename = "partitionHash")]
        partition_hash: H256,
    },
    Unstake,
}

/// WARNING: THE BELOW ENCODING/DECODING IS NON-CANONICAL (NOT TO THE RLP SPEC) WHICH IS WHY CommitmentTransaction/CommitmentTypeV2 WERE CREATED
/// THESE ARE HERE FOR BACKWARDS COMPATIBILITY
impl Encodable for CommitmentTypeV1 {
    fn encode(&self, out: &mut dyn bytes::BufMut) {
        // note: RLP headers should not be used for single byte values
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
                partition_hash,
            } => {
                out.put_u8(COMMITMENT_TYPE_UNPLEDGE);
                pledge_count_before_executing.encode(out);
                out.put_slice(&partition_hash.0);
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
            } => 1 + pledge_count_before_executing.length(),
            Self::Unpledge {
                pledge_count_before_executing,
                ..
            } => 1 + pledge_count_before_executing.length() + PARTITION_HASH_SIZE,
        }
    }
}

impl Decodable for CommitmentTypeV1 {
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
                if buf.len() < PARTITION_HASH_SIZE {
                    return Err(RlpError::InputTooShort);
                }
                let mut ph = [0_u8; 32];
                ph.copy_from_slice(&buf[..PARTITION_HASH_SIZE]);
                buf.advance(PARTITION_HASH_SIZE);
                Ok(Self::Unpledge {
                    pledge_count_before_executing: count,
                    partition_hash: ph.into(),
                })
            }
            COMMITMENT_TYPE_UNSTAKE => Ok(Self::Unstake),
            _ => Err(RlpError::Custom("unknown commitment type")),
        }
    }
}

impl Versioned for CommitmentTransactionV1 {
    const VERSION: u8 = 1;
}

// Manual implementation of Compact for CommitmentType
impl reth_codecs::Compact for CommitmentTypeV1 {
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
                partition_hash,
            } => {
                buf.put_u8(COMMITMENT_TYPE_UNPLEDGE);
                buf.put_u64_le(*pledge_count_before_executing);
                buf.put_slice(&partition_hash.0);
                TYPE_DISCRIMINANT_SIZE + U64_SIZE + PARTITION_HASH_SIZE
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
                let rem = &buf[required_size..];
                if rem.len() < PARTITION_HASH_SIZE {
                    panic!(
                        "CommitmentType::from_compact: buffer too short for Unpledge partition hash, expected at least {} bytes but got {}",
                        PARTITION_HASH_SIZE, rem.len()
                    );
                }
                let mut ph = [0_u8; 32];
                ph.copy_from_slice(&rem[..PARTITION_HASH_SIZE]);
                (
                    Self::Unpledge {
                        pledge_count_before_executing: count,
                        partition_hash: ph.into(),
                    },
                    &rem[PARTITION_HASH_SIZE..],
                )
            }
            COMMITMENT_TYPE_UNSTAKE => (Self::Unstake, &buf[TYPE_DISCRIMINANT_SIZE..]),
            _ => panic!(
                "CommitmentType::from_compact: unknown commitment type discriminant: {type_id}"
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
    #[case::stake(CommitmentTypeV1::Stake)]
    #[case::pledge_zero(CommitmentTypeV1::Pledge { pledge_count_before_executing: 0 })]
    #[case::pledge_one(CommitmentTypeV1::Pledge { pledge_count_before_executing: 1 })]
    #[case::pledge_hundred(CommitmentTypeV1::Pledge { pledge_count_before_executing: 100 })]
    #[case::pledge_max(CommitmentTypeV1::Pledge { pledge_count_before_executing: u64::MAX })]
    #[case::unpledge(CommitmentTypeV1::Unpledge { pledge_count_before_executing: 42, partition_hash: [255_u8; 32].into() })]
    fn test_commitment_type_rlp_roundtrip(#[case] original: CommitmentTypeV1) {
        // Encode
        let mut buf = BytesMut::new();
        original.encode(&mut buf);

        // Decode
        let mut slice = buf.as_ref();
        let decoded = CommitmentTypeV1::decode(&mut slice).unwrap();

        assert_eq!(original, decoded);
        assert!(slice.is_empty(), "Buffer should be fully consumed");
    }

    #[rstest]
    #[case::stake(CommitmentTypeV1::Stake, 1, COMMITMENT_TYPE_STAKE)]
    #[case::pledge_zero(CommitmentTypeV1::Pledge { pledge_count_before_executing: 0 }, 9, COMMITMENT_TYPE_PLEDGE)]
    #[case::pledge_one(CommitmentTypeV1::Pledge { pledge_count_before_executing: 1 }, 9, COMMITMENT_TYPE_PLEDGE)]
    #[case::pledge_hundred(CommitmentTypeV1::Pledge { pledge_count_before_executing: 100 }, 9, COMMITMENT_TYPE_PLEDGE)]
    #[case::pledge_max(CommitmentTypeV1::Pledge { pledge_count_before_executing: u64::MAX }, 9, COMMITMENT_TYPE_PLEDGE)]
    #[case::unpledge(CommitmentTypeV1::Unpledge { pledge_count_before_executing: 42, partition_hash: [7_u8; 32].into() }, 1 + 8 + 32, COMMITMENT_TYPE_UNPLEDGE)]
    fn test_commitment_type_compact_roundtrip(
        #[case] original: CommitmentTypeV1,
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
        let (decoded, remaining) = CommitmentTypeV1::from_compact(&buf, buf.len());

        assert_eq!(original, decoded);
        assert!(remaining.is_empty(), "Buffer should be fully consumed");
    }

    #[rstest]
    #[case::empty_buffer(&[])]
    #[case::invalid_discriminant(&[99_u8])]
    #[case::pledge_buffer_too_short(&[COMMITMENT_TYPE_PLEDGE])]
    fn test_commitment_type_rlp_decode_errors(#[case] buf: &[u8]) {
        let mut slice = buf;
        assert!(CommitmentTypeV1::decode(&mut slice).is_err());
    }

    #[test]
    #[should_panic(expected = "buffer too short, expected at least 1 byte")]
    fn test_commitment_type_compact_decode_empty_buffer() {
        std::panic::set_hook(Box::new(|_| {})); // Suppress panic output
        let empty_buf = vec![];
        CommitmentTypeV1::from_compact(&empty_buf, 0);
    }

    #[test]
    #[should_panic(expected = "unknown commitment type discriminant: 99")]
    fn test_commitment_type_compact_decode_invalid_type() {
        std::panic::set_hook(Box::new(|_| {})); // Suppress panic output
        let invalid_buf = vec![99_u8];
        CommitmentTypeV1::from_compact(&invalid_buf, invalid_buf.len());
    }

    #[test]
    #[should_panic(expected = "buffer too short for Pledge variant")]
    fn test_commitment_type_compact_decode_pledge_buffer_too_short() {
        std::panic::set_hook(Box::new(|_| {})); // Suppress panic output
        let short_buf = vec![COMMITMENT_TYPE_PLEDGE, 1, 2, 3]; // Only 4 bytes, need 9
        CommitmentTypeV1::from_compact(&short_buf, short_buf.len());
    }

    #[rstest]
    #[case::stake(CommitmentTypeV1::Stake, 1)]
    #[case::pledge(CommitmentTypeV1::Pledge { pledge_count_before_executing: 100 }, 1 + 100_u64.length())]
    #[case::unpledge(CommitmentTypeV1::Unpledge { pledge_count_before_executing: 5, partition_hash: [9_u8; 32].into() }, 1 + 5_u64.length() + 32)]
    fn test_commitment_type_rlp_length(
        #[case] commitment_type: CommitmentTypeV1,
        #[case] expected_length: usize,
    ) {
        assert_eq!(commitment_type.length(), expected_length);
    }

    #[test]
    fn test_unpledge_rlp_roundtrip() {
        use bytes::BytesMut;
        let original = CommitmentTypeV1::Unpledge {
            pledge_count_before_executing: 3,
            partition_hash: H256::random(),
        };
        let mut buf = BytesMut::new();
        original.encode(&mut buf);
        let mut slice = buf.as_ref();
        let decoded = CommitmentTypeV1::decode(&mut slice).unwrap();
        assert_eq!(original, decoded);
        assert!(slice.is_empty());
    }
}
