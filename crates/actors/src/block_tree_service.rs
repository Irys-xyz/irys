use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, RwLock, RwLockReadGuard},
    time::SystemTime,
};

use crate::{
    block_discovery::BlockPreValidatedMessage,
    block_index_service::BlockIndexService,
    block_producer::{BlockConfirmedMessage, BlockProducerActor, RegisterBlockProducerMessage},
    chunk_migration_service::ChunkMigrationService,
    mempool_service::MempoolService,
    validation_service::{RequestValidationMessage, ValidationService},
    BlockFinalizedMessage,
};
use actix::prelude::*;
use base58::ToBase58;
use eyre::ensure;
use irys_database::{block_header_by_hash, tx_header_by_txid, BlockIndex, Initialized, Ledger};
use irys_types::{
    Address, BlockHash, DatabaseProvider, IrysBlockHeader, IrysTransactionHeader,
    IrysTransactionId, H256, U256,
};
use reth_db::{transaction::DbTx, Database};
use tracing::{debug, error};

//==============================================================================
// BlockTreeReadGuard
//------------------------------------------------------------------------------

/// Wraps the internal Arc<`RwLock`<>> to make the reference readonly
#[derive(Debug, Clone, MessageResponse)]
pub struct BlockTreeReadGuard {
    block_tree_cache: Arc<RwLock<BlockTreeCache>>,
}

impl BlockTreeReadGuard {
    /// Creates a new `ReadGard` for the block_tree cache
    pub const fn new(block_tree_cache: Arc<RwLock<BlockTreeCache>>) -> Self {
        Self { block_tree_cache }
    }

    /// Accessor method to get a read guard for the block_tree cache
    pub fn read(&self) -> RwLockReadGuard<'_, BlockTreeCache> {
        self.block_tree_cache.read().unwrap()
    }
}

/// Retrieve a read only reference to the block_tree's cache
#[derive(Message, Debug)]
#[rtype(result = "BlockTreeReadGuard")]
pub struct GetBlockTreeGuardMessage;

impl Handler<GetBlockTreeGuardMessage> for BlockTreeService {
    type Result = BlockTreeReadGuard; // Return guard directly

    fn handle(&mut self, _msg: GetBlockTreeGuardMessage, _ctx: &mut Self::Context) -> Self::Result {
        BlockTreeReadGuard::new(self.cache.clone().unwrap())
    }
}

//==============================================================================
// BlockTree Actor
//------------------------------------------------------------------------------

/// `BlockDiscoveryActor` listens for discovered blocks & validates them.
#[derive(Debug, Default)]
pub struct BlockTreeService {
    db: Option<DatabaseProvider>,
    /// Needs to know the current block to build on
    block_producer: Option<Addr<BlockProducerActor>>,
    /// Block tree internal state
    pub cache: Option<Arc<RwLock<BlockTreeCache>>>,
    /// The wallet address of the local miner
    pub miner_address: Address,
}

impl Actor for BlockTreeService {
    type Context = Context<Self>;
}

/// Adds this actor the the local service registry
impl Supervised for BlockTreeService {}

impl SystemService for BlockTreeService {
    fn service_started(&mut self, ctx: &mut Context<Self>) {
        println!("service started: block_tree (Default)");
    }
}

impl BlockTreeService {
    /// Initializes a BlockTreeActor without a block_producer address
    pub fn new(
        db: DatabaseProvider,
        block_index: Arc<RwLock<BlockIndex<Initialized>>>,
        miner_address: &Address,
    ) -> Self {
        let cache = BlockTreeCache::initialize_from_list(block_index, db.clone());

        Self {
            db: Some(db),
            block_producer: None,
            cache: Some(Arc::new(RwLock::new(cache))),
            miner_address: *miner_address,
        }
    }

    fn send_storage_finalized_message(&self, block_hash: BlockHash) -> eyre::Result<()> {
        let tx = self
            .db
            .clone()
            .unwrap()
            .tx()
            .map_err(|e| eyre::eyre!("Failed to create transaction: {}", e))?;

        let block_header = match block_header_by_hash(&tx, &block_hash) {
            Ok(Some(header)) => header,
            Ok(None) => {
                return Err(eyre::eyre!("No block header found for hash {}", block_hash));
            }
            Err(e) => {
                return Err(eyre::eyre!("Failed to get previous block header: {}", e));
            }
        };

        // Get all the transactions for the finalized block, error if not found
        // TODO: Eventually abstract this for support of `n` ledgers
        let submit_txs = get_ledger_tx_headers(&tx, &block_header, Ledger::Submit);
        let publish_txs = get_ledger_tx_headers(&tx, &block_header, Ledger::Publish);

        let all_txs = {
            let mut combined = submit_txs.unwrap_or_default();
            combined.extend(publish_txs.unwrap_or_default());
            combined
        };

        let chunk_migration = ChunkMigrationService::from_registry();
        let block_index = BlockIndexService::from_registry();
        let block_finalized_message = BlockFinalizedMessage {
            block_header: Arc::new(block_header),
            all_txs: Arc::new(all_txs),
        };

        let _ = block_index.do_send(block_finalized_message.clone());
        let _ = chunk_migration.do_send(block_finalized_message.clone());
        Ok(())
    }
}

//==============================================================================
// Messages and Handlers
//------------------------------------------------------------------------------

#[derive(Debug)]
pub enum ValidationResult {
    Valid,
    Invalid,
}

#[derive(Message, Debug)]
#[rtype(result = "()")]
pub struct ValidationResultMessage {
    pub block_hash: H256,
    pub validation_result: ValidationResult,
}

impl Handler<RegisterBlockProducerMessage> for BlockTreeService {
    type Result = ();
    fn handle(
        &mut self,
        msg: RegisterBlockProducerMessage,
        _ctx: &mut Context<Self>,
    ) -> Self::Result {
        self.block_producer = Some(msg.0);
    }
}

/// Handles pre-validated blocks received from the validation service.
///
/// The handling differs based on whether the block was produced locally:
/// - For locally mined blocks: Added as BlockState::Unknown to allow chain extension
///   while validation is pending
/// - For peer blocks: Added normally via add_block
///
/// After adding the block, it's scheduled for full validation and the previous
/// block is marked for storage finalization.
impl Handler<BlockPreValidatedMessage> for BlockTreeService {
    type Result = ();
    fn handle(&mut self, msg: BlockPreValidatedMessage, _ctx: &mut Context<Self>) -> Self::Result {
        let block = msg.0;
        let all_txs = msg.1;
        let block_hash = &block.block_hash;
        let finalized_block_hash = block.previous_block_hash;

        let binding = self.cache.clone().unwrap();
        let mut cache = binding.write().unwrap();

        // Handle block addition differently based on origin
        let add_result = if block.miner_address == self.miner_address {
            // For locally mined blocks: Add as `BlockState::Unknown `to allow chain
            // extension while full validation is still pending. This prevents blocking
            // new block production while validation completes.
            cache.add_validated_block((*block).clone(), BlockState::Unknown, all_txs.clone())
        } else {
            // For blocks from peers: Add via standard path requiring validation
            cache.add_block(&block, all_txs)
        };

        if add_result.is_ok() {
            // Schedule block for full validation regardless of origin
            let validation_service = ValidationService::from_registry();
            validation_service.do_send(RequestValidationMessage(block.clone()));

            // Update block state to reflect scheduled validation
            if cache
                .mark_block_as_validation_scheduled(&block_hash)
                .is_err()
            {
                error!("Unable to mark block as ValidationScheduled");
            }
            debug!(
                "scheduling block for validation: {}",
                block_hash.0.to_base58()
            );
        }
    }
}

impl Handler<ValidationResultMessage> for BlockTreeService {
    type Result = ();
    fn handle(&mut self, msg: ValidationResultMessage, _ctx: &mut Context<Self>) -> Self::Result {
        let block_hash = msg.block_hash;
        let validation_result = msg.validation_result;

        match validation_result {
            ValidationResult::Valid => {
                let binding = self.cache.clone().unwrap();
                let mut cache: std::sync::RwLockWriteGuard<'_, BlockTreeCache> =
                    binding.write().unwrap();

                debug!("validated: {}", block_hash.0.to_base58());
                if cache.mark_block_as_valid(&block_hash).is_err() {
                    error!(
                        "Unable to mark block as Validated: {}",
                        block_hash.0.to_base58()
                    );
                }
                if let Some((block_entry, _a, _system_time)) =
                    cache.get_earliest_not_onchain_in_longest_chain()
                {
                    let block_hash = block_entry.block.block_hash;
                    let arc_block = Arc::new(block_entry.block.clone());
                    let arc_all_tx = block_entry.all_tx.clone();
                    if cache.mark_tip(&block_hash).is_err() {
                        error!("Unable to mark block as tip: {}", block_hash.0.to_base58());
                    } else {
                        // If we have a new tip, let the node know about it
                        let block_confirm_message =
                            BlockConfirmedMessage(arc_block.clone(), arc_all_tx);
                        let mempool = MempoolService::from_registry();

                        if let Some(block_producer) = &self.block_producer {
                            block_producer.do_send(block_confirm_message.clone());
                        }
                        mempool.do_send(block_confirm_message);
                    }

                    // Mark the previous block for storage finalization
                    // TODO: Replace with confirmation-based finalization using storage_config.num_confirmations_for_finality
                    // We check block_height greater than one because block.height -> 0 is finalized by default
                    if arc_block.height > 1 {
                        let finalized_block_hash = arc_block.previous_block_hash;

                        if self
                            .send_storage_finalized_message(finalized_block_hash)
                            .is_err()
                        {
                            error!("Unable to send block finalized message");
                        }
                    }
                }
            }
            ValidationResult::Invalid => {
                // TODO: Potentially remove this block from the tree?
                error!("{} INVALID BLOCK", block_hash.0.to_base58());
            }
        }
    }
}

/// Fetches full transaction headers from a ledger in a block.
/// Returns None if any headers are missing or on DB errors.
fn get_ledger_tx_headers<T: DbTx>(
    tx: &T,
    block_header: &IrysBlockHeader,
    ledger: Ledger,
) -> Option<Vec<IrysTransactionHeader>> {
    match block_header.ledgers[ledger]
        .txids
        .iter()
        .map(|txid| {
            tx_header_by_txid(tx, txid)
                .map_err(|e| eyre::eyre!("Failed to get tx header: {}", e))
                .and_then(|opt| {
                    opt.ok_or_else(|| eyre::eyre!("No tx header found for txid {:?}", txid))
                })
        })
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(txs) => Some(txs),
        Err(e) => {
            error!(
                "Failed to collect tx headers for {:?} ledger: {}",
                ledger, e
            );
            None
        }
    }
}

/// Number of blocks to retain in cache from chain head
const BLOCK_CACHE_DEPTH: u64 = 50;

type ChainCacheEntry = (BlockHash, Vec<IrysTransactionId>, Vec<IrysTransactionId>); // (block_hash, publish_txs, submit_txs)

#[derive(Debug)]
pub struct BlockTreeCache {
    // Main block storage
    blocks: HashMap<BlockHash, BlockEntry>,

    // Track solutions -> block hashes
    solutions: HashMap<H256, HashSet<BlockHash>>,

    // Current tip
    tip: BlockHash,

    // Track max cumulative difficulty
    max_cumulative_difficulty: (U256, BlockHash), // (difficulty, block_hash)

    // Height -> Hash mapping
    height_index: BTreeMap<u64, HashSet<BlockHash>>,

    // Cache of longest chain: (block/tx pairs, count of non-onchain blocks)
    longest_chain_cache: (Vec<ChainCacheEntry>, usize),
}

#[derive(Debug)]
pub struct BlockEntry {
    block: IrysBlockHeader,
    all_tx: Arc<Vec<IrysTransactionHeader>>,
    chain_state: ChainState,
    timestamp: SystemTime,
    children: HashSet<H256>,
}

/// Represents the ChainState of a block, is it Onchain? or a valid fork?
#[derive(Debug, PartialEq, Clone)]
pub enum ChainState {
    /// Block is confirmed and part of the main chain
    Onchain,
    /// Block is validated but may not be on the main chain
    /// For locally produced blocks, can have ValidationScheduled `BlockState`
    /// while maintaining `ChainState` validity
    Validated(BlockState),
    /// Block exists but is awaiting block validation
    NotOnchain(BlockState),
}

/// Represents the validation state of a block, independent of its `ChainState`
#[derive(Debug, PartialEq, Clone)]
pub enum BlockState {
    /// Initial state, validation not yet started
    Unknown,
    /// Validation has been requested but not completed
    ValidationScheduled,
    /// Block has passed all validation checks
    ValidBlock,
}

impl BlockTreeCache {
    /// Create a new cache initialized with a starting block. The block is marked as
    /// on-chain and set as the tip.
    pub fn new(block: &IrysBlockHeader) -> Self {
        let block_hash = block.block_hash;
        let solution_hash = block.solution_hash;
        let height = block.height;
        let cumulative_diff = block.cumulative_diff;

        let mut blocks = HashMap::new();
        let mut solutions = HashMap::new();
        let mut height_index = BTreeMap::new();

        // No transactions to cache for genesis block - transaction data
        // has already been confirmed and stored in the permanent chain state
        let all_tx = Arc::new(vec![]);

        // Create initial block entry for genesis block, marking it as confirmed
        // and part of the canonical chain
        let block_entry = BlockEntry {
            block: block.clone(),
            all_tx: all_tx,
            chain_state: ChainState::Onchain,
            timestamp: SystemTime::now(),
            children: HashSet::new(),
        };

        // Initialize all indices
        blocks.insert(block_hash, block_entry);
        solutions.insert(solution_hash, HashSet::from([block_hash]));
        height_index.insert(height, HashSet::from([block_hash]));

        // Initialize longest chain cache with just the genesis block
        let longest_chain_cache = (
            vec![(
                block_hash,
                block.ledgers[Ledger::Publish].txids.0.clone(), // Publish ledger txs
                block.ledgers[Ledger::Submit].txids.0.clone(),  // Submit ledger txs
            )],
            0,
        );

        Self {
            blocks,
            solutions,
            tip: block_hash,
            max_cumulative_difficulty: (cumulative_diff, block_hash),
            height_index,
            longest_chain_cache,
        }
    }

    /// Initializes the cache from a list of validated blocks.
    /// The most recent block in the list is marked as the tip.
    /// The input blocks must be sorted in descending order, from newest to oldest.
    pub fn initialize_from_list(
        block_index: Arc<RwLock<BlockIndex<Initialized>>>,
        db: DatabaseProvider,
    ) -> Self {
        let block_index = block_index.read().unwrap();
        assert!(block_index.num_blocks() > 0, "Block list must not be empty");

        //block_index.print_items();

        let tx = db.tx().unwrap();

        let start = block_index
            .num_blocks()
            .saturating_sub(BLOCK_CACHE_DEPTH - 1);
        let end = block_index.num_blocks();

        // Initialize cache with the start block
        let start_block_hash = block_index.get_item(start as usize).unwrap().block_hash;
        let start_block = block_header_by_hash(&tx, &start_block_hash)
            .unwrap()
            .unwrap();
        debug!(
            "block tree start block - hash: {} height: {}",
            start_block_hash.0.to_base58(),
            start_block.height
        );
        let mut cache = Self::new(&start_block);

        // Add remaining blocks
        for block_height in (start + 1)..end {
            let block_hash = block_index
                .get_item(block_height as usize)
                .unwrap()
                .block_hash;
            let block = block_header_by_hash(&tx, &block_hash).unwrap().unwrap();
            let all_txs = Arc::new(Self::get_all_txs_for_block(&block, db.clone()).unwrap());

            cache
                .add_validated_block(block, BlockState::ValidBlock, all_txs)
                .unwrap();
        }

        let tip_hash = block_index.get_latest_item().unwrap().block_hash;
        cache.mark_tip(&tip_hash).unwrap();
        cache
    }

    fn get_all_txs_for_block(
        block: &IrysBlockHeader,
        db: DatabaseProvider,
    ) -> Result<Vec<IrysTransactionHeader>, eyre::Report> {
        // Collect submit transactions
        let submit_txs = block.ledgers[Ledger::Submit]
            .txids
            .iter()
            .map(|txid| {
                db.view_eyre(|tx| tx_header_by_txid(tx, txid))
                    .and_then(|opt| {
                        opt.ok_or_else(|| eyre::eyre!("No tx header found for txid {:?}", txid))
                    })
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                error!("Failed to collect submit tx headers: {}", e);
                e
            })?;

        // Collect publish transactions
        let publish_txs = block.ledgers[Ledger::Publish]
            .txids
            .iter()
            .map(|txid| {
                db.view_eyre(|tx| tx_header_by_txid(tx, txid))
                    .and_then(|opt| {
                        opt.ok_or_else(|| eyre::eyre!("No tx header found for txid {:?}", txid))
                    })
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                error!("Failed to collect publish tx headers: {}", e);
                e
            })?;

        // Combine transactions
        let mut all_txs = submit_txs;
        all_txs.extend(publish_txs);
        Ok(all_txs)
    }

    fn add_common(
        &mut self,
        hash: BlockHash,
        block: &IrysBlockHeader,
        all_tx: Arc<Vec<IrysTransactionHeader>>,
        status: ChainState,
    ) -> eyre::Result<()> {
        let prev_hash = block.previous_block_hash;

        // Get parent
        let prev_entry = self
            .blocks
            .get_mut(&prev_hash)
            .ok_or_else(|| eyre::eyre!("Previous block not found"))?;

        // Update indices
        prev_entry.children.insert(hash);
        self.solutions
            .entry(block.solution_hash)
            .or_default()
            .insert(hash);
        self.height_index
            .entry(block.height)
            .or_default()
            .insert(hash);

        if block.cumulative_diff > self.max_cumulative_difficulty.0 {
            self.max_cumulative_difficulty = (block.cumulative_diff, hash);
        }

        self.blocks.insert(
            hash,
            BlockEntry {
                block: block.clone(),
                all_tx: all_tx,
                chain_state: status,
                timestamp: SystemTime::now(),
                children: HashSet::new(),
            },
        );

        self.update_longest_chain_cache();
        Ok(())
    }

    pub fn add_block(
        &mut self,
        block: &IrysBlockHeader,
        all_tx: Arc<Vec<IrysTransactionHeader>>,
    ) -> eyre::Result<()> {
        let hash = block.block_hash;

        debug!(
            "adding block: {} height: {}",
            block.block_hash.0.to_base58(),
            block.height
        );

        if matches!(
            self.blocks.get(&hash).map(|b| b.chain_state.clone()),
            Some(ChainState::Onchain)
        ) {
            return Ok(());
        }
        self.add_common(
            hash,
            block,
            all_tx,
            ChainState::NotOnchain(BlockState::Unknown),
        )
    }

    /// Adds a validated block to the cache.
    ///
    /// During development, this function allows flexibility in the validation state
    /// of locally produced blocks. While blocks received from peers must have a
    /// BlockState::ValidBlock before being considered part of the longest chain,
    /// blocks produced by the local node can be added as ChainState::Validated
    /// but have have their BlockState overridden.This enables the node to
    /// continue building the chain while still performing complete validation
    /// checks, helping catch any validation errors during development without
    /// halting chain progress of the local node.
    pub fn add_validated_block(
        &mut self,
        block: IrysBlockHeader,
        block_state: BlockState,
        all_tx: Arc<Vec<IrysTransactionHeader>>,
    ) -> eyre::Result<()> {
        let hash = block.block_hash;
        let prev_hash = block.previous_block_hash;

        debug!(
            "adding validated block - hash: {} height: {}",
            block.block_hash.0.to_base58(),
            block.height,
        );

        // Verify parent is validated
        ensure!(
            !matches!(
                self.blocks.get(&prev_hash).map(|b| b.chain_state.clone()),
                Some(ChainState::NotOnchain(_))
            ),
            "Previous block not validated"
        );

        self.add_common(hash, &block, all_tx, ChainState::Validated(block_state))
    }

    /// Helper function to delete a single block without recursion
    fn delete_block(&mut self, block_hash: &BlockHash) -> eyre::Result<()> {
        let block_entry = self
            .blocks
            .get(block_hash)
            .ok_or_else(|| eyre::eyre!("Block not found"))?;

        let solution_hash = block_entry.block.solution_hash;
        let height = block_entry.block.height;
        let prev_hash = block_entry.block.previous_block_hash;

        // Update parent's children set
        if let Some(prev_entry) = self.blocks.get_mut(&prev_hash) {
            prev_entry.children.remove(block_hash);
        }

        // Update height index
        if let Some(height_set) = self.height_index.get_mut(&height) {
            height_set.remove(block_hash);
            if height_set.is_empty() {
                self.height_index.remove(&height);
            }
        }

        // Update solutions map
        if let Some(solutions) = self.solutions.get_mut(&solution_hash) {
            solutions.remove(block_hash);
            if solutions.is_empty() {
                self.solutions.remove(&solution_hash);
            }
        }

        // Remove the block
        self.blocks.remove(block_hash);

        // Update max_cumulative_difficulty if necessary
        if self.max_cumulative_difficulty.1 == *block_hash {
            self.max_cumulative_difficulty = self.find_max_difficulty();
        }

        self.update_longest_chain_cache();
        Ok(())
    }

    /// Removes a block and all its descendants recursively
    pub fn remove_block(&mut self, block_hash: &BlockHash) -> eyre::Result<()> {
        // Get children before deleting the block
        let children = self
            .blocks
            .get(block_hash)
            .map(|entry| entry.children.iter().cloned().collect::<Vec<_>>())
            .ok_or_else(|| eyre::eyre!("Block not found"))?;

        // Recursively remove all children first
        for child in children {
            self.remove_block(&child)?;
        }

        // Delete this block
        self.delete_block(block_hash)
    }

    // Helper to find new max difficulty when current max is removed
    fn find_max_difficulty(&self) -> (U256, BlockHash) {
        self.blocks
            .iter()
            .map(|(hash, entry)| (entry.block.cumulative_diff, *hash))
            .max_by_key(|(diff, _)| *diff)
            .unwrap_or((U256::zero(), BlockHash::default()))
    }

    pub fn get_canonical_chain(&self) -> (Vec<ChainCacheEntry>, usize) {
        self.longest_chain_cache.clone()
    }

    fn update_longest_chain_cache(&mut self) {
        let mut pairs = Vec::new();
        let mut not_onchain_count = 0;

        let mut current = self.max_cumulative_difficulty.1;
        let mut blocks_to_collect = BLOCK_CACHE_DEPTH;

        while let Some(entry) = self.blocks.get(&current) {
            match &entry.chain_state {
                // For blocks awaiting initial validation, restart chain from parent
                ChainState::NotOnchain(BlockState::Unknown)
                | ChainState::NotOnchain(BlockState::ValidationScheduled) => {
                    // Reset everything and continue from parent block
                    pairs.clear();
                    not_onchain_count = 0;
                    current = entry.block.previous_block_hash;
                    blocks_to_collect = BLOCK_CACHE_DEPTH;
                    continue;
                }

                ChainState::Onchain => {
                    // Include OnChain blocks in pairs
                    let publish_txs = entry.block.ledgers[Ledger::Publish].txids.0.clone();
                    let submit_txs = entry.block.ledgers[Ledger::Submit].txids.0.clone();
                    pairs.push((current, publish_txs, submit_txs));

                    if blocks_to_collect == 0 {
                        break;
                    }
                    blocks_to_collect -= 1;
                }

                // For Validated or other NotOnchain states
                ChainState::Validated(_) | ChainState::NotOnchain(_) => {
                    let publish_txs = entry.block.ledgers[Ledger::Publish].txids.0.clone();
                    let submit_txs = entry.block.ledgers[Ledger::Submit].txids.0.clone();
                    pairs.push((current, publish_txs, submit_txs));
                    not_onchain_count += 1;

                    if blocks_to_collect == 0 {
                        break;
                    }
                    blocks_to_collect -= 1;
                }
            }

            if entry.block.height == 0 {
                break;
            } else {
                current = entry.block.previous_block_hash;
            }
        }

        pairs.reverse();
        self.longest_chain_cache = (pairs, not_onchain_count);
    }

    /// Helper to mark off-chain blocks in a set
    fn mark_off_chain(&mut self, children: HashSet<H256>, current: &BlockHash) {
        for child in children {
            if child == *current {
                continue;
            }
            if let Some(entry) = self.blocks.get_mut(&child) {
                if matches!(entry.chain_state, ChainState::Onchain) {
                    entry.chain_state = ChainState::Validated(BlockState::ValidBlock);
                    // Recursively mark children of this block
                    let children = entry.children.clone();
                    self.mark_off_chain(children, current);
                }
            }
        }
    }

    /// Helper to recursively mark blocks on-chain
    fn mark_on_chain(&mut self, block: &IrysBlockHeader) -> eyre::Result<()> {
        let prev_hash = block.previous_block_hash;

        match self.blocks.get(&prev_hash) {
            None => Ok(()), // Reached the end
            Some(prev_entry) => {
                let prev_block = prev_entry.block.clone();
                let prev_children = prev_entry.children.clone();

                match prev_entry.chain_state {
                    ChainState::NotOnchain(_) => Err(eyre::eyre!("invalid_tip")),
                    ChainState::Onchain => {
                        // Mark other branches as validated
                        self.mark_off_chain(prev_children, &block.block_hash);
                        Ok(())
                    }
                    ChainState::Validated(_) => {
                        // Update previous block to on_chain
                        if let Some(entry) = self.blocks.get_mut(&prev_hash) {
                            entry.chain_state = ChainState::Onchain;
                        }
                        // Recursively mark previous blocks
                        self.mark_on_chain(&prev_block)
                    }
                }
            }
        }
    }

    /// Marks a block as the new tip
    pub fn mark_tip(&mut self, block_hash: &BlockHash) -> eyre::Result<bool> {
        // Get the current block
        let block_entry = self
            .blocks
            .get(block_hash)
            .ok_or_else(|| eyre::eyre!("Block not found in cache"))?;

        let block = block_entry.block.clone();
        let old_tip = self.tip;

        // Recursively mark previous blocks
        self.mark_on_chain(&block)?;

        // Mark the tip block as on_chain
        if let Some(entry) = self.blocks.get_mut(block_hash) {
            entry.chain_state = ChainState::Onchain;
        }

        self.tip = *block_hash;
        self.update_longest_chain_cache();

        debug!("new tip: {}", block_hash.0.to_base58());

        Ok(old_tip != *block_hash)
    }

    pub fn mark_block_as_validation_scheduled(
        &mut self,
        block_hash: &BlockHash,
    ) -> eyre::Result<()> {
        if let Some(entry) = self.blocks.get_mut(block_hash) {
            if let ChainState::NotOnchain(BlockState::Unknown) = entry.chain_state {
                entry.chain_state = ChainState::NotOnchain(BlockState::ValidationScheduled);
                self.update_longest_chain_cache();
            } else if let ChainState::Validated(BlockState::Unknown) = entry.chain_state {
                entry.chain_state = ChainState::Validated(BlockState::ValidationScheduled);
                self.update_longest_chain_cache();
            }
        }
        Ok(())
    }

    pub fn mark_block_as_valid(&mut self, block_hash: &BlockHash) -> eyre::Result<()> {
        if let Some(entry) = self.blocks.get_mut(block_hash) {
            if let ChainState::NotOnchain(BlockState::ValidationScheduled) = entry.chain_state {
                entry.chain_state = ChainState::NotOnchain(BlockState::ValidBlock);
                self.update_longest_chain_cache();
            }
        }
        Ok(())
    }

    /// Gets block by hash
    pub fn get_block(&self, block_hash: &BlockHash) -> Option<&IrysBlockHeader> {
        self.blocks.get(block_hash).map(|entry| &entry.block)
    }

    /// Gets block and its current validation status    
    pub fn get_block_and_status(
        &self,
        block_hash: &BlockHash,
    ) -> Option<(&IrysBlockHeader, &ChainState)> {
        self.blocks
            .get(block_hash)
            .map(|entry| (&entry.block, &entry.chain_state))
    }

    /// Collect previous blocks up to the last on-chain block
    fn get_fork_blocks(&self, block: &IrysBlockHeader) -> Vec<&IrysBlockHeader> {
        let mut prev_hash = block.previous_block_hash;
        let mut fork_blocks = Vec::new();

        while let Some(prev_entry) = self.blocks.get(&prev_hash) {
            match prev_entry.chain_state {
                ChainState::Onchain => {
                    fork_blocks.push(&prev_entry.block);
                    break;
                }
                ChainState::Validated(_) | ChainState::NotOnchain(_) => {
                    fork_blocks.push(&prev_entry.block);
                    prev_hash = prev_entry.block.previous_block_hash;
                }
            }
        }

        fork_blocks
    }

    /// Finds the earliest not validated block, walking back the chain
    /// until finding a validated block, reaching block height 0, or exceeding cache depth
    pub fn get_earliest_not_onchain<'a>(
        &'a self,
        block: &'a BlockEntry,
    ) -> Option<(&'a BlockEntry, Vec<&'a IrysBlockHeader>, SystemTime)> {
        let mut current_entry = block;
        let mut prev_block = &current_entry.block;
        let mut depth_count = 0;

        while prev_block.height > 0 && depth_count < BLOCK_CACHE_DEPTH {
            let prev_hash = prev_block.previous_block_hash;
            let prev_entry = self.blocks.get(&prev_hash)?;
            match prev_entry.chain_state {
                ChainState::Validated(BlockState::ValidBlock) | ChainState::Onchain => {
                    return Some((
                        current_entry,
                        self.get_fork_blocks(prev_block),
                        current_entry.timestamp,
                    ));
                }
                ChainState::NotOnchain(_) | ChainState::Validated(_) => {
                    current_entry = prev_entry;
                    prev_block = &current_entry.block;
                    depth_count += 1;
                }
            }
        }

        // If we've reached height 0 or exceeded cache depth, return None
        None
    }

    /// Get the earliest unvalidated block from the longest chain
    /// Relies on the longest_chain_cache
    pub fn get_earliest_not_onchain_in_longest_chain(
        &self,
    ) -> Option<(&BlockEntry, Vec<&IrysBlockHeader>, SystemTime)> {
        // Get the block with max cumulative difficulty
        let (_, max_diff_hash) = self.max_cumulative_difficulty;

        // Get the tip's cumulative difficulty
        let tip_entry = self.blocks.get(&self.tip)?;
        let tip_cdiff = tip_entry.block.cumulative_diff;

        // Check if tip's difficulty exceeds max difficulty
        if tip_cdiff >= self.max_cumulative_difficulty.0 {
            return None;
        }

        // Get the block with max difficulty
        let entry = self.blocks.get(&max_diff_hash)?;

        // Check if it's not validated
        if let ChainState::NotOnchain(_) | ChainState::Validated(BlockState::ValidationScheduled) =
            &entry.chain_state
        {
            // Get earliest not validated walking back the chain
            self.get_earliest_not_onchain(entry)
        } else {
            None
        }
    }

    /// Gets block with matching solution hash, excluding specified block.
    /// Returns a block meeting these requirements:
    /// - Has matching solution_hash
    /// - Is not the excluded block
    /// - Either has same cumulative_diff as input or meets double-signing criteria
    pub fn get_by_solution_hash(
        &self,
        solution_hash: &H256,
        excluding: &BlockHash,
        cumulative_difficulty: U256,
        previous_cumulative_difficulty: U256,
    ) -> Option<&IrysBlockHeader> {
        // Get set of blocks with this solution hash
        let block_hashes = self.solutions.get(solution_hash)?;

        let mut best_block = None;

        // Examine each block hash
        for &hash in block_hashes {
            // Skip the excluded block
            if hash == *excluding {
                continue;
            }

            if let Some(entry) = self.blocks.get(&hash) {
                let block = &entry.block;

                // Case 1: Exact cumulative_diff match - return immediately
                if block.cumulative_diff == cumulative_difficulty {
                    return Some(block);
                }

                // Case 2: Double signing case - return immediately
                if block.cumulative_diff > previous_cumulative_difficulty
                    && cumulative_difficulty > block.previous_cumulative_diff
                {
                    return Some(block);
                }

                // Store as best block seen so far if we haven't found one yet
                if best_block.is_none() {
                    best_block = Some(block);
                }
            }
        }

        // Return best block found (if any)
        best_block
    }

    /// Prunes blocks below specified depth from tip. When pruning an on-chain block,
    /// removes all its non-on-chain children regardless of their height.
    pub fn prune(&mut self, depth: u64) -> eyre::Result<()> {
        if self.blocks.is_empty() {
            return Ok(());
        }

        let tip_height = self
            .blocks
            .get(&self.tip)
            .ok_or_else(|| eyre::eyre!("Tip block not found"))?
            .block
            .height;
        let min_keep_height = tip_height.saturating_sub(depth);

        let min_height = match self.height_index.keys().min() {
            Some(&h) => h,
            None => return Ok(()),
        };

        let mut current_height = min_height;
        while current_height < min_keep_height {
            let Some(hashes) = self.height_index.get(&current_height) else {
                current_height += 1;
                continue;
            };

            // Clone hashes to avoid borrow issues during removal
            let hashes: Vec<_> = hashes.iter().cloned().collect();

            for hash in hashes {
                if let Some(entry) = self.blocks.get(&hash) {
                    if matches!(entry.chain_state, ChainState::Onchain) {
                        // First remove all non-on-chain children
                        let children = entry.children.clone();
                        for child in children {
                            if let Some(child_entry) = self.blocks.get(&child) {
                                if !matches!(child_entry.chain_state, ChainState::Onchain) {
                                    self.remove_block(&child)?;
                                }
                            }
                        }

                        // Now remove just this block
                        self.delete_block(&hash)?;
                    }
                }
            }

            current_height += 1;
        }

        Ok(())
    }

    /// Returns true if solution hash exists in cache
    pub fn is_known_solution_hash(&self, solution_hash: &H256) -> bool {
        self.solutions.contains_key(solution_hash)
    }
}

//==============================================================================
// Tests
//------------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use eyre::ensure;
    use irys_database::Ledger;

    #[actix::test]
    async fn test_block_cache() {
        let b1 = random_block(U256::from(0));

        // For the purposes of these tests, the block cache will not track transaction headers
        let all_tx = Arc::new(vec![]);

        // Initialize block tree cache from `b1`
        let mut cache = BlockTreeCache::new(&b1);

        // Verify cache returns `None` for unknown hashes
        assert_eq!(cache.get_block(&H256::random()), None);
        assert_eq!(
            cache.get_by_solution_hash(&H256::random(), &H256::random(), U256::one(), U256::one()),
            None
        );

        // Verify cache returns the expected block
        assert_eq!(cache.get_block(&b1.block_hash), Some(&b1));
        assert_eq!(
            cache.get_by_solution_hash(
                &b1.solution_hash,
                &H256::random(),
                U256::one(),
                U256::one()
            ),
            Some(&b1)
        );

        // Verify getting by `solution_hash` excludes the expected block
        assert_matches!(
            cache.get_by_solution_hash(&b1.solution_hash, &b1.block_hash, U256::one(), U256::one()),
            None
        );

        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(_));

        // Adding `b1` again shouldn't change the state because it is confirmed
        // onchain
        let mut b1_test = b1.clone();
        b1_test.ledgers[Ledger::Submit].txids.push(H256::random());
        assert_matches!(cache.add_block(&b1_test, all_tx.clone()), Ok(_));
        assert_eq!(
            cache.get_block(&b1.block_hash).unwrap().ledgers[Ledger::Submit]
                .txids
                .len(),
            0
        );
        assert_eq!(
            cache
                .get_by_solution_hash(&b1.solution_hash, &H256::random(), U256::one(), U256::one())
                .unwrap()
                .ledgers[Ledger::Submit]
                .txids
                .len(),
            0
        );
        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(_));

        // Same as above, `get_deepest_unvalidated_in_longest_chain` should not
        // modify state
        assert_matches!(cache.get_earliest_not_onchain_in_longest_chain(), None);
        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(_));

        // Add b2 block as not_validated
        let mut b2 = extend_chain(random_block(U256::from(1)), &b1);
        assert_matches!(cache.add_block(&b2, all_tx.clone()), Ok(_));
        assert_eq!(
            cache
                .get_earliest_not_onchain_in_longest_chain()
                .unwrap()
                .0
                .block
                .block_hash,
            b2.block_hash
        );

        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(_));

        // Add a TXID to b2, and re-add it to the cache, but still don't mark as validated
        let txid = H256::random();
        b2.ledgers[Ledger::Submit].txids.push(txid.clone());
        assert_matches!(cache.add_block(&b2, all_tx.clone()), Ok(_));
        assert_eq!(
            cache.get_block(&b2.block_hash).unwrap().ledgers[Ledger::Submit].txids[0],
            txid
        );
        assert_eq!(
            cache
                .get_by_solution_hash(&b2.solution_hash, &H256::random(), U256::one(), U256::one())
                .unwrap()
                .ledgers[Ledger::Submit]
                .txids[0],
            txid
        );
        assert_eq!(
            cache
                .get_by_solution_hash(&b2.solution_hash, &b1.block_hash, U256::one(), U256::one())
                .unwrap()
                .ledgers[Ledger::Submit]
                .txids[0],
            txid
        );

        // Remove b2_1
        assert_matches!(cache.remove_block(&b2.block_hash), Ok(_));
        assert_eq!(cache.get_block(&b2.block_hash), None);
        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(_));

        // Remove b2_1 again
        assert_matches!(cache.remove_block(&b2.block_hash), Err(_));

        // Re-add b2_1 and add a competing b2 block called b1_2, it will be built
        // on b1 but share the same solution_hash
        assert_matches!(cache.add_block(&b2, all_tx.clone()), Ok(_));
        let mut b1_2 = extend_chain(random_block(U256::from(2)), &b1);
        b1_2.solution_hash = b1.solution_hash;
        assert_matches!(cache.add_block(&b1_2, all_tx.clone()), Ok(_));

        println!(
            "b1:   {} cdiff: {} solution_hash: {}",
            b1.block_hash, b1.cumulative_diff, b1.solution_hash
        );
        println!(
            "b2:   {} cdiff: {} solution_hash: {}",
            b2.block_hash, b2.cumulative_diff, b2.solution_hash
        );
        println!(
            "b1_2: {} cdiff: {} solution_hash: {}",
            b1_2.block_hash, b1_2.cumulative_diff, b1_2.solution_hash
        );

        // Verify if we exclude b1_2 we wont get it back
        assert_eq!(
            cache
                .get_by_solution_hash(
                    &b1.solution_hash,
                    &b1_2.block_hash,
                    U256::one(),
                    U256::one()
                )
                .unwrap()
                .block_hash,
            b1.block_hash
        );

        // Verify that we do get b1_2 back when not excluding it
        assert_eq!(
            cache
                .get_by_solution_hash(
                    &b1.solution_hash,
                    &BlockHash::random(),
                    b1_2.cumulative_diff,
                    U256::one()
                )
                .unwrap()
                .block_hash,
            b1_2.block_hash
        );

        // Get result with empty excluded hash
        let result = cache
            .get_by_solution_hash(
                &b1.solution_hash,
                &BlockHash::default(), // Empty/zeroed hash
                U256::one(),
                U256::one(),
            )
            .unwrap();

        // Assert result is either b1 or b1_2
        assert!(
            result.block_hash == b1.block_hash || result.block_hash == b1_2.block_hash,
            "Expected either b1 or b1_2 to be returned"
        );
        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(_));

        // Even though b2 is marked as a tip, it is still lower difficulty than B1_2 so will
        // not be included in the longest chain
        assert_matches!(cache.mark_tip(&b2.block_hash), Ok(_));
        assert_eq!(Some(&b1_2), cache.get_block(&b1_2.block_hash));
        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(_));

        // Remove b1_2, causing b2 to now be the tip of the heaviest chain
        assert_matches!(cache.remove_block(&b1_2.block_hash), Ok(_));
        assert_eq!(cache.get_block(&b1_2.block_hash), None);
        assert_eq!(
            cache
                .get_by_solution_hash(
                    &b1.solution_hash,
                    &BlockHash::random(),
                    U256::zero(),
                    U256::zero()
                )
                .unwrap()
                .block_hash,
            b1.block_hash
        );
        assert_matches!(check_longest_chain(&[&b1, &b2], 0, &cache), Ok(_));

        // Prune to a depth of 1 behind the tip
        assert_matches!(cache.prune(1), Ok(_));
        assert_eq!(Some(&b1), cache.get_block(&b1.block_hash));
        assert_eq!(
            cache
                .get_by_solution_hash(
                    &b1.solution_hash,
                    &BlockHash::random(),
                    U256::zero(),
                    U256::zero()
                )
                .unwrap()
                .block_hash,
            b1.block_hash
        );
        assert_matches!(check_longest_chain(&[&b1, &b2], 0, &cache), Ok(_));

        // Prune at the tip, removing all ancestors (verify b1 is pruned)
        assert_matches!(cache.prune(0), Ok(_));
        assert_eq!(None, cache.get_block(&b1.block_hash));
        assert_eq!(
            None,
            cache.get_by_solution_hash(
                &b1.solution_hash,
                &BlockHash::random(),
                U256::zero(),
                U256::zero()
            )
        );
        assert_matches!(check_longest_chain(&[&b2], 0, &cache), Ok(_));

        // Again, this time to make sure b1_2 is really gone
        assert_matches!(cache.prune(0), Ok(_));
        assert_eq!(None, cache.get_block(&b1_2.block_hash));
        assert_eq!(
            None,
            cache.get_by_solution_hash(
                &b1_2.solution_hash,
                &BlockHash::random(),
                U256::zero(),
                U256::zero()
            )
        );
        assert_matches!(check_longest_chain(&[&b2], 0, &cache), Ok(_));

        // <Reset the cache>
        // b1_2->b1 fork is the heaviest, but only b1 is validated. b2_2->b2->b1 is longer but
        // has a lower cdiff.
        let mut cache = BlockTreeCache::new(&b1);
        assert_matches!(cache.add_block(&b1_2, all_tx.clone()), Ok(_));
        assert_matches!(cache.add_block(&b2, all_tx.clone()), Ok(_));
        assert_matches!(cache.mark_tip(&b2.block_hash), Ok(_));
        let b2_2 = extend_chain(random_block(U256::one()), &b2);
        println!(
            "b2_2: {} cdiff: {} solution_hash: {}",
            b2_2.block_hash, b2_2.cumulative_diff, b2_2.solution_hash
        );
        assert_matches!(cache.add_block(&b2_2, all_tx.clone()), Ok(_));
        assert_eq!(
            cache
                .get_earliest_not_onchain_in_longest_chain()
                .unwrap()
                .0
                .block
                .block_hash,
            b1_2.block_hash
        );

        // b2_3->b2_2->b2->b1 is longer and heavier but only b2->b1 are validated.
        let b2_3 = extend_chain(random_block(U256::from(3)), &b2_2);
        println!(
            "b2_3: {} cdiff: {} solution_hash: {}",
            b2_3.block_hash, b2_3.cumulative_diff, b2_3.solution_hash
        );
        assert_matches!(cache.add_block(&b2_3, all_tx.clone()), Ok(_));
        assert_eq!(
            cache
                .get_earliest_not_onchain_in_longest_chain()
                .unwrap()
                .0
                .block
                .block_hash,
            b2_2.block_hash
        );
        assert_matches!(cache.mark_tip(&b2_3.block_hash), Err(_));
        assert_matches!(check_longest_chain(&[&b1, &b2], 0, &cache), Ok(_));

        // Now b2_2->b2->b1 are validated.
        assert_matches!(
            cache.add_validated_block(b2_2.clone(), BlockState::ValidBlock, all_tx.clone()),
            Ok(_)
        );
        assert_eq!(
            cache.get_block_and_status(&b2_2.block_hash).unwrap(),
            (&b2_2, &ChainState::Validated(BlockState::ValidBlock))
        );
        assert_eq!(
            cache
                .get_earliest_not_onchain_in_longest_chain()
                .unwrap()
                .0
                .block
                .block_hash,
            b2_3.block_hash
        );
        assert_matches!(check_longest_chain(&[&b1, &b2, &b2_2], 1, &cache), Ok(_));

        // Now the b3->b2->b1 fork is heaviest
        let b3 = extend_chain(random_block(U256::from(4)), &b2);
        println!(
            "b3:   {} cdiff: {} solution_hash: {}",
            b3.block_hash, b3.cumulative_diff, b3.solution_hash
        );
        assert_matches!(cache.add_block(&b3, all_tx.clone()), Ok(_));
        assert_matches!(
            cache.add_validated_block(b3.clone(), BlockState::ValidBlock, all_tx.clone()),
            Ok(_)
        );
        assert_matches!(cache.mark_tip(&b3.block_hash), Ok(_));
        assert_matches!(cache.get_earliest_not_onchain_in_longest_chain(), None);
        assert_matches!(check_longest_chain(&[&b1, &b2, &b3], 0, &cache), Ok(_));

        // b3->b2->b1 fork is still heaviest
        assert_matches!(cache.mark_tip(&b2_2.block_hash), Ok(_));
        assert_matches!(cache.get_earliest_not_onchain_in_longest_chain(), None);
        assert_matches!(check_longest_chain(&[&b1, &b2, &b3], 1, &cache), Ok(_));

        // add not validated b4, b3->b2->b1 fork is still heaviest
        let b4 = extend_chain(random_block(U256::from(5)), &b3);
        println!(
            "b4:   {} cdiff: {} solution_hash: {}",
            b4.block_hash, b4.cumulative_diff, b4.solution_hash
        );
        assert_matches!(cache.add_block(&b4, all_tx.clone()), Ok(_));
        assert_eq!(
            cache
                .get_earliest_not_onchain_in_longest_chain()
                .unwrap()
                .0
                .block
                .block_hash,
            b4.block_hash
        );
        assert_matches!(check_longest_chain(&[&b1, &b2, &b3], 1, &cache), Ok(_));

        // Prune to a depth of 1 past the tip and verify b1 and the b1_2 branch are pruned
        assert_matches!(cache.prune(1), Ok(_));
        assert_eq!(None, cache.get_block(&b1.block_hash));
        assert_eq!(
            None,
            cache.get_by_solution_hash(
                &b1.solution_hash,
                &BlockHash::random(),
                U256::zero(),
                U256::zero()
            )
        );
        assert_matches!(check_longest_chain(&[&b2, &b3], 1, &cache), Ok(_));

        // Mark a new tip for the longest chain, validating b2_3, and pruning again
        assert_matches!(cache.mark_tip(&b2_3.block_hash), Ok(_));
        assert_matches!(cache.prune(1), Ok(_));
        assert_eq!(None, cache.get_block(&b2.block_hash));
        assert_eq!(
            None,
            cache.get_by_solution_hash(
                &b2.solution_hash,
                &BlockHash::random(),
                U256::zero(),
                U256::zero()
            )
        );
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(_));

        // Also make sure b3 (the old tip) got pruned
        assert_matches!(cache.prune(1), Ok(_));
        assert_eq!(None, cache.get_block(&b3.block_hash));
        assert_eq!(
            None,
            cache.get_by_solution_hash(
                &b3.solution_hash,
                &BlockHash::random(),
                U256::zero(),
                U256::zero()
            )
        );
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(_));

        // and that the not yet validated b4 was also pruned
        assert_matches!(cache.prune(1), Ok(_));
        assert_eq!(None, cache.get_block(&b4.block_hash));
        assert_eq!(
            None,
            cache.get_by_solution_hash(
                &b4.solution_hash,
                &BlockHash::random(),
                U256::zero(),
                U256::zero()
            )
        );
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(_));

        // Now make sure b2_2 and b2_3 are still in the cache/state
        assert_matches!(cache.prune(1), Ok(_));
        assert_eq!(Some(&b2_2), cache.get_block(&b2_2.block_hash));
        assert_eq!(
            cache
                .get_by_solution_hash(
                    &b2_2.solution_hash,
                    &BlockHash::random(),
                    U256::zero(),
                    U256::zero()
                )
                .unwrap()
                .block_hash,
            b2_2.block_hash
        );
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(_));

        assert_matches!(cache.prune(1), Ok(_));
        assert_eq!(Some(&b2_3), cache.get_block(&b2_3.block_hash));
        assert_eq!(
            cache
                .get_by_solution_hash(
                    &b2_3.solution_hash,
                    &BlockHash::random(),
                    U256::zero(),
                    U256::zero()
                )
                .unwrap()
                .block_hash,
            b2_3.block_hash
        );
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(_));

        // Verify previously pruned b3 can be safely removed again, with longest chain cache staying stable
        assert_matches!(cache.remove_block(&b3.block_hash), Err(e) if e.to_string() == "Block not found");
        assert_eq!(None, cache.get_block(&b3.block_hash));
        assert_eq!(
            None,
            cache.get_by_solution_hash(
                &b3.solution_hash,
                &BlockHash::random(),
                U256::zero(),
                U256::zero()
            )
        );
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(_));

        // Same safety check for b4 - removing already pruned block shouldn't affect chain state
        assert_matches!(cache.remove_block(&b4.block_hash), Err(e) if e.to_string() == "Block not found");
        assert_eq!(None, cache.get_block(&b4.block_hash));
        assert_eq!(
            None,
            cache.get_by_solution_hash(
                &b4.solution_hash,
                &BlockHash::random(),
                U256::zero(),
                U256::zero()
            )
        );
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(_));

        // <Reset the cache>
        let b11 = random_block(U256::zero());
        let mut cache = BlockTreeCache::new(&b11);
        let b12 = extend_chain(random_block(U256::one()), &b11);
        assert_matches!(cache.add_block(&b12, all_tx.clone()), Ok(_));
        let b13 = extend_chain(random_block(U256::one()), &b11);

        println!("---");
        println!(
            "b11: {} cdiff: {} solution_hash: {}",
            b11.block_hash, b11.cumulative_diff, b11.solution_hash
        );
        println!(
            "b12: {} cdiff: {} solution_hash: {}",
            b12.block_hash, b12.cumulative_diff, b12.solution_hash
        );
        println!(
            "b13: {} cdiff: {} solution_hash: {}",
            b13.block_hash, b13.cumulative_diff, b13.solution_hash
        );
        println!("tip: {} before mark_tip()", cache.tip);

        assert_matches!(
            cache.add_validated_block(b13.clone(), BlockState::ValidBlock, all_tx.clone()),
            Ok(_)
        );
        let reorg = cache.mark_tip(&b13.block_hash).unwrap();

        // The tip does change here, even though it's not part of the longest
        // chain, this seems like a bug
        println!("tip: {} after mark_tip()", cache.tip);
        assert!(reorg);

        assert_matches!(cache.get_earliest_not_onchain_in_longest_chain(), None);
        // Although b13 becomes the tip, it's not included in the longest_chain_cache.
        // This is because the cache follows blocks from max_cumulative_difficulty, which
        // was set to b12 when it was first added. When multiple blocks have the same
        // cumulative difficulty, max_cumulative_difficulty preserves the first one seen.
        // Since b13's difficulty equals b12's (rather than exceeds it), b12 remains the
        // reference point for longest chain calculations.

        // Block tree state:
        //
        //                     [B13] cdiff=1, Validated
        //                    /  ⚡ marked as tip but not in longest chain
        //                   /     because B12 has same cdiff & was first
        //                  /
        // [B11] cdiff=0 --+-- [B12] cdiff=1, NotValidated (first added)
        // (genesis)             ⚠ not counted as onchain due to not being validated
        //
        // ▶ Longest chain contains: [B11]
        // ▶ Not on chain count: 0
        // ▶ First added wins longest_chain with equal cdiff

        // DMac's Note:
        // Issue: tip and longest chain can become misaligned when marking a tip that has
        //   equal difficulty to an earlier block. The tip could point to b13 while the
        //   longest chain contains b12, as b12 was seen first with same difficulty.
        // Fix: mark_tip() should reject attempts to change tip to a block that has equal
        //   (rather than greater) difficulty compared to the current max_difficulty block.
        //   This would ensure tip always follows the longest chain.
        assert_matches!(check_longest_chain(&[&b11], 0, &cache), Ok(_));

        // Extend the b13->b11 chain
        let b14 = extend_chain(random_block(U256::from(2)), &b13);
        assert_matches!(cache.add_block(&b14, all_tx.clone()), Ok(_));
        assert_eq!(
            cache
                .get_earliest_not_onchain_in_longest_chain()
                .unwrap()
                .0
                .block
                .block_hash,
            b14.block_hash
        );
        // by adding b14 we've now made the b13->b11 chain heavier and because
        // b13 is already validated it is included in the longest chain
        // b14 isn't validated so it doesn't count towards the not_onchain_count
        assert_matches!(check_longest_chain(&[&b11, &b13], 0, &cache), Ok(_));

        // Try to mutate the state of the cache with some validations
        assert_matches!(
            cache.mark_block_as_validation_scheduled(&BlockHash::random()),
            Ok(_)
        );
        assert_matches!(cache.mark_block_as_valid(&BlockHash::random()), Ok(_));
        // Attempt to mark the already onchain b13 to prior vdf states
        assert_matches!(
            cache.mark_block_as_validation_scheduled(&b13.block_hash),
            Ok(_)
        );
        assert_matches!(cache.mark_block_as_valid(&b13.block_hash), Ok(_));
        // Verify its state wasn't changed
        assert_eq!(
            cache.get_block_and_status(&b13.block_hash).unwrap(),
            (&b13, &ChainState::Onchain)
        );
        assert_eq!(
            cache.get_block_and_status(&b14.block_hash).unwrap(),
            (&b14, &ChainState::NotOnchain(BlockState::Unknown))
        );
        // Verify none of this affected the longest chain cache
        assert_matches!(check_longest_chain(&[&b11, &b13], 0, &cache), Ok(_));

        // Move b14 though the vdf validation states
        assert_matches!(
            cache.mark_block_as_validation_scheduled(&b14.block_hash),
            Ok(_)
        );
        assert_eq!(
            cache.get_block_and_status(&b14.block_hash).unwrap(),
            (
                &b14,
                &ChainState::NotOnchain(BlockState::ValidationScheduled)
            )
        );
        assert_matches!(
            check_earliest_not_validated(
                &b14.block_hash,
                &ChainState::NotOnchain(BlockState::ValidationScheduled),
                &cache
            ),
            Ok(_)
        );
        // Verify none of this affected the longest chain cache
        assert_matches!(check_longest_chain(&[&b11, &b13], 0, &cache), Ok(_));

        // now mark b14 as vdf validated
        assert_matches!(cache.mark_block_as_valid(&b14.block_hash), Ok(_));
        assert_eq!(
            cache.get_block_and_status(&b14.block_hash).unwrap(),
            (&b14, &ChainState::NotOnchain(BlockState::ValidBlock))
        );
        assert_matches!(
            check_earliest_not_validated(
                &b14.block_hash,
                &ChainState::NotOnchain(BlockState::ValidBlock),
                &cache
            ),
            Ok(_)
        );
        // Now that b14 is vdf validated it can be considered a NotOnchain
        // part of the longest chain
        assert_matches!(check_longest_chain(&[&b11, &b13, &b14], 1, &cache), Ok(_));

        // add a b15 block
        let b15 = extend_chain(random_block(U256::from(3)), &b14);
        assert_matches!(cache.add_block(&b15, all_tx.clone()), Ok(_));
        assert_matches!(
            check_earliest_not_validated(
                &b14.block_hash,
                &ChainState::NotOnchain(BlockState::ValidBlock),
                &cache
            ),
            Ok(_)
        );
        assert_matches!(check_longest_chain(&[&b11, &b13, &b14], 1, &cache), Ok(_));

        // Validate b14
        assert_matches!(
            cache.add_validated_block(b14.clone(), BlockState::ValidBlock, all_tx.clone()),
            Ok(_)
        );
        assert_matches!(
            check_earliest_not_validated(
                &b15.block_hash,
                &ChainState::NotOnchain(BlockState::Unknown),
                &cache
            ),
            Ok(_)
        );
        assert_eq!(
            cache.get_block_and_status(&b14.block_hash).unwrap(),
            (&b14, &ChainState::Validated(BlockState::ValidBlock))
        );
        // b14 is validated, but wont be onchain until is tip_height or lower
        assert_matches!(check_longest_chain(&[&b11, &b13, &b14], 1, &cache), Ok(_));

        // add a b16 block
        let b16 = extend_chain(random_block(U256::from(4)), &b15);
        assert_matches!(cache.add_block(&b16, all_tx.clone()), Ok(_));
        assert_matches!(
            cache.mark_block_as_validation_scheduled(&b16.block_hash),
            Ok(_)
        );
        assert_matches!(
            check_earliest_not_validated(
                &b15.block_hash,
                &ChainState::NotOnchain(BlockState::Unknown),
                &cache
            ),
            Ok(_)
        );
        // Verify the longest chain state isn't changed by b16 pending Vdf validation
        assert_matches!(check_longest_chain(&[&b11, &b13, &b14], 1, &cache), Ok(_));

        // Mark b16 as vdf validated eve though b15 is not
        assert_matches!(cache.mark_block_as_valid(&b16.block_hash), Ok(_));
        assert_matches!(
            check_earliest_not_validated(
                &b15.block_hash,
                &ChainState::NotOnchain(BlockState::Unknown),
                &cache
            ),
            Ok(_)
        );
        assert_eq!(
            cache.get_block_and_status(&b16.block_hash).unwrap(),
            (&b16, &ChainState::NotOnchain(BlockState::ValidBlock))
        );
        assert_matches!(check_longest_chain(&[&b11, &b13, &b14], 1, &cache), Ok(_));

        // Make b14 the tip (making it OnChain)
        assert_matches!(cache.mark_tip(&b14.block_hash), Ok(_));
        assert_eq!(
            cache.get_block_and_status(&b14.block_hash).unwrap(),
            (&b14, &ChainState::Onchain)
        );
        assert_matches!(
            check_earliest_not_validated(
                &b15.block_hash,
                &ChainState::NotOnchain(BlockState::Unknown),
                &cache
            ),
            Ok(_)
        );
        assert_matches!(check_longest_chain(&[&b11, &b13, &b14], 0, &cache), Ok(_));

        // <Reset the cache>
        let b11 = random_block(U256::zero());
        let mut cache = BlockTreeCache::new(&b11);
        let b12 = extend_chain(random_block(U256::one()), &b11);
        assert_matches!(cache.add_block(&b12, all_tx.clone()), Ok(_));
        let b13 = extend_chain(random_block(U256::one()), &b11);
        println!("---");
        assert_matches!(cache.mark_tip(&b11.block_hash), Ok(_));

        // Verify the longest chain state isn't changed by b16 pending Vdf validation
        assert_matches!(check_longest_chain(&[&b11], 0, &cache), Ok(_));

        // Now add the subsequent block, but as awaitingValidation
        assert_matches!(
            cache.add_validated_block(b12.clone(), BlockState::ValidationScheduled, all_tx.clone()),
            Ok(())
        );
        assert_matches!(check_longest_chain(&[&b11, &b12], 1, &cache), Ok(_));

        // When a locally produced block is added as validated "onchain" but it
        // hasn't yet been validated by the validation_service
        assert_matches!(
            check_earliest_not_validated(
                &b12.block_hash,
                &ChainState::Validated(BlockState::ValidationScheduled),
                &cache
            ),
            Ok(_)
        );
    }

    fn random_block(cumulative_diff: U256) -> IrysBlockHeader {
        let mut block = IrysBlockHeader::new();
        block.block_hash = BlockHash::random();
        block.solution_hash = H256::random(); // Ensure unique solution hash
        block.height = 0; // Default to genesis
        block.cumulative_diff = cumulative_diff;
        block
    }

    fn extend_chain(
        mut new_block: IrysBlockHeader,
        previous_block: &IrysBlockHeader,
    ) -> IrysBlockHeader {
        new_block.previous_block_hash = previous_block.block_hash;
        new_block.height = previous_block.height + 1;
        new_block.previous_cumulative_diff = previous_block.cumulative_diff;
        // Don't modify solution_hash - keep the random one from block creation
        new_block
    }

    fn check_earliest_not_validated(
        block_hash: &BlockHash,
        chain_state: &ChainState,
        cache: &BlockTreeCache,
    ) -> eyre::Result<()> {
        if let Some((block_entry, _, _)) = cache.get_earliest_not_onchain_in_longest_chain() {
            let c_s = &block_entry.chain_state;

            ensure!(
                block_entry.block.block_hash == *block_hash,
                "Wrong unvalidated block found: {} expected:{}",
                block_entry.block.block_hash,
                block_hash
            );

            ensure!(
                chain_state == c_s,
                "Wrong validation_state found: {:?}",
                c_s
            );
        } else {
            if let ChainState::NotOnchain(_) = chain_state {
                return Err(eyre::eyre!("No unvalidated blocks found in longest chain"));
            }
        }

        Ok(())
    }

    fn check_longest_chain(
        expected_blocks: &[&IrysBlockHeader],
        expected_not_onchain: usize,
        cache: &BlockTreeCache,
    ) -> eyre::Result<()> {
        let (canonical_blocks, not_onchain_count) = cache.get_canonical_chain();
        let actual_blocks: Vec<_> = canonical_blocks.iter().map(|(hash, _, _)| *hash).collect();

        ensure!(
            actual_blocks
                == expected_blocks
                    .iter()
                    .map(|b| b.block_hash)
                    .collect::<Vec<_>>(),
            "Canonical chain does not match expected blocks"
        );
        ensure!(
            not_onchain_count == expected_not_onchain,
            "Number of not-onchain blocks does not match expected"
        );
        Ok(())
    }
}
