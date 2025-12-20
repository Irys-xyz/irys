pub use crate::{
    address_base58_stringify, decode_rlp_version, encode_rlp_version,
    ingress::IngressProof,
    optional_string_u64, string_u64,
    versioning::{
        compact_with_discriminant, split_discriminant, Signable, VersionDiscriminant, Versioned,
        VersioningError,
    },
    Arbitrary, Base64, CommitmentTransactionV1, CommitmentTransactionV2, CommitmentTypeV1,
    CommitmentValidationError, Compact, ConsensusConfig, IrysAddress, IrysSignature, Node,
    PledgeDataProvider, Proof, Signature, H256, U256,
};

use alloy_rlp::Encodable as _;
use irys_macros_integer_tagged::IntegerTagged;

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

// #[derive(
//     PartialEq,
//     Debug,
//     Default,
//     Eq,
//     Clone,
//     Copy,
//     Hash,
//     serde::Serialize,
//     serde::Deserialize,
//     arbitrary::Arbitrary,
// )]
// #[serde(rename_all = "camelCase", tag = "type")]
// pub enum CommitmentType {
//     #[default]
//     Stake,
//     Pledge {
//         #[serde(rename = "pledgeCountBeforeExecuting", with = "string_u64")]
//         pledge_count_before_executing: u64,
//     },
//     Unpledge {
//         #[serde(rename = "pledgeCountBeforeExecuting", with = "string_u64")]
//         pledge_count_before_executing: u64,
//         #[serde(rename = "partitionHash")]
//         partition_hash: H256,
//     },
//     Unstake,
// }

// Commitment Transaction versioned wrapper
#[derive(Clone, Debug, Eq, IntegerTagged, PartialEq, Arbitrary, Hash)]
#[repr(u8)]
#[integer_tagged(tag = "version")]
pub enum CommitmentTransaction {
    #[integer_tagged(version = 1)]
    V1(CommitmentTransactionV1) = 1,
    #[integer_tagged(version = 2)]
    V2(CommitmentTransactionV2) = 2,
}

impl Default for CommitmentTransaction {
    fn default() -> Self {
        Self::V2(CommitmentTransactionV2::default())
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
    /// Delegates to the inner type's implementation
    pub fn calculate_pledge_value_at_count(config: &ConsensusConfig, pledge_count: u64) -> U256 {
        CommitmentTransactionV1::calculate_pledge_value_at_count(config, pledge_count)
    }

    /// Get the commitment type from any version
    /// DO NOT USE FOR SIGNING AND/OR ENCODE-DECODE
    /// TODO: switch this to CommitmentTypeV2
    #[inline]
    pub fn commitment_type(&self) -> CommitmentTypeV1 {
        match self {
            Self::V1(v1) => v1.commitment_type,
            Self::V2(v2) => v2.commitment_type.into(),
        }
    }

    /// Get the ID from any version
    #[inline]
    pub fn id(&self) -> H256 {
        match self {
            Self::V1(v1) => v1.id,
            Self::V2(v2) => v2.id,
        }
    }

    /// Get the fee from any version
    #[inline]
    pub fn fee(&self) -> u64 {
        match self {
            Self::V1(v1) => v1.fee,
            Self::V2(v2) => v2.fee,
        }
    }

    /// Get the value from any version
    #[inline]
    pub fn value(&self) -> U256 {
        match self {
            Self::V1(v1) => v1.value,
            Self::V2(v2) => v2.value,
        }
    }

    /// Get the signer address from any version
    #[inline]
    pub fn signer(&self) -> IrysAddress {
        match self {
            Self::V1(v1) => v1.signer,
            Self::V2(v2) => v2.signer,
        }
    }

    /// Get the anchor from any version
    #[inline]
    pub fn anchor(&self) -> H256 {
        match self {
            Self::V1(v1) => v1.anchor,
            Self::V2(v2) => v2.anchor,
        }
    }

    /// Get the signature from any version
    #[inline]
    pub fn signature(&self) -> &IrysSignature {
        match self {
            Self::V1(v1) => &v1.signature,
            Self::V2(v2) => &v2.signature,
        }
    }

    /// Set the signer address
    #[inline]
    pub fn set_signer(&mut self, signer: IrysAddress) {
        match self {
            Self::V1(v1) => v1.signer = signer,
            Self::V2(v2) => v2.signer = signer,
        }
    }

    /// Set the signature
    #[inline]
    pub fn set_signature(&mut self, signature: IrysSignature) {
        match self {
            Self::V1(v1) => v1.signature = signature,
            Self::V2(v2) => v2.signature = signature,
        }
    }

    /// Set the transaction ID
    #[inline]
    pub fn set_id(&mut self, id: H256) {
        match self {
            Self::V1(v1) => v1.id = id,
            Self::V2(v2) => v2.id = id,
        }
    }

    /// Set the anchor
    #[inline]
    pub fn set_anchor(&mut self, anchor: H256) {
        match self {
            Self::V1(v1) => v1.anchor = anchor,
            Self::V2(v2) => v2.anchor = anchor,
        }
    }

    /// Set the fee
    #[inline]
    pub fn set_fee(&mut self, fee: u64) {
        match self {
            Self::V1(v1) => v1.fee = fee,
            Self::V2(v2) => v2.fee = fee,
        }
    }

    /// Set the value
    #[inline]
    pub fn set_value(&mut self, value: U256) {
        match self {
            Self::V1(v1) => v1.value = value,
            Self::V2(v2) => v2.value = value,
        }
    }

    /// Set the commitment type
    /// TODO: change to CommitmentTypeV2
    #[inline]
    pub fn set_commitment_type(&mut self, commitment_type: CommitmentTypeV1) {
        match self {
            Self::V1(v1) => v1.commitment_type = commitment_type,
            Self::V2(v2) => v2.commitment_type = commitment_type.into(),
        }
    }

    /// Set the chain ID
    #[inline]
    pub fn set_chain_id(&mut self, chain_id: u64) {
        match self {
            Self::V1(v1) => v1.chain_id = chain_id,
            Self::V2(v2) => v2.chain_id = chain_id,
        }
    }
}

impl Compact for CommitmentTransaction {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        match self {
            Self::V1(inner) => compact_with_discriminant(1, inner, buf),
            Self::V2(inner) => compact_with_discriminant(1, inner, buf),
        }
    }
    fn from_compact(buf: &[u8], _len: usize) -> (Self, &[u8]) {
        let (disc, rest) = split_discriminant(buf);
        match disc {
            1 => {
                let (inner, rest2) = CommitmentTransactionV1::from_compact(rest, rest.len());
                (Self::V1(inner), rest2)
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
            Self::V1(inner) => inner.encode(&mut buf),
            Self::V2(inner) => inner.encode(&mut buf),
        }
        encode_rlp_version(buf, self.version(), out);
    }
}

impl alloy_rlp::Decodable for CommitmentTransaction {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let (version, buf) = decode_rlp_version(buf)?;
        let buf = &mut &buf[..];

        match version {
            1 => {
                let inner = CommitmentTransactionV1::decode(buf)?;
                Ok(Self::V1(inner))
            }
            _ => Err(alloy_rlp::Error::Custom("Unsupported version")),
        }
    }
}

impl CommitmentTransaction {
    /// Create a new CommitmentTransaction wrapped in the versioned wrapper
    pub fn new(config: &ConsensusConfig) -> Self {
        Self::V1(CommitmentTransactionV1::new(config))
    }

    /// Create a new stake transaction with the configured stake fee as value
    pub fn new_stake(config: &ConsensusConfig, anchor: H256) -> Self {
        Self::V1(CommitmentTransactionV1::new_stake(config, anchor))
    }

    /// Create a new unstake transaction with the configured stake fee as value
    pub fn new_unstake(config: &ConsensusConfig, anchor: H256) -> Self {
        Self::V1(CommitmentTransactionV1::new_unstake(config, anchor))
    }

    /// Create a new pledge transaction with decreasing cost per pledge
    pub async fn new_pledge(
        config: &ConsensusConfig,
        anchor: H256,
        provider: &impl PledgeDataProvider,
        signer_address: IrysAddress,
    ) -> Self {
        Self::V1(
            CommitmentTransactionV1::new_pledge(config, anchor, provider, signer_address).await,
        )
    }

    /// Create a new unpledge transaction that refunds the most recent pledge's cost
    pub async fn new_unpledge(
        config: &ConsensusConfig,
        anchor: H256,
        provider: &impl PledgeDataProvider,
        signer_address: IrysAddress,
        partition_hash: H256,
    ) -> Self {
        Self::V1(
            CommitmentTransactionV1::new_unpledge(
                config,
                anchor,
                provider,
                signer_address,
                partition_hash,
            )
            .await,
        )
    }

    /// Validates that the commitment transaction has a sufficient fee
    pub fn validate_fee(&self, config: &ConsensusConfig) -> Result<(), CommitmentValidationError> {
        match self {
            Self::V1(v1) => v1.validate_fee(config),
            Self::V2(v2) => v2.validate_fee(config),
        }
    }

    /// Validates the value field based on commitment type
    pub fn validate_value(
        &self,
        config: &ConsensusConfig,
    ) -> Result<(), CommitmentValidationError> {
        match self {
            Self::V1(v1) => v1.validate_value(config),
            Self::V2(v2) => v2.validate_value(config),
        }
    }
}

impl Versioned for CommitmentTransactionV1 {
    const VERSION: u8 = 1;
}

// Ordering for `CommitmentTransaction` prioritizes transactions as follows:
/// 1. Stake commitments (fee desc, then id tie-breaker)
/// 2. Pledge commitments (count asc, then fee desc, then id tie-breaker)
/// 3. Unpledge commitments (count asc, then fee desc, then id tie-breaker)
/// 4. Unstake commitments (last, fee desc, then id tie-breaker)
pub fn compare_commitment_transactions(
    self_type: &CommitmentTypeV1,
    self_fee: U256,
    self_id: H256,
    other_type: &CommitmentTypeV1,
    other_fee: U256,
    other_id: H256,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    fn commitment_priority(commitment_type: &CommitmentTypeV1) -> u8 {
        match commitment_type {
            CommitmentTypeV1::Stake => 0,
            CommitmentTypeV1::Pledge { .. } => 1,
            CommitmentTypeV1::Unpledge { .. } => 2,
            CommitmentTypeV1::Unstake => 3,
        }
    }

    let self_priority = commitment_priority(self_type);
    let other_priority = commitment_priority(other_type);

    match self_priority.cmp(&other_priority) {
        Ordering::Less => Ordering::Less,
        Ordering::Greater => Ordering::Greater,
        Ordering::Equal => match (self_type, other_type) {
            (CommitmentTypeV1::Stake, CommitmentTypeV1::Stake) => other_fee
                .cmp(&self_fee)
                .then_with(|| self_id.cmp(&other_id)),
            (
                CommitmentTypeV1::Pledge {
                    pledge_count_before_executing: count_a,
                },
                CommitmentTypeV1::Pledge {
                    pledge_count_before_executing: count_b,
                },
            ) => count_a
                .cmp(count_b)
                .then_with(|| other_fee.cmp(&self_fee))
                .then_with(|| self_id.cmp(&other_id)),
            (
                CommitmentTypeV1::Unpledge {
                    pledge_count_before_executing: count_a,
                    ..
                },
                CommitmentTypeV1::Unpledge {
                    pledge_count_before_executing: count_b,
                    ..
                },
            ) => count_b
                .cmp(count_a)
                .then_with(|| other_fee.cmp(&self_fee))
                .then_with(|| self_id.cmp(&other_id)),
            (CommitmentTypeV1::Unstake, CommitmentTypeV1::Unstake) => other_fee
                .cmp(&self_fee)
                .then_with(|| self_id.cmp(&other_id)),
            _ => Ordering::Equal,
        },
    }
}
