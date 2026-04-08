use crate::CommitmentTypeV2;
pub use crate::{
    Arbitrary, Base64, CommitmentTransactionMetadata, CommitmentTransactionV1,
    CommitmentTransactionV2, CommitmentTypeV1, CommitmentValidationError, Compact, ConsensusConfig,
    H256, IrysAddress, IrysSignature, Node, PledgeDataProvider, Proof, Signature, U256,
    address_base58_stringify, decode_rlp_version, encode_rlp_version,
    ingress::IngressProof,
    optional_string_u64, string_u64,
    versioning::{
        Signable, VersionDiscriminant, Versioned, VersioningError, compact_with_discriminant,
        split_discriminant,
    },
};

use alloy_rlp::Encodable as _;
use irys_macros_integer_tagged::IntegerTagged;
use serde::{Deserialize, Serialize};

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

// Wrapper structs to hold transaction + metadata for each version
// These are transparent wrappers that delegate serde to the inner transaction
#[derive(Clone, Debug, Eq, PartialEq, Hash, Arbitrary, Serialize, Deserialize)]
pub struct CommitmentV1WithMetadata {
    #[serde(flatten)]
    pub tx: CommitmentTransactionV1,
    #[serde(skip)]
    pub metadata: CommitmentTransactionMetadata,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Arbitrary, Serialize, Deserialize)]
pub struct CommitmentV2WithMetadata {
    #[serde(flatten)]
    pub tx: CommitmentTransactionV2,
    #[serde(skip)]
    pub metadata: CommitmentTransactionMetadata,
}

// Commitment Transaction versioned wrapper with metadata
#[derive(Clone, Debug, Eq, IntegerTagged, PartialEq, Arbitrary, Hash)]
#[repr(u8)]
#[integer_tagged(tag = "version")]
pub enum CommitmentTransaction {
    #[integer_tagged(version = 1)]
    V1(CommitmentV1WithMetadata) = 1,
    #[integer_tagged(version = 2)]
    V2(CommitmentV2WithMetadata) = 2,
}

impl Default for CommitmentTransaction {
    fn default() -> Self {
        Self::V2(CommitmentV2WithMetadata {
            tx: CommitmentTransactionV2::default(),
            metadata: CommitmentTransactionMetadata::new(),
        })
    }
}

impl Ord for CommitmentTransaction {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        compare_commitment_transactions(
            &self.commitment_type(),
            self.user_fee(),
            self.id(),
            &other.commitment_type(),
            other.user_fee(),
            other.id(),
        )
    }
}

impl PartialOrd for CommitmentTransaction {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl VersionDiscriminant for CommitmentTransaction {
    fn version(&self) -> u8 {
        match self {
            Self::V1(_) => 1,
            Self::V2(_) => 2,
        }
    }
}

impl CommitmentTransaction {
    /// Calculate the value for a pledge at the given count
    /// Delegates to CommitmentTransactionV2's implementation
    pub fn calculate_pledge_value_at_count(config: &ConsensusConfig, pledge_count: u64) -> U256 {
        CommitmentTransactionV2::calculate_pledge_value_at_count(config, pledge_count)
    }

    /// Get the commitment type from any version
    /// DO NOT USE FOR SIGNING AND/OR ENCODE-DECODE
    #[inline]
    pub fn commitment_type(&self) -> CommitmentTypeV2 {
        match self {
            Self::V1(v1) => v1.tx.commitment_type.into(),
            Self::V2(v2) => v2.tx.commitment_type,
        }
    }

    /// Get the ID from any version
    #[inline]
    pub fn id(&self) -> H256 {
        match self {
            Self::V1(v1) => v1.tx.id,
            Self::V2(v2) => v2.tx.id,
        }
    }

    /// Get the fee from any version
    #[inline]
    pub fn fee(&self) -> u64 {
        match self {
            Self::V1(v1) => v1.tx.fee,
            Self::V2(v2) => v2.tx.fee,
        }
    }

    /// Get the value from any version
    #[inline]
    pub fn value(&self) -> U256 {
        match self {
            Self::V1(v1) => v1.tx.value,
            Self::V2(v2) => v2.tx.value,
        }
    }

    /// Returns the amount this commitment adds to the protocol treasury.
    ///
    /// Only Stake and Pledge lock funds into the treasury at inclusion time.
    /// Unstake/Unpledge refunds are handled on epoch boundaries (not here),
    /// and UpdateRewardAddress has no treasury impact.
    #[inline]
    pub fn treasury_delta(&self) -> U256 {
        match self.commitment_type() {
            CommitmentTypeV2::Stake | CommitmentTypeV2::Pledge { .. } => self.value(),
            CommitmentTypeV2::Unstake
            | CommitmentTypeV2::Unpledge { .. }
            | CommitmentTypeV2::UpdateRewardAddress { .. } => U256::zero(),
        }
    }

    /// Get the signer address from any version
    #[inline]
    pub fn signer(&self) -> IrysAddress {
        match self {
            Self::V1(v1) => v1.tx.signer,
            Self::V2(v2) => v2.tx.signer,
        }
    }

    /// Get the anchor from any version
    #[inline]
    pub fn anchor(&self) -> H256 {
        match self {
            Self::V1(v1) => v1.tx.anchor,
            Self::V2(v2) => v2.tx.anchor,
        }
    }

    /// Get the signature from any version
    #[inline]
    pub fn signature(&self) -> &IrysSignature {
        match self {
            Self::V1(v1) => &v1.tx.signature,
            Self::V2(v2) => &v2.tx.signature,
        }
    }

    /// Set the signer address
    #[inline]
    pub fn set_signer(&mut self, signer: IrysAddress) {
        match self {
            Self::V1(v1) => v1.tx.signer = signer,
            Self::V2(v2) => v2.tx.signer = signer,
        }
    }

    /// Set the signature
    #[inline]
    pub fn set_signature(&mut self, signature: IrysSignature) {
        match self {
            Self::V1(v1) => v1.tx.signature = signature,
            Self::V2(v2) => v2.tx.signature = signature,
        }
    }

    /// Set the transaction ID
    #[inline]
    pub fn set_id(&mut self, id: H256) {
        match self {
            Self::V1(v1) => v1.tx.id = id,
            Self::V2(v2) => v2.tx.id = id,
        }
    }

    /// Set the anchor
    #[inline]
    pub fn set_anchor(&mut self, anchor: H256) {
        match self {
            Self::V1(v1) => v1.tx.anchor = anchor,
            Self::V2(v2) => v2.tx.anchor = anchor,
        }
    }

    /// Set the fee
    #[inline]
    pub fn set_fee(&mut self, fee: u64) {
        match self {
            Self::V1(v1) => v1.tx.fee = fee,
            Self::V2(v2) => v2.tx.fee = fee,
        }
    }

    /// Set the value
    #[inline]
    pub fn set_value(&mut self, value: U256) {
        match self {
            Self::V1(v1) => v1.tx.value = value,
            Self::V2(v2) => v2.tx.value = value,
        }
    }

    /// Set the commitment type
    /// Note: This will panic if called on a V1 transaction, as V1 transactions
    /// should only come from deserialization of historical data and should not be modified.
    #[inline]
    pub fn set_commitment_type(&mut self, commitment_type: CommitmentTypeV2) {
        match self {
            Self::V1(_) => panic!("Cannot set commitment type on V1 transaction"),
            Self::V2(v2) => v2.tx.commitment_type = commitment_type,
        }
    }

    /// Set the chain ID
    #[inline]
    pub fn set_chain_id(&mut self, chain_id: u64) {
        match self {
            Self::V1(v1) => v1.tx.chain_id = chain_id,
            Self::V2(v2) => v2.tx.chain_id = chain_id,
        }
    }

    /// Get the metadata
    #[inline]
    pub fn metadata(&self) -> &CommitmentTransactionMetadata {
        match self {
            Self::V1(v1) => &v1.metadata,
            Self::V2(v2) => &v2.metadata,
        }
    }

    /// Get mutable metadata
    #[inline]
    pub fn metadata_mut(&mut self) -> &mut CommitmentTransactionMetadata {
        match self {
            Self::V1(v1) => &mut v1.metadata,
            Self::V2(v2) => &mut v2.metadata,
        }
    }

    /// Set the metadata
    #[inline]
    pub fn set_metadata(&mut self, new_metadata: CommitmentTransactionMetadata) {
        match self {
            Self::V1(v1) => v1.metadata = new_metadata,
            Self::V2(v2) => v2.metadata = new_metadata,
        }
    }
}

impl Compact for CommitmentTransaction {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        match self {
            Self::V1(inner) => {
                compact_with_discriminant(CommitmentTransactionV1::VERSION, &inner.tx, buf)
            }
            Self::V2(inner) => {
                compact_with_discriminant(CommitmentTransactionV2::VERSION, &inner.tx, buf)
            }
        }
    }
    fn from_compact(buf: &[u8], _len: usize) -> (Self, &[u8]) {
        let (disc, rest) = split_discriminant(buf);
        match disc {
            CommitmentTransactionV1::VERSION => {
                let (inner, rest2) = CommitmentTransactionV1::from_compact(rest, rest.len());
                (
                    Self::V1(CommitmentV1WithMetadata {
                        tx: inner,
                        metadata: CommitmentTransactionMetadata::new(),
                    }),
                    rest2,
                )
            }
            CommitmentTransactionV2::VERSION => {
                let (inner, rest2) = CommitmentTransactionV2::from_compact(rest, rest.len());
                (
                    Self::V2(CommitmentV2WithMetadata {
                        tx: inner,
                        metadata: CommitmentTransactionMetadata::new(),
                    }),
                    rest2,
                )
            }
            other => panic!("{:?}", VersioningError::UnsupportedVersion(other)),
        }
    }
}

impl Signable for CommitmentTransaction {
    fn encode_for_signing(&self, out: &mut dyn bytes::BufMut) {
        self.encode(out);
    }
}

impl alloy_rlp::Encodable for CommitmentTransaction {
    fn encode(&self, out: &mut dyn bytes::BufMut) {
        let mut buf = Vec::new();
        match self {
            Self::V1(inner) => inner.tx.encode(&mut buf),
            Self::V2(inner) => inner.tx.encode(&mut buf),
        }
        encode_rlp_version(buf, self.version(), out);
    }
}

impl alloy_rlp::Decodable for CommitmentTransaction {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let (version, buf) = decode_rlp_version(buf)?;
        let buf = &mut &buf[..];

        match version {
            CommitmentTransactionV1::VERSION => {
                let inner = CommitmentTransactionV1::decode(buf)?;
                Ok(Self::V1(CommitmentV1WithMetadata {
                    tx: inner,
                    metadata: CommitmentTransactionMetadata::new(),
                }))
            }
            CommitmentTransactionV2::VERSION => {
                let inner = CommitmentTransactionV2::decode(buf)?;
                Ok(Self::V2(CommitmentV2WithMetadata {
                    tx: inner,
                    metadata: CommitmentTransactionMetadata::new(),
                }))
            }
            _ => Err(alloy_rlp::Error::Custom("Unsupported version")),
        }
    }
}

impl CommitmentTransaction {
    /// Create a new CommitmentTransaction wrapped in the versioned wrapper
    pub fn new(config: &ConsensusConfig) -> Self {
        Self::V2(CommitmentV2WithMetadata {
            tx: CommitmentTransactionV2::new(config),
            metadata: CommitmentTransactionMetadata::new(),
        })
    }

    /// Create a new stake transaction with the configured stake fee as value
    pub fn new_stake(config: &ConsensusConfig, anchor: H256) -> Self {
        Self::V2(CommitmentV2WithMetadata {
            tx: CommitmentTransactionV2::new_stake(config, anchor),
            metadata: CommitmentTransactionMetadata::new(),
        })
    }

    /// Create a new unstake transaction with the configured stake fee as value
    pub fn new_unstake(config: &ConsensusConfig, anchor: H256) -> Self {
        Self::V2(CommitmentV2WithMetadata {
            tx: CommitmentTransactionV2::new_unstake(config, anchor),
            metadata: CommitmentTransactionMetadata::new(),
        })
    }

    /// Create a new pledge transaction with decreasing cost per pledge
    pub async fn new_pledge(
        config: &ConsensusConfig,
        anchor: H256,
        provider: &impl PledgeDataProvider,
        signer_address: IrysAddress,
    ) -> Self {
        Self::V2(CommitmentV2WithMetadata {
            tx: CommitmentTransactionV2::new_pledge(config, anchor, provider, signer_address).await,
            metadata: CommitmentTransactionMetadata::new(),
        })
    }

    /// Create a new unpledge transaction that refunds the most recent pledge's cost
    pub async fn new_unpledge(
        config: &ConsensusConfig,
        anchor: H256,
        provider: &impl PledgeDataProvider,
        signer_address: IrysAddress,
        partition_hash: H256,
    ) -> Self {
        Self::V2(CommitmentV2WithMetadata {
            tx: CommitmentTransactionV2::new_unpledge(
                config,
                anchor,
                provider,
                signer_address,
                partition_hash,
            )
            .await,
            metadata: CommitmentTransactionMetadata::new(),
        })
    }

    /// Validates that the commitment transaction has a sufficient fee
    pub fn validate_fee(&self, config: &ConsensusConfig) -> Result<(), CommitmentValidationError> {
        match self {
            Self::V1(v1) => v1.tx.validate_fee(config),
            Self::V2(v2) => v2.tx.validate_fee(config),
        }
    }

    /// Validates the value field based on commitment type
    pub fn validate_value(
        &self,
        config: &ConsensusConfig,
    ) -> Result<(), CommitmentValidationError> {
        match self {
            Self::V1(v1) => v1.tx.validate_value(config),
            Self::V2(v2) => v2.tx.validate_value(config),
        }
    }
}

// Ordering for `CommitmentTransaction` prioritizes transactions as follows:
/// 1. Stake commitments (fee desc, then id tie-breaker)
/// 2. Pledge commitments (count asc, then fee desc, then id tie-breaker)
/// 3. UpdateRewardAddress commitments (fee asc, then id tie-breaker) - lowest fee first so highest fee wins at epoch snapshot
/// 4. Unpledge commitments (count desc, then fee desc, then id tie-breaker)
/// 5. Unstake commitments (fee desc, then id tie-breaker)
pub fn compare_commitment_transactions(
    self_type: &CommitmentTypeV2,
    self_fee: U256,
    self_id: H256,
    other_type: &CommitmentTypeV2,
    other_fee: U256,
    other_id: H256,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    fn commitment_priority(commitment_type: &CommitmentTypeV2) -> u8 {
        match commitment_type {
            CommitmentTypeV2::Stake => 0,
            CommitmentTypeV2::Pledge { .. } => 1,
            CommitmentTypeV2::UpdateRewardAddress { .. } => 2,
            CommitmentTypeV2::Unpledge { .. } => 3,
            CommitmentTypeV2::Unstake => 4,
        }
    }

    let self_priority = commitment_priority(self_type);
    let other_priority = commitment_priority(other_type);

    match self_priority.cmp(&other_priority) {
        Ordering::Less => Ordering::Less,
        Ordering::Greater => Ordering::Greater,
        Ordering::Equal => match (self_type, other_type) {
            (CommitmentTypeV2::Stake, CommitmentTypeV2::Stake) => other_fee
                .cmp(&self_fee)
                .then_with(|| self_id.cmp(&other_id)),
            (
                CommitmentTypeV2::Pledge {
                    pledge_count_before_executing: count_a,
                },
                CommitmentTypeV2::Pledge {
                    pledge_count_before_executing: count_b,
                },
            ) => count_a // lowest pledge_count_before_executing first
                .cmp(count_b)
                .then_with(|| other_fee.cmp(&self_fee))
                .then_with(|| self_id.cmp(&other_id)),
            (
                CommitmentTypeV2::Unpledge {
                    pledge_count_before_executing: count_a,
                    ..
                },
                CommitmentTypeV2::Unpledge {
                    pledge_count_before_executing: count_b,
                    ..
                },
            ) => count_b // highest pledge_count_before_executing first
                .cmp(count_a)
                .then_with(|| other_fee.cmp(&self_fee))
                .then_with(|| self_id.cmp(&other_id)),
            (CommitmentTypeV2::Unstake, CommitmentTypeV2::Unstake) => other_fee
                .cmp(&self_fee)
                .then_with(|| self_id.cmp(&other_id)),
            (
                CommitmentTypeV2::UpdateRewardAddress { .. },
                CommitmentTypeV2::UpdateRewardAddress { .. },
            ) => self_fee // lowest fee first (so highest fee wins at epoch snapshot)
                .cmp(&other_fee)
                .then_with(|| self_id.cmp(&other_id)),
            _ => unreachable!("equal priorities imply identical commitment types"),
        },
    }
}

#[cfg(test)]
mod tests {
    /// WRITTEN BY CLAUDE
    use super::*;
    use rstest::rstest;
    use std::cmp::Ordering;

    fn stake() -> CommitmentTypeV2 {
        CommitmentTypeV2::Stake
    }

    fn pledge(count: u64) -> CommitmentTypeV2 {
        CommitmentTypeV2::Pledge {
            pledge_count_before_executing: count,
        }
    }

    fn unpledge(count: u64) -> CommitmentTypeV2 {
        CommitmentTypeV2::Unpledge {
            pledge_count_before_executing: count,
            partition_hash: H256::zero(),
        }
    }

    fn unstake() -> CommitmentTypeV2 {
        CommitmentTypeV2::Unstake
    }

    fn update_reward_address() -> CommitmentTypeV2 {
        CommitmentTypeV2::UpdateRewardAddress {
            new_reward_address: IrysAddress::ZERO,
        }
    }

    // ===================
    // Cross-type priority: Stake < Pledge < UpdateRewardAddress < Unpledge < Unstake
    // ===================
    #[rstest]
    #[case(stake(), pledge(0), Ordering::Less)]
    #[case(stake(), update_reward_address(), Ordering::Less)]
    #[case(stake(), unpledge(0), Ordering::Less)]
    #[case(stake(), unstake(), Ordering::Less)]
    #[case(pledge(0), update_reward_address(), Ordering::Less)]
    #[case(pledge(0), unpledge(0), Ordering::Less)]
    #[case(pledge(0), unstake(), Ordering::Less)]
    #[case(update_reward_address(), unpledge(0), Ordering::Less)]
    #[case(update_reward_address(), unstake(), Ordering::Less)]
    #[case(unpledge(0), unstake(), Ordering::Less)]
    #[case(unstake(), stake(), Ordering::Greater)]
    #[case(unpledge(0), pledge(0), Ordering::Greater)]
    #[case(unpledge(0), update_reward_address(), Ordering::Greater)]
    fn test_cross_type_priority(
        #[case] self_type: CommitmentTypeV2,
        #[case] other_type: CommitmentTypeV2,
        #[case] expected: Ordering,
    ) {
        let result = compare_commitment_transactions(
            &self_type,
            U256::from(100),
            H256::zero(),
            &other_type,
            U256::from(100),
            H256::zero(),
        );
        assert_eq!(result, expected);
    }

    // ===================
    // Stake: fee desc, then id asc
    // ===================
    #[rstest]
    #[case(200, 100, Ordering::Less)] // higher fee comes first
    #[case(100, 200, Ordering::Greater)]
    #[case(100, 100, Ordering::Equal)]
    fn test_stake_fee_ordering(
        #[case] self_fee: u64,
        #[case] other_fee: u64,
        #[case] expected: Ordering,
    ) {
        let result = compare_commitment_transactions(
            &stake(),
            U256::from(self_fee),
            H256::zero(),
            &stake(),
            U256::from(other_fee),
            H256::zero(),
        );
        assert_eq!(result, expected);
    }

    #[test]
    fn test_id_tiebreaker() {
        let id_low = H256::from_low_u64_be(1);
        let id_high = H256::from_low_u64_be(2);

        // Test all commitment types with the same pattern
        let test_cases = [
            (stake(), stake()),
            (pledge(0), pledge(0)),
            (update_reward_address(), update_reward_address()),
            (unpledge(0), unpledge(0)),
            (unstake(), unstake()),
        ];

        for (self_type, other_type) in test_cases {
            let result = compare_commitment_transactions(
                &self_type,
                U256::from(100),
                id_low,
                &other_type,
                U256::from(100),
                id_high,
            );
            assert_eq!(result, Ordering::Less);
        }
    }

    // ===================
    // Pledge: count asc (lowest first), then fee desc, then id asc
    // ===================
    #[rstest]
    #[case(1, 5, Ordering::Less)] // lower count comes first
    #[case(5, 1, Ordering::Greater)]
    #[case(3, 3, Ordering::Equal)]
    fn test_pledge_count_ordering(
        #[case] self_count: u64,
        #[case] other_count: u64,
        #[case] expected: Ordering,
    ) {
        let result = compare_commitment_transactions(
            &pledge(self_count),
            U256::from(100),
            H256::zero(),
            &pledge(other_count),
            U256::from(100),
            H256::zero(),
        );
        assert_eq!(result, expected);
    }

    #[rstest]
    #[case(200, 100, Ordering::Less)] // same count, higher fee comes first
    #[case(100, 200, Ordering::Greater)]
    fn test_pledge_fee_after_count(
        #[case] self_fee: u64,
        #[case] other_fee: u64,
        #[case] expected: Ordering,
    ) {
        let result = compare_commitment_transactions(
            &pledge(5),
            U256::from(self_fee),
            H256::zero(),
            &pledge(5),
            U256::from(other_fee),
            H256::zero(),
        );
        assert_eq!(result, expected);
    }

    // ===================
    // Unpledge: count desc (highest first), then fee desc, then id asc
    // ===================
    #[rstest]
    #[case(5, 1, Ordering::Less)] // higher count comes first
    #[case(1, 5, Ordering::Greater)]
    #[case(3, 3, Ordering::Equal)]
    fn test_unpledge_count_ordering(
        #[case] self_count: u64,
        #[case] other_count: u64,
        #[case] expected: Ordering,
    ) {
        let result = compare_commitment_transactions(
            &unpledge(self_count),
            U256::from(100),
            H256::zero(),
            &unpledge(other_count),
            U256::from(100),
            H256::zero(),
        );
        assert_eq!(result, expected);
    }

    #[rstest]
    #[case(200, 100, Ordering::Less)] // same count, higher fee comes first
    #[case(100, 200, Ordering::Greater)]
    fn test_unpledge_fee_after_count(
        #[case] self_fee: u64,
        #[case] other_fee: u64,
        #[case] expected: Ordering,
    ) {
        let result = compare_commitment_transactions(
            &unpledge(5),
            U256::from(self_fee),
            H256::zero(),
            &unpledge(5),
            U256::from(other_fee),
            H256::zero(),
        );
        assert_eq!(result, expected);
    }

    // ===================
    // Unstake: fee desc, then id asc
    // ===================
    #[rstest]
    #[case(200, 100, Ordering::Less)]
    #[case(100, 200, Ordering::Greater)]
    fn test_unstake_fee_ordering(
        #[case] self_fee: u64,
        #[case] other_fee: u64,
        #[case] expected: Ordering,
    ) {
        let result = compare_commitment_transactions(
            &unstake(),
            U256::from(self_fee),
            H256::zero(),
            &unstake(),
            U256::from(other_fee),
            H256::zero(),
        );
        assert_eq!(result, expected);
    }

    // ===================
    // UpdateRewardAddress: fee asc (lowest fee first, so highest fee wins at epoch snapshot), then id asc
    // ===================
    #[rstest]
    #[case(100, 200, Ordering::Less)] // lower fee comes first
    #[case(200, 100, Ordering::Greater)]
    #[case(100, 100, Ordering::Equal)]
    fn test_update_reward_address_fee_ordering(
        #[case] self_fee: u64,
        #[case] other_fee: u64,
        #[case] expected: Ordering,
    ) {
        let result = compare_commitment_transactions(
            &update_reward_address(),
            U256::from(self_fee),
            H256::zero(),
            &update_reward_address(),
            U256::from(other_fee),
            H256::zero(),
        );
        assert_eq!(result, expected);
    }
}

#[cfg(test)]
mod prop_serde_roundtrip_tests {
    use super::*;
    use proptest::prelude::*;

    fn arb_commitment_type_v2() -> impl Strategy<Value = CommitmentTypeV2> {
        prop_oneof![
            Just(CommitmentTypeV2::Stake),
            any::<u64>().prop_map(|count| CommitmentTypeV2::Pledge {
                pledge_count_before_executing: count,
            }),
            (any::<u64>(), proptest::array::uniform32(any::<u8>())).prop_map(|(count, hash)| {
                CommitmentTypeV2::Unpledge {
                    pledge_count_before_executing: count,
                    partition_hash: H256(hash),
                }
            }),
            Just(CommitmentTypeV2::Unstake),
            proptest::array::uniform20(any::<u8>()).prop_map(|addr| {
                CommitmentTypeV2::UpdateRewardAddress {
                    new_reward_address: IrysAddress::from(addr),
                }
            }),
        ]
    }

    fn arb_commitment_tx_v2() -> impl Strategy<Value = CommitmentTransactionV2> {
        (
            proptest::array::uniform32(any::<u8>()),
            proptest::array::uniform32(any::<u8>()),
            proptest::array::uniform20(any::<u8>()),
            arb_commitment_type_v2(),
            any::<u64>(),
            any::<u64>(),
            proptest::array::uniform32(any::<u8>()),
        )
            .prop_map(
                |(id, anchor, signer, commitment_type, chain_id, fee, value_bytes)| {
                    CommitmentTransactionV2 {
                        id: H256(id),
                        anchor: H256(anchor),
                        signer: IrysAddress::from(signer),
                        commitment_type,
                        chain_id,
                        fee,
                        value: U256::from_be_bytes(value_bytes),
                        signature: IrysSignature::default(),
                    }
                },
            )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn prop_commitment_type_v2_json_roundtrip(
            ct in arb_commitment_type_v2(),
        ) {
            let json = serde_json::to_string(&ct).unwrap();
            let decoded: CommitmentTypeV2 = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(&ct, &decoded);
        }

        #[test]
        fn prop_commitment_tx_v2_json_roundtrip(
            tx in arb_commitment_tx_v2(),
        ) {
            let json = serde_json::to_string(&tx).unwrap();
            let decoded: CommitmentTransactionV2 = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(&tx, &decoded);
        }

        #[test]
        fn prop_commitment_status_json_roundtrip(
            variant in 1_u8..=4_u8,
        ) {
            let status = CommitmentStatus::try_from(variant).unwrap();
            let json = serde_json::to_string(&status).unwrap();
            let decoded: CommitmentStatus = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(status, decoded);
        }

        #[test]
        fn prop_commitment_transaction_rlp_roundtrip(
            tx in arb_commitment_tx_v2(),
        ) {
            use alloy_rlp::{Decodable as _, Encodable as _};
            let versioned = CommitmentTransaction::V2(CommitmentV2WithMetadata {
                tx,
                metadata: CommitmentTransactionMetadata::new(),
            });
            let mut buf = Vec::new();
            versioned.encode(&mut buf);
            let mut slice = &buf[..];
            let decoded = CommitmentTransaction::decode(&mut slice).unwrap();
            prop_assert!(matches!(&decoded, CommitmentTransaction::V2(_)), "expected V2 after RLP roundtrip");
            prop_assert!(slice.is_empty(), "trailing bytes after RLP decode");
            prop_assert_eq!(decoded.id(), H256::zero(), "id is rlp-skipped, should default");
            prop_assert_eq!(versioned.fee(), decoded.fee());
            prop_assert_eq!(versioned.value(), decoded.value());
            prop_assert_eq!(versioned.signer(), decoded.signer());
            prop_assert_eq!(versioned.anchor(), decoded.anchor());
            prop_assert_eq!(versioned.commitment_type(), decoded.commitment_type());
            if let (CommitmentTransaction::V2(orig), CommitmentTransaction::V2(got)) = (&versioned, &decoded) {
                prop_assert_eq!(orig.tx.chain_id, got.tx.chain_id);
            }
        }

        #[test]
        fn prop_commitment_transaction_compact_roundtrip(
            tx in arb_commitment_tx_v2(),
        ) {
            let versioned = CommitmentTransaction::V2(CommitmentV2WithMetadata {
                tx,
                metadata: CommitmentTransactionMetadata::new(),
            });
            let mut buf = Vec::new();
            let len = versioned.to_compact(&mut buf);
            let (decoded, remaining) = CommitmentTransaction::from_compact(&buf, len);
            prop_assert!(matches!(&decoded, CommitmentTransaction::V2(_)), "expected V2 after compact roundtrip");
            prop_assert_eq!(versioned.id(), decoded.id());
            prop_assert_eq!(versioned.fee(), decoded.fee());
            prop_assert_eq!(versioned.value(), decoded.value());
            prop_assert_eq!(versioned.signer(), decoded.signer());
            prop_assert_eq!(versioned.anchor(), decoded.anchor());
            prop_assert_eq!(versioned.commitment_type(), decoded.commitment_type());
            if let (CommitmentTransaction::V2(orig), CommitmentTransaction::V2(got)) = (&versioned, &decoded) {
                prop_assert_eq!(orig.tx.chain_id, got.tx.chain_id);
            }
            prop_assert!(remaining.is_empty(), "trailing bytes after compact decode");
        }
    }

    #[rstest::rstest]
    #[case::zero(0)]
    #[case::five(5)]
    #[case::max(u8::MAX)]
    fn commitment_status_try_from_rejects_invalid(#[case] value: u8) {
        assert!(CommitmentStatus::try_from(value).is_err());
    }

    #[rstest::rstest]
    #[case::pending(1, CommitmentStatus::Pending)]
    #[case::active(2, CommitmentStatus::Active)]
    #[case::inactive(3, CommitmentStatus::Inactive)]
    #[case::slashed(4, CommitmentStatus::Slashed)]
    fn commitment_status_try_from_valid(#[case] value: u8, #[case] expected: CommitmentStatus) {
        assert_eq!(CommitmentStatus::try_from(value).unwrap(), expected);
    }

    #[test]
    #[should_panic(expected = "Cannot set commitment type on V1 transaction")]
    fn set_commitment_type_panics_on_v1() {
        let mut tx = CommitmentTransaction::V1(crate::CommitmentV1WithMetadata {
            tx: crate::CommitmentTransactionV1::default(),
            metadata: Default::default(),
        });
        tx.set_commitment_type(CommitmentTypeV2::Stake);
    }
}
