pub use crate::{
    address_base58_stringify, decode_rlp_version, encode_rlp_version,
    ingress::IngressProof,
    optional_string_u64, string_u64,
    versioning::{
        compact_with_discriminant, split_discriminant, Signable, VersionDiscriminant, Versioned,
        VersioningError,
    },
    Arbitrary, Base64, CommitmentTransaction, CommitmentTypeV1, CommitmentTypeV2, Compact,
    ConsensusConfig, DataTransactionMetadata, IrysAddress, IrysSignature, Node, Proof, Signature,
    TxChunkOffset, UnpackedChunk, H256, U256,
};

use alloy_primitives::keccak256;
use alloy_rlp::{Encodable as _, RlpDecodable, RlpEncodable};
use irys_macros_integer_tagged::IntegerTagged;
use serde::{Deserialize, Serialize};
use std::ops::{Deref, DerefMut};
use thiserror::Error;

pub mod bounded_fee;
pub mod fee_distribution;

use crate::DataLedger;
pub use bounded_fee::BoundedFee;

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
    /// Invalid unpledge because pledge_count_before_executing is zero
    #[error("Invalid unpledge: pledge_count_before_executing must be > 0")]
    InvalidUnpledgeCountZero,
    #[error("Signer address is not allowed to stake/pledge")]
    ForbiddenSigner,
    /// Invalid update reward address value (must be zero)
    #[error("Invalid update reward address value: {provided} (must be 0)")]
    InvalidUpdateRewardAddressValue { provided: U256 },
}

// Wrapper struct to hold transaction + metadata
// This is a transparent wrapper that delegates serde to the inner transaction
#[derive(Clone, Debug, Default, Arbitrary, Serialize, Deserialize)]
pub struct DataTransactionHeaderV1WithMetadata {
    #[serde(flatten)]
    pub tx: DataTransactionHeaderV1,
    #[serde(skip)]
    pub metadata: DataTransactionMetadata,
}

#[derive(Clone, Debug, IntegerTagged, Arbitrary)]
#[repr(u8)]
#[integer_tagged(tag = "version")]
pub enum DataTransactionHeader {
    #[integer_tagged(version = 1)]
    V1(DataTransactionHeaderV1WithMetadata) = 1,
}

impl VersionDiscriminant for DataTransactionHeader {
    fn version(&self) -> u8 {
        match self {
            Self::V1(_) => 1,
        }
    }
}

impl Default for DataTransactionHeader {
    fn default() -> Self {
        Self::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1::default(),
            metadata: DataTransactionMetadata::new(),
        })
    }
}

// deref & derefmut will only work while we have a single version
// this is what allows us to "hide" the complexity for the rest of the codebase.
impl Deref for DataTransactionHeader {
    type Target = DataTransactionHeaderV1;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::V1(wrapper) => &wrapper.tx,
        }
    }
}

impl DerefMut for DataTransactionHeader {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            Self::V1(wrapper) => &mut wrapper.tx,
        }
    }
}

impl Compact for DataTransactionHeader {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        match self {
            Self::V1(wrapper) => compact_with_discriminant(1, &wrapper.tx, buf),
        }
    }
    fn from_compact(buf: &[u8], _len: usize) -> (Self, &[u8]) {
        let (disc, rest) = split_discriminant(buf);
        match disc {
            1 => {
                let (wrapper, rest2) = DataTransactionHeaderV1::from_compact(rest, rest.len());
                (
                    Self::V1(DataTransactionHeaderV1WithMetadata {
                        tx: wrapper,
                        metadata: DataTransactionMetadata::new(),
                    }),
                    rest2,
                )
            }
            other => panic!("{:?}", VersioningError::UnsupportedVersion(other)),
        }
    }
}

impl Signable for DataTransactionHeader {
    fn encode_for_signing(&self, out: &mut dyn bytes::BufMut) {
        self.encode(out);
    }
}

impl alloy_rlp::Encodable for DataTransactionHeader {
    fn encode(&self, out: &mut dyn bytes::BufMut) {
        let mut buf = Vec::new();
        match self {
            Self::V1(wrapper) => wrapper.tx.encode(&mut buf),
        }
        encode_rlp_version(buf, self.version(), out);
    }
}

impl alloy_rlp::Decodable for DataTransactionHeader {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let (version, buf) = decode_rlp_version(buf)?;
        let buf = &mut &buf[..];

        match version {
            1 => {
                let tx = DataTransactionHeaderV1::decode(buf)?;
                Ok(Self::V1(DataTransactionHeaderV1WithMetadata {
                    tx,
                    metadata: DataTransactionMetadata::new(),
                }))
            }
            _ => Err(alloy_rlp::Error::Custom("Unsupported version")),
        }
    }
}

impl DataTransactionHeader {
    /// Create a new DataTransactionHeader wrapped in the versioned wrapper
    pub fn new(config: &ConsensusConfig) -> Self {
        Self::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1::new(config),
            metadata: DataTransactionMetadata::new(),
        })
    }

    pub fn try_as_header_v1(&self) -> Option<&DataTransactionHeaderV1> {
        match self {
            Self::V1(v1) => Some(&v1.tx),
        }
    }

    pub fn eq_tx(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::V1(v1_self), Self::V1(v1_other)) => v1_self.tx == v1_other.tx,
        }
    }

    pub fn compare_tx(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (Self::V1(v1_self), Self::V1(v1_other)) => v1_self.tx.cmp(&v1_other.tx),
        }
    }

    /// Get the metadata
    #[inline]
    pub fn metadata(&self) -> &DataTransactionMetadata {
        match self {
            Self::V1(v1) => &v1.metadata,
        }
    }

    /// Get mutable metadata
    #[inline]
    pub fn metadata_mut(&mut self) -> &mut DataTransactionMetadata {
        match self {
            Self::V1(v1) => &mut v1.metadata,
        }
    }

    /// Set the metadata
    #[inline]
    pub fn set_metadata(&mut self, new_metadata: DataTransactionMetadata) {
        match self {
            Self::V1(v1) => v1.metadata = new_metadata,
        }
    }

    /// Convenience method to get promoted_height from metadata
    #[inline]
    pub fn promoted_height(&self) -> Option<u64> {
        self.metadata().promoted_height
    }

    /// Convenience method to set promoted_height in metadata
    #[inline]
    pub fn set_promoted_height(&mut self, height: Option<u64>) {
        self.metadata_mut().promoted_height = height;
    }
}

impl Versioned for DataTransactionHeaderV1 {
    const VERSION: u8 = 1;
}

// impl HasInnerVersion for DataTransactionHeaderV1 {
//     fn inner_version(&self) -> u8 {
//         self.VERSION
//     }
// }

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
)]
#[rlp(trailing)]
/// Stores deserialized fields from a JSON formatted Irys transaction header.
/// will decode from strings or numeric literals for u64 fields, due to JS's max safe int being 2^53-1 instead of 2^64
/// We include the Irys prefix to differentiate from EVM transactions.
/// NOTE: be CAREFUL with using serde(default) it should ONLY be for `Option`al fields.
#[serde(rename_all = "camelCase")]
pub struct DataTransactionHeaderV1 {
    /// A 256-bit hash of the transaction signature.
    #[rlp(skip)]
    #[rlp(default)]
    // NOTE: both rlp skip AND rlp default must be present in order for field skipping to work
    pub id: H256,

    /// block_hash of a recent (last 50) blocks or the a recent transaction id
    /// from the signer. Multiple transactions can share the same anchor.
    pub anchor: H256,

    /// The ecdsa/secp256k1 public key of the transaction signer
    // #[serde(with = "address_base58_stringify")]
    pub signer: IrysAddress,

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
    pub term_fee: BoundedFee,

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
    pub perm_fee: Option<BoundedFee>,
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
#[derive(Clone, Default, Debug, Serialize, Deserialize)]
pub struct DataTransaction {
    pub header: DataTransactionHeader,
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
            signer: IrysAddress::default(),
            data_root: H256::zero(),
            data_size: 0,
            header_size: 0,
            term_fee: BoundedFee::zero(),
            perm_fee: None,
            ledger_id: DataLedger::Publish.into(),
            bundle_format: None,
            chain_id: config.chain_id,
            signature: Signature::test_signature().into(),
        }
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

    pub fn user_fee(&self) -> BoundedFee {
        self.term_fee
    }

    pub fn total_cost(&self) -> BoundedFee {
        self.perm_fee.unwrap_or_default() + self.term_fee
    }
}

pub type TxPath = Vec<u8>;

/// sha256(tx_path)
pub type TxPathHash = H256;

// Trait to abstract common behavior
pub trait IrysTransactionCommon {
    fn is_signature_valid(&self) -> bool;
    fn id(&self) -> IrysTransactionId;
    fn total_cost(&self) -> U256;
    fn signer(&self) -> IrysAddress;
    fn signature(&self) -> &IrysSignature;
    fn anchor(&self) -> H256;
    fn user_fee(&self) -> U256;

    /// Sign this transaction with the provided signer
    fn sign(self, signer: &crate::irys::IrysSigner) -> Result<Self, eyre::Error>
    where
        Self: Sized;

    /// Computed using a combination of signature bytes + prehash + ID bytes to uniquely identify even invalid transactions (i.e with an invalid ID/signature set)
    fn fingerprint(&self) -> H256;
}

impl DataTransactionHeader {
    pub fn user_fee(&self) -> BoundedFee {
        // Return term_fee as the user fee for prioritization
        // todo: use TermFeeCharges to get the fee that will go to the miner
        self.term_fee
    }

    pub fn total_cost(&self) -> BoundedFee {
        self.perm_fee.unwrap_or_default() + self.term_fee
    }
}

impl IrysTransactionCommon for DataTransactionHeader {
    fn is_signature_valid(&self) -> bool {
        self.signature
            .validate_signature(self.signature_hash(), self.signer)
            && keccak256(self.signature.as_bytes()).0 == self.id.0
    }

    fn id(&self) -> IrysTransactionId {
        self.id
    }

    fn total_cost(&self) -> U256 {
        Self::total_cost(self).into()
    }

    fn signer(&self) -> IrysAddress {
        self.signer
    }

    fn signature(&self) -> &IrysSignature {
        &self.signature
    }

    fn anchor(&self) -> H256 {
        self.anchor
    }

    fn user_fee(&self) -> U256 {
        Self::user_fee(self).into()
    }

    fn sign(mut self, signer: &crate::irys::IrysSigner) -> Result<Self, eyre::Error> {
        use crate::IrysAddress;
        use alloy_primitives::keccak256;

        // Store the signer address
        self.signer = IrysAddress::from_public_key(signer.signer.verifying_key());

        // Create the signature hash and sign it
        let prehash = self.signature_hash();
        let signature: Signature = signer.signer.sign_prehash_recoverable(&prehash)?.into();

        self.signature = IrysSignature::new(signature);

        // Derive the txid by hashing the signature
        let id: [u8; 32] = keccak256(signature.as_bytes()).into();
        self.id = H256::from(id);

        Ok(self)
    }

    fn fingerprint(&self) -> H256 {
        // Compute composite fingerprint: keccak(signature + prehash + id)
        let prehash = self.signature_hash();
        let mut buf = Vec::with_capacity(65 + 32 + 32);
        buf.extend_from_slice(&self.signature.as_bytes());
        buf.extend_from_slice(&prehash);
        buf.extend_from_slice(&self.id.0);
        H256::from(alloy_primitives::keccak256(&buf).0)
    }
}

impl CommitmentTransaction {
    pub fn user_fee(&self) -> U256 {
        U256::from(self.fee())
    }

    pub fn total_cost(&self) -> U256 {
        let additional_fee = match &self.commitment_type() {
            CommitmentTypeV2::Stake => self.value(),
            CommitmentTypeV2::Pledge { .. } => self.value(),
            CommitmentTypeV2::Unpledge { .. } => U256::zero(),
            CommitmentTypeV2::Unstake => U256::zero(),
            CommitmentTypeV2::UpdateRewardAddress { .. } => U256::zero(),
        };
        U256::from(self.fee()).saturating_add(additional_fee)
    }
}

impl IrysTransactionCommon for CommitmentTransaction {
    fn is_signature_valid(&self) -> bool {
        self.signature()
            .validate_signature(self.signature_hash(), self.signer())
            && keccak256(self.signature().as_bytes()).0 == self.id().0
    }

    fn id(&self) -> IrysTransactionId {
        self.id()
    }

    fn total_cost(&self) -> U256 {
        Self::total_cost(self)
    }

    fn signer(&self) -> IrysAddress {
        self.signer()
    }

    fn anchor(&self) -> H256 {
        self.anchor()
    }

    fn signature(&self) -> &IrysSignature {
        self.signature()
    }

    fn user_fee(&self) -> U256 {
        Self::user_fee(self)
    }

    fn sign(mut self, signer: &crate::irys::IrysSigner) -> Result<Self, eyre::Error> {
        use alloy_primitives::keccak256;

        // Store the signer address
        self.set_signer(signer.address());

        // Create the signature hash and sign it
        let prehash = self.signature_hash();
        let signature: Signature = signer.signer.sign_prehash_recoverable(&prehash)?.into();

        self.set_signature(IrysSignature::new(signature));

        // Derive the txid by hashing the signature
        let id: [u8; 32] = keccak256(signature.as_bytes()).into();
        self.set_id(H256::from(id));

        Ok(self)
    }

    fn fingerprint(&self) -> H256 {
        // Compute composite fingerprint: keccak(signature + prehash + id)
        let prehash = self.signature_hash();
        let mut buf = Vec::with_capacity(65 + 32 + 32);
        buf.extend_from_slice(&self.signature().as_bytes());
        buf.extend_from_slice(&prehash);
        buf.extend_from_slice(&self.id().0);
        H256::from(alloy_primitives::keccak256(&buf).0)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum IrysTransaction {
    Data(DataTransactionHeader),
    Commitment(CommitmentTransaction),
}

impl TryInto<DataTransactionHeader> for IrysTransaction {
    type Error = eyre::Report;
    fn try_into(self) -> Result<DataTransactionHeader, Self::Error> {
        match self {
            Self::Data(tx) => Ok(tx),
            Self::Commitment(_) => Err(eyre::eyre!("This is a commitment tx")),
        }
    }
}

impl TryInto<CommitmentTransaction> for IrysTransaction {
    type Error = eyre::Report;

    fn try_into(self) -> Result<CommitmentTransaction, Self::Error> {
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

    fn signer(&self) -> IrysAddress {
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
            Self::Data(tx) => tx.total_cost().into(),
            Self::Commitment(tx) => tx.total_cost(),
        }
    }

    fn user_fee(&self) -> U256 {
        match self {
            Self::Data(tx) => tx.user_fee().into(),
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

    fn fingerprint(&self) -> H256 {
        match self {
            Self::Data(tx) => tx.fingerprint(),
            Self::Commitment(tx) => tx.fingerprint(),
        }
    }
}

impl From<DataTransactionHeader> for IrysTransaction {
    fn from(tx: DataTransactionHeader) -> Self {
        Self::Data(tx)
    }
}

impl From<CommitmentTransaction> for IrysTransaction {
    fn from(tx: CommitmentTransaction) -> Self {
        Self::Commitment(tx)
    }
}

// API variant (extra serialisation logic)
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum IrysTransactionResponse {
    #[serde(rename = "commitment")]
    Commitment(CommitmentTransaction),

    #[serde(rename = "storage")]
    Storage(DataTransactionHeader),
}

impl PartialEq for IrysTransactionResponse {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Commitment(tx1), Self::Commitment(tx2)) => tx1 == tx2,
            (Self::Storage(tx1), Self::Storage(tx2)) => tx1.eq_tx(tx2),
            _ => false,
        }
    }
}

impl Eq for IrysTransactionResponse {}

impl From<CommitmentTransaction> for IrysTransactionResponse {
    fn from(tx: CommitmentTransaction) -> Self {
        Self::Commitment(tx)
    }
}

impl From<DataTransactionHeader> for IrysTransactionResponse {
    fn from(tx: DataTransactionHeader) -> Self {
        Self::Storage(tx)
    }
}

/// Trait for providing pledge count information for dynamic fee calculation
#[async_trait::async_trait]
pub trait PledgeDataProvider {
    /// Returns the number of existing pledges for a given user address
    async fn pledge_count(&self, user_address: IrysAddress) -> u64;
}

#[async_trait::async_trait]
impl PledgeDataProvider for u64 {
    async fn pledge_count(&self, _user_address: IrysAddress) -> Self {
        *self
    }
}

#[cfg(test)]
mod test_helpers {
    use super::*;
    use std::collections::HashMap;

    pub(super) struct MockPledgeProvider {
        pub pledge_counts: HashMap<IrysAddress, u64>,
    }

    impl MockPledgeProvider {
        pub(super) fn new() -> Self {
            Self {
                pledge_counts: HashMap::new(),
            }
        }

        pub(super) fn with_pledge_count(mut self, address: IrysAddress, count: u64) -> Self {
            self.pledge_counts.insert(address, count);
            self
        }
    }

    #[async_trait::async_trait]
    impl PledgeDataProvider for MockPledgeProvider {
        async fn pledge_count(&self, user_address: IrysAddress) -> u64 {
            self.pledge_counts.get(&user_address).copied().unwrap_or(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode_rlp_version, encode_rlp_version, irys::IrysSigner, DataLedger};

    use alloy_rlp::Decodable as _;
    use k256::ecdsa::SigningKey;
    use serde_json;

    #[test]
    fn test_complex_versioned_rlp_roundtrip() {
        // this tests multiple versions, as well as deeper structures.
        // as we don't have any V2/nested versioned structures right now

        #[derive(Clone, Debug, Eq, IntegerTagged, PartialEq, Arbitrary)]
        #[repr(u8)]
        #[integer_tagged(tag = "version")]
        enum DataTransactionHeader2 {
            #[integer_tagged(version = 1)]
            V1(DataTransactionHeaderV1) = 1,
            #[integer_tagged(version = 2)]
            V2(DataTransactionHeaderV1) = 2,
        }

        impl Default for DataTransactionHeader2 {
            fn default() -> Self {
                Self::V1(Default::default())
            }
        }

        impl VersionDiscriminant for DataTransactionHeader2 {
            fn version(&self) -> u8 {
                match self {
                    Self::V1(_) => 1,
                    &Self::V2(_) => 2,
                }
            }
        }

        impl alloy_rlp::Encodable for DataTransactionHeader2 {
            fn encode(&self, out: &mut dyn bytes::BufMut) {
                let mut tmp = Vec::new();
                match self {
                    Self::V1(inner) => inner.encode(&mut tmp),
                    Self::V2(inner) => inner.encode(&mut tmp),
                }
                encode_rlp_version(tmp, self.version(), out);
            }
        }

        impl alloy_rlp::Decodable for DataTransactionHeader2 {
            fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
                let (version, buf) = decode_rlp_version(buf)?;
                let buf = &mut &buf[..];
                match version {
                    1 => Ok(Self::V1(DataTransactionHeaderV1::decode(buf)?)),
                    2 => Ok(Self::V2(DataTransactionHeaderV1::decode(buf)?)),
                    _ => Err(alloy_rlp::Error::Custom("Unsupported version")),
                }
            }
        }

        #[expect(dead_code)]
        #[derive(
            Clone, Debug, Default, Eq, Serialize, Deserialize, PartialEq, RlpEncodable, RlpDecodable,
        )]
        struct Test {
            version: u8,
            tx: DataTransactionHeaderV1,
        }

        // setup
        let inner_header = DataTransactionHeaderV1 {
            id: H256::zero(),
            anchor: H256::from([1_u8; 32]),
            signer: IrysAddress::default(),
            data_root: H256::from([3_u8; 32]),
            data_size: 1024,
            header_size: 0,
            term_fee: BoundedFee::from(100_u64),
            perm_fee: Some(BoundedFee::from(200_u64)),
            ledger_id: DataLedger::Submit.into(),
            bundle_format: None,
            chain_id: 1,
            signature: IrysSignature::new(Signature::try_from([0_u8; 65].as_slice()).unwrap()),
        };

        // encode as a v2
        let header = DataTransactionHeader2::V2(inner_header.clone());

        // action - test RLP encoding/decoding the outer versioned structure
        let mut buffer = vec![];
        header.encode(&mut buffer);

        let decoded = DataTransactionHeader2::decode(&mut buffer.as_slice()).unwrap();

        // Assert

        assert_eq!(header, decoded);
        // Verify version discriminant is preserved in RLP encoding
        assert_eq!(decoded.version(), 2);

        // Nested data structure tests
        // setup

        #[derive(
            Clone, Debug, Default, Eq, Serialize, Deserialize, PartialEq, RlpEncodable, RlpDecodable,
        )]
        struct Nested {
            a: DataTransactionHeader2,
            b: DataTransactionHeaderV1,
        }

        #[derive(
            Clone, Debug, Default, Eq, Serialize, Deserialize, PartialEq, RlpEncodable, RlpDecodable,
        )]
        struct NestedOuter {
            a: DataTransactionHeader2,
            b: Nested, // make sure nested doesn't interfere with elements before or after it
            c: DataTransactionHeader2,
        }

        let nested = Nested {
            a: header.clone(),
            b: inner_header,
        };

        let nested_outer = NestedOuter {
            a: header.clone(),
            b: nested,
            c: header,
        };

        // action: RLP encode/decode roundtrip
        let mut buf = Vec::new();

        nested_outer.encode(&mut buf);
        let dec = NestedOuter::decode(&mut &buf[..]).unwrap();

        // Assert: verify the nested structure
        assert_eq!(nested_outer, dec);
    }

    #[test]
    fn test_irys_transaction_header_rlp_round_trip() {
        // setup
        let config = ConsensusConfig::testing();
        let mut header = mock_header(&config);

        // action - test RLP encoding/decoding the outer versioned structure
        let mut buffer = vec![];
        header.encode(&mut buffer);
        let decoded = DataTransactionHeader::decode(&mut buffer.as_slice()).unwrap();

        // Assert
        // zero out the id and signature, those do not get encoded
        header.id = H256::zero();
        header.signature = IrysSignature::new(Signature::try_from([0_u8; 65].as_slice()).unwrap());

        assert!(header.try_as_header_v1().is_some());
        assert_eq!(header.try_as_header_v1(), decoded.try_as_header_v1());
        // Verify version discriminant is preserved in RLP encoding
        assert_eq!(decoded.version(), 1);
    }

    #[test]
    fn test_commitment_transaction_rlp_round_trip() {
        // setup
        let config = ConsensusConfig::testing();
        let mut header = mock_commitment_tx(&config);

        // test RLP encoding/decoding the outer versioned structure
        let mut buffer = vec![];
        header.encode(&mut buffer);
        let decoded = CommitmentTransaction::decode(&mut buffer.as_slice()).unwrap();

        // Assert
        // zero out the id and signature, those do not get encoded
        header.set_id(H256::zero());
        header.set_signature(IrysSignature::new(
            Signature::try_from([0_u8; 65].as_slice()).unwrap(),
        ));
        assert_eq!(header, decoded);
        // Verify version discriminant is preserved in RLP encoding
        assert_eq!(decoded.version(), 2);
    }

    #[test]
    fn test_irys_transaction_header_compact_round_trip() {
        // setup
        let config = ConsensusConfig::testing();
        let original_header = mock_header(&config);

        // action - test Compact encoding/decoding the outer versioned structure
        let mut buffer = vec![];
        original_header.to_compact(&mut buffer);
        let (decoded_header, rest) = DataTransactionHeader::from_compact(&buffer, buffer.len());

        assert!(original_header.try_as_header_v1().is_some());
        // Assert - Compact encodes ALL fields including id and signature (unlike RLP)
        assert_eq!(
            original_header.try_as_header_v1(),
            decoded_header.try_as_header_v1()
        );
        // Verify version discriminant is preserved in Compact encoding
        assert_eq!(decoded_header.version(), 1);
        assert_eq!(buffer[0], 1); // First byte should be the version discriminant
        assert!(rest.is_empty(), "the whole buffer should be consumed");
    }

    #[test]
    fn test_commitment_transaction_compact_round_trip() {
        // setup
        let config = ConsensusConfig::testing();
        let original_tx = mock_commitment_tx(&config);

        // action - test Compact encoding/decoding the outer versioned structure
        let mut buffer = vec![];
        original_tx.to_compact(&mut buffer);
        let (decoded_tx, rest) = CommitmentTransaction::from_compact(&buffer, buffer.len());

        // Assert - Compact encodes ALL fields including id and signature (unlike RLP)
        assert_eq!(original_tx, decoded_tx);
        // Verify version discriminant is preserved in Compact encoding
        assert_eq!(decoded_tx.version(), 2);
        assert_eq!(buffer[0], 2); // First byte should be the version discriminant
        assert!(rest.is_empty(), "the whole buffer should be consumed");
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
        // Deserialize the JSON back to DataTransactionHeader
        let deserialized: DataTransactionHeader =
            serde_json::from_str(&serialized).expect("Failed to deserialize");

        assert!(original_header.try_as_header_v1().is_some());
        // Ensure the deserialized struct matches the original
        assert_eq!(
            original_header.try_as_header_v1(),
            deserialized.try_as_header_v1()
        );
    }

    #[test]
    fn test_commitment_transaction_serde() {
        // Create a sample commitment tx
        let config = ConsensusConfig::testing();
        let original_tx = mock_commitment_tx(&config);

        // Serialize the commitment tx to JSON
        let serialized = serde_json::to_string_pretty(&original_tx).expect("Failed to serialize");

        println!("{}", &serialized);
        // Deserialize the JSON back to a CommitmentTransaction
        let deserialized: CommitmentTransaction =
            serde_json::from_str(&serialized).expect("Failed to deserialize");

        // Ensure the deserialized tx matches the original
        assert_eq!(original_tx, deserialized);
    }

    #[test]
    fn test_tx_encode_and_signing() {
        let config = ConsensusConfig::testing();
        let signer = IrysSigner {
            signer: SigningKey::random(&mut rand::thread_rng()),
            chain_id: config.chain_id,
            chunk_size: config.chunk_size,
        };

        // Test signing the header directly using the outer versioned type
        let header = mock_header(&config);
        let signed_header = header.sign(&signer).unwrap();
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
        let config = ConsensusConfig::testing();
        let signer = IrysSigner {
            signer: SigningKey::random(&mut rand::thread_rng()),
            chain_id: config.chain_id,
            chunk_size: config.chunk_size,
        };

        // Test signing the outer versioned type directly
        let tx = mock_commitment_tx(&config);
        let signed_tx = tx.sign(&signer).unwrap();

        println!(
            "{}",
            serde_json::to_string_pretty(&signed_tx).expect("Failed to serialize")
        );

        assert!(signed_tx.is_signature_valid());
    }

    #[test]
    fn test_data_transaction_signature_validation() {
        // setup
        let config = ConsensusConfig::testing();
        let signer = IrysSigner {
            signer: SigningKey::random(&mut rand::thread_rng()),
            chain_id: config.chain_id,
            chunk_size: config.chunk_size,
        };

        let tx = DataTransaction {
            header: mock_header(&config),
            ..Default::default()
        };

        // Sign the transaction
        let mut signed_tx = signer.sign_transaction(tx).unwrap();

        // Verify initial signature is valid
        assert!(signed_tx.header.is_signature_valid());

        // Test: changing the ID should make validation fail
        signed_tx.header.id = H256::random();
        assert!(
            !signed_tx.header.is_signature_valid(),
            "Signature validation should fail when ID is changed"
        );
    }

    #[test]
    fn test_commitment_transaction_signature_validation() {
        // setup
        let config = ConsensusConfig::testing();
        let signer = IrysSigner {
            signer: SigningKey::random(&mut rand::thread_rng()),
            chain_id: config.chain_id,
            chunk_size: config.chunk_size,
        };

        let tx = mock_commitment_tx(&config);

        // Sign the transaction
        let mut signed_tx = tx.sign(&signer).unwrap();

        // Verify initial signature is valid
        assert!(signed_tx.is_signature_valid());

        // Test: changing the ID should make validation fail
        signed_tx.set_id(H256::random());
        assert!(
            !signed_tx.is_signature_valid(),
            "Signature validation should fail when ID is changed"
        );
    }

    fn mock_header(config: &ConsensusConfig) -> DataTransactionHeader {
        DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                id: H256::from([255_u8; 32]),
                anchor: H256::from([1_u8; 32]),
                signer: IrysAddress::default(),
                data_root: H256::from([3_u8; 32]),
                data_size: 1024,
                header_size: 0,
                term_fee: BoundedFee::from(100_u64),
                perm_fee: Some(BoundedFee::from(200_u64)),
                ledger_id: DataLedger::Submit.into(),
                bundle_format: None,
                chain_id: config.chain_id,
                signature: Signature::test_signature().into(),
            },
            metadata: DataTransactionMetadata::new(),
        })
    }

    fn mock_commitment_tx(config: &ConsensusConfig) -> CommitmentTransaction {
        let mut tx = CommitmentTransaction::new_stake(config, H256::from([1_u8; 32]));
        tx.set_id(H256::from([255_u8; 32]));
        tx.set_signer(IrysAddress::default());
        tx.set_signature(Signature::test_signature().into());
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

        use crate::CommitmentTransactionV1;
        let mut config = ConsensusConfig::testing();
        config.pledge_base_value = crate::storage_pricing::Amount::token(dec!(20000.0)).unwrap();
        config.pledge_decay = crate::storage_pricing::Amount::percentage(dec!(0.9)).unwrap();

        // Create provider with existing pledge count
        let signer_address = IrysAddress::default();
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

        use crate::CommitmentTransactionV1;
        let mut config = ConsensusConfig::testing();
        config.pledge_base_value = crate::storage_pricing::Amount::token(dec!(20000.0)).unwrap();
        config.pledge_decay = crate::storage_pricing::Amount::percentage(dec!(0.9)).unwrap();

        // Create provider with existing pledge count
        let signer_address = IrysAddress::default();
        let provider =
            MockPledgeProvider::new().with_pledge_count(signer_address, existing_pledges);

        // Create an unpledge transaction
        let unpledge_tx = CommitmentTransactionV1::new_unpledge(
            &config,
            H256::zero(),
            &provider,
            signer_address,
            H256::zero(),
        )
        .await;

        // Verify the commitment type is correct
        assert!(matches!(
            unpledge_tx.commitment_type,
            CommitmentTypeV1::Unpledge { .. }
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

    #[tokio::test]
    async fn test_new_unpledge_includes_partition_hash() {
        let config = ConsensusConfig::testing();
        let signer = IrysAddress::default();
        let provider = MockPledgeProvider::new().with_pledge_count(signer, 2);
        let ph = H256::from([0xAB_u8; 32]);
        let tx =
            CommitmentTransaction::new_unpledge(&config, H256::zero(), &provider, signer, ph).await;

        match tx.commitment_type() {
            CommitmentTypeV2::Unpledge {
                partition_hash,
                pledge_count_before_executing,
            } => {
                assert_eq!(pledge_count_before_executing, 2);
                assert_eq!(partition_hash, ph);
            }
            _ => panic!("unexpected type"),
        }
    }
}

#[cfg(test)]
mod commitment_ordering_tests {
    use crate::CommitmentTransactionV1;

    use super::*;

    fn create_test_commitment(
        id: &str,
        commitment_type: CommitmentTypeV1,
        fee: u64,
    ) -> CommitmentTransactionV1 {
        CommitmentTransactionV1 {
            id: H256::from_slice(&[id.as_bytes()[0]; 32]),
            anchor: H256::zero(),
            signer: IrysAddress::default(),
            signature: IrysSignature::default(),
            fee,
            value: U256::zero(),
            commitment_type,
            chain_id: 1,
        }
    }

    fn partition_hash(tag: u8) -> H256 {
        let mut bytes = [0_u8; 32];
        bytes[31] = tag;
        bytes.into()
    }

    #[test]
    fn test_stake_comes_before_pledge() {
        let stake = create_test_commitment("stake", CommitmentTypeV1::Stake, 100);
        let pledge = create_test_commitment(
            "pledge",
            CommitmentTypeV1::Pledge {
                pledge_count_before_executing: 1,
            },
            200,
        );

        assert!(stake < pledge);
    }

    #[test]
    fn test_stake_sorted_by_fee() {
        let stake_low = create_test_commitment("stake1", CommitmentTypeV1::Stake, 50);
        let stake_high = create_test_commitment("stake2", CommitmentTypeV1::Stake, 150);

        assert!(stake_high < stake_low);
    }

    #[test]
    fn test_pledge_sorted_by_count_then_fee() {
        let pledge_count2_fee100 = create_test_commitment(
            "p1",
            CommitmentTypeV1::Pledge {
                pledge_count_before_executing: 2,
            },
            100,
        );
        let pledge_count2_fee200 = create_test_commitment(
            "p2",
            CommitmentTypeV1::Pledge {
                pledge_count_before_executing: 2,
            },
            200,
        );
        let pledge_count5_fee300 = create_test_commitment(
            "p3",
            CommitmentTypeV1::Pledge {
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
    fn test_unpledge_sorted_by_count_then_fee() {
        let unpledge_count1_fee50 = create_test_commitment(
            "unpledge_1_fee50",
            CommitmentTypeV1::Unpledge {
                pledge_count_before_executing: 1,
                partition_hash: partition_hash(1),
            },
            50,
        );
        let unpledge_count1_fee10 = create_test_commitment(
            "unpledge_1_fee10",
            CommitmentTypeV1::Unpledge {
                pledge_count_before_executing: 1,
                partition_hash: partition_hash(2),
            },
            10,
        );
        let unpledge_count4_fee80 = create_test_commitment(
            "unpledge_4_fee80",
            CommitmentTypeV1::Unpledge {
                pledge_count_before_executing: 4,
                partition_hash: partition_hash(3),
            },
            80,
        );

        assert!(unpledge_count1_fee50 > unpledge_count4_fee80);
        assert!(unpledge_count1_fee50 < unpledge_count1_fee10);
    }

    #[test]
    fn test_complete_ordering() {
        // Create commitments with distinct IDs for easier verification
        let stake_high = create_test_commitment("stake_high", CommitmentTypeV1::Stake, 150);
        let stake_low = create_test_commitment("stake_low", CommitmentTypeV1::Stake, 50);
        let pledge_2_high = create_test_commitment(
            "pledge_2_high",
            CommitmentTypeV1::Pledge {
                pledge_count_before_executing: 2,
            },
            200,
        );
        let pledge_2_low = create_test_commitment(
            "pledge_2_low",
            CommitmentTypeV1::Pledge {
                pledge_count_before_executing: 2,
            },
            50,
        );
        let pledge_5 = create_test_commitment(
            "pledge_5",
            CommitmentTypeV1::Pledge {
                pledge_count_before_executing: 5,
            },
            100,
        );
        let pledge_10 = create_test_commitment(
            "pledge_10",
            CommitmentTypeV1::Pledge {
                pledge_count_before_executing: 10,
            },
            300,
        );
        let unpledge_count1 = create_test_commitment(
            "unpledge_1",
            CommitmentTypeV1::Unpledge {
                pledge_count_before_executing: 1,
                partition_hash: partition_hash(4),
            },
            40,
        );
        let unpledge_count3 = create_test_commitment(
            "unpledge_3",
            CommitmentTypeV1::Unpledge {
                pledge_count_before_executing: 3,
                partition_hash: partition_hash(5),
            },
            20,
        );
        let unstake = create_test_commitment("unstake", CommitmentTypeV1::Unstake, 75);

        let mut commitments = vec![
            pledge_5.clone(),
            stake_low.clone(),
            pledge_2_high.clone(),
            stake_high.clone(),
            pledge_2_low.clone(),
            pledge_10.clone(),
            unstake.clone(),
            unpledge_count1.clone(),
            unpledge_count3.clone(),
        ];

        commitments.sort();

        // Verify the expected order:
        // 1. stake_high (Stake, fee=150)
        // 2. stake_low (Stake, fee=50)
        // 3. pledge_2_high (Pledge count=2, fee=200)
        // 4. pledge_2_low (Pledge count=2, fee=50)
        // 5. pledge_5 (Pledge count=5, fee=100)
        // 6. pledge_10 (Pledge count=10, fee=300)
        // 7. unpledge_count3 (Unpledge count=3, fee=20)
        // 8. unpledge_count1 (Unpledge count=1, fee=40)
        // 9. unstake (Other type, fee=75)

        assert_eq!(commitments[0].id, stake_high.id);
        assert_eq!(commitments[1].id, stake_low.id);
        assert_eq!(commitments[2].id, pledge_2_high.id);
        assert_eq!(commitments[3].id, pledge_2_low.id);
        assert_eq!(commitments[4].id, pledge_5.id);
        assert_eq!(commitments[5].id, pledge_10.id);
        assert_eq!(commitments[6].id, unpledge_count3.id);
        assert_eq!(commitments[7].id, unpledge_count1.id);
        assert_eq!(commitments[8].id, unstake.id);
    }
}
