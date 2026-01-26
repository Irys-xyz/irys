use crate::Versioned;
pub use crate::{
    address_base58_stringify, compare_commitment_transactions, ingress::IngressProof,
    optional_string_u64, string_u64, Arbitrary, Base64, CommitmentTypeV1,
    CommitmentValidationError, Compact, ConsensusConfig, IrysAddress, IrysSignature,
    IrysTransactionId, Node, PledgeDataProvider, Proof, Signature, H256, U256,
};
use alloy_rlp::{Decodable, Encodable, Error as RlpError, RlpDecodable, RlpEncodable};
use bytes::Buf as _;
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
pub struct CommitmentTransactionV2 {
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
    pub commitment_type: CommitmentTypeV2,

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

impl Ord for CommitmentTransactionV2 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        compare_commitment_transactions(
            &self.commitment_type,
            self.user_fee(),
            self.id,
            &other.commitment_type,
            other.user_fee(),
            other.id,
        )
    }
}

impl PartialOrd for CommitmentTransactionV2 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl CommitmentTransactionV2 {
    /// Create a new CommitmentTransaction with default values from config
    pub fn new(config: &ConsensusConfig) -> Self {
        Self {
            id: H256::zero(),
            anchor: H256::zero(),
            signer: IrysAddress::default(),
            commitment_type: CommitmentTypeV2::default(),
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
            commitment_type: CommitmentTypeV2::Stake,
            anchor,
            fee: config.mempool.commitment_fee,
            value: config.stake_value.amount,
            ..Self::new(config)
        }
    }

    /// Create a new unstake transaction with the configured stake fee as value
    pub fn new_unstake(config: &ConsensusConfig, anchor: H256) -> Self {
        Self {
            commitment_type: CommitmentTypeV2::Unstake,
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
            commitment_type: CommitmentTypeV2::Pledge {
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
            commitment_type: CommitmentTypeV2::Unpledge {
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
            CommitmentTypeV2::Stake => self.value,
            CommitmentTypeV2::Pledge { .. } => self.value,
            CommitmentTypeV2::Unpledge { .. } => U256::zero(),
            CommitmentTypeV2::Unstake => U256::zero(),
            CommitmentTypeV2::UpdateRewardAddress { .. } => U256::zero(),
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
            CommitmentTypeV2::Stake | CommitmentTypeV2::Unstake => {
                // For stake/unstake, value must match configured stake value
                let expected_value = config.stake_value.amount;
                if self.value != expected_value {
                    return Err(CommitmentValidationError::InvalidStakeValue {
                        provided: self.value,
                        expected: expected_value,
                    });
                }
            }
            CommitmentTypeV2::Pledge {
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
            CommitmentTypeV2::Unpledge {
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
            CommitmentTypeV2::UpdateRewardAddress { .. } => {
                // UpdateRewardAddress must have zero value (fee-only)
                if self.value != U256::zero() {
                    return Err(CommitmentValidationError::InvalidUpdateRewardAddressValue {
                        provided: self.value,
                    });
                }
            }
        }

        Ok(())
    }
}

// Type discriminants for CommitmentTypeV2 encoding
const COMMITMENT_TYPE_STAKE: u8 = 1;
const COMMITMENT_TYPE_PLEDGE: u8 = 2;
const COMMITMENT_TYPE_UNPLEDGE: u8 = 3;
const COMMITMENT_TYPE_UNSTAKE: u8 = 4;
const COMMITMENT_TYPE_UPDATE_REWARD_ADDRESS: u8 = 5;

// Size constants
const TYPE_DISCRIMINANT_SIZE: usize = 1;
const U64_SIZE: usize = 8;
const PARTITION_HASH_SIZE: usize = 32;
const IRYS_ADDRESS_SIZE: usize = 20;
const U256_SIZE: usize = 32;

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
pub enum CommitmentTypeV2 {
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
    UpdateRewardAddress {
        #[serde(rename = "newRewardAddress")]
        new_reward_address: IrysAddress,
        nonce: U256,
    },
}

impl From<CommitmentTypeV1> for CommitmentTypeV2 {
    fn from(value: CommitmentTypeV1) -> Self {
        match value {
            CommitmentTypeV1::Stake => Self::Stake,
            CommitmentTypeV1::Pledge {
                pledge_count_before_executing,
            } => Self::Pledge {
                pledge_count_before_executing,
            },
            CommitmentTypeV1::Unpledge {
                pledge_count_before_executing,
                partition_hash,
            } => Self::Unpledge {
                pledge_count_before_executing,
                partition_hash,
            },
            CommitmentTypeV1::Unstake => Self::Unstake,
        }
    }
}

impl Encodable for CommitmentTypeV2 {
    fn encode(&self, acc: &mut dyn bytes::BufMut) {
        // note: RLP headers should not be used for single byte values
        match self {
            Self::Stake => COMMITMENT_TYPE_STAKE.encode(acc),
            Self::Pledge {
                pledge_count_before_executing,
            } => {
                alloy_rlp::Header {
                    list: true,
                    payload_length: self.alloy_rlp_payload_length(),
                }
                .encode(acc);
                COMMITMENT_TYPE_PLEDGE.encode(acc);
                pledge_count_before_executing.encode(acc);
            }
            Self::Unpledge {
                pledge_count_before_executing,
                partition_hash,
            } => {
                alloy_rlp::Header {
                    list: true,
                    payload_length: self.alloy_rlp_payload_length(),
                }
                .encode(acc);
                COMMITMENT_TYPE_UNPLEDGE.encode(acc);
                pledge_count_before_executing.encode(acc);
                partition_hash.encode(acc)
            }
            Self::Unstake => COMMITMENT_TYPE_UNSTAKE.encode(acc),
            Self::UpdateRewardAddress { new_reward_address, nonce } => {
                alloy_rlp::Header {
                    list: true,
                    payload_length: self.alloy_rlp_payload_length(),
                }
                .encode(acc);
                COMMITMENT_TYPE_UPDATE_REWARD_ADDRESS.encode(acc);
                new_reward_address.encode(acc);
                nonce.encode(acc);
            }
        };
    }

    fn length(&self) -> usize {
        let payload_length = self.alloy_rlp_payload_length();
        match self {
            Self::Stake | Self::Unstake => payload_length,
            _ => payload_length + alloy_rlp::length_of_length(payload_length), // for the header
        }
    }
}

impl CommitmentTypeV2 {
    // note: this is separate from the `length` impl in Encodable
    fn alloy_rlp_payload_length(&self) -> usize {
        match self {
            Self::Stake | Self::Unstake => 1,
            Self::Pledge {
                pledge_count_before_executing,
            } => 1 + pledge_count_before_executing.length(),
            Self::Unpledge {
                pledge_count_before_executing,
                partition_hash,
            } => 1 + pledge_count_before_executing.length() + partition_hash.length(),
            Self::UpdateRewardAddress { new_reward_address, nonce } => {
                1 + new_reward_address.length() + nonce.length()
            }
        }
    }
}

impl Decodable for CommitmentTypeV2 {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        if buf.is_empty() {
            return Err(RlpError::InputTooShort);
        }

        let first_byte = buf[0];

        // If first byte < 0x80, it's a raw single-byte value (no RLP header)
        // This handles STAKE and UNSTAKE
        if first_byte < 0x80 {
            buf.advance(1);
            return match first_byte {
                COMMITMENT_TYPE_STAKE => Ok(Self::Stake),
                COMMITMENT_TYPE_UNSTAKE => Ok(Self::Unstake),
                // Reject PLEDGE/UNPLEDGE discriminants without headers
                _ => Err(RlpError::Custom("unexpected raw commitment type")),
            };
        }

        // Otherwise, we have an RLP header - decode it
        let header = alloy_rlp::Header::decode(buf)?;
        if !header.list {
            return Err(RlpError::UnexpectedString);
        }

        let type_id = buf[0];
        buf.advance(1);

        match type_id {
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

                // let mut ph = [0_u8; 32];
                // ph.copy_from_slice(&buf[..PARTITION_HASH_SIZE]);
                // buf.advance(PARTITION_HASH_SIZE);
                Ok(Self::Unpledge {
                    pledge_count_before_executing: count,
                    partition_hash: H256::decode(buf)?,
                })
            }
            COMMITMENT_TYPE_UPDATE_REWARD_ADDRESS => {
                let new_reward_address = IrysAddress::decode(buf)?;
                let nonce = U256::decode(buf)?;
                Ok(Self::UpdateRewardAddress { new_reward_address, nonce })
            }
            _ => Err(RlpError::Custom("unknown commitment type in header")),
        }
    }
}

impl Versioned for CommitmentTransactionV2 {
    const VERSION: u8 = 2;
}

// Manual implementation of Compact for CommitmentTypeV2
impl reth_codecs::Compact for CommitmentTypeV2 {
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
            Self::UpdateRewardAddress { new_reward_address, nonce } => {
                buf.put_u8(COMMITMENT_TYPE_UPDATE_REWARD_ADDRESS);
                buf.put_slice(new_reward_address.0.as_slice());
                buf.put_slice(&nonce.to_be_bytes());
                TYPE_DISCRIMINANT_SIZE + IRYS_ADDRESS_SIZE + U256_SIZE
            }
        }
    }

    fn from_compact(buf: &[u8], _len: usize) -> (Self, &[u8]) {
        // Check minimum buffer size
        if buf.is_empty() {
            panic!("CommitmentTypeV2::from_compact: buffer too short, expected at least 1 byte for type discriminant");
        }

        let type_id = buf[0];

        match type_id {
            COMMITMENT_TYPE_STAKE => (Self::Stake, &buf[TYPE_DISCRIMINANT_SIZE..]),
            COMMITMENT_TYPE_PLEDGE => {
                let required_size = TYPE_DISCRIMINANT_SIZE + U64_SIZE;
                if buf.len() < required_size {
                    panic!(
                        "CommitmentTypeV2::from_compact: buffer too short for Pledge variant, \
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
                        "CommitmentTypeV2::from_compact: buffer too short for Unpledge variant, \
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
                        "CommitmentTypeV2::from_compact: buffer too short for Unpledge partition hash, expected at least {} bytes but got {}",
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
            COMMITMENT_TYPE_UPDATE_REWARD_ADDRESS => {
                let required_size = TYPE_DISCRIMINANT_SIZE + IRYS_ADDRESS_SIZE + U256_SIZE;
                if buf.len() < required_size {
                    panic!(
                        "CommitmentTypeV2::from_compact: buffer too short for UpdateRewardAddress variant, \
                         expected at least {} bytes but got {}",
                        required_size,
                        buf.len()
                    );
                }
                let mut addr_bytes = [0_u8; 20];
                addr_bytes.copy_from_slice(&buf[TYPE_DISCRIMINANT_SIZE..TYPE_DISCRIMINANT_SIZE + IRYS_ADDRESS_SIZE]);
                let mut nonce_bytes = [0_u8; 32];
                nonce_bytes.copy_from_slice(&buf[TYPE_DISCRIMINANT_SIZE + IRYS_ADDRESS_SIZE..required_size]);
                (
                    Self::UpdateRewardAddress {
                        new_reward_address: IrysAddress(alloy_primitives::FixedBytes(addr_bytes)),
                        nonce: U256::from_be_bytes(nonce_bytes),
                    },
                    &buf[required_size..],
                )
            }
            _ => panic!(
                "CommitmentTypeV2::from_compact: unknown commitment type discriminant: {type_id}"
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
    #[case::stake(CommitmentTypeV2::Stake)]
    #[case::pledge_zero(CommitmentTypeV2::Pledge { pledge_count_before_executing: 0 })]
    #[case::pledge_one(CommitmentTypeV2::Pledge { pledge_count_before_executing: 1 })]
    #[case::pledge_hundred(CommitmentTypeV2::Pledge { pledge_count_before_executing: 100 })]
    #[case::pledge_max(CommitmentTypeV2::Pledge { pledge_count_before_executing: u64::MAX })]
    #[case::unpledge_one(CommitmentTypeV2::Unpledge { pledge_count_before_executing: 1, partition_hash: [u8::MIN; 32].into() })]
    #[case::unpledge_hundred(CommitmentTypeV2::Unpledge { pledge_count_before_executing: 100, partition_hash: [128_u8; 32].into() })]
    #[case::unpledge_max(CommitmentTypeV2::Unpledge { pledge_count_before_executing: u64::MAX, partition_hash: [u8::MAX; 32].into() })]
    #[case::unstake(CommitmentTypeV2::Unstake)]
    fn test_commitment_type_rlp_roundtrip(#[case] original: CommitmentTypeV2) {
        // Encode
        let mut buf = BytesMut::new();
        original.encode(&mut buf);

        // Decode
        let mut slice = buf.as_ref();
        let decoded = CommitmentTypeV2::decode(&mut slice).unwrap();

        assert_eq!(original, decoded);
        assert!(slice.is_empty(), "Buffer should be fully consumed");
    }

    #[rstest]
    #[case::stake(CommitmentTypeV2::Stake, 1, COMMITMENT_TYPE_STAKE)]
    #[case::pledge_zero(CommitmentTypeV2::Pledge { pledge_count_before_executing: 0 }, 9, COMMITMENT_TYPE_PLEDGE)]
    #[case::pledge_one(CommitmentTypeV2::Pledge { pledge_count_before_executing: 1 }, 9, COMMITMENT_TYPE_PLEDGE)]
    #[case::pledge_hundred(CommitmentTypeV2::Pledge { pledge_count_before_executing: 100 }, 9, COMMITMENT_TYPE_PLEDGE)]
    #[case::pledge_max(CommitmentTypeV2::Pledge { pledge_count_before_executing: u64::MAX }, 9, COMMITMENT_TYPE_PLEDGE)]
    #[case::unpledge(CommitmentTypeV2::Unpledge { pledge_count_before_executing: 42, partition_hash: [7_u8; 32].into() }, 1 + 8 + 32, COMMITMENT_TYPE_UNPLEDGE)]
    #[case::unstake(CommitmentTypeV2::Unstake, 1, COMMITMENT_TYPE_UNSTAKE)]
    fn test_commitment_type_compact_roundtrip(
        #[case] original: CommitmentTypeV2,
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
        let (decoded, remaining) = CommitmentTypeV2::from_compact(&buf, buf.len());

        assert_eq!(original, decoded);
        assert!(remaining.is_empty(), "Buffer should be fully consumed");
    }

    #[rstest]
    #[case::empty_buffer(&[])]
    #[case::invalid_discriminant(&[99_u8])]
    #[case::pledge_buffer_too_short(&[COMMITMENT_TYPE_PLEDGE])]
    fn test_commitment_type_rlp_decode_errors(#[case] buf: &[u8]) {
        let mut slice = buf;
        assert!(CommitmentTypeV2::decode(&mut slice).is_err());
    }

    #[test]
    #[should_panic(expected = "buffer too short, expected at least 1 byte")]
    fn test_commitment_type_compact_decode_empty_buffer() {
        std::panic::set_hook(Box::new(|_| {})); // Suppress panic output
        let empty_buf = vec![];
        CommitmentTypeV2::from_compact(&empty_buf, 0);
    }

    #[test]
    #[should_panic(expected = "unknown commitment type discriminant: 99")]
    fn test_commitment_type_compact_decode_invalid_type() {
        std::panic::set_hook(Box::new(|_| {})); // Suppress panic output
        let invalid_buf = vec![99_u8];
        CommitmentTypeV2::from_compact(&invalid_buf, invalid_buf.len());
    }

    #[test]
    #[should_panic(expected = "buffer too short for Pledge variant")]
    fn test_commitment_type_compact_decode_pledge_buffer_too_short() {
        std::panic::set_hook(Box::new(|_| {})); // Suppress panic output
        let short_buf = vec![COMMITMENT_TYPE_PLEDGE, 1, 2, 3]; // Only 4 bytes, need 9
        CommitmentTypeV2::from_compact(&short_buf, short_buf.len());
    }

    #[rstest]
    #[case::stake(CommitmentTypeV2::Stake, 1)]
    // note: plus ones are for the RLP header length
    #[case::pledge(CommitmentTypeV2::Pledge { pledge_count_before_executing: 100 }, 1 + 100_u64.length() + 1)]
    #[case::unpledge(CommitmentTypeV2::Unpledge { pledge_count_before_executing: 5, partition_hash: [9_u8; 32].into() }, 1 + 1 + 5_u64.length() + 32 + 1)]
    fn test_commitment_type_rlp_length(
        #[case] commitment_type: CommitmentTypeV2,
        #[case] expected_length: usize,
    ) {
        assert_eq!(commitment_type.length(), expected_length);
    }

    #[test]
    fn test_unpledge_rlp_roundtrip() {
        use bytes::BytesMut;
        let original = CommitmentTypeV2::Unpledge {
            pledge_count_before_executing: 3,
            partition_hash: H256::random(),
        };
        let mut buf = BytesMut::new();
        original.encode(&mut buf);
        let mut slice = buf.as_ref();
        let decoded = CommitmentTypeV2::decode(&mut slice).unwrap();
        assert_eq!(original, decoded);
        assert!(slice.is_empty());
    }
}
