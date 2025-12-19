pub use crate::ingress::IngressProof;
pub use crate::CommitmentType;
pub use crate::{
    address_base58_stringify, optional_string_u64, string_u64, Arbitrary, Base64, Compact,
    ConsensusConfig, IrysAddress, IrysSignature, Node, Proof, Signature, H256, U256,
};
use crate::{decode_rlp_version, CommitmentValidationError, IrysTransactionId, PledgeDataProvider};
use crate::{
    encode_rlp_version,
    versioning::{
        compact_with_discriminant, split_discriminant, Signable, VersionDiscriminant, Versioned,
        VersioningError,
    },
};

use alloy_rlp::{Encodable as _, RlpDecodable, RlpEncodable};
use irys_macros_integer_tagged::IntegerTagged;
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
    pub commitment_type: CommitmentType,

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
            &self.commitment_type,
            self.user_fee(),
            self.id,
            &other.commitment_type,
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
            commitment_type: CommitmentType::default(),
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
            commitment_type: CommitmentType::Stake,
            anchor,
            fee: config.mempool.commitment_fee,
            value: config.stake_value.amount,
            ..Self::new(config)
        }
    }

    /// Create a new unstake transaction with the configured stake fee as value
    pub fn new_unstake(config: &ConsensusConfig, anchor: H256) -> Self {
        Self {
            commitment_type: CommitmentType::Unstake,
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
            commitment_type: CommitmentType::Pledge {
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
        let value = Self::calculate_pledge_value_at_count(config, count.checked_sub(1).unwrap());

        Self {
            commitment_type: CommitmentType::Unpledge {
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
            CommitmentType::Stake => self.value,
            CommitmentType::Pledge { .. } => self.value,
            CommitmentType::Unpledge { .. } => U256::zero(),
            CommitmentType::Unstake => U256::zero(),
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
            CommitmentType::Stake | CommitmentType::Unstake => {
                // For stake/unstake, value must match configured stake value
                let expected_value = config.stake_value.amount;
                if self.value != expected_value {
                    return Err(CommitmentValidationError::InvalidStakeValue {
                        provided: self.value,
                        expected: expected_value,
                    });
                }
            }
            CommitmentType::Pledge {
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
            CommitmentType::Unpledge {
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

// CommitmentTransactionV2

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
    pub commitment_type: CommitmentType,

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
            commitment_type: CommitmentType::default(),
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
            commitment_type: CommitmentType::Stake,
            anchor,
            fee: config.mempool.commitment_fee,
            value: config.stake_value.amount,
            ..Self::new(config)
        }
    }

    /// Create a new unstake transaction with the configured stake fee as value
    pub fn new_unstake(config: &ConsensusConfig, anchor: H256) -> Self {
        Self {
            commitment_type: CommitmentType::Unstake,
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
            commitment_type: CommitmentType::Pledge {
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
        let value = Self::calculate_pledge_value_at_count(config, count.checked_sub(1).unwrap());

        Self {
            commitment_type: CommitmentType::Unpledge {
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
            CommitmentType::Stake => self.value,
            CommitmentType::Pledge { .. } => self.value,
            CommitmentType::Unpledge { .. } => U256::zero(),
            CommitmentType::Unstake => U256::zero(),
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
            CommitmentType::Stake | CommitmentType::Unstake => {
                // For stake/unstake, value must match configured stake value
                let expected_value = config.stake_value.amount;
                if self.value != expected_value {
                    return Err(CommitmentValidationError::InvalidStakeValue {
                        provided: self.value,
                        expected: expected_value,
                    });
                }
            }
            CommitmentType::Pledge {
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
            CommitmentType::Unpledge {
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
        Self::V1(CommitmentTransactionV1::default())
    }
}

impl Ord for CommitmentTransaction {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        compare_commitment_transactions(
            self.commitment_type(),
            self.user_fee(),
            self.id(),
            other.commitment_type(),
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
    #[inline]
    pub fn commitment_type(&self) -> &CommitmentType {
        match self {
            Self::V1(v1) => &v1.commitment_type,
            Self::V2(v2) => &v2.commitment_type,
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
    #[inline]
    pub fn set_commitment_type(&mut self, commitment_type: CommitmentType) {
        match self {
            Self::V1(v1) => v1.commitment_type = commitment_type,
            Self::V2(v2) => v2.commitment_type = commitment_type,
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
fn compare_commitment_transactions(
    self_type: &CommitmentType,
    self_fee: U256,
    self_id: H256,
    other_type: &CommitmentType,
    other_fee: U256,
    other_id: H256,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    fn commitment_priority(commitment_type: &CommitmentType) -> u8 {
        match commitment_type {
            CommitmentType::Stake => 0,
            CommitmentType::Pledge { .. } => 1,
            CommitmentType::Unpledge { .. } => 2,
            CommitmentType::Unstake => 3,
        }
    }

    let self_priority = commitment_priority(self_type);
    let other_priority = commitment_priority(other_type);

    match self_priority.cmp(&other_priority) {
        Ordering::Less => Ordering::Less,
        Ordering::Greater => Ordering::Greater,
        Ordering::Equal => match (self_type, other_type) {
            (CommitmentType::Stake, CommitmentType::Stake) => other_fee
                .cmp(&self_fee)
                .then_with(|| self_id.cmp(&other_id)),
            (
                CommitmentType::Pledge {
                    pledge_count_before_executing: count_a,
                },
                CommitmentType::Pledge {
                    pledge_count_before_executing: count_b,
                },
            ) => count_a
                .cmp(count_b)
                .then_with(|| other_fee.cmp(&self_fee))
                .then_with(|| self_id.cmp(&other_id)),
            (
                CommitmentType::Unpledge {
                    pledge_count_before_executing: count_a,
                    ..
                },
                CommitmentType::Unpledge {
                    pledge_count_before_executing: count_b,
                    ..
                },
            ) => count_b
                .cmp(count_a)
                .then_with(|| other_fee.cmp(&self_fee))
                .then_with(|| self_id.cmp(&other_id)),
            (CommitmentType::Unstake, CommitmentType::Unstake) => other_fee
                .cmp(&self_fee)
                .then_with(|| self_id.cmp(&other_id)),
            _ => Ordering::Equal,
        },
    }
}
