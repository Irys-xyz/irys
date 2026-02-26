//! Contains a common set of types used across all of the `irys-chain` modules.
//!
//! This module implements a single location where these types are managed,
//! making them easy to reference and maintain.
use crate::block_production::SolutionContext;
use crate::storage_pricing::{phantoms::IrysPrice, phantoms::Usd, Amount};
use crate::versioning::{
    compact_with_discriminant, split_discriminant, Signable, VersionDiscriminant, Versioned,
    VersioningError,
};
use crate::{decode_rlp_version, encode_rlp_version, IrysTransactionCommon as _};
use crate::{
    generate_data_root, generate_leaves_from_data_roots, option_u64_stringify,
    partition::PartitionHash,
    resolve_proofs,
    serialization::{optional_string_u64, string_u64},
    time::UnixTimestampMs,
    transaction::DataTransactionHeader,
    u64_stringify, Arbitrary, Base64, Compact, Config, DataRootLeaf, H256List, IngressProofsList,
    IrysSignature, Proof, H256, U256,
};
use crate::{CommitmentTransaction, IrysAddress, SystemLedger};

use alloy_primitives::map::foldhash::HashSet;
use alloy_primitives::{keccak256, TxHash, B256};
use alloy_rlp::{Encodable, RlpDecodable, RlpEncodable};
use derive_more::Display;
use irys_macros_integer_tagged::IntegerTagged;
use openssl::sha;
use reth_db::table::{Decode, Encode};
use reth_db::DatabaseError;
use reth_primitives_traits::Header;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::ops::{Deref, DerefMut, Index, IndexMut};
use std::sync::Arc;
use tracing::debug;

pub type BlockHash = H256;

pub type BlockHeight = u64;

pub type EvmBlockHash = B256;

/// Stores the `vdf_limiter_info` in the [`IrysBlockHeaderV1`]
#[derive(
    Clone,
    Debug,
    Eq,
    Default,
    Serialize,
    Deserialize,
    PartialEq,
    Arbitrary,
    Compact,
    RlpEncodable,
    RlpDecodable,
)]
#[rlp(trailing)]
#[serde(rename_all = "camelCase")]
pub struct VDFLimiterInfo {
    /// The output of the latest step - the source of the entropy for the mining nonces.
    pub output: H256,
    /// The global sequence number of the nonce limiter step at which the block was found.
    #[serde(with = "string_u64")]
    pub global_step_number: u64,
    /// The hash of the latest block mined below the current reset line.
    pub seed: H256,
    /// The hash of the latest block mined below the future reset line.
    pub next_seed: H256,
    /// The output of the latest step of the previous block
    pub prev_output: H256,
    /// VDF_CHECKPOINT_COUNT_IN_STEP checkpoints from the most recent step in the nonce limiter process.
    pub last_step_checkpoints: H256List,
    /// A list of the output of each step of the nonce limiting process. Note: each step
    /// has VDF_CHECKPOINT_COUNT_IN_STEP checkpoints, the last of which is that step's output.
    /// This field would be more accurately named "steps" as checkpoints are between steps.
    pub steps: H256List,
    /// The number of SHA2-256 iterations in a single VDF checkpoint. The protocol aims to keep the
    /// checkpoint calculation time to around 40ms by varying this parameter. Note: there are
    /// 25 checkpoints in a single VDF step - so the protocol aims to keep the step calculation at
    /// 1 second by varying this parameter.
    #[serde(default, with = "option_u64_stringify")]
    pub vdf_difficulty: Option<u64>,
    /// The VDF difficulty scheduled for to be applied after the next VDF reset line.
    #[serde(default, with = "option_u64_stringify")]
    pub next_vdf_difficulty: Option<u64>,
}

impl VDFLimiterInfo {
    pub fn new(
        solution: &SolutionContext,
        prev_block_header: &IrysBlockHeader,
        steps: H256List,
        config: &Config,
    ) -> Self {
        let mut vdf_limiter_info = Self {
            global_step_number: solution.vdf_step,
            output: solution.seed.clone().into_inner(),
            last_step_checkpoints: solution.checkpoints.clone(),
            prev_output: prev_block_header.vdf_limiter_info.output,
            steps,
            // Next two lines are going to be overridden by `set_next_seed`.
            seed: prev_block_header.vdf_limiter_info.seed,
            next_seed: Default::default(),
            ..Self::default()
        };

        let reset_frequency = config.vdf.reset_frequency;
        vdf_limiter_info.set_seeds(reset_frequency as u64, prev_block_header);

        vdf_limiter_info
    }

    /// Returns the global step number for the first step in the block.
    pub fn first_step_number(&self) -> u64 {
        // It is + 1 because there's always at least one step. I.e., in case if there's only
        // one step, the first step and the last step are the same, in case if there are two
        // steps, the first step is the last step - 1, and so on.
        self.global_step_number - self.steps.len() as u64 + 1
    }

    /// Returns the reset step if the block contains one
    pub fn reset_step(&self, reset_frequency: u64) -> Option<u64> {
        let first_step = self.first_step_number();
        (first_step..=self.global_step_number)
            .find(|step_number| step_number % reset_frequency == 0)
    }

    pub fn set_seeds(&mut self, reset_frequency: u64, parent_header: &IrysBlockHeader) {
        let (next_seed, seed) = self.calculate_seeds(reset_frequency, parent_header);
        debug!(
            "Setting VDF seeds: next_seed: {}, seed: {}",
            next_seed, seed
        );
        self.next_seed = next_seed;
        self.seed = seed;
    }

    /// Returns a pair of expected seeds for the VDF limiter. The first value is the `next_seed`,
    /// and the second value is the `seed`.
    pub fn calculate_seeds(
        &self,
        reset_frequency: u64,
        parent_header: &IrysBlockHeader,
    ) -> (H256, H256) {
        if let Some(step) = self.reset_step(reset_frequency) {
            debug!(
                "VDFInfo contains a reset step {}, switching the seeds",
                step
            );
            (
                parent_header.block_hash,
                parent_header.vdf_limiter_info.next_seed,
            )
        } else {
            debug!(
                "Using previous VDF seeds. First step: {}, last step: {}, reset_frequency: {}",
                self.first_step_number(),
                self.global_step_number,
                reset_frequency
            );
            (
                parent_header.vdf_limiter_info.next_seed,
                parent_header.vdf_limiter_info.seed,
            )
        }
    }
}

#[derive(Clone, Debug, Eq, IntegerTagged, PartialEq, Arbitrary, Display)]
#[integer_tagged(tag = "version")]
pub enum IrysBlockHeader {
    #[integer_tagged(version = 1)]
    V1(IrysBlockHeaderV1),
}

impl Default for IrysBlockHeader {
    fn default() -> Self {
        Self::V1(IrysBlockHeaderV1::default())
    }
}

impl VersionDiscriminant for IrysBlockHeader {
    fn version(&self) -> u8 {
        match self {
            Self::V1(_) => 1,
        }
    }
}

impl Deref for IrysBlockHeader {
    type Target = IrysBlockHeaderV1;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::V1(v1) => v1,
        }
    }
}

impl DerefMut for IrysBlockHeader {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            Self::V1(v1) => v1,
        }
    }
}

impl Compact for IrysBlockHeader {
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
                let (inner, rest2) = IrysBlockHeaderV1::from_compact(rest, rest.len());
                (Self::V1(inner), rest2)
            }
            other => panic!("{:?}", VersioningError::UnsupportedVersion(other)),
        }
    }
}

impl Signable for IrysBlockHeader {
    fn encode_for_signing(&self, out: &mut dyn bytes::BufMut) {
        self.encode(out);
    }
}

impl alloy_rlp::Encodable for IrysBlockHeader {
    fn encode(&self, out: &mut dyn bytes::BufMut) {
        let mut buf = Vec::new();
        match self {
            Self::V1(inner) => inner.encode(&mut buf),
        }
        encode_rlp_version(buf, self.version(), out);
    }
}

impl alloy_rlp::Decodable for IrysBlockHeader {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let (version, buf) = decode_rlp_version(buf)?;
        let buf = &mut &buf[..];

        match version {
            1 => {
                let inner = IrysBlockHeaderV1::decode(buf)?;
                Ok(Self::V1(inner))
            }
            _ => Err(alloy_rlp::Error::Custom("Unsupported version")),
        }
    }
}

impl IrysBlockHeader {
    /// Create a new mock header wrapped in the versioned wrapper
    pub fn new_mock_header() -> Self {
        Self::V1(IrysBlockHeaderV1::new_mock_header())
    }

    /// Get the block hash (convenience method for `Arc<IrysBlockHeader>`)
    pub fn block_hash(&self) -> BlockHash {
        self.block_hash
    }

    /// Get the block height (convenience method for `Arc<IrysBlockHeader>`)
    pub fn height(&self) -> u64 {
        self.height
    }

    /// Validate the block signature
    pub fn is_signature_valid(&self) -> bool {
        match self {
            Self::V1(inner) => {
                // First, verify the signature is valid for the data
                let signature_valid = inner
                    .signature
                    .validate_signature(self.signature_hash(), inner.miner_address);

                // Second, verify the block_hash matches the hash of the signature
                let expected_block_hash = H256::from(keccak256(inner.signature.as_bytes()).0);
                let block_hash_valid = inner.block_hash == expected_block_hash;

                signature_valid && block_hash_valid
            }
        }
    }

    /// Get the chunk hash (convenience method)
    pub fn chunk_hash(&self) -> H256 {
        self.chunk_hash
    }

    /// Get the timestamp (convenience method)
    pub fn timestamp(&self) -> UnixTimestampMs {
        self.timestamp
    }

    /// Get the timestamp as seconds (convenience for hardfork comparisons)
    pub fn timestamp_secs(&self) -> crate::UnixTimestamp {
        self.timestamp.to_secs()
    }
}

impl Versioned for IrysBlockHeaderV1 {
    const VERSION: u8 = 1;
}
// impl HasInnerVersion for IrysBlockHeaderV1 {
//     fn inner_version(&self) -> u8 {
//         self.version
//     }
// }

/// Stores deserialized fields from a JSON formatted Irys block header.
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
#[serde(rename_all = "camelCase")]
pub struct IrysBlockHeaderV1 {
    /// The block identifier.
    /// Excluded from RLP encoding as it's derived from the signature hash.
    #[rlp(skip)]
    #[rlp(default)]
    pub block_hash: BlockHash,

    /// The block signature.
    /// Excluded from RLP encoding as signatures are computed over the RLP-encoded data.
    #[rlp(skip)]
    #[rlp(default)]
    pub signature: IrysSignature,

    /// The block height.
    #[serde(with = "string_u64")]
    pub height: u64,

    /// Difficulty threshold used to produce the current block.
    pub diff: U256,

    /// The sum of the average number of hashes computed by the network to
    /// produce the past blocks including this one.
    pub cumulative_diff: U256,

    /// The solution hash for the block hash(chunk_bytes + partition_chunk_offset + mining_seed)
    pub solution_hash: H256,

    /// timestamp (in milliseconds) since UNIX_EPOCH of the last difficulty adjustment
    pub last_diff_timestamp: UnixTimestampMs,

    /// The solution hash of the previous block in the chain.
    pub previous_solution_hash: H256,

    /// The block hash of the last epoch block
    pub last_epoch_hash: H256,

    /// `SHA-256` hash of the PoA chunk (unencoded) bytes.
    pub chunk_hash: H256,

    // Previous block identifier.
    pub previous_block_hash: H256,

    /// The cumulative difficulty of the previous block
    pub previous_cumulative_diff: U256,

    /// The recall chunk proof
    pub poa: PoaData,

    /// The address that the block reward should be sent to
    // #[serde(with = "address_base58_stringify")]
    pub reward_address: IrysAddress,

    /// The amount of Irys tokens that must be rewarded to the `self.reward_address`
    pub reward_amount: U256,

    /// The address of the block producer - used to validate the block hash/signature & the PoA chunk (as the packing key)
    /// We allow for miners to send rewards to a separate address
    // #[serde(with = "address_base58_stringify")]
    pub miner_address: IrysAddress,

    /// timestamp (in milliseconds) since UNIX_EPOCH of when the block was discovered/produced
    pub timestamp: UnixTimestampMs,

    /// A list of system transaction ledgers
    pub system_ledgers: Vec<SystemTransactionLedger>,

    /// A list of storage transaction ledgers, one for each active data ledger
    /// Maintains the block->tx_root->data_root relationship for each block
    /// and ledger.
    pub data_ledgers: Vec<DataTransactionLedger>,

    /// Evm block hash (32 bytes)
    pub evm_block_hash: B256,

    /// Metadata about the verifiable delay function, used for block verification purposes
    pub vdf_limiter_info: VDFLimiterInfo,

    /// $IRYS token price expressed in $USD, returned from the oracle
    pub oracle_irys_price: IrysTokenPrice,

    /// $IRYS token price expressed in $USD, updated only on EMA recalculation blocks.
    /// This is what the protocol uses for different pricing calculation purposes.
    pub ema_irys_price: IrysTokenPrice,

    /// Treasury balance tracking
    pub treasury: U256,
}

pub type IrysTokenPrice = Amount<(IrysPrice, Usd)>;

impl IrysBlockHeaderV1 {
    /// Returns true if the block is the genesis block, false otherwise
    pub fn is_genesis(&self) -> bool {
        self.height == 0
    }

    /// Proxy method for `Encodable::encode`
    ///
    /// Packs all the header data into a byte buffer, using RLP encoding.
    pub fn digest_for_signing(&self, buf: &mut dyn alloy_rlp::BufMut) {
        // Using trait directly because `reth_db_api` also has an `encode` method.
        Encodable::encode(&self, buf);
    }

    // treat any block whose height is a multiple of blocks_in_price_adjustment_interval
    pub fn is_ema_recalculation_block(&self, blocks_in_price_adjustment_interval: u64) -> bool {
        is_ema_recalculation_block(self.height, blocks_in_price_adjustment_interval)
    }

    /// Returns the height of the "previous" EMA recalculation block.
    ///
    /// - For the first two intervals (`height < 2 * blocks_in_price_adjustment_interval`), always return 0.
    /// - Otherwise, return the largest multiple of `blocks_in_price_adjustment_interval` less than `height`.
    ///   (If the current block is exactly on an interval boundary, step one interval back.)
    pub fn previous_ema_recalculation_block_height(
        &self,
        blocks_in_price_adjustment_interval: u64,
    ) -> u64 {
        previous_ema_recalculation_block_height(self.height, blocks_in_price_adjustment_interval)
    }

    pub fn block_height_to_use_for_price(&self, blocks_in_price_adjustment_interval: u64) -> u64 {
        block_height_to_use_for_price(self.height, blocks_in_price_adjustment_interval)
    }

    /// Returns a borrowed slice of commitment ledger transaction IDs.
    pub fn commitment_tx_ids(&self) -> &[H256] {
        self.system_ledgers
            .iter()
            .find(|l| l.ledger_id == SystemLedger::Commitment as u32)
            .map(|l| l.tx_ids.0.as_slice())
            .unwrap_or_default()
    }

    /// Retrieves a map of the data transaction ids by ledger type, uses a Vec
    /// to ensure the txids are ordered identically to their order in the block.
    pub fn get_data_ledger_tx_ids(&self) -> HashMap<DataLedger, Vec<H256>> {
        let mut data_txids: HashMap<DataLedger, Vec<H256>> = HashMap::new();
        for data_ledger in self.data_ledgers.iter() {
            data_txids.insert(
                DataLedger::from_u32(data_ledger.ledger_id).unwrap(),
                data_ledger.tx_ids.0.clone().into_iter().collect(),
            );
        }
        data_txids
    }

    /// Returns the transaction IDs for a specific data ledger in their original order
    pub fn get_data_ledger_tx_ids_ordered(&self, ledger_id: DataLedger) -> Option<&[H256]> {
        self.data_ledgers
            .iter()
            .find(|l| l.ledger_id == ledger_id as u32)
            .map(|l| l.tx_ids.0.as_slice())
    }
}

// treat any block whose height is a multiple of blocks_in_price_adjustment_interval
pub fn is_ema_recalculation_block(height: u64, blocks_in_price_adjustment_interval: u64) -> bool {
    // the first 2 adjustment intervals have special handling where we calculate the
    // EMA for each block using the value from the preceding one
    if height < (blocks_in_price_adjustment_interval * 2) {
        true
    } else {
        (height.saturating_add(1)).is_multiple_of(blocks_in_price_adjustment_interval)
    }
}

pub fn block_height_to_use_for_price(height: u64, blocks_in_price_adjustment_interval: u64) -> u64 {
    // we need to use the genesis price
    if height < (blocks_in_price_adjustment_interval * 2) {
        0
    } else {
        // we need to use the price from 2 intervals ago
        let prev_ema_height =
            prev_ema_ignore_genesis_rules(height, blocks_in_price_adjustment_interval);

        // the preceding ema
        prev_ema_ignore_genesis_rules(prev_ema_height, blocks_in_price_adjustment_interval)
    }
}

/// Returns the height of the "previous" EMA recalculation block.
pub fn previous_ema_recalculation_block_height(
    height: u64,
    blocks_in_price_adjustment_interval: u64,
) -> u64 {
    // the first 2 adjustment intervals have special handling where we calculate the
    // EMA for each block using the value from the preceding one
    if height < (blocks_in_price_adjustment_interval * 2) {
        return height.saturating_sub(1);
    }

    // After the first 2 adjustment intervals we start calculating the EMA
    // only using the last EMA block
    prev_ema_ignore_genesis_rules(height, blocks_in_price_adjustment_interval)
}

fn prev_ema_ignore_genesis_rules(height: u64, blocks_in_price_adjustment_interval: u64) -> u64 {
    // heights are zero indexed hence adding +1
    let remainder = (height + 1) % blocks_in_price_adjustment_interval;
    if remainder == 0 {
        // If the current block is on an interval boundary, go one interval back.
        height.saturating_sub(blocks_in_price_adjustment_interval)
    } else {
        // Otherwise, drop the remainder.
        height.saturating_sub(remainder)
    }
}

#[derive(
    Default,
    Clone,
    Debug,
    Eq,
    PartialEq,
    Serialize,
    Deserialize,
    Compact,
    Arbitrary,
    RlpDecodable,
    RlpEncodable,
)]
#[rlp(trailing)]
#[serde(rename_all = "camelCase")]
/// Stores deserialized fields from a `poa` (Proof of Access) JSON
pub struct PoaData {
    pub partition_chunk_offset: u32,
    pub partition_hash: PartitionHash,
    pub chunk: Option<Base64>,
    pub ledger_id: Option<u32>,
    pub tx_path: Option<Base64>,
    pub data_path: Option<Base64>,
}

pub type TxRoot = H256;

#[derive(
    Default,
    Clone,
    Debug,
    Eq,
    PartialEq,
    Serialize,
    Deserialize,
    Compact,
    Arbitrary,
    RlpDecodable,
    RlpEncodable,
)]
#[rlp(trailing)]
#[serde(rename_all = "camelCase")]
pub struct DataTransactionLedger {
    /// Unique identifier for this ledger, maps to discriminant in `Ledger` enum
    pub ledger_id: u32,
    /// Root of the merkle tree built from the ledger transaction data_roots
    pub tx_root: H256,
    /// List of transaction ids included in the block
    pub tx_ids: H256List,
    /// The total number of chunks in this ledger since genesis
    #[serde(default, with = "u64_stringify")]
    pub total_chunks: u64,
    /// This ledger expires after how many epochs
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_string_u64"
    )]
    pub expires: Option<u64>,
    /// When transactions are promoted they must include their ingress proofs
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proofs: Option<IngressProofsList>,
    /// Ingress proofs required per transaction for promotion. Stored per-ledger to enable
    /// different replication levels per ledger and simplify validation / config mappings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_proof_count: Option<u8>,
}

impl DataTransactionLedger {
    /// Computes the tx_root and tx_paths. The TX Root is composed of taking the data_roots of each of the storage
    /// transactions included, in order, and building a merkle tree out of them. The root of this tree is the tx_root.
    pub fn merklize_tx_root(data_txs: &[DataTransactionHeader]) -> (H256, Vec<Proof>) {
        if data_txs.is_empty() {
            return (H256::zero(), vec![]);
        }
        let txs_data_roots = data_txs
            .iter()
            .map(|h| DataRootLeaf {
                data_root: h.data_root,
                tx_size: h.data_size as usize, // TODO: check this
            })
            .collect::<Vec<DataRootLeaf>>();
        let data_root_leaves = generate_leaves_from_data_roots(&txs_data_roots).unwrap();
        let root = generate_data_root(data_root_leaves).unwrap();
        let root_id = root.id;
        let proofs = resolve_proofs(root, None).unwrap();
        (H256(root_id), proofs)
    }
}

impl Index<DataLedger> for Vec<DataTransactionLedger> {
    type Output = DataTransactionLedger;

    fn index(&self, ledger: DataLedger) -> &Self::Output {
        self.iter()
            .find(|tx_ledger| tx_ledger.ledger_id == ledger as u32)
            .expect("No transaction ledger found for given ledger type")
    }
}

impl IndexMut<DataLedger> for Vec<DataTransactionLedger> {
    fn index_mut(&mut self, ledger: DataLedger) -> &mut Self::Output {
        self.iter_mut()
            .find(|tx_ledger| tx_ledger.ledger_id == ledger as u32)
            .expect("No transaction ledger found for given ledger type")
    }
}

/// Retrieves the [`IngressProofsList`] for the given txid.
///
/// Checks for the correct number of proofs based on the `required_proof_count` field of the [`DataTransactionLedger`]
pub fn get_ingress_proofs(
    data_ledger: &DataTransactionLedger,
    txid: &H256,
) -> eyre::Result<IngressProofsList> {
    let index = data_ledger
        .tx_ids
        .iter()
        .position(|id| id == txid)
        .ok_or_else(|| eyre::eyre!("Transaction {} not found in ledger", txid))?;

    let proof_count = data_ledger
        .required_proof_count
        .ok_or_else(|| eyre::eyre!("Required proof count not set for ledger"))?
        as usize;

    let proofs_list = data_ledger
        .proofs
        .as_ref()
        .ok_or_else(|| eyre::eyre!("No proofs available in ledger"))?;

    let start_index = index * proof_count;
    let end_index = start_index + proof_count;

    // Check bounds
    if end_index > proofs_list.len() {
        eyre::bail!("Not enough proofs available for transaction");
    }

    let proofs = IngressProofsList((proofs_list[start_index..end_index]).to_vec());
    Ok(proofs)
}

#[derive(
    Default,
    Clone,
    Debug,
    Eq,
    PartialEq,
    Serialize,
    Deserialize,
    Compact,
    Arbitrary,
    RlpDecodable,
    RlpEncodable,
)]
#[rlp(trailing)]
#[serde(rename_all = "camelCase")]
pub struct SystemTransactionLedger {
    /// Unique identifier for this ledger, maps to discriminant in `SystemLedger` enum
    pub ledger_id: u32,
    /// List of system transaction ids added to the system ledger in this block
    pub tx_ids: H256List,
}

impl fmt::Display for IrysBlockHeaderV1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Convert the struct to a JSON string using serde_json
        match serde_json::to_string_pretty(self) {
            Ok(json) => write!(f, "{json}"), // Write the JSON string to the formatter
            Err(_) => write!(f, "Failed to serialize IrysBlockHeader"), // Handle serialization errors
        }
    }
}

/// Compute the solution hash as sha256(poa_chunk || offset_le || seed)
///
/// This is the canonical utility used across the codebase to avoid duplicate
/// implementations of the hashing logic.
///
/// # Examples
/// ```
/// use irys_types::{compute_solution_hash, H256};
///
/// let poa_chunk = b"example chunk data";
/// let offset = 42u32;
/// let seed = H256::from([7u8; 32]);
///
/// // This should compile and produce a 32-byte hash.
/// let _hash = compute_solution_hash(poa_chunk, offset, &seed);
/// ```
pub fn compute_solution_hash(poa_chunk: &[u8], offset_le: u32, seed: &H256) -> H256 {
    let mut hasher = sha::Sha256::new();
    hasher.update(poa_chunk);
    hasher.update(&offset_le.to_le_bytes());
    hasher.update(seed.as_bytes());
    H256::from(hasher.finish())
}

impl IrysBlockHeaderV1 {
    pub fn new_mock_header() -> Self {
        use std::str::FromStr as _;
        use std::time::{SystemTime, UNIX_EPOCH};

        let tx_ids = H256List::new();
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();

        // Compute default PoA chunk, chunk_hash, and solution_hash so mock headers satisfy solution_hash_link_is_valid
        let default_poa_chunk: Vec<u8> = Vec::new();
        let default_partition_chunk_offset: u32 = 0;
        let default_vdf_output = H256::zero();
        let default_chunk_hash: H256 = H256(openssl::sha::sha256(&default_poa_chunk));
        let default_solution_hash = compute_solution_hash(
            &default_poa_chunk,
            default_partition_chunk_offset,
            &default_vdf_output,
        );

        // Create a sample IrysBlockHeader object with mock data
        Self {
            diff: U256::from(1000),
            cumulative_diff: U256::from(5000),
            last_diff_timestamp: UnixTimestampMs::from_millis(1622543200),
            solution_hash: default_solution_hash,
            previous_solution_hash: H256::zero(),
            last_epoch_hash: H256::random(),
            chunk_hash: default_chunk_hash,
            height: 42,
            block_hash: H256::zero(),
            previous_block_hash: H256::zero(),
            previous_cumulative_diff: U256::from(4000),
            poa: PoaData {
                tx_path: None,
                data_path: None,
                chunk: Some(Base64::from_str("").unwrap()),
                partition_hash: PartitionHash::zero(),
                partition_chunk_offset: 0,

                ledger_id: None,
            },
            reward_address: IrysAddress::ZERO,
            signature: IrysSignature::new(alloy_signer::Signature::test_signature()),
            timestamp: UnixTimestampMs::from_millis(now.as_millis()),
            system_ledgers: vec![], // Many tests will fail if you add fake txids to this ledger
            data_ledgers: vec![
                // Permanent Publish Ledger
                DataTransactionLedger {
                    ledger_id: DataLedger::Publish.into(),
                    tx_root: H256::zero(),
                    tx_ids,
                    total_chunks: 0,
                    expires: None,
                    proofs: None,
                    required_proof_count: Some(1),
                },
                // Term Submit Ledger
                DataTransactionLedger {
                    ledger_id: DataLedger::Submit.into(),
                    tx_root: H256::zero(),
                    tx_ids: H256List::new(),
                    total_chunks: 0,
                    expires: Some(5),
                    proofs: None,
                    required_proof_count: None,
                },
            ],
            evm_block_hash: B256::ZERO,
            miner_address: IrysAddress::ZERO,
            oracle_irys_price: Amount::token(dec!(1.0))
                .expect("dec!(1.0) must evaluate to a valid token amount"),
            ema_irys_price: Amount::token(dec!(1.0))
                .expect("dec!(1.0) must evaluate to a valid token amount"),
            treasury: U256::zero(),
            ..Default::default()
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ExecutionHeader {
    #[serde(flatten)]
    pub header: Header,
    pub transactions: Vec<TxHash>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CombinedBlockHeader {
    #[serde(flatten)]
    pub irys: IrysBlockHeader,
    pub execution: ExecutionHeader,
}

/// Names for each of the data ledgers as well as their `ledger_id` discriminant
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    Compact,
    PartialOrd,
    Ord,
    Hash,
    Arbitrary,
)]
#[repr(u32)]
#[derive(Default)]
pub enum DataLedger {
    /// The permanent publish ledger
    #[default]
    Publish = 0,
    /// An expiring term ledger used for submitting to the publish ledger
    Submit = 1,
    // Add more term ledgers as they exist
    OneYear = 10,
    ThirtyDay = 20,
}

impl PartialEq<u32> for DataLedger {
    fn eq(&self, other: &u32) -> bool {
        self.get_id() == *other
    }
}

impl PartialEq<DataLedger> for u32 {
    fn eq(&self, other: &DataLedger) -> bool {
        *self == other.get_id()
    }
}

impl Decode for DataLedger {
    fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
        if value.len() != 4 {
            return Err(DatabaseError::Decode);
        }

        // Decode bytes to u32 (big-endian for consistent database key ordering)
        let id = u32::from_be_bytes(value.try_into().map_err(|_| DatabaseError::Decode)?);

        // Convert u32 to DataLedger
        Self::from_u32(id).ok_or(DatabaseError::Decode)
    }
}

impl Encode for DataLedger {
    type Encoded = [u8; 4]; // u32 is 4 bytes

    fn encode(self) -> Self::Encoded {
        self.get_id().to_be_bytes()
    }
}

impl DataLedger {
    /// An array of all the Ledger numbers in order
    pub const ALL: [Self; 4] = [Self::Publish, Self::Submit, Self::OneYear, Self::ThirtyDay];

    /// Make it possible to iterate over all the data ledgers in order
    pub fn iter() -> impl Iterator<Item = Self> {
        Self::ALL.iter().copied()
    }
    /// get the associated numeric ID
    pub const fn get_id(&self) -> u32 {
        *self as u32
    }

    fn from_u32(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Publish),
            1 => Some(Self::Submit),
            10 => Some(Self::OneYear),
            20 => Some(Self::ThirtyDay),
            _ => None,
        }
    }
}

impl From<DataLedger> for u32 {
    fn from(ledger: DataLedger) -> Self {
        ledger as Self
    }
}

impl TryFrom<DataLedger> for usize {
    type Error = eyre::Report;

    fn try_from(value: DataLedger) -> Result<Self, Self::Error> {
        match value {
            DataLedger::Publish => Ok(0),
            DataLedger::Submit => Ok(1),
            DataLedger::OneYear => Ok(10),
            DataLedger::ThirtyDay => Ok(20),
        }
    }
}

impl TryFrom<u32> for DataLedger {
    type Error = eyre::Report;

    fn try_from(value: u32) -> eyre::Result<Self> {
        Self::from_u32(value).ok_or_else(|| eyre::eyre!("Invalid ledger number"))
    }
}

impl TryFrom<&str> for DataLedger {
    type Error = eyre::Report;

    fn try_from(value: &str) -> eyre::Result<Self> {
        let x = value.parse()?;
        Self::from_u32(x).ok_or_else(|| eyre::eyre!("Invalid ledger number"))
    }
}

impl std::fmt::Display for DataLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Publish => write!(f, "publish"),
            Self::Submit => write!(f, "submit"),
            Self::OneYear => write!(f, "one_year"),
            Self::ThirtyDay => write!(f, "one_month"),
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BlockIndexQuery {
    pub height: usize,
    pub limit: usize,
}

/// Core metadata of the BlockIndex this struct tracks the ledger size and
/// tx root for each ledger per block. Enabling lookups to that find the `tx_root`
/// for a ledger at a particular byte offset in the ledger.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockIndexItem {
    /// The hash of the block
    pub block_hash: H256, // 32 bytes
    /// The number of ledgers this block tracks
    pub num_ledgers: u8, // 1 byte
    /// The metadata about each of the blocks ledgers
    pub ledgers: Vec<LedgerIndexItem>, // Vec of 40 byte items
}

/// A [`BlockIndexItem`] contains a vec of [`LedgerIndexItem`]s which store the size
/// and and the `tx_root` of the ledger in that block.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize, Arbitrary, Compact)]
pub struct LedgerIndexItem {
    /// The total number of chunks in this ledger since genesis
    #[serde(with = "string_u64")]
    pub total_chunks: u64, // 8 bytes
    /// The merkle root of the TX that apply to this ledger in the current block
    pub tx_root: H256, // 32 bytes
    pub ledger: DataLedger, // SubKey
}

// Used exclusively for reading to and from the block_index file, can be removed
// after all nodes have mdbx based block_index's
impl LedgerIndexItem {
    fn to_bytes(&self) -> [u8; 40] {
        // Fixed size of 40 bytes
        let mut bytes = [0_u8; 40];
        bytes[0..8].copy_from_slice(&self.total_chunks.to_le_bytes()); // First 8 bytes
        bytes[8..40].copy_from_slice(self.tx_root.as_bytes()); // Next 32 bytes
        bytes
    }

    fn from_bytes(bytes: &[u8]) -> Self {
        let mut item = Self::default();

        // Read ledger size (first 8 bytes)
        let mut size_bytes = [0_u8; 8];
        size_bytes.copy_from_slice(&bytes[0..8]);
        item.total_chunks = u64::from_le_bytes(size_bytes);

        // Read tx root (next 32 bytes)
        item.tx_root = H256::from_slice(&bytes[8..40]);

        item
    }
}

impl Index<DataLedger> for Vec<LedgerIndexItem> {
    type Output = LedgerIndexItem;

    fn index(&self, ledger: DataLedger) -> &Self::Output {
        self.iter()
            .find(|item| item.ledger == ledger)
            .expect("No ledger index item found for given ledger type")
    }
}

impl IndexMut<DataLedger> for Vec<LedgerIndexItem> {
    fn index_mut(&mut self, ledger: DataLedger) -> &mut Self::Output {
        self.iter_mut()
            .find(|item| item.ledger == ledger)
            .expect("No ledger index item found for given ledger type")
    }
}

impl BlockIndexItem {
    // Serialize the BlockIndexItem to bytes
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(33 + self.ledgers.len() * 40);

        // Write fixed fields
        bytes.extend_from_slice(self.block_hash.as_bytes()); // 32 bytes
        bytes.push(self.num_ledgers); // 1 byte

        // Write each ledger item
        for ledger_index_item in &self.ledgers {
            bytes.extend_from_slice(&ledger_index_item.to_bytes()); // 40 bytes each
        }

        bytes
    }

    // Deserialize bytes to BlockIndexItem
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let mut item = Self {
            block_hash: H256::from_slice(&bytes[0..32]),
            num_ledgers: bytes[32],
            ..Default::default()
        };

        // Read ledger items
        let num_ledgers = item.num_ledgers as usize;
        item.ledgers = Vec::with_capacity(num_ledgers);

        for i in 0..num_ledgers {
            let start = 33 + (i * 40);
            let ledger_bytes = &bytes[start..start + 40];
            item.ledgers.push(LedgerIndexItem::from_bytes(ledger_bytes));
        }

        item
    }
}

/// Transactions for a block, organized by ledger type.
/// Used to pass pre-fetched transactions to block discovery.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BlockTransactions {
    /// System transactions organized by ledger type (commitments, etc.)
    pub system_txs: HashMap<SystemLedger, Vec<CommitmentTransaction>>,
    /// Data transactions organized by ledger type
    pub data_txs: HashMap<DataLedger, Vec<DataTransactionHeader>>,
}

impl BlockTransactions {
    /// Get transactions for a specific data ledger
    pub fn get_ledger_txs(&self, ledger: DataLedger) -> &[DataTransactionHeader] {
        self.data_txs
            .get(&ledger)
            .map(std::vec::Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Iterate over all data transactions across all ledgers
    pub fn all_data_txs(&self) -> impl Iterator<Item = &DataTransactionHeader> {
        self.data_txs.values().flatten()
    }

    /// Get transactions for a specific system ledger
    pub fn get_ledger_system_txs(&self, ledger: SystemLedger) -> &[CommitmentTransaction] {
        self.system_txs
            .get(&ledger)
            .map(std::vec::Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Iterate over all system transactions across all system ledgers
    pub fn all_system_txs(&self) -> impl Iterator<Item = &CommitmentTransaction> {
        self.system_txs.values().flatten()
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct BlockBody {
    pub block_hash: BlockHash,
    pub data_transactions: Vec<DataTransactionHeader>,
    pub commitment_transactions: Vec<CommitmentTransaction>,
}

impl BlockBody {
    /// Verify all transaction signatures and ids
    pub fn verify_tx_signatures(&self) -> bool {
        for tx in &self.data_transactions {
            if !tx.is_signature_valid() {
                return false;
            }
        }

        for tx in &self.commitment_transactions {
            if !tx.is_signature_valid() {
                return false;
            }
        }

        true
    }
    pub fn tx_ids_match_the_header(&self, header: &IrysBlockHeader) -> eyre::Result<bool> {
        let res = self.verify_tx_signatures();

        if !res {
            return Ok(false);
        }

        let expected_commitment_tx_ids: HashSet<H256> =
            header.commitment_tx_ids().iter().copied().collect();
        let expected_data_tx_ids: HashSet<H256> = header
            .data_ledgers
            .iter()
            .flat_map(|ledger| ledger.tx_ids.0.iter().copied())
            .collect();

        let actual_commitment_tx_ids: HashSet<H256> = self
            .commitment_transactions
            .iter()
            .map(super::transaction::IrysTransactionCommon::id)
            .collect();
        let actual_data_tx_ids: HashSet<H256> = self
            .data_transactions
            .iter()
            .map(super::transaction::IrysTransactionCommon::id)
            .collect();

        Ok(expected_commitment_tx_ids == actual_commitment_tx_ids
            && expected_data_tx_ids == actual_data_tx_ids)
    }
}

#[derive(Debug)]
pub struct SealedBlock {
    header: Arc<IrysBlockHeader>,
    transactions: Arc<BlockTransactions>,
}

impl SealedBlock {
    pub fn new(header: impl Into<Arc<IrysBlockHeader>>, body: BlockBody) -> eyre::Result<Self> {
        let header: Arc<IrysBlockHeader> = header.into();
        eyre::ensure!(
            header.is_signature_valid(),
            "Invalid block signature for block hash {:?}",
            header.block_hash
        );
        eyre::ensure!(
            header.block_hash == body.block_hash,
            "Header block hash does not match body block hash. Header: {:?}, Body: {:?}",
            header.block_hash,
            body.block_hash
        );
        eyre::ensure!(
            body.verify_tx_signatures(),
            "Invalid transaction signatures for block {:?}",
            header.block_hash
        );

        // Order and validate transactions against header specification
        let transactions = Self::order_transactions(
            &header,
            body.data_transactions,
            body.commitment_transactions,
        )?;

        Ok(Self {
            header,
            transactions: Arc::new(transactions),
        })
    }

    /// Test-only constructor that skips signature and hash validation.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn new_unchecked(header: Arc<IrysBlockHeader>, transactions: BlockTransactions) -> Self {
        Self {
            header,
            transactions: Arc::new(transactions),
        }
    }

    pub fn header(&self) -> &Arc<IrysBlockHeader> {
        &self.header
    }

    pub fn transactions(&self) -> &Arc<BlockTransactions> {
        &self.transactions
    }

    /// Reconstruct a [`BlockBody`] from the sealed block's header and transactions.
    /// Used for P2P serving when peers request the block body.
    pub fn to_block_body(&self) -> BlockBody {
        BlockBody {
            block_hash: self.header.block_hash,
            data_transactions: self.transactions.all_data_txs().cloned().collect(),
            commitment_transactions: self.transactions.all_system_txs().cloned().collect(),
        }
    }

    /// Order pre-fetched transactions into BlockTransactions structure.
    ///
    /// Transactions are returned in the exact order specified in the block header,
    /// which is critical for commitment transaction validation (e.g., stake must come before pledge).
    fn order_transactions(
        block_header: &IrysBlockHeader,
        data_txs: Vec<DataTransactionHeader>,
        commitment_txs: Vec<CommitmentTransaction>,
    ) -> eyre::Result<BlockTransactions> {
        // Single lookup map for all body data transactions
        let mut data_tx_map: HashMap<H256, DataTransactionHeader> =
            HashMap::with_capacity(data_txs.len());
        for tx in data_txs {
            data_tx_map.insert(tx.id, tx);
        }

        // Build per-ledger ordered lists by iterating header IDs directly (no clones).
        // A tx can appear in multiple ledgers (e.g. published after submission), so:
        //  - first ledger to claim a tx moves it out of the map (no clone)
        //  - subsequent ledgers find it in already-built results and clone
        let mut result_data_txs: HashMap<DataLedger, Vec<DataTransactionHeader>> = HashMap::new();
        for ledger in &block_header.data_ledgers {
            let ledger_type = DataLedger::try_from(ledger.ledger_id).map_err(|_| {
                eyre::eyre!(
                    "Invalid ledger_id {} in block {:?}",
                    ledger.ledger_id,
                    block_header.block_hash
                )
            })?;

            let mut ledger_txs = Vec::with_capacity(ledger.tx_ids.0.len());
            for expected_id in &ledger.tx_ids.0 {
                if let Some(tx) = data_tx_map.remove(expected_id) {
                    ledger_txs.push(tx);
                } else {
                    // TX was already claimed by another ledger (cross-ledger tx) — clone it
                    let tx = result_data_txs
                        .values()
                        .flat_map(|txs| txs.iter())
                        .find(|tx| tx.id == *expected_id)
                        .cloned()
                        .ok_or_else(|| {
                            eyre::eyre!(
                                "Header/body mismatch in block {:?}: {:?} ledger missing tx {}",
                                block_header.block_hash,
                                ledger_type,
                                expected_id,
                            )
                        })?;
                    ledger_txs.push(tx);
                }
            }
            result_data_txs.insert(ledger_type, ledger_txs);
        }

        if !data_tx_map.is_empty() {
            let extra_ids: Vec<_> = data_tx_map.keys().collect();
            return Err(eyre::eyre!(
                "Header/body mismatch in block {:?}: body contains {} extra data transaction(s) \
                 not referenced by header: {:?}",
                block_header.block_hash,
                extra_ids.len(),
                extra_ids,
            ));
        }

        // System transactions — each appears in exactly one ledger, so simple remove suffices
        let mut commitment_tx_map: HashMap<H256, CommitmentTransaction> =
            HashMap::with_capacity(commitment_txs.len());
        for tx in commitment_txs {
            commitment_tx_map.insert(tx.id(), tx);
        }

        let mut result_system_txs: HashMap<SystemLedger, Vec<CommitmentTransaction>> =
            HashMap::new();
        for ledger in &block_header.system_ledgers {
            let ledger_type = SystemLedger::try_from(ledger.ledger_id).map_err(|_| {
                eyre::eyre!(
                    "Invalid system ledger_id {} in block {:?}",
                    ledger.ledger_id,
                    block_header.block_hash
                )
            })?;

            let mut ledger_txs = Vec::with_capacity(ledger.tx_ids.0.len());
            for expected_id in &ledger.tx_ids.0 {
                let tx = commitment_tx_map.remove(expected_id).ok_or_else(|| {
                    eyre::eyre!(
                        "Header/body mismatch in block {:?}: {:?} system ledger missing tx {}",
                        block_header.block_hash,
                        ledger_type,
                        expected_id,
                    )
                })?;
                ledger_txs.push(tx);
            }
            result_system_txs.insert(ledger_type, ledger_txs);
        }

        if !commitment_tx_map.is_empty() {
            let extra_ids: Vec<_> = commitment_tx_map.keys().collect();
            return Err(eyre::eyre!(
                "Header/body mismatch in block {:?}: body contains {} extra commitment \
                 transaction(s) not referenced by header: {:?}",
                block_header.block_hash,
                extra_ids.len(),
                extra_ids,
            ));
        }

        Ok(BlockTransactions {
            system_txs: result_system_txs,
            data_txs: result_data_txs,
        })
    }
}

#[cfg(test)]
#[expect(
    clippy::useless_vec,
    reason = "Tests need Vec to exercise Index<DataLedger> for Vec<T> impls"
)]
mod tests {
    use crate::ingress::{IngressProof, IngressProofV1};
    use crate::{validate_path, Config, NodeConfig};

    use super::*;
    use alloy_primitives::{keccak256, Signature};
    use alloy_rlp::Decodable;
    use rand::{rngs::StdRng, Rng as _, SeedableRng as _};
    use rstest::rstest;
    use serde_json;
    use zerocopy::IntoBytes as _;

    #[test]
    fn test_poa_data_rlp_round_trip() {
        // setup
        let data = PoaData {
            partition_chunk_offset: 321,
            partition_hash: H256::random(),
            chunk: Some(Base64(vec![42; 16])),
            ledger_id: Some(44),
            tx_path: None,
            data_path: Some(Base64(vec![13; 16])),
        };

        // action
        let mut buffer = vec![];
        data.encode(&mut buffer);
        let decoded = Decodable::decode(&mut buffer.as_slice()).unwrap();

        // Assert
        assert_eq!(data, decoded);
    }

    #[test]
    fn test_vdf_limiter_info_rlp_round_trip() {
        let data = VDFLimiterInfo {
            output: H256::random(),
            global_step_number: 42,
            seed: H256::random(),
            next_seed: H256::random(),
            prev_output: H256::random(),
            last_step_checkpoints: H256List(vec![H256::random(), H256::random()]),
            steps: H256List(vec![H256::random(), H256::random()]),
            vdf_difficulty: Some(123),
            next_vdf_difficulty: Some(321),
        };

        // action
        let mut buffer = vec![];
        data.encode(&mut buffer);
        let decoded = Decodable::decode(&mut buffer.as_slice()).unwrap();

        // Assert
        assert_eq!(data, decoded);
    }

    #[test]
    fn test_vdf_limiter_info_compact_round_trip() {
        let data = VDFLimiterInfo {
            output: H256::random(),
            global_step_number: 42,
            seed: H256::random(),
            next_seed: H256::random(),
            prev_output: H256::random(),
            last_step_checkpoints: H256List(vec![H256::random(), H256::random()]),
            steps: H256List(vec![H256::random(), H256::random()]),
            vdf_difficulty: Some(123),
            next_vdf_difficulty: Some(321),
        };

        // action
        let mut buffer = vec![];
        data.to_compact(&mut buffer);
        let (decoded, ..) = VDFLimiterInfo::from_compact(buffer.as_slice(), buffer.len());

        // Assert
        assert_eq!(data, decoded);
    }

    #[test]
    fn test_storage_transaction_ledger_rlp_round_trip() {
        // setup
        let data = DataTransactionLedger {
            ledger_id: DataLedger::Submit.into(),
            tx_root: H256::random(),
            tx_ids: H256List(vec![]),
            total_chunks: 55,
            expires: None,
            proofs: Some(IngressProofsList(vec![IngressProof::V1(IngressProofV1 {
                proof: H256::random(),
                signature: Default::default(), // signature is ignored by RLP & substituted with the default value
                data_root: H256::random(),
                chain_id: 1,
                anchor: H256::from([10_u8; 32]),
            })])),
            required_proof_count: None,
        };

        // action
        let mut buffer = vec![];
        data.encode(&mut buffer);
        let decoded = Decodable::decode(&mut buffer.as_slice()).unwrap();

        // Assert
        assert_eq!(data, decoded);
    }

    #[test]
    fn test_system_transaction_ledger_rlp_round_trip() {
        // setup
        let system = SystemTransactionLedger {
            ledger_id: SystemLedger::Commitment.into(),
            tx_ids: H256List(vec![H256::random(), H256::random()]),
        };

        // action
        let mut buffer = vec![];
        system.encode(&mut buffer);
        let decoded = Decodable::decode(&mut buffer.as_slice()).unwrap();

        // Assert
        assert_eq!(system, decoded);
    }

    #[test]
    fn test_irys_block_header_serde_round_trip() {
        // setup
        let header = mock_header();

        // action
        let serialized = serde_json::to_string_pretty(&header).unwrap();
        let deserialized: IrysBlockHeaderV1 = serde_json::from_str(&serialized).unwrap();

        // Assert
        assert_eq!(header, deserialized);
    }

    #[test]
    fn test_irys_block_header_rlp_round_trip() {
        // setup
        let mut data = mock_header();

        // action
        let mut buffer = vec![];
        Encodable::encode(&data, &mut buffer);
        let decoded = Decodable::decode(&mut buffer.as_slice()).unwrap();

        // Assert
        // (the following fields just get zeroed out once encoded)
        data.block_hash = H256::zero();
        data.signature = IrysSignature::new(Signature::try_from([0_u8; 65].as_slice()).unwrap());
        assert_eq!(data, decoded);
    }

    #[test]
    fn test_irys_header_compact_round_trip() {
        // setup
        let mut header = mock_header();
        let mut buf = vec![];

        header.data_ledgers[DataLedger::Publish].proofs = Some(IngressProofsList(vec![
            IngressProof::default(),
            IngressProof::default(),
            IngressProof::default(),
        ]));

        println!("{}", serde_json::to_string_pretty(&header).unwrap());

        // action
        header.to_compact(&mut buf);
        assert!(!buf.is_empty(), "expect data to be written into the buffer");
        let (derived_header, rest_of_the_buffer) = IrysBlockHeaderV1::from_compact(&buf, buf.len());
        assert!(
            rest_of_the_buffer.is_empty(),
            "the whole buffer should be read"
        );

        // assert
        assert_eq!(derived_header, header);
    }

    #[rstest]
    #[case(0, 0)]
    #[case(1, 0)]
    #[case(15, 14)]
    #[case(19, 18)]
    #[case(20, 19)]
    #[case(21, 19)]
    #[case(29, 19)]
    #[case(30, 29)]
    #[case(31, 29)]
    #[case(39, 29)]
    #[case(40, 39)]
    #[case(41, 39)]
    #[case(99, 89)]
    #[case(100, 99)]
    fn test_previous_ema_for_multiple_intervals(
        #[case] height: u64,
        #[case] expected_prev_ema: u64,
    ) {
        let interval = 10;
        let header = IrysBlockHeaderV1 {
            height,
            ..Default::default()
        };
        let result = header.previous_ema_recalculation_block_height(interval);

        assert_eq!(
            result, expected_prev_ema,
            "For height={height}, expected {expected_prev_ema} but got {result}"
        );
    }

    #[rstest]
    #[case(0, true)]
    #[case(1, true)]
    #[case(9, true)]
    #[case(10, true)]
    #[case(11, true)]
    #[case(19, true)]
    #[case(20, false)]
    #[case(21, false)]
    #[case(30, false)]
    #[case(99, true)]
    #[case(100, false)]
    fn test_is_ema_recalculation_block(#[case] height: u64, #[case] expected_is_ema: bool) {
        let interval = 10;
        let header = IrysBlockHeaderV1 {
            height,
            ..Default::default()
        };
        let is_ema = header.is_ema_recalculation_block(interval);

        assert_eq!(
        is_ema, expected_is_ema,
        "For height={height}, expected is_ema_recalculation_block={expected_is_ema} but got {is_ema}"
    );
    }

    #[test]
    fn test_validate_tx_path() {
        let mut txs: Vec<DataTransactionHeader> = vec![DataTransactionHeader::default(); 10];
        for tx in txs.iter_mut() {
            tx.data_root = H256::from([3_u8; 32]);
            tx.data_size = 64
        }

        let (tx_root, proofs) = DataTransactionLedger::merklize_tx_root(&txs);

        for proof in proofs {
            let encoded_proof = Base64(proof.proof.clone());
            validate_path(tx_root.0, &encoded_proof, proof.last_byte_index as u128).unwrap();
        }
    }

    #[test]
    fn test_irys_block_header_signing() {
        // setup
        let mut header = IrysBlockHeader::V1(mock_header());
        let testing_config = NodeConfig::testing();
        let config = Config::new_with_random_peer_id(testing_config);
        let signer = config.irys_signer();

        // action
        // sign the block header
        signer.sign_block_header(&mut header).unwrap();
        let block_hash = keccak256(header.signature.as_bytes()).0;

        // assert signature
        assert_eq!(H256::from(block_hash), header.block_hash);
        assert!(header.is_signature_valid());
        let mut rng = StdRng::from_seed([42_u8; 32]);

        // assert that updating values changes the hash
        let fields: &[fn(&mut IrysBlockHeader) -> &mut [u8]] = &[
            |h: &mut IrysBlockHeader| h.height.as_mut_bytes(),
            |h: &mut IrysBlockHeader| h.last_diff_timestamp.as_mut_bytes(),
            |h: &mut IrysBlockHeader| h.solution_hash.as_bytes_mut(),
            |h: &mut IrysBlockHeader| h.previous_solution_hash.as_bytes_mut(),
            |h: &mut IrysBlockHeader| h.last_epoch_hash.as_bytes_mut(),
            |h: &mut IrysBlockHeader| h.chunk_hash.as_bytes_mut(),
            |h: &mut IrysBlockHeader| h.previous_block_hash.as_bytes_mut(),
            |h: &mut IrysBlockHeader| h.reward_address.as_mut_bytes(),
            |h: &mut IrysBlockHeader| h.miner_address.as_mut_bytes(),
            |h: &mut IrysBlockHeader| h.timestamp.as_mut_bytes(),
            |h: &mut IrysBlockHeader| h.data_ledgers[0].ledger_id.as_mut_bytes(),
            |h: &mut IrysBlockHeader| h.data_ledgers[0].total_chunks.as_mut_bytes(),
            |h: &mut IrysBlockHeader| h.evm_block_hash.as_mut_bytes(),
            |h: &mut IrysBlockHeader| h.vdf_limiter_info.global_step_number.as_mut_bytes(),
        ];
        for get_field in fields {
            let mut header_clone = header.clone();
            let get_field = get_field(&mut header_clone);
            rng.fill(get_field);
            assert!(!header_clone.is_signature_valid());
        }

        // assert that changing the block hash changes the validation result to invalid
        header.block_hash = H256::random();
        assert!(!header.is_signature_valid());
    }

    fn mock_header() -> IrysBlockHeaderV1 {
        IrysBlockHeaderV1::new_mock_header()
    }

    #[test]
    fn test_block_header_serialization() {
        let header = IrysBlockHeaderV1 {
            height: u64::MAX,
            ..Default::default()
        };

        let json = serde_json::to_string(&header).unwrap();
        assert!(json.contains(&format!("\"height\":\"{}\"", u64::MAX)));

        let deserialized: IrysBlockHeaderV1 = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.height, u64::MAX);
    }

    #[test]
    fn test_vdf_limiter_info_serialization() {
        let vdf_info = VDFLimiterInfo {
            global_step_number: u64::MAX,
            ..Default::default()
        };

        let json = serde_json::to_string(&vdf_info).unwrap();
        assert!(json.contains(&format!("\"globalStepNumber\":\"{}\"", u64::MAX)));

        let deserialized: VDFLimiterInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.global_step_number, u64::MAX);
    }

    #[test]
    fn test_ledger_index_item_serialization() {
        let ledger_item = LedgerIndexItem {
            total_chunks: u64::MAX,
            ..Default::default()
        };

        let json = serde_json::to_string(&ledger_item).unwrap();
        assert!(json.contains(&format!("\"total_chunks\":\"{}\"", u64::MAX)));

        let deserialized: LedgerIndexItem = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.total_chunks, u64::MAX);
    }

    #[test]
    fn test_data_transaction_ledger_expires_serialization() {
        use serde_json;

        let ledger = DataTransactionLedger {
            expires: Some(u64::MAX),
            ..Default::default()
        };

        let json = serde_json::to_string(&ledger).unwrap();
        assert!(json.contains(&format!("\"expires\":\"{}\"", u64::MAX)));

        let deserialized: DataTransactionLedger = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.expires, Some(u64::MAX));
    }

    #[test]
    fn test_order_transactions_rejects_extra_data_txs() {
        // Header references one data tx
        let referenced_id = H256::random();
        let extra_id = H256::random();

        let mut header = IrysBlockHeader::default();
        let IrysBlockHeader::V1(ref mut v1) = header;
        v1.data_ledgers = vec![DataTransactionLedger {
            ledger_id: DataLedger::Publish.into(),
            tx_ids: H256List(vec![referenced_id]),
            ..Default::default()
        }];
        v1.system_ledgers = vec![];

        // Body has the referenced tx plus an extra one not in the header
        let mut referenced_tx = DataTransactionHeader::default();
        referenced_tx.id = referenced_id;
        let mut extra_tx = DataTransactionHeader::default();
        extra_tx.id = extra_id;

        let result =
            SealedBlock::order_transactions(&header, vec![referenced_tx, extra_tx], vec![]);

        assert!(
            result.is_err(),
            "Should reject body with extra data transactions not referenced by header"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains(&format!("{extra_id:?}")),
            "Error should mention the extra tx id, got: {err_msg}"
        );
    }

    #[test]
    fn test_order_transactions_rejects_extra_commitment_txs() {
        // Header references one commitment tx
        let referenced_id = H256::random();
        let extra_id = H256::random();

        let mut header = IrysBlockHeader::default();
        let IrysBlockHeader::V1(ref mut v1) = header;
        v1.data_ledgers = vec![];
        v1.system_ledgers = vec![SystemTransactionLedger {
            ledger_id: SystemLedger::Commitment.into(),
            tx_ids: H256List(vec![referenced_id]),
        }];

        // Body has the referenced commitment tx plus an extra one
        let mut referenced_tx = CommitmentTransaction::default();
        referenced_tx.set_id(referenced_id);
        let mut extra_tx = CommitmentTransaction::default();
        extra_tx.set_id(extra_id);

        let result =
            SealedBlock::order_transactions(&header, vec![], vec![referenced_tx, extra_tx]);

        assert!(
            result.is_err(),
            "Should reject body with extra commitment transactions not referenced by header"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains(&format!("{extra_id:?}")),
            "Error should mention the extra commitment tx id, got: {err_msg}"
        );
    }

    #[test]
    fn test_vec_data_transaction_ledger_index_non_contiguous_discriminants() {
        let ledgers = vec![
            DataTransactionLedger {
                ledger_id: DataLedger::Publish.into(),
                total_chunks: 100,
                ..Default::default()
            },
            DataTransactionLedger {
                ledger_id: DataLedger::Submit.into(),
                total_chunks: 200,
                ..Default::default()
            },
            DataTransactionLedger {
                ledger_id: DataLedger::OneYear.into(),
                total_chunks: 300,
                ..Default::default()
            },
            DataTransactionLedger {
                ledger_id: DataLedger::ThirtyDay.into(),
                total_chunks: 400,
                ..Default::default()
            },
        ];
        assert_eq!(ledgers[DataLedger::Publish].total_chunks, 100);
        assert_eq!(ledgers[DataLedger::Submit].total_chunks, 200);
        assert_eq!(ledgers[DataLedger::OneYear].total_chunks, 300);
        assert_eq!(ledgers[DataLedger::ThirtyDay].total_chunks, 400);
    }

    #[test]
    #[should_panic(expected = "No transaction ledger found")]
    fn test_vec_data_transaction_ledger_index_missing_ledger_panics() {
        let ledgers = vec![
            DataTransactionLedger {
                ledger_id: DataLedger::Publish.into(),
                ..Default::default()
            },
            DataTransactionLedger {
                ledger_id: DataLedger::Submit.into(),
                ..Default::default()
            },
        ];
        let _ = &ledgers[DataLedger::OneYear];
    }

    #[test]
    fn test_vec_data_transaction_ledger_index_mut_non_contiguous() {
        let mut ledgers = vec![
            DataTransactionLedger {
                ledger_id: DataLedger::Publish.into(),
                total_chunks: 100,
                ..Default::default()
            },
            DataTransactionLedger {
                ledger_id: DataLedger::Submit.into(),
                total_chunks: 200,
                ..Default::default()
            },
            DataTransactionLedger {
                ledger_id: DataLedger::OneYear.into(),
                total_chunks: 300,
                ..Default::default()
            },
            DataTransactionLedger {
                ledger_id: DataLedger::ThirtyDay.into(),
                total_chunks: 400,
                ..Default::default()
            },
        ];
        ledgers[DataLedger::OneYear].total_chunks = 999;
        assert_eq!(ledgers[DataLedger::OneYear].total_chunks, 999);
    }

    #[test]
    fn test_vec_ledger_index_item_index_non_contiguous_discriminants() {
        let items = vec![
            LedgerIndexItem {
                ledger: DataLedger::Publish,
                total_chunks: 10,
                tx_root: H256::random(),
            },
            LedgerIndexItem {
                ledger: DataLedger::Submit,
                total_chunks: 20,
                tx_root: H256::random(),
            },
            LedgerIndexItem {
                ledger: DataLedger::OneYear,
                total_chunks: 30,
                tx_root: H256::random(),
            },
            LedgerIndexItem {
                ledger: DataLedger::ThirtyDay,
                total_chunks: 40,
                tx_root: H256::random(),
            },
        ];
        assert_eq!(items[DataLedger::Publish].total_chunks, 10);
        assert_eq!(items[DataLedger::Submit].total_chunks, 20);
        assert_eq!(items[DataLedger::OneYear].total_chunks, 30);
        assert_eq!(items[DataLedger::ThirtyDay].total_chunks, 40);
    }

    #[test]
    #[should_panic(expected = "No ledger index item found")]
    fn test_vec_ledger_index_item_missing_ledger_panics() {
        let items = vec![
            LedgerIndexItem {
                ledger: DataLedger::Publish,
                total_chunks: 10,
                tx_root: H256::zero(),
            },
            LedgerIndexItem {
                ledger: DataLedger::Submit,
                total_chunks: 20,
                tx_root: H256::zero(),
            },
        ];
        let _ = &items[DataLedger::OneYear];
    }

    #[test]
    fn test_block_index_item_roundtrip_with_four_ledgers() {
        let item = BlockIndexItem {
            block_hash: H256::random(),
            num_ledgers: 4,
            ledgers: vec![
                LedgerIndexItem {
                    total_chunks: 100,
                    tx_root: H256::random(),
                    ledger: DataLedger::Publish,
                },
                LedgerIndexItem {
                    total_chunks: 200,
                    tx_root: H256::random(),
                    ledger: DataLedger::Submit,
                },
                LedgerIndexItem {
                    total_chunks: 300,
                    tx_root: H256::random(),
                    ledger: DataLedger::OneYear,
                },
                LedgerIndexItem {
                    total_chunks: 400,
                    tx_root: H256::random(),
                    ledger: DataLedger::ThirtyDay,
                },
            ],
        };
        let bytes = item.to_bytes();
        let restored = BlockIndexItem::from_bytes(&bytes);
        assert_eq!(restored.num_ledgers, 4);
        assert_eq!(restored.block_hash, item.block_hash);
        assert_eq!(restored.ledgers.len(), 4);
        assert_eq!(restored.ledgers[0].total_chunks, 100);
        assert_eq!(restored.ledgers[0].tx_root, item.ledgers[0].tx_root);
        assert_eq!(restored.ledgers[1].total_chunks, 200);
        assert_eq!(restored.ledgers[2].total_chunks, 300);
        assert_eq!(restored.ledgers[3].total_chunks, 400);
        assert_eq!(restored.ledgers[3].tx_root, item.ledgers[3].tx_root);
    }

    #[test]
    fn test_block_index_item_roundtrip_with_two_ledgers() {
        let item = BlockIndexItem {
            block_hash: H256::random(),
            num_ledgers: 2,
            ledgers: vec![
                LedgerIndexItem {
                    total_chunks: 100,
                    tx_root: H256::random(),
                    ledger: DataLedger::Publish,
                },
                LedgerIndexItem {
                    total_chunks: 200,
                    tx_root: H256::random(),
                    ledger: DataLedger::Submit,
                },
            ],
        };
        let bytes = item.to_bytes();
        let restored = BlockIndexItem::from_bytes(&bytes);
        assert_eq!(restored.num_ledgers, 2);
        assert_eq!(restored.ledgers.len(), 2);
        assert_eq!(restored.ledgers[0].total_chunks, 100);
        assert_eq!(restored.ledgers[1].total_chunks, 200);
    }

    #[test]
    fn test_mock_header_has_exactly_two_ledgers() {
        let header = IrysBlockHeader::new_mock_header();
        assert_eq!(header.data_ledgers.len(), 2);
        assert_eq!(header.data_ledgers[0].ledger_id, DataLedger::Publish as u32);
        assert_eq!(header.data_ledgers[1].ledger_id, DataLedger::Submit as u32);
    }
}
