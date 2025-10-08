pub use crate::ingress::IngressProof;
use crate::versioning::{
    compact_with_discriminant, split_discriminant, HasInnerVersion, Signable, VersionDiscriminant,
    Versioned, VersioningError,
};
pub use crate::{
    address_base58_stringify, optional_string_u64, string_u64, Address, Arbitrary, Base64, Compact,
    ConsensusConfig, IrysSignature, Node, Proof, Signature, H256, U256,
};
use crate::{TxChunkOffset, UnpackedChunk};
use alloy_primitives::keccak256;
use alloy_rlp::{Encodable as _, RlpDecodable, RlpEncodable};
pub use irys_primitives::CommitmentType;
use serde::{Deserialize, Serialize};
use std::ops::{Deref, DerefMut};
use thiserror::Error;
use tracing::error;

pub mod fee_distribution;

pub type IrysTransactionId = H256;

/// Errors that can occur during commitment transaction validation
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CommitmentValidationError {
    /// The provided fee is insufficient
    #[error("Insufficient fee: {provided} < required {required}")]
    InsufficientFee { provided: u64, required: u64 },
    /// Invalid stake or unstake value
    #[error("Invalid stake value: {provided} != expected {expected}")]
    InvalidStakeValue { provided: U256, expected: U256 },
    /// Invalid pledge value
    #[error(
        "Invalid pledge value: {provided} != expected {expected} (pledge_count: {pledge_count})"
    )]
    InvalidPledgeValue {
        provided: U256,
        expected: U256,
        pledge_count: u64,
    },
    /// Invalid unpledge value
    #[error(
        "Invalid unpledge value: {provided} != expected {expected} (pledge_count: {pledge_count})"
    )]
    InvalidUnpledgeValue {
        provided: U256,
        expected: U256,
        pledge_count: u64,
    },
    #[error("Signer address is not allowed to stake/pledge")]
    ForbiddenSigner,
}

#[derive(Clone, Debug, Eq, Serialize, Deserialize, PartialEq, Arbitrary)]
#[repr(u8)]
#[serde(untagged)]
pub enum VersionedDataTransactionHeader {
    V1(DataTransactionHeaderV1) = 1,
}

impl VersionDiscriminant for VersionedDataTransactionHeader {
    fn discriminant(&self) -> u8 {
        match self {
            Self::V1(_) => 1,
        }
    }
}

impl Default for VersionedDataTransactionHeader {
    fn default() -> Self {
        Self::V1(DataTransactionHeaderV1::default())
    }
}

impl Ord for VersionedDataTransactionHeader {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (Self::V1(a), Self::V1(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for VersionedDataTransactionHeader {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

// deref & derefmut will only work while we have a single version
// this is what allows us to "hide" the complexity for the rest of the codebase.
impl Deref for VersionedDataTransactionHeader {
    type Target = DataTransactionHeaderV1;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::V1(v1) => v1,
        }
    }
}

impl DerefMut for VersionedDataTransactionHeader {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            Self::V1(v1) => v1,
        }
    }
}

// TODO: write tests for this
impl Compact for VersionedDataTransactionHeader {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        match self {
            Self::V1(inner) => compact_with_discriminant(1, inner, buf),
        }
    }
    fn from_compact(buf: &[u8], _len: usize) -> (Self, &[u8]) {
        let (disc, rest) = split_discriminant(buf);
        match disc {
            1 => {
                let (inner, rest2) = DataTransactionHeaderV1::from_compact(rest, rest.len());
                (Self::V1(inner), rest2)
            }
            other => panic!("{:?}", VersioningError::UnsupportedVersion(other)),
        }
    }
}

impl Signable for VersionedDataTransactionHeader {
    fn encode_for_signing(&self, out: &mut dyn bytes::BufMut) {
        out.put_u8(self.discriminant());
        match self {
            Self::V1(inner) => {
                let mut tmp = Vec::new();
                inner.encode_for_signing(&mut tmp);
                out.put_slice(&tmp);
            }
        }
    }
}

impl VersionedDataTransactionHeader {
    /// Create a new DataTransactionHeader wrapped in the versioned wrapper
    pub fn new(config: &ConsensusConfig) -> Self {
        Self::V1(DataTransactionHeaderV1::new(config))
    }
}

impl Default for DataTransactionHeaderV1 {
    fn default() -> Self {
        Self {
            version: 1,
            id: Default::default(),
            anchor: Default::default(),
            signer: Default::default(),
            data_root: Default::default(),
            data_size: Default::default(),
            header_size: Default::default(),
            term_fee: Default::default(),
            perm_fee: Default::default(),
            ledger_id: Default::default(),
            bundle_format: Default::default(),
            chain_id: Default::default(),
            signature: Default::default(),
            promoted_height: Default::default(),
        }
    }
}
impl Versioned for DataTransactionHeaderV1 {
    const VERSION: u8 = 1;
}
impl HasInnerVersion for DataTransactionHeaderV1 {
    fn inner_version(&self) -> u8 {
        self.version
    }
}

// Commitment Transaction versioned wrapper
#[derive(Clone, Debug, Eq, Serialize, Deserialize, PartialEq, Arbitrary, Hash)]
#[repr(u8)]
#[serde(untagged)]
pub enum VersionedCommitmentTransaction {
    V1(CommitmentTransactionV1) = 1,
}

impl Default for VersionedCommitmentTransaction {
    fn default() -> Self {
        Self::V1(CommitmentTransactionV1::default())
    }
}

impl Ord for VersionedCommitmentTransaction {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (Self::V1(a), Self::V1(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for VersionedCommitmentTransaction {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl VersionDiscriminant for VersionedCommitmentTransaction {
    fn discriminant(&self) -> u8 {
        match self {
            Self::V1(_) => 1,
        }
    }
}

impl Deref for VersionedCommitmentTransaction {
    type Target = CommitmentTransactionV1;
    fn deref(&self) -> &Self::Target {
        match self {
            Self::V1(v) => v,
        }
    }
}
impl DerefMut for VersionedCommitmentTransaction {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            Self::V1(v) => v,
        }
    }
}

impl VersionedCommitmentTransaction {
    /// Calculate the value for a pledge at the given count
    /// Delegates to the inner type's implementation
    pub fn calculate_pledge_value_at_count(config: &ConsensusConfig, pledge_count: u64) -> U256 {
        CommitmentTransactionV1::calculate_pledge_value_at_count(config, pledge_count)
    }
}

impl Compact for VersionedCommitmentTransaction {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        match self {
            Self::V1(inner) => compact_with_discriminant(1, inner, buf),
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

impl Signable for VersionedCommitmentTransaction {
    fn encode_for_signing(&self, out: &mut dyn bytes::BufMut) {
        out.put_u8(self.discriminant());
        match self {
            Self::V1(inner) => {
                let mut tmp = Vec::new();
                inner.encode_for_signing(&mut tmp);
                out.put_slice(&tmp);
            }
        }
    }
}

impl VersionedCommitmentTransaction {
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
        signer_address: Address,
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
        signer_address: Address,
    ) -> Self {
        Self::V1(
            CommitmentTransactionV1::new_unpledge(config, anchor, provider, signer_address).await,
        )
    }
}

impl Default for CommitmentTransactionV1 {
    fn default() -> Self {
        Self {
            id: Default::default(),
            anchor: Default::default(),
            signer: Default::default(),
            commitment_type: Default::default(),
            version: 1,
            chain_id: Default::default(),
            fee: Default::default(),
            value: Default::default(),
            signature: Default::default(),
        }
    }
}

impl Versioned for CommitmentTransactionV1 {
    const VERSION: u8 = 1;
}
impl HasInnerVersion for CommitmentTransactionV1 {
    fn inner_version(&self) -> u8 {
        self.version
    }
}

#[derive(
    Clone,
    Debug,
    Eq,
    Serialize,
    Deserialize,
    PartialEq,
    Arbitrary,
    Compact,
    RlpEncodable,
    RlpDecodable,
)]
#[rlp(trailing)]
/// Stores deserialized fields from a JSON formatted Irys transaction header.
/// will decode from strings or numeric literals for u64 fields, due to JS's max safe int being 2^53-1 instead of 2^64
/// We include the Irys prefix to differentiate from EVM transactions.
/// NOTE: be CAREFUL with using serde(default) it should ONLY be for `Option`al fields.
#[serde(rename_all = "camelCase")]
pub struct DataTransactionHeaderV1 {
    /// The transaction's version
    pub version: u8,

    /// A 256-bit hash of the transaction signature.
    #[rlp(skip)]
    #[rlp(default)]
    // NOTE: both rlp skip AND rlp default must be present in order for field skipping to work
    pub id: H256,

    /// block_hash of a recent (last 50) blocks or the a recent transaction id
    /// from the signer. Multiple transactions can share the same anchor.
    pub anchor: H256,

    /// The ecdsa/secp256k1 public key of the transaction signer
    #[serde(with = "address_base58_stringify")]
    pub signer: Address,

    /// The merkle root of the transactions data chunks
    // #[serde(default, with = "address_base58_stringify")]
    pub data_root: H256,

    /// Size of the transaction data in bytes
    #[serde(with = "string_u64")]
    pub data_size: u64,

    /// Size of the header (to store tags etc.) in bytes
    #[serde(with = "string_u64")]
    pub header_size: u64,

    /// Funds the storage of the transaction data during the storage term (protocol-enforced cost)
    pub term_fee: U256,

    /// Destination ledger for the transaction, default is 0 - Permanent Ledger
    pub ledger_id: u32,

    /// EVM chain ID - used to prevent cross-chain replays
    #[serde(with = "string_u64")]
    pub chain_id: u64,

    /// Transaction signature bytes
    #[rlp(skip)]
    #[rlp(default)]
    pub signature: IrysSignature,

    #[serde(default, with = "optional_string_u64")]
    /// WARNING: None == Some(0) for RLP!!
    pub bundle_format: Option<u64>,

    /// Funds the storage of the transaction for the next 200+ years (protocol-enforced cost)
    #[serde(default)]
    pub perm_fee: Option<U256>,

    /// INTERNAL: Tracks what block this transaction was promoted in, can look up ingress proofs there
    #[rlp(skip)]
    #[rlp(default)]
    #[serde(skip)]
    pub promoted_height: Option<u64>,
}

/// Ordering for DataTransactionHeader by transaction ID
impl Ord for DataTransactionHeaderV1 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.id.cmp(&other.id)
    }
}

impl PartialOrd for DataTransactionHeaderV1 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl DataTransactionHeaderV1 {
    /// RLP Encoding of Transactions for Signing
    ///
    /// When RLP encoding a transaction for signing, an extra byte is included
    /// for the transaction type. This serves to simplify future parsing and
    /// decoding of RLP-encoded headers.
    ///
    /// When signing a transaction, the prehash is formed by RLP encoding the
    /// transaction's header fields. It's important to note that the prehash
    ///
    /// **excludes** certain fields:
    ///
    /// - **Transaction ID**: This is excluded from the prehash.
    /// - **Signature fields**: These are not part of the prehash.
    /// - **Optional fields**: Any optional fields that are `Option::None` are
    ///                        also excluded from the prehash.
    ///
    /// This method ensures that the transaction signature reflects only the
    /// essential data needed for validation and security purposes.
    pub fn encode_for_signing(&self, out: &mut dyn alloy_rlp::BufMut) {
        self.encode(out)
    }
}

/// Wrapper for the underlying DataTransactionHeader fields, this wrapper
/// contains the data/chunk/proof info that is necessary for clients to seed
/// a transactions data to the network.
#[derive(Clone, Default, Debug, Serialize, Deserialize, PartialEq)]
pub struct DataTransaction {
    pub header: VersionedDataTransactionHeader,
    // TODO: make this compatible with stream/iterator data sources
    pub data: Option<Base64>,
    #[serde(skip)]
    pub chunks: Vec<Node>,
    #[serde(skip)]
    pub proofs: Vec<Proof>,
}

impl DataTransaction {
    pub fn signature_hash(&self) -> [u8; 32] {
        self.header.signature_hash()
    }

    pub fn data_chunks(&self) -> eyre::Result<Vec<UnpackedChunk>> {
        // TODO: find a better version
        let data = match &self.data {
            Some(d) => d,
            None => eyre::bail!("missing required tx data"),
        };
        let mut chunks = Vec::with_capacity(self.chunks.len());
        for (idx, chunk) in self.chunks.iter().enumerate() {
            let unpacked_chunk = UnpackedChunk {
                data_root: self.header.data_root,
                data_size: self.header.data_size,
                data_path: Base64(self.proofs[idx].proof.clone()),
                bytes: Base64(data.0[chunk.min_byte_range..chunk.max_byte_range].to_vec()),
                tx_offset: TxChunkOffset::from(idx as u32),
            };
            chunks.push(unpacked_chunk);
        }
        Ok(chunks)
    }
}

impl DataTransactionHeaderV1 {
    pub fn new(config: &ConsensusConfig) -> Self {
        Self {
            id: H256::zero(),
            anchor: H256::zero(),
            signer: Address::default(),
            data_root: H256::zero(),
            data_size: 0,
            header_size: 0,
            term_fee: U256::zero(),
            perm_fee: None,
            ledger_id: 0,
            bundle_format: None,
            version: 1,
            chain_id: config.chain_id,
            signature: Signature::test_signature().into(),
            promoted_height: None,
        }
    }

    /// Simple getter methods for IrysTransaction compatibility
    pub fn id(&self) -> IrysTransactionId {
        self.id
    }

    pub fn signer(&self) -> Address {
        self.signer
    }

    pub fn signature(&self) -> &IrysSignature {
        &self.signature
    }

    pub fn anchor(&self) -> H256 {
        self.anchor
    }

    pub fn user_fee(&self) -> U256 {
        self.term_fee
    }

    pub fn total_cost(&self) -> U256 {
        self.perm_fee.unwrap_or(U256::zero()) + self.term_fee
    }
}

pub type TxPath = Vec<u8>;

/// sha256(tx_path)
pub type TxPathHash = H256;

#[derive(
    Clone,
    Debug,
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
    #[serde(with = "address_base58_stringify")]
    pub signer: Address,

    /// The type of commitment Stake/UnStake Pledge/UnPledge
    pub commitment_type: CommitmentType,

    /// The transaction's version
    pub version: u8,

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

/// Ordering for CommitmentTransaction prioritizes transactions as follows:
/// 1. Stake commitments (sorted by fee, highest first)
/// 2. Pledge commitments (sorted by pledge_count_before_executing ascending, then by fee descending)
/// 3. Other commitment types (sorted by fee)
impl Ord for CommitmentTransactionV1 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // First, compare by commitment type (Stake > Pledge/Unpledge)
        match (&self.commitment_type, &other.commitment_type) {
            (CommitmentType::Stake, CommitmentType::Stake) => {
                // Both are stakes, sort by fee (higher first)
                other.user_fee().cmp(&self.user_fee())
            }
            (CommitmentType::Stake, _) => std::cmp::Ordering::Less, // Stake comes first
            (_, CommitmentType::Stake) => std::cmp::Ordering::Greater, // Stake comes first
            (
                CommitmentType::Pledge {
                    pledge_count_before_executing: count_a,
                },
                CommitmentType::Pledge {
                    pledge_count_before_executing: count_b,
                },
            ) => count_a
                .cmp(count_b)
                .then_with(|| other.user_fee().cmp(&self.user_fee())),
            // Handle other cases (Unpledge, Unstake) - sort by fee
            _ => other.user_fee().cmp(&self.user_fee()),
        }
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
            signer: Address::default(),
            commitment_type: CommitmentType::default(),
            version: 1,
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
        signer_address: Address,
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
        signer_address: Address,
    ) -> Self {
        let count = provider.pledge_count(signer_address).await;

        // If user has no pledges, they get 0 back
        let value = if count == 0 {
            U256::zero()
        } else {
            Self::calculate_pledge_value_at_count(config, count - 1)
        };

        Self {
            commitment_type: CommitmentType::Unpledge {
                pledge_count_before_executing: count,
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

    pub fn signer(&self) -> Address {
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
            } => {
                // For unpledge, validate using the embedded pledge count
                // Calculate expected refund value
                let expected_value = Self::calculate_pledge_value_at_count(
                    config,
                    pledge_count_before_executing.saturating_sub(1),
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

// Trait to abstract common behavior
pub trait IrysTransactionCommon {
    fn is_signature_valid(&self) -> bool;
    fn id(&self) -> IrysTransactionId;
    fn total_cost(&self) -> U256;
    fn signer(&self) -> Address;
    fn signature(&self) -> &IrysSignature;
    fn anchor(&self) -> H256;
    fn user_fee(&self) -> U256;

    /// Sign this transaction with the provided signer
    fn sign(self, signer: &crate::irys::IrysSigner) -> Result<Self, eyre::Error>
    where
        Self: Sized;
}

impl VersionedDataTransactionHeader {
    pub fn user_fee(&self) -> U256 {
        // Return term_fee as the user fee for prioritization
        // todo: use TermFeeCharges to get the fee that will go to the miner
        self.term_fee
    }

    pub fn total_cost(&self) -> U256 {
        self.perm_fee.unwrap_or(U256::zero()) + self.term_fee
    }
}

impl IrysTransactionCommon for VersionedDataTransactionHeader {
    fn is_signature_valid(&self) -> bool {
        self.signature
            .validate_signature(self.signature_hash(), self.signer)
            && keccak256(self.signature.as_bytes()).0 == self.id.0
    }

    fn id(&self) -> IrysTransactionId {
        self.id
    }

    fn total_cost(&self) -> U256 {
        self.total_cost()
    }

    fn signer(&self) -> Address {
        self.signer
    }

    fn signature(&self) -> &IrysSignature {
        &self.signature
    }

    fn anchor(&self) -> H256 {
        self.anchor
    }

    fn user_fee(&self) -> U256 {
        self.user_fee()
    }

    fn sign(mut self, signer: &crate::irys::IrysSigner) -> Result<Self, eyre::Error> {
        use crate::Address;
        use alloy_primitives::keccak256;

        // Store the signer address
        self.signer = Address::from_public_key(signer.signer.verifying_key());
        self.version = 1;

        // Create the signature hash and sign it
        let prehash = self.signature_hash();
        let signature: Signature = signer.signer.sign_prehash_recoverable(&prehash)?.into();

        self.signature = IrysSignature::new(signature);

        // Derive the txid by hashing the signature
        let id: [u8; 32] = keccak256(signature.as_bytes()).into();
        self.id = H256::from(id);

        Ok(self)
    }
}

impl VersionedCommitmentTransaction {
    pub fn user_fee(&self) -> U256 {
        U256::from(self.fee)
    }

    pub fn total_cost(&self) -> U256 {
        let additional_fee = match &self.commitment_type {
            CommitmentType::Stake => self.value,
            CommitmentType::Pledge { .. } => self.value,
            CommitmentType::Unpledge { .. } => U256::zero(),
            CommitmentType::Unstake => U256::zero(),
        };
        U256::from(self.fee).saturating_add(additional_fee)
    }
}

impl IrysTransactionCommon for VersionedCommitmentTransaction {
    fn is_signature_valid(&self) -> bool {
        self.signature
            .validate_signature(self.signature_hash(), self.signer)
            && keccak256(self.signature.as_bytes()).0 == self.id.0
    }

    fn id(&self) -> IrysTransactionId {
        self.id
    }

    fn total_cost(&self) -> U256 {
        self.total_cost()
    }

    fn signer(&self) -> Address {
        self.signer
    }

    fn anchor(&self) -> H256 {
        self.anchor
    }

    fn signature(&self) -> &IrysSignature {
        &self.signature
    }

    fn user_fee(&self) -> U256 {
        self.user_fee()
    }

    fn sign(mut self, signer: &crate::irys::IrysSigner) -> Result<Self, eyre::Error> {
        use alloy_primitives::keccak256;

        // Store the signer address
        self.signer = signer.address();

        // Create the signature hash and sign it
        let prehash = self.signature_hash();
        let signature: Signature = signer.signer.sign_prehash_recoverable(&prehash)?.into();

        self.signature = IrysSignature::new(signature);

        // Derive the txid by hashing the signature
        let id: [u8; 32] = keccak256(signature.as_bytes()).into();
        self.id = H256::from(id);

        Ok(self)
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub enum IrysTransaction {
    Data(VersionedDataTransactionHeader),
    Commitment(VersionedCommitmentTransaction),
}

impl TryInto<VersionedDataTransactionHeader> for IrysTransaction {
    type Error = eyre::Report;
    fn try_into(self) -> Result<VersionedDataTransactionHeader, Self::Error> {
        match self {
            Self::Data(tx) => Ok(tx),
            Self::Commitment(_) => Err(eyre::eyre!("This is a commitment tx")),
        }
    }
}

impl TryInto<VersionedCommitmentTransaction> for IrysTransaction {
    type Error = eyre::Report;

    fn try_into(self) -> Result<VersionedCommitmentTransaction, Self::Error> {
        match self {
            Self::Data(_) => Err(eyre::eyre!("This is a data tx")),
            Self::Commitment(tx) => Ok(tx),
        }
    }
}

// route to IrysTransactionCommon impl
// TODO: a better way to do this?
impl IrysTransactionCommon for IrysTransaction {
    fn is_signature_valid(&self) -> bool {
        match self {
            Self::Data(tx) => tx.is_signature_valid(),
            Self::Commitment(tx) => tx.is_signature_valid(),
        }
    }

    fn id(&self) -> IrysTransactionId {
        match self {
            Self::Data(tx) => tx.id(),
            Self::Commitment(tx) => tx.id(),
        }
    }

    fn signer(&self) -> Address {
        match self {
            Self::Data(tx) => tx.signer(),
            Self::Commitment(tx) => tx.signer(),
        }
    }

    fn signature(&self) -> &IrysSignature {
        match self {
            Self::Data(tx) => tx.signature(),
            Self::Commitment(tx) => tx.signature(),
        }
    }

    fn anchor(&self) -> H256 {
        match self {
            Self::Data(tx) => tx.anchor(),
            Self::Commitment(tx) => tx.anchor(),
        }
    }

    fn total_cost(&self) -> U256 {
        match self {
            Self::Data(tx) => tx.total_cost(),
            Self::Commitment(tx) => tx.total_cost(),
        }
    }

    fn user_fee(&self) -> U256 {
        match self {
            Self::Data(tx) => tx.user_fee(),
            Self::Commitment(tx) => tx.user_fee(),
        }
    }

    fn sign(self, signer: &crate::irys::IrysSigner) -> Result<Self, eyre::Error>
    where
        Self: Sized,
    {
        Ok(match self {
            Self::Data(tx) => Self::Data(tx.sign(signer)?),
            Self::Commitment(tx) => Self::Commitment(tx.sign(signer)?),
        })
    }
}

impl From<VersionedDataTransactionHeader> for IrysTransaction {
    fn from(tx: VersionedDataTransactionHeader) -> Self {
        Self::Data(tx)
    }
}

impl From<VersionedCommitmentTransaction> for IrysTransaction {
    fn from(tx: VersionedCommitmentTransaction) -> Self {
        Self::Commitment(tx)
    }
}

// API variant (extra serialisation logic)
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum IrysTransactionResponse {
    #[serde(rename = "commitment")]
    Commitment(VersionedCommitmentTransaction),

    #[serde(rename = "storage")]
    Storage(VersionedDataTransactionHeader),
}

impl From<VersionedCommitmentTransaction> for IrysTransactionResponse {
    fn from(tx: VersionedCommitmentTransaction) -> Self {
        Self::Commitment(tx)
    }
}

impl From<VersionedDataTransactionHeader> for IrysTransactionResponse {
    fn from(tx: VersionedDataTransactionHeader) -> Self {
        Self::Storage(tx)
    }
}

/// Trait for providing pledge count information for dynamic fee calculation
#[async_trait::async_trait]
pub trait PledgeDataProvider {
    /// Returns the number of existing pledges for a given user address
    async fn pledge_count(&self, user_address: Address) -> u64;
}

#[async_trait::async_trait]
impl PledgeDataProvider for u64 {
    async fn pledge_count(&self, _user_address: Address) -> Self {
        *self
    }
}

#[cfg(test)]
mod test_helpers {
    use super::*;
    use std::collections::HashMap;

    pub(super) struct MockPledgeProvider {
        pub pledge_counts: HashMap<Address, u64>,
    }

    impl MockPledgeProvider {
        pub(super) fn new() -> Self {
            Self {
                pledge_counts: HashMap::new(),
            }
        }

        pub(super) fn with_pledge_count(mut self, address: Address, count: u64) -> Self {
            self.pledge_counts.insert(address, count);
            self
        }
    }

    #[async_trait::async_trait]
    impl PledgeDataProvider for MockPledgeProvider {
        async fn pledge_count(&self, user_address: Address) -> u64 {
            self.pledge_counts.get(&user_address).copied().unwrap_or(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::irys::IrysSigner;

    use alloy_rlp::Decodable as _;

    use k256::ecdsa::SigningKey;
    use serde_json;

    #[test]
    fn test_irys_transaction_header_rlp_round_trip() {
        // setup
        let config = ConsensusConfig::testing();
        let header_versioned = mock_header(&config);
        // Extract inner type for RLP round-trip testing
        let VersionedDataTransactionHeader::V1(mut header) = header_versioned;

        // action
        let mut buffer = vec![];
        header.encode(&mut buffer);
        let decoded = DataTransactionHeaderV1::decode(&mut buffer.as_slice()).unwrap();

        // Assert
        // zero out the id and signature, those do not get encoded
        header.id = H256::zero();
        header.signature = IrysSignature::new(Signature::try_from([0_u8; 65].as_slice()).unwrap());
        assert_eq!(header, decoded);
    }

    #[test]
    fn test_commitment_transaction_rlp_round_trip() {
        // setup
        let config = ConsensusConfig::testing();
        let header_versioned = mock_commitment_tx(&config);
        // Extract inner type for RLP round-trip testing
        let VersionedCommitmentTransaction::V1(mut header) = header_versioned;

        // action
        let mut buffer = vec![];
        header.encode(&mut buffer);
        let decoded = CommitmentTransactionV1::decode(&mut buffer.as_slice()).unwrap();

        // Assert
        // zero out the id and signature, those do not get encoded
        header.id = H256::zero();
        header.signature = IrysSignature::new(Signature::try_from([0_u8; 65].as_slice()).unwrap());
        assert_eq!(header, decoded);
    }

    #[test]
    fn test_irys_transaction_header_serde() {
        // Create a sample DataTransactionHeader
        let config = ConsensusConfig::testing();
        let original_header = mock_header(&config);

        // Serialize the DataTransactionHeader to JSON
        let serialized =
            serde_json::to_string_pretty(&original_header).expect("Failed to serialize");

        println!("{}", &serialized);
        // Deserialize the JSON back to VersionedDataTransactionHeader
        let deserialized: VersionedDataTransactionHeader =
            serde_json::from_str(&serialized).expect("Failed to deserialize");

        // Ensure the deserialized struct matches the original
        assert_eq!(original_header, deserialized);
    }

    #[test]
    fn test_commitment_transaction_serde() {
        // Create a sample commitment tx
        let config = ConsensusConfig::testing();
        let original_tx = mock_commitment_tx(&config);

        // Serialize the commitment tx to JSON
        let serialized = serde_json::to_string_pretty(&original_tx).expect("Failed to serialize");

        println!("{}", &serialized);
        // Deserialize the JSON back to a VersionedCommitmentTransaction
        let deserialized: VersionedCommitmentTransaction =
            serde_json::from_str(&serialized).expect("Failed to deserialize");

        // Ensure the deserialized tx matches the original
        assert_eq!(original_tx, deserialized);
    }

    #[test]
    fn test_tx_encode_and_signing() {
        // setup
        let config = ConsensusConfig::testing();
        let original_header = mock_header(&config);
        // For RLP encoding without discriminant, extract inner type
        let mut sig_data = Vec::new();
        match &original_header {
            VersionedDataTransactionHeader::V1(inner) => inner.encode(&mut sig_data),
        }
        // Decode back to inner type
        let decoded_inner = DataTransactionHeaderV1::decode(&mut sig_data.as_slice()).unwrap();
        // Wrap it back to versioned
        let versioned = VersionedDataTransactionHeader::V1(decoded_inner);

        // action
        let signer = IrysSigner {
            signer: SigningKey::random(&mut rand::thread_rng()),
            chain_id: config.chain_id,
            chunk_size: config.chunk_size,
        };

        // Test signing the header directly using the trait method with versioned type
        let signed_header = versioned.sign(&signer).unwrap();
        assert!(signed_header.is_signature_valid());

        // Also test with DataTransaction
        let tx = DataTransaction {
            header: mock_header(&config),
            ..Default::default()
        };

        let signed_tx = signer.sign_transaction(tx).unwrap();
        assert!(signed_tx.header.is_signature_valid());
    }

    #[test]
    fn test_commitment_tx_encode_and_signing() {
        // setup
        let config = ConsensusConfig::testing();
        let original_tx = mock_commitment_tx(&config);
        // For RLP encoding without discriminant, extract inner type
        let mut sig_data = Vec::new();
        match &original_tx {
            VersionedCommitmentTransaction::V1(inner) => inner.encode(&mut sig_data),
        }
        // Decode back to inner type and wrap
        let decoded_inner = CommitmentTransactionV1::decode(&mut sig_data.as_slice()).unwrap();
        let _dec = VersionedCommitmentTransaction::V1(decoded_inner);

        // action
        let signer = IrysSigner {
            signer: SigningKey::random(&mut rand::thread_rng()),
            chain_id: config.chain_id,
            chunk_size: config.chunk_size,
        };

        // Test using the trait method with versioned type
        let signed_tx = original_tx.sign(&signer).unwrap();

        println!(
            "{}",
            serde_json::to_string_pretty(&signed_tx).expect("Failed to serialize")
        );

        assert!(signed_tx.is_signature_valid());
    }

    fn mock_header(config: &ConsensusConfig) -> VersionedDataTransactionHeader {
        VersionedDataTransactionHeader::V1(DataTransactionHeaderV1 {
            id: H256::from([255_u8; 32]),
            anchor: H256::from([1_u8; 32]),
            signer: Address::default(),
            data_root: H256::from([3_u8; 32]),
            data_size: 1024,
            header_size: 0,
            term_fee: U256::from(100),
            perm_fee: Some(U256::from(200)),
            ledger_id: 1,
            bundle_format: None,
            chain_id: config.chain_id,
            version: 1,
            promoted_height: None,
            signature: Signature::test_signature().into(),
        })
    }

    fn mock_commitment_tx(config: &ConsensusConfig) -> VersionedCommitmentTransaction {
        let mut tx = VersionedCommitmentTransaction::new_stake(config, H256::from([1_u8; 32]));
        tx.id = H256::from([255_u8; 32]);
        tx.signer = Address::default();
        tx.signature = Signature::test_signature().into();
        tx.version = 1; // ensure supported version for signing tests
        tx
    }
}

#[cfg(test)]
mod pledge_decay_parametrized_tests {
    use super::test_helpers::MockPledgeProvider;
    use super::*;
    use crate::storage_pricing::Amount;
    use rstest::rstest;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    #[tokio::test]
    #[rstest]
    #[case(0, dec!(20000.0))]
    #[case(1, dec!(10717.7))]
    #[case(2, dec!(7440.8))]
    #[case(3, dec!(5743.4))]
    #[case(4, dec!(4698.4))]
    #[case(5, dec!(3987.4))]
    #[case(6, dec!(3470.8))]
    #[case(7, dec!(3077.8))]
    #[case(8, dec!(2768.2))]
    #[case(9, dec!(2517.8))]
    #[case(10, dec!(2310.8))]
    #[case(11, dec!(2136.8))]
    #[case(12, dec!(1988.2))]
    #[case(13, dec!(1860.0))]
    #[case(14, dec!(1748.0))]
    #[case(15, dec!(1649.3))]
    #[case(16, dec!(1561.8))]
    #[case(17, dec!(1483.4))]
    #[case(18, dec!(1413.0))]
    #[case(19, dec!(1349.2))]
    #[case(20, dec!(1291.3))]
    #[case(21, dec!(1238.3))]
    #[case(22, dec!(1189.8))]
    #[case(23, dec!(1145.0))]
    #[case(24, dec!(1103.7))]
    async fn test_pledge_cost_with_decay(
        #[case] existing_pledges: u64,
        #[case] expected_cost: Decimal,
    ) {
        // Setup config with $20,000 base fee and 0.9 decay rate
        let mut config = ConsensusConfig::testing();
        config.pledge_base_value = crate::storage_pricing::Amount::token(dec!(20000.0)).unwrap();
        config.pledge_decay = crate::storage_pricing::Amount::percentage(dec!(0.9)).unwrap();

        // Create provider with existing pledge count
        let signer_address = Address::default();
        let provider =
            MockPledgeProvider::new().with_pledge_count(signer_address, existing_pledges);

        // Create a new pledge transaction
        let pledge_tx =
            CommitmentTransactionV1::new_pledge(&config, H256::zero(), &provider, signer_address)
                .await;

        // Convert actual value to decimal for comparison
        let actual_amount = Amount::<()>::new(pledge_tx.value)
            .token_to_decimal()
            .unwrap();

        assert_eq!(actual_amount.round_dp(0), expected_cost.round_dp(0));
    }

    #[tokio::test]
    #[rstest]
    #[case(0, dec!(0))]
    #[case(1, dec!(20000.0))]
    #[case(2, dec!(10717.7))]
    #[case(3, dec!(7440.8))]
    #[case(4, dec!(5743.4))]
    #[case(5, dec!(4698.4))]
    #[case(6, dec!(3987.4))]
    #[case(7, dec!(3470.8))]
    #[case(8, dec!(3077.8))]
    #[case(9, dec!(2768.2))]
    #[case(10,dec!(2517.8))]
    #[case(11,dec!(2310.8))]
    #[case(12,dec!(2136.8))]
    #[case(13,dec!(1988.2))]
    #[case(14,dec!(1860.0))]
    #[case(15,dec!(1748.0))]
    #[case(16,dec!(1649.3))]
    #[case(17,dec!(1561.8))]
    #[case(18,dec!(1483.4))]
    #[case(19,dec!(1413.0))]
    #[case(20,dec!(1349.2))]
    #[case(21,dec!(1291.3))]
    #[case(22,dec!(1238.3))]
    #[case(23,dec!(1189.8))]
    #[case(24,dec!(1145.0))]
    async fn test_unpledge_cost(
        #[case] existing_pledges: u64,
        #[case] expected_unpledge_value: Decimal,
    ) {
        // Setup config with 20,000 IRYS base fee and 0.9 decay rate (same as test_pledge_cost_with_decay)
        let mut config = ConsensusConfig::testing();
        config.pledge_base_value = crate::storage_pricing::Amount::token(dec!(20000.0)).unwrap();
        config.pledge_decay = crate::storage_pricing::Amount::percentage(dec!(0.9)).unwrap();

        // Create provider with existing pledge count
        let signer_address = Address::default();
        let provider =
            MockPledgeProvider::new().with_pledge_count(signer_address, existing_pledges);

        // Create an unpledge transaction
        let unpledge_tx =
            CommitmentTransactionV1::new_unpledge(&config, H256::zero(), &provider, signer_address)
                .await;

        // Verify the commitment type is correct
        assert!(matches!(
            unpledge_tx.commitment_type,
            CommitmentType::Unpledge { .. }
        ));

        // Verify unpledge total cost only includes fee (not value)
        assert_eq!(
            unpledge_tx.total_cost(),
            U256::from(unpledge_tx.fee),
            "Unpledge total cost should only include fee"
        );

        // Convert actual value to decimal for comparison
        let actual_amount = Amount::<()>::new(unpledge_tx.value)
            .token_to_decimal()
            .unwrap();

        assert_eq!(
            actual_amount.round_dp(0),
            expected_unpledge_value.round_dp(0)
        );
    }
}

#[cfg(test)]
mod commitment_ordering_tests {
    use super::*;

    fn create_test_commitment(
        id: &str,
        commitment_type: CommitmentType,
        fee: u64,
    ) -> CommitmentTransactionV1 {
        CommitmentTransactionV1 {
            id: H256::from_slice(&[id.as_bytes()[0]; 32]),
            anchor: H256::zero(),
            signer: Address::default(),
            signature: IrysSignature::default(),
            fee,
            value: U256::zero(),
            commitment_type,
            version: 1,
            chain_id: 1,
        }
    }

    #[test]
    fn test_stake_comes_before_pledge() {
        let stake = create_test_commitment("stake", CommitmentType::Stake, 100);
        let pledge = create_test_commitment(
            "pledge",
            CommitmentType::Pledge {
                pledge_count_before_executing: 1,
            },
            200,
        );

        assert!(stake < pledge);
    }

    #[test]
    fn test_stake_sorted_by_fee() {
        let stake_low = create_test_commitment("stake1", CommitmentType::Stake, 50);
        let stake_high = create_test_commitment("stake2", CommitmentType::Stake, 150);

        assert!(stake_high < stake_low);
    }

    #[test]
    fn test_pledge_sorted_by_count_then_fee() {
        let pledge_count2_fee100 = create_test_commitment(
            "p1",
            CommitmentType::Pledge {
                pledge_count_before_executing: 2,
            },
            100,
        );
        let pledge_count2_fee200 = create_test_commitment(
            "p2",
            CommitmentType::Pledge {
                pledge_count_before_executing: 2,
            },
            200,
        );
        let pledge_count5_fee300 = create_test_commitment(
            "p3",
            CommitmentType::Pledge {
                pledge_count_before_executing: 5,
            },
            300,
        );

        // Lower count comes first
        assert!(pledge_count2_fee100 < pledge_count5_fee300);
        assert!(pledge_count2_fee200 < pledge_count5_fee300);

        // Same count, higher fee comes first
        assert!(pledge_count2_fee200 < pledge_count2_fee100);
    }

    #[test]
    fn test_complete_ordering() {
        // Create commitments with distinct IDs for easier verification
        let stake_high = create_test_commitment("stake_high", CommitmentType::Stake, 150);
        let stake_low = create_test_commitment("stake_low", CommitmentType::Stake, 50);
        let pledge_2_high = create_test_commitment(
            "pledge_2_high",
            CommitmentType::Pledge {
                pledge_count_before_executing: 2,
            },
            200,
        );
        let pledge_2_low = create_test_commitment(
            "pledge_2_low",
            CommitmentType::Pledge {
                pledge_count_before_executing: 2,
            },
            50,
        );
        let pledge_5 = create_test_commitment(
            "pledge_5",
            CommitmentType::Pledge {
                pledge_count_before_executing: 5,
            },
            100,
        );
        let pledge_10 = create_test_commitment(
            "pledge_10",
            CommitmentType::Pledge {
                pledge_count_before_executing: 10,
            },
            300,
        );
        let unstake = create_test_commitment("unstake", CommitmentType::Unstake, 75);

        let mut commitments = vec![
            pledge_5.clone(),
            stake_low.clone(),
            pledge_2_high.clone(),
            stake_high.clone(),
            pledge_2_low.clone(),
            pledge_10.clone(),
            unstake.clone(),
        ];

        commitments.sort();

        // Verify the expected order:
        // 1. stake_high (Stake, fee=150)
        // 2. stake_low (Stake, fee=50)
        // 3. pledge_2_high (Pledge count=2, fee=200)
        // 4. pledge_2_low (Pledge count=2, fee=50)
        // 5. pledge_5 (Pledge count=5, fee=100)
        // 6. pledge_10 (Pledge count=10, fee=300)
        // 7. unstake (Other type, fee=75)

        assert_eq!(commitments[0].id, stake_high.id);
        assert_eq!(commitments[1].id, stake_low.id);
        assert_eq!(commitments[2].id, pledge_2_high.id);
        assert_eq!(commitments[3].id, pledge_2_low.id);
        assert_eq!(commitments[4].id, pledge_5.id);
        assert_eq!(commitments[5].id, pledge_10.id);
        assert_eq!(commitments[6].id, unstake.id);
    }
}
