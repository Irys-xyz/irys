//! Contains a common set of types used across all of the `irys-chain` modules.
//!
//! This module implements a single location where these types are managed,
//! making them easy to reference and maintain.
use crate::block_production::SolutionContext;
use crate::block_provider::{BlockIndex, ResetSeedCache};
use crate::storage_pricing::{phantoms::IrysPrice, phantoms::Usd, Amount};
use crate::{
    generate_data_root, generate_leaves_from_data_roots, option_u64_stringify,
    partition::PartitionHash, resolve_proofs, string_u128, u64_stringify, Arbitrary, Base64,
    Compact, Config, DataRootLeave, H256List, IngressProofsList, IrysSignature,
    IrysTransactionHeader, Proof, H256, U256,
};
use actix::MessageResponse;
use alloy_primitives::{keccak256, Address, TxHash, B256};
use alloy_rlp::{Encodable, RlpDecodable, RlpEncodable};
use reth_primitives::Header;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::ops::{Index, IndexMut};

pub type BlockHash = H256;

pub type EvmBlockHash = B256;

/// Stores the `vdf_limiter_info` in the [`IrysBlockHeader`]
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
    pub fn new<BI: BlockIndex>(
        solution: &SolutionContext,
        prev_block_header: &IrysBlockHeader,
        steps: H256List,
        config: &Config,
        reset_seed_manager: &ResetSeedCache<BI>,
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

        let reset_frequency = config.consensus.vdf.reset_frequency;
        vdf_limiter_info.set_seeds(
            reset_frequency as u64,
            prev_block_header,
            reset_seed_manager,
        );

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

    pub fn set_seeds<BI: BlockIndex>(
        &mut self,
        reset_frequency: u64,
        parent_header: &IrysBlockHeader,
        reset_seed_manager: &ResetSeedCache<BI>,
    ) {
        if let Some(step) = self.reset_step(reset_frequency) {
            // If the current block is a reset step, we set the next seed to the parent block's hash.
            self.next_seed = reset_seed_manager
                .block_hash_that_contains_step(step.saturating_sub(reset_frequency))
                .expect("Reset seed must always be available for the current step");
            self.seed = parent_header.vdf_limiter_info.next_seed;
        } else {
            // Otherwise, we set the next seed to the previous block next_seed.
            self.next_seed = parent_header.vdf_limiter_info.next_seed;
            self.seed = parent_header.vdf_limiter_info.seed;
        }
    }
}

/// Stores deserialized fields from a JSON formatted Irys block header.
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
pub struct IrysBlockHeader {
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
    pub height: u64,

    /// Difficulty threshold used to produce the current block.
    pub diff: U256,

    /// The sum of the average number of hashes computed by the network to
    /// produce the past blocks including this one.
    pub cumulative_diff: U256,

    /// The solution hash for the block hash(chunk_bytes + partition_chunk_offset + mining_seed)
    pub solution_hash: H256,

    /// timestamp (in milliseconds) since UNIX_EPOCH of the last difficulty adjustment
    #[serde(with = "string_u128")]
    pub last_diff_timestamp: u128,

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
    pub reward_address: Address,

    /// The amount of Irys tokens that must be rewarded to the `self.reward_address`
    pub reward_amount: U256,

    /// The address of the block producer - used to validate the block hash/signature & the PoA chunk (as the packing key)
    /// We allow for miners to send rewards to a separate address
    pub miner_address: Address,

    /// timestamp (in milliseconds) since UNIX_EPOCH of when the block was discovered/produced
    #[serde(with = "string_u128")]
    pub timestamp: u128,

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
}

pub type IrysTokenPrice = Amount<(IrysPrice, Usd)>;

impl IrysBlockHeader {
    /// Returns true if the block is the genesis block, false otherwise
    pub fn is_genesis(&self) -> bool {
        self.height == 0
    }

    /// Proxy method for `Encodable::encode`
    ///
    /// Packs all the header data into a byte buffer, using RLP encoding.
    pub fn digest_for_signing<B>(&self, buf: &mut B)
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        // Using trait directly because `reth_db_api` also has an `encode` method.
        Encodable::encode(&self, buf);
    }

    /// Create a `keccak256` hash of the [`IrysBlockHeader`]
    pub fn signature_hash(&self) -> [u8; 32] {
        // allocate the buffer, guesstimate the required capacity
        let mut bytes = Vec::with_capacity(size_of::<Self>() * 3);
        self.digest_for_signing(&mut bytes);
        keccak256(bytes).0
    }

    /// Validates the block hash signature by:
    /// 1.) generating the prehash
    /// 2.) recovering the sender address, and comparing it to the block headers miner_address (miner_address MUST be part of the prehash)
    pub fn is_signature_valid(&self) -> bool {
        self.signature
            .validate_signature(self.signature_hash(), self.miner_address)
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

    /// get storage ledger txs from blocks data ledger
    pub fn get_commitment_ledger_tx_ids(&self) -> Vec<H256> {
        let mut commitment_txids = Vec::new();
        // Because of a circular dependency the types crate can't import the SystemLedger enum
        // SystemLedger::Commitments = 0, so finding `ledger_id: 0` here, locates the commitment ledger
        let commitment_ledger = self.system_ledgers.iter().find(|l| l.ledger_id == 0);

        if let Some(commitment_ledger) = commitment_ledger {
            commitment_txids = commitment_ledger.tx_ids.0.clone();
        }

        commitment_txids
    }

    pub fn get_data_ledger_tx_ids(&self) -> HashMap<DataLedger, HashSet<H256>> {
        let mut data_txids = HashMap::new();
        for data_ledger in self.data_ledgers.iter() {
            data_txids.insert(
                DataLedger::from_u32(data_ledger.ledger_id).unwrap(),
                data_ledger.tx_ids.0.clone().into_iter().collect(),
            );
        }
        data_txids
    }
}

// treat any block whose height is a multiple of blocks_in_price_adjustment_interval
pub fn is_ema_recalculation_block(height: u64, blocks_in_price_adjustment_interval: u64) -> bool {
    // the first 2 adjustment intervals have special handling where we calculate the
    // EMA for each block using the value from the preceding one
    if height < (blocks_in_price_adjustment_interval * 2) {
        true
    } else {
        (height.saturating_add(1)) % blocks_in_price_adjustment_interval == 0
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
    pub recall_chunk_index: u32,
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
    /// The size of this ledger (in chunks) since genesis
    #[serde(default, with = "u64_stringify")]
    pub max_chunk_offset: u64,
    /// This ledger expires after how many epochs
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires: Option<u64>,
    /// When transactions are promoted they must include their ingress proofs
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proofs: Option<IngressProofsList>,
}

impl DataTransactionLedger {
    /// Computes the tx_root and tx_paths. The TX Root is composed of taking the data_roots of each of the storage
    /// transactions included, in order, and building a merkle tree out of them. The root of this tree is the tx_root.
    pub fn merklize_tx_root(data_txs: &[IrysTransactionHeader]) -> (H256, Vec<Proof>) {
        if data_txs.is_empty() {
            return (H256::zero(), vec![]);
        }
        let txs_data_roots = data_txs
            .iter()
            .map(|h| DataRootLeave {
                data_root: h.data_root,
                tx_size: h.data_size as usize, // TODO: check this
            })
            .collect::<Vec<DataRootLeave>>();
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

impl fmt::Display for IrysBlockHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Convert the struct to a JSON string using serde_json
        match serde_json::to_string_pretty(self) {
            Ok(json) => write!(f, "{}", json), // Write the JSON string to the formatter
            Err(_) => write!(f, "Failed to serialize IrysBlockHeader"), // Handle serialization errors
        }
    }
}

impl IrysBlockHeader {
    pub fn new_mock_header() -> Self {
        use std::str::FromStr as _;
        use std::time::{SystemTime, UNIX_EPOCH};

        let tx_ids = H256List::new();
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();

        // Create a sample IrysBlockHeader object with mock data
        Self {
            diff: U256::from(1000),
            cumulative_diff: U256::from(5000),
            last_diff_timestamp: 1622543200,
            solution_hash: H256::zero(),
            previous_solution_hash: H256::zero(),
            last_epoch_hash: H256::random(),
            chunk_hash: H256::zero(),
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
                recall_chunk_index: 0,
                ledger_id: None,
            },
            reward_address: Address::ZERO,
            signature: IrysSignature::new(alloy_signer::Signature::test_signature()),
            timestamp: now.as_millis(),
            system_ledgers: vec![], // Many tests will fail if you add fake txids to this ledger
            data_ledgers: vec![
                // Permanent Publish Ledger
                DataTransactionLedger {
                    ledger_id: 0, // Publish ledger_id
                    tx_root: H256::zero(),
                    tx_ids,
                    max_chunk_offset: 0,
                    expires: None,
                    proofs: None,
                },
                // Term Submit Ledger
                DataTransactionLedger {
                    ledger_id: 1, // Submit ledger_id
                    tx_root: H256::zero(),
                    tx_ids: H256List::new(),
                    max_chunk_offset: 0,
                    expires: Some(1622543200),
                    proofs: None,
                },
            ],
            evm_block_hash: B256::ZERO,
            miner_address: Address::ZERO,
            oracle_irys_price: Amount::token(dec!(1.0))
                .expect("dec!(1.0) must evaluate to a valid token amount"),
            ema_irys_price: Amount::token(dec!(1.0))
                .expect("dec!(1.0) must evaluate to a valid token amount"),
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

/// Names for each of the ledgers as well as their `ledger_id` discriminant
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Compact, PartialOrd, Ord, Hash,
)]
#[repr(u32)]
pub enum DataLedger {
    /// The permanent publish ledger
    Publish = 0,
    /// An expiring term ledger used for submitting to the publish ledger
    Submit = 1,
    // Add more term ledgers as they exist
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

impl Default for DataLedger {
    fn default() -> Self {
        Self::Publish
    }
}

impl DataLedger {
    /// An array of all the Ledger numbers in order
    pub const ALL: [Self; 2] = [Self::Publish, Self::Submit];

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
            _ => None,
        }
    }
}

impl From<DataLedger> for u32 {
    fn from(ledger: DataLedger) -> Self {
        ledger as Self
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

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BlockIndexQuery {
    pub height: usize,
    pub limit: usize,
}

/// Core metadata of the [`BlockIndex`] this struct tracks the ledger size and
/// tx root for each ledger per block. Enabling lookups to that find the `tx_root`
/// for a ledger at a particular byte offset in the ledger.
#[derive(Debug, Clone, Default, PartialEq, Eq, MessageResponse, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct LedgerIndexItem {
    /// Size in bytes of the ledger
    pub max_chunk_offset: u64, // 8 bytes
    /// The merkle root of the TX that apply to this ledger in the current block
    pub tx_root: H256, // 32 bytes
}

impl LedgerIndexItem {
    fn to_bytes(&self) -> [u8; 40] {
        // Fixed size of 40 bytes
        let mut bytes = [0_u8; 40];
        bytes[0..8].copy_from_slice(&self.max_chunk_offset.to_le_bytes()); // First 8 bytes
        bytes[8..40].copy_from_slice(self.tx_root.as_bytes()); // Next 32 bytes
        bytes
    }

    fn from_bytes(bytes: &[u8]) -> Self {
        let mut item = Self::default();

        // Read ledger size (first 8 bytes)
        let mut size_bytes = [0_u8; 8];
        size_bytes.copy_from_slice(&bytes[0..8]);
        item.max_chunk_offset = u64::from_le_bytes(size_bytes);

        // Read tx root (next 32 bytes)
        item.tx_root = H256::from_slice(&bytes[8..40]);

        item
    }
}

impl Index<DataLedger> for Vec<LedgerIndexItem> {
    type Output = LedgerIndexItem;

    fn index(&self, ledger: DataLedger) -> &Self::Output {
        &self[ledger as usize]
    }
}

impl IndexMut<DataLedger> for Vec<LedgerIndexItem> {
    fn index_mut(&mut self, ledger: DataLedger) -> &mut Self::Output {
        &mut self[ledger as usize]
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

#[cfg(test)]
mod tests {
    use crate::{validate_path, Config, NodeConfig, TxIngressProof};

    use super::*;
    use alloy_primitives::Signature;
    use alloy_rlp::Decodable;
    use rand::{rngs::StdRng, Rng as _, SeedableRng as _};
    use rstest::rstest;
    use serde_json;
    use zerocopy::IntoBytes as _;

    #[test]
    fn test_poa_data_rlp_round_trip() {
        // setup
        let data = PoaData {
            recall_chunk_index: 123,
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
            ledger_id: 1,
            tx_root: H256::random(),
            tx_ids: H256List(vec![]),
            max_chunk_offset: 55,
            expires: None,
            proofs: Some(IngressProofsList(vec![TxIngressProof {
                proof: H256::random(),
                signature: IrysSignature::new(Signature::test_signature()),
            }])),
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
            ledger_id: 0, // System Ledger
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
        let deserialized: IrysBlockHeader = serde_json::from_str(&serialized).unwrap();

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
        let header = mock_header();
        let mut buf = vec![];

        println!("{}", serde_json::to_string_pretty(&header).unwrap());

        // action
        header.to_compact(&mut buf);
        assert!(!buf.is_empty(), "expect data to be written into the buffer");
        let (derived_header, rest_of_the_buffer) = IrysBlockHeader::from_compact(&buf, buf.len());
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
        let header = IrysBlockHeader {
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
        let header = IrysBlockHeader {
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
        let mut txs: Vec<IrysTransactionHeader> = vec![IrysTransactionHeader::default(); 10];
        for tx in txs.iter_mut() {
            tx.data_root = H256::from([3_u8; 32]);
            tx.data_size = 64
        }

        let (tx_root, proofs) = DataTransactionLedger::merklize_tx_root(&txs);

        for proof in proofs {
            let encoded_proof = Base64(proof.proof.clone());
            validate_path(tx_root.0, &encoded_proof, proof.offset as u128).unwrap();
        }
    }

    #[test]
    fn test_irys_block_header_signing() {
        // setup
        let mut header = mock_header();
        let testnet_config = NodeConfig::testnet();
        let config = Config::new(testnet_config);
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
            |h: &mut IrysBlockHeader| h.data_ledgers[0].max_chunk_offset.as_mut_bytes(),
            |h: &mut IrysBlockHeader| h.evm_block_hash.as_mut_bytes(),
            |h: &mut IrysBlockHeader| h.vdf_limiter_info.global_step_number.as_mut_bytes(),
        ];
        for get_field in fields {
            let mut header_clone = header.clone();
            let get_field = get_field(&mut header_clone);
            rng.fill(get_field);
            assert!(!header_clone.is_signature_valid());
        }

        // assert that changing the block hash, the signature is still valid (because the validation does not validate )
        header.block_hash = H256::random();
        assert!(header.is_signature_valid());
    }

    fn mock_header() -> IrysBlockHeader {
        IrysBlockHeader::new_mock_header()
    }
}
