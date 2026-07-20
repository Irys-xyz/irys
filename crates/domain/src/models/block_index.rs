//! Manages a list of `{block_hash, weave_size, tx_root}`entries, indexed by
//! block height.
//!
//! Block index items are stored in MDBX via the `IrysBlockIndexItems` and
//! `MigratedBlockHashes` tables.

use eyre::Result;
use irys_database::{
    block_index_item_by_height, block_index_latest_height, block_index_num_blocks,
    db::IrysDatabaseExt as _, delete_block_index_range, insert_block_index_item,
};
use irys_types::{
    BlockIndexItem, DataLedger, DataTransactionHeader, DatabaseProvider, H256, LedgerChunkOffset,
    LedgerIndexItem, NodeConfig, SealedBlock,
};
use reth_db::transaction::{DbTx, DbTxMut};
use std::fs::OpenOptions;
use std::io::{BufReader, Read as _};
use std::path::Path;
use tracing::{info, instrument};

#[derive(Debug, Clone)]
pub struct BlockIndex {
    db: DatabaseProvider,
}

impl BlockIndex {
    /// Initializes a block index backed by MDBX.
    ///
    /// On first run, if the DB is empty and a legacy `index.dat` file exists,
    /// performs a one-time migration of the file contents into the DB.
    pub fn new(config: &NodeConfig, db: DatabaseProvider) -> Result<Self> {
        let block_index = Self { db: db.clone() };

        // Check if migration from file is needed
        let num_blocks = db.view_eyre(block_index_num_blocks)?;

        if num_blocks == 0 {
            let block_index_dir = config.block_index_dir();
            let index_file = block_index_dir.join(FILE_NAME);

            if index_file.exists() {
                info!("Migrating block index from index.dat to MDBX...");
                let items = load_index_from_file(&index_file)?;
                let item_count = items.len();

                if !items.is_empty() {
                    db.update_eyre(|tx| {
                        for (height, item) in items.iter().enumerate() {
                            insert_block_index_item(tx, height as u64, item)?;
                        }
                        Ok(())
                    })?;
                    info!(
                        "Migration complete: {} blocks migrated from index.dat to MDBX",
                        item_count
                    );
                }

                // Rename the legacy file so it won't be re-read on next startup
                let migrated_path = block_index_dir.join("index.dat.migrated");
                if let Err(e) = std::fs::rename(&index_file, &migrated_path) {
                    tracing::warn!("Failed to rename index.dat to index.dat.migrated: {e}");
                } else {
                    info!("Renamed index.dat to index.dat.migrated");
                }
            }
        }

        Ok(block_index)
    }

    /// Creates a `BlockIndex` backed by the given DB without migration logic.
    /// For use in tests only.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn new_for_testing(db: DatabaseProvider) -> Self {
        Self { db }
    }

    /// Returns a reference to the underlying database provider.
    pub fn db(&self) -> &DatabaseProvider {
        &self.db
    }

    /// Retrieves the number of blocks in the index
    pub fn num_blocks(&self) -> u64 {
        self.db.view_eyre(block_index_num_blocks).unwrap_or(0)
    }

    /// Returns the latest block height stored by the block index
    pub fn latest_height(&self) -> u64 {
        self.db
            .view_eyre(block_index_latest_height)
            .ok()
            .flatten()
            .unwrap_or(0)
    }

    /// Fallible variant of [`Self::latest_height`]. `Ok(None)` means the index
    /// is empty; a DB read error propagates instead of being masked as height
    /// `0` (which [`Self::latest_height`] cannot distinguish from an empty
    /// index). Callers that must not treat a read failure as "no blocks" — e.g.
    /// the deep-reorg rollback guard — should use this.
    pub fn try_latest_height(&self) -> eyre::Result<Option<u64>> {
        self.db.view_eyre(block_index_latest_height)
    }

    /// Retrieves a [`BlockIndexItem`] from the block index by block height
    pub fn get_item(&self, block_height: u64) -> Option<BlockIndexItem> {
        self.db
            .view_eyre(|tx| block_index_item_by_height(tx, &block_height))
            .ok()
    }

    /// Retrieves the most recent [`BlockIndexItem`] from the block index
    pub fn get_latest_item(&self) -> Option<BlockIndexItem> {
        let latest_height = self
            .db
            .view_eyre(block_index_latest_height)
            .ok()
            .flatten()?;
        self.get_item(latest_height)
    }

    /// Retrieves a range of [`BlockIndexItem`]s from the block index
    pub fn get_range(&self, start_height: u64, limit: usize) -> Vec<BlockIndexItem> {
        self.db
            .view_eyre(|tx| {
                let total = block_index_num_blocks(tx)?;
                let start = (start_height).min(total);
                let end = start.saturating_add(limit as u64).min(total);
                let mut items = Vec::with_capacity((end - start) as usize);
                for h in start..end {
                    items.push(block_index_item_by_height(tx, &h)?);
                }
                Ok(items)
            })
            .unwrap_or_default()
    }

    /// Removes all block index entries above `target_height`.
    pub fn truncate_to_height(&self, target_height: u64) -> eyre::Result<()> {
        self.db
            .update_eyre(|tx| delete_block_index_range(tx, (target_height + 1)..))
    }

    /// Pushes a new [`BlockIndexItem`] at the given height
    pub fn push_item(&self, block_index_item: &BlockIndexItem, height: u64) -> eyre::Result<()> {
        self.db
            .update_eyre(|tx| insert_block_index_item(tx, height, block_index_item))
    }

    /// Computes and inserts a [`BlockIndexItem`] for a sealed block.
    ///
    /// Extracts Submit/Publish transactions from the [`SealedBlock`], computes
    /// cumulative chunk counts (looking up the previous block in the index),
    /// and writes the resulting [`BlockIndexItem`] to the database.
    ///
    /// This allows the block index write to participate in a larger atomic transaction
    /// (e.g. combined with block header and tx persistence).
    #[instrument(skip_all, err, fields(block.hash = %sealed_block.header().block_hash, block.height = %sealed_block.header().height))]
    pub fn push_block(
        tx: &(impl DbTxMut + DbTx),
        sealed_block: &SealedBlock,
        chunk_size: u64,
    ) -> eyre::Result<()> {
        let block = sealed_block.header();
        let transactions = sealed_block.transactions();

        fn calculate_chunks_added(txs: &[DataTransactionHeader], chunk_size: u64) -> u64 {
            let bytes_added = txs.iter().fold(0, |acc, tx| {
                acc + tx.data_size.div_ceil(chunk_size) * chunk_size
            });
            bytes_added / chunk_size
        }

        let num_blocks = block_index_num_blocks(tx)?;
        let is_genesis = num_blocks == 0 && block.height == 0;

        let prev_block = if is_genesis {
            None
        } else {
            let prev_height = block.height.saturating_sub(1);
            let prev = block_index_item_by_height(tx, &prev_height)?;
            if prev.block_hash != block.previous_block_hash {
                eyre::bail!(
                    "prev_block at index {} does not match current block's prev_block_hash (expected: {}, actual: {})",
                    prev_height,
                    block.previous_block_hash,
                    prev.block_hash
                );
            }
            Some(prev)
        };

        // Build ledger index items from all data ledgers present in the block header
        let mut ledgers = Vec::with_capacity(block.data_ledgers.len());
        for dl in &block.data_ledgers {
            let ledger = DataLedger::try_from(dl.ledger_id)
                .map_err(|_| eyre::eyre!("Unknown ledger_id {} in block header", dl.ledger_id))?;

            let ledger_txs = transactions.get_ledger_txs(ledger);
            let chunks_added = calculate_chunks_added(ledger_txs, chunk_size);

            // Genesis block has no transactions — start all ledgers at 0 chunks.
            // For all other cases, accumulate from the previous block's total.
            let total_chunks = if is_genesis && ledger == DataLedger::Publish {
                0
            } else if let Some(prev) = &prev_block {
                let prev_total = prev
                    .ledgers
                    .iter()
                    .find(|item| item.ledger == ledger)
                    .map(|item| item.total_chunks)
                    .unwrap_or(0);
                // A saturating add would silently clamp and corrupt the persisted
                // ledger boundary; reject the block instead so callers see the overflow.
                prev_total.checked_add(chunks_added).ok_or_else(|| {
                    eyre::eyre!(
                        "ledger {:?} total_chunks overflow at height {}: prev_total {} + chunks_added {}",
                        ledger,
                        block.height,
                        prev_total,
                        chunks_added
                    )
                })?
            } else {
                chunks_added
            };

            ledgers.push(LedgerIndexItem {
                total_chunks,
                tx_root: dl.tx_root,
                ledger,
            });
        }

        let block_index_item = BlockIndexItem {
            block_hash: block.block_hash,
            num_ledgers: ledgers.len() as u8,
            ledgers,
        };

        insert_block_index_item(tx, block.height, &block_index_item)
    }

    /// For a given chunk offset in a ledger, what block was responsible for adding
    /// that chunk to the data ledger?
    ///
    /// Thin wrapper over [`Self::block_bounds_in_tx`] in a fresh view
    /// transaction. Callers composing multiple reads that must observe one
    /// consistent index snapshot should open their own view transaction and
    /// call the `_in_tx` variant instead.
    pub fn get_block_bounds(
        &self,
        ledger: DataLedger,
        chunk_offset: LedgerChunkOffset,
    ) -> Result<BlockBounds, BlockBoundsError> {
        self.db
            .view_eyre(|tx| Ok(Self::block_bounds_in_tx(tx, ledger, chunk_offset)))
            .map_err(BlockBoundsError::Internal)?
    }

    /// Like [`Self::get_block_bounds`], but anchored on `anchor_height`
    /// instead of the latest indexed height. See
    /// [`Self::block_bounds_at_height_in_tx`] for the anchoring semantics.
    pub fn get_block_bounds_at_height(
        &self,
        ledger: DataLedger,
        chunk_offset: LedgerChunkOffset,
        anchor_height: u64,
    ) -> Result<BlockBounds, BlockBoundsError> {
        self.db
            .view_eyre(|tx| {
                Ok(Self::block_bounds_at_height_in_tx(
                    tx,
                    ledger,
                    chunk_offset,
                    anchor_height,
                ))
            })
            .map_err(BlockBoundsError::Internal)?
    }

    /// [`Self::block_bounds_at_height_in_tx`] anchored on the latest height
    /// indexed in the same transaction's snapshot.
    pub fn block_bounds_in_tx<T: DbTx>(
        tx: &T,
        ledger: DataLedger,
        chunk_offset: LedgerChunkOffset,
    ) -> Result<BlockBounds, BlockBoundsError> {
        let anchor_height = block_index_latest_height(tx)?.ok_or(BlockBoundsError::IndexEmpty)?;
        Self::block_bounds_at_height_in_tx(tx, ledger, chunk_offset, anchor_height)
    }

    // Note: `block_bounds_at_height_in_tx` and `get_block_index_item` both
    // binary-search the index but intentionally diverge on how they treat a
    // probed block whose ledger entry is missing; any future refactor that
    // unifies them must take a missing-ledger-policy parameter rather than
    // collapse the two behaviours.

    /// Resolves the block introducing `chunk_offset`, anchored on
    /// `anchor_height`. Anchoring produces fork-deterministic results across
    /// peers regardless of how far their local indices have advanced past the
    /// anchor — the caller (e.g. PoA pre-validation) passes the block's parent
    /// height so two honest peers on the same fork compute identical bounds.
    ///
    /// Runs entirely inside the caller's view transaction, so every probe of
    /// the binary search — and any further reads the caller makes with the
    /// same `tx` — observes one consistent index snapshot even while a
    /// concurrent deep-reorg truncation rewrites the index.
    ///
    /// Missing-ledger policy: a probed block whose ledger entry is absent is
    /// treated as pre-introduction (`total_chunks = 0`) so the search moves
    /// right, preserving searchability from heights earlier than a
    /// late-introduced ledger. This differs intentionally from
    /// [`Self::get_block_index_item`], which reports a missing ledger as
    /// `Err`; see the section comment above for why the two policies must
    /// remain distinct. The *anchor* block lacking the ledger entirely is
    /// reported as [`BlockBoundsError::LedgerInactive`].
    pub fn block_bounds_at_height_in_tx<T: DbTx>(
        tx: &T,
        ledger: DataLedger,
        chunk_offset: LedgerChunkOffset,
        anchor_height: u64,
    ) -> Result<BlockBounds, BlockBoundsError> {
        let anchor_item = block_index_item_by_height(tx, &anchor_height)?;
        let anchor_max = anchor_item
            .ledgers
            .iter()
            .find(|l| l.ledger == ledger)
            .map(|l| l.total_chunks)
            .ok_or(BlockBoundsError::LedgerInactive {
                ledger,
                anchor_height,
            })?;

        let chunk_offset_val: u64 = chunk_offset.into();
        if chunk_offset_val >= anchor_max {
            return Err(BlockBoundsError::OffsetBeyondFrontier {
                ledger,
                chunk_offset: chunk_offset_val,
                frontier: anchor_max,
                anchor_height,
            });
        }

        // Binary search bounded by anchor_height (inclusive) so this never
        // consults blocks beyond the parent — fork-deterministic regardless
        // of how far the local tip has advanced. A probed block lacking
        // this ledger entirely (ledger introduced at a later height) is
        // equivalent to total_chunks = 0; the search just moves right.
        let (block_height, found_item) = {
            let mut lo: u64 = 0;
            let mut hi: u64 = anchor_height;

            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                let item = block_index_item_by_height(tx, &mid)?;
                let total = item
                    .ledgers
                    .iter()
                    .find(|l| l.ledger == ledger)
                    .map(|l| l.total_chunks)
                    .unwrap_or(0);
                if chunk_offset_val < total {
                    hi = mid;
                } else {
                    lo = mid + 1;
                }
            }

            let item = block_index_item_by_height(tx, &lo)?;
            (lo, item)
        };

        // prev_total is 0 for genesis (no predecessor) and for blocks that
        // first introduce this ledger (predecessor has no entry).
        let prev_total = if block_height == 0 {
            0
        } else {
            let previous_item = block_index_item_by_height(tx, &(block_height - 1))?;
            previous_item
                .ledgers
                .iter()
                .find(|l| l.ledger == ledger)
                .map(|l| l.total_chunks)
                .unwrap_or(0)
        };

        let found_ledger = found_item
            .ledgers
            .iter()
            .find(|l| l.ledger == ledger)
            .ok_or_else(|| {
                BlockBoundsError::Internal(eyre::eyre!(
                    "Ledger {:?} not found in block at height {}",
                    ledger,
                    block_height
                ))
            })?;

        Ok(BlockBounds {
            height: block_height,
            block_hash: found_item.block_hash,
            ledger,
            start_chunk_offset: prev_total,
            end_chunk_offset: found_ledger.total_chunks,
            tx_root: found_ledger.tx_root,
        })
    }

    /// Returns the block height + block index item containing the given chunk offset
    ///
    /// Missing-ledger policy: a probed block whose ledger entry is absent is
    /// reported as `Err` rather than treated as pre-introduction. Callers of
    /// this method require an exact ledger-entry hit, so a hole in the
    /// searched range is a corruption signal. This differs intentionally
    /// from [`Self::get_block_bounds_at_height`], which tolerates missing
    /// entries so late-introduced ledgers remain searchable from earlier
    /// heights.
    pub fn get_block_index_item(
        &self,
        ledger: DataLedger,
        chunk_offset: u64,
    ) -> Result<(u64, BlockIndexItem)> {
        self.db.view_eyre(|tx| {
            let latest_height = block_index_latest_height(tx)?
                .ok_or_else(|| eyre::eyre!("Block index is empty"))?;

            let last_item = block_index_item_by_height(tx, &latest_height)?;
            let last_max = last_item
                .ledgers
                .iter()
                .find(|l| l.ledger == ledger)
                .map(|l| l.total_chunks)
                .ok_or_else(|| {
                    eyre::eyre!(
                        "Ledger {:?} not found in block at height {}",
                        ledger,
                        latest_height
                    )
                })?;

            eyre::ensure!(
                chunk_offset < last_max,
                "chunk_offset {} beyond last block's max_chunk_offset {}, last block height {}",
                chunk_offset,
                last_max,
                latest_height + 1
            );

            // Binary search
            let mut lo: u64 = 0;
            let mut hi: u64 = latest_height;

            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                let item = block_index_item_by_height(tx, &mid)?;
                let total = item
                    .ledgers
                    .iter()
                    .find(|l| l.ledger == ledger)
                    .map(|l| l.total_chunks)
                    .ok_or_else(|| {
                        eyre::eyre!("Ledger {:?} not found in block at height {}", ledger, mid)
                    })?;
                if chunk_offset < total {
                    hi = mid;
                } else {
                    lo = mid + 1;
                }
            }

            let item = block_index_item_by_height(tx, &lo)?;
            Ok((lo, item))
        })
    }

    pub fn print_items(&self) {
        for height in 0..self.num_blocks() {
            if let Some(item) = self.get_item(height) {
                tracing::info!("height: {} hash: {}", height, item.block_hash);
            } else {
                tracing::error!("height: {} missing in block index", height);
            }
        }
    }
}

/// `BlockBounds` describe the size of a ledger at the start of a block
/// and then after the blocks transactions were applied to the ledger
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BlockBounds {
    /// Block height where these bounds apply
    pub height: u64,
    /// Hash of the block introducing these bounds, taken from the same index
    /// item (and so the same snapshot) the search resolved — callers must not
    /// re-read the index for it, or a concurrent truncation could pair these
    /// bounds with a different fork's hash.
    pub block_hash: H256,
    /// Target ledger (Publish or Submit)
    pub ledger: DataLedger,
    /// First chunk offset included in this block (inclusive)
    pub start_chunk_offset: u64,
    /// Exclusive upper bound on the chunk-offset range owned by this block.
    /// Equal to the block's `total_chunks` for this ledger (one past the
    /// highest chunk offset).
    pub end_chunk_offset: u64,
    /// Merkle root (`tx_root`) of all transactions this block applied to the ledger
    pub tx_root: H256,
}

/// Typed outcome of a block-bounds lookup, so callers can tell "this offset
/// is not (yet) allocated in this ledger" apart from a real lookup failure
/// without re-deriving the ledger frontier themselves: API routes map the
/// first three variants to 404, PoA pre-validation to its typed consensus
/// errors, and background services skip the work item.
#[derive(Debug, thiserror::Error)]
pub enum BlockBoundsError {
    #[error("block index is empty")]
    IndexEmpty,
    /// The anchor block carries no entry for this ledger — e.g. a Cascade
    /// term ledger before activation.
    #[error("ledger {ledger:?} has no entry in the anchor block at height {anchor_height}")]
    LedgerInactive {
        ledger: DataLedger,
        anchor_height: u64,
    },
    #[error(
        "chunk offset {chunk_offset} is beyond the {ledger:?} ledger frontier ({frontier} chunks) at anchor height {anchor_height}"
    )]
    OffsetBeyondFrontier {
        ledger: DataLedger,
        chunk_offset: u64,
        frontier: u64,
        anchor_height: u64,
    },
    #[error("block bounds lookup failed: {0}")]
    Internal(eyre::Report),
}

impl From<eyre::Report> for BlockBoundsError {
    fn from(err: eyre::Report) -> Self {
        Self::Internal(err)
    }
}

// === Migration-only code (temporary, will be removed once all nodes have migrated) ===

const FILE_NAME: &str = "index.dat";
const HEADER_SIZE: usize = 33; // 32 bytes block_hash + 1 byte num_ledgers
const LEDGER_ITEM_SIZE: usize = 40; // 8 bytes total_chunks + 32 bytes tx_root
const EXPECTED_NUM_LEDGERS: u8 = 2; // Publish and Submit ledgers

/// Loads the block index from file using streaming deserialization.
///
/// **Migration-only** — this function will be removed in a future PR once
/// all nodes have migrated from `index.dat` to MDBX.
#[tracing::instrument(level = "trace", skip_all, err)]
fn load_index_from_file(file_path: &Path) -> eyre::Result<Vec<BlockIndexItem>> {
    let file = match OpenOptions::new().read(true).open(file_path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Vec::new());
        }
        Err(e) => return Err(e.into()),
    };

    let file_size = file.metadata()?.len();
    if file_size == 0 {
        return Ok(Vec::new());
    }

    let estimated_item_size = HEADER_SIZE + (EXPECTED_NUM_LEDGERS as usize * LEDGER_ITEM_SIZE);
    let estimated_items = (file_size as usize / estimated_item_size) + 1;

    let mut block_index_items = Vec::with_capacity(estimated_items);
    let mut reader = BufReader::with_capacity(64 * 1024, file);

    let mut header_buf = [0_u8; HEADER_SIZE];
    let mut ledger_buf = [0_u8; LEDGER_ITEM_SIZE];

    'outer: loop {
        match reader.read_exact(&mut header_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                if block_index_items.is_empty() {
                    tracing::trace!("Empty block index file");
                } else {
                    tracing::warn!(
                        loaded_blocks = block_index_items.len(),
                        "Block index file truncated mid-header; dropping trailing partial block"
                    );
                }
                break;
            }
            Err(e) => return Err(e.into()),
        }

        let block_hash = H256::from_slice(&header_buf[..32]);
        let num_ledgers_byte = header_buf[32];

        eyre::ensure!(
            num_ledgers_byte == EXPECTED_NUM_LEDGERS,
            "Corrupted block index: expected {} ledgers, found {}",
            EXPECTED_NUM_LEDGERS,
            num_ledgers_byte
        );

        let num_ledgers = num_ledgers_byte as usize;

        // Read ledger entries
        let mut ledgers = Vec::with_capacity(num_ledgers);
        for i in 0..num_ledgers {
            match reader.read_exact(&mut ledger_buf) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    tracing::warn!(
                        loaded_blocks = block_index_items.len(),
                        block_hash = %block_hash,
                        "Block index file truncated mid-ledger; dropping trailing partial block"
                    );
                    break 'outer;
                }
                Err(e) => return Err(e.into()),
            }

            let total_chunks = u64::from_le_bytes(
                ledger_buf[0..8]
                    .try_into()
                    .expect("slice is exactly 8 bytes"),
            );
            let tx_root = H256::from_slice(&ledger_buf[8..40]);

            ledgers.push(LedgerIndexItem {
                total_chunks,
                tx_root,
                ledger: DataLedger::try_from(i as u32)?,
            });
        }

        block_index_items.push(BlockIndexItem {
            block_hash,
            num_ledgers: num_ledgers as u8,
            ledgers,
        });
    }

    Ok(block_index_items)
}

#[cfg(test)]
mod tests {
    use super::BlockIndex;
    use super::*;
    use crate::BlockBounds;
    use irys_database::tables::IrysTables;
    use irys_database::{IrysDatabaseArgs as _, open_or_create_db};
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::H256;
    use reth_db::mdbx::DatabaseArguments;
    use std::fs::{self, File};
    use std::io::Write as _;
    use std::sync::Arc;

    fn create_test_db(path: &std::path::Path) -> DatabaseProvider {
        let db = open_or_create_db(
            path,
            IrysTables::ALL,
            DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();
        DatabaseProvider(Arc::new(db))
    }

    fn save_block_index(
        block_index_items: &[BlockIndexItem],
        config: &NodeConfig,
    ) -> eyre::Result<()> {
        fs::create_dir_all(config.block_index_dir())?;
        let path = config.block_index_dir().join(FILE_NAME);
        let mut file = File::create(path)?;
        for item in block_index_items {
            let bytes = item.to_bytes();
            file.write_all(&bytes)?;
        }
        Ok(())
    }

    fn make_test_items() -> Vec<BlockIndexItem> {
        vec![
            BlockIndexItem {
                block_hash: H256::random(),
                num_ledgers: 2,
                ledgers: vec![
                    LedgerIndexItem {
                        total_chunks: 100,
                        tx_root: H256::random(),
                        ledger: DataLedger::Publish,
                    },
                    LedgerIndexItem {
                        total_chunks: 1000,
                        tx_root: H256::random(),
                        ledger: DataLedger::Submit,
                    },
                ],
            },
            BlockIndexItem {
                block_hash: H256::random(),
                num_ledgers: 2,
                ledgers: vec![
                    LedgerIndexItem {
                        total_chunks: 200,
                        tx_root: H256::random(),
                        ledger: DataLedger::Publish,
                    },
                    LedgerIndexItem {
                        total_chunks: 2000,
                        tx_root: H256::random(),
                        ledger: DataLedger::Submit,
                    },
                ],
            },
            BlockIndexItem {
                block_hash: H256::random(),
                num_ledgers: 2,
                ledgers: vec![
                    LedgerIndexItem {
                        total_chunks: 300,
                        tx_root: H256::random(),
                        ledger: DataLedger::Publish,
                    },
                    LedgerIndexItem {
                        total_chunks: 3000,
                        tx_root: H256::random(),
                        ledger: DataLedger::Submit,
                    },
                ],
            },
        ]
    }

    #[test]
    fn read_and_write_block_index() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("read_and_write_block_index_db")
            .with_tracing()
            .build();
        let base_path = tmp_dir.path().to_path_buf();
        let db = create_test_db(&base_path.join("db"));
        let block_index = BlockIndex::new_for_testing(db);

        let block_items = make_test_items();

        // Write items
        for (i, item) in block_items.iter().enumerate() {
            block_index.push_item(item, i as u64)?;
        }

        assert_eq!(block_index.num_blocks(), 3);
        assert_eq!(block_index.get_item(0).unwrap(), block_items[0]);
        assert_eq!(block_index.get_item(1).unwrap(), block_items[1]);
        assert_eq!(block_index.get_item(2).unwrap(), block_items[2]);

        // an offset past the frontier is reported as the typed
        // OffsetBeyondFrontier, carrying the frontier value
        let invalid_block_bounds =
            block_index.get_block_bounds(DataLedger::Publish, LedgerChunkOffset::from(300));
        assert!(
            matches!(
                invalid_block_bounds,
                Err(BlockBoundsError::OffsetBeyondFrontier {
                    frontier: 300,
                    chunk_offset: 300,
                    ..
                })
            ),
            "expected OffsetBeyondFrontier, got {invalid_block_bounds:?}"
        );

        // check valid block bound
        let block_bounds = block_index
            .get_block_bounds(DataLedger::Publish, LedgerChunkOffset::from(150))
            .expect("expected valid block bounds");
        assert_eq!(
            block_bounds,
            BlockBounds {
                height: 1,
                block_hash: block_items[1].block_hash,
                ledger: DataLedger::Publish,
                start_chunk_offset: 100,
                end_chunk_offset: 200,
                tx_root: block_items[1].ledgers[DataLedger::Publish].tx_root
            }
        );

        let block_bounds = block_index
            .get_block_bounds(DataLedger::Submit, LedgerChunkOffset::from(1000))
            .expect("expected valid block bounds");
        assert_eq!(
            block_bounds,
            BlockBounds {
                height: 1,
                block_hash: block_items[1].block_hash,
                ledger: DataLedger::Submit,
                start_chunk_offset: 1000,
                end_chunk_offset: 2000,
                tx_root: block_items[1].ledgers[DataLedger::Submit].tx_root
            }
        );

        let item = block_index.get_item(2).unwrap();
        assert_eq!(item, block_items[2]);

        Ok(())
    }

    #[test]
    fn block_index_encoding_layout_matches_constants() {
        let item = BlockIndexItem {
            block_hash: H256::zero(),
            num_ledgers: EXPECTED_NUM_LEDGERS,
            ledgers: vec![
                LedgerIndexItem {
                    total_chunks: 0,
                    tx_root: H256::zero(),
                    ledger: DataLedger::Publish,
                },
                LedgerIndexItem {
                    total_chunks: 0,
                    tx_root: H256::zero(),
                    ledger: DataLedger::Submit,
                },
            ],
        };

        let bytes = item.to_bytes();
        assert_eq!(
            bytes.len(),
            HEADER_SIZE + (EXPECTED_NUM_LEDGERS as usize * LEDGER_ITEM_SIZE),
            "BlockIndexItem encoding does not match HEADER_SIZE and LEDGER_ITEM_SIZE constants"
        );

        // Verify num_ledgers field is at the expected position
        assert_eq!(
            bytes[32], EXPECTED_NUM_LEDGERS,
            "num_ledgers byte does not match EXPECTED_NUM_LEDGERS constant"
        );
    }

    /// Builds a `BlockIndex` with both Publish and Submit ledgers present in
    /// every block, with linearly increasing chunk counts. Heights `0..n`:
    /// Publish total_chunks = (h+1)*100, Submit total_chunks = (h+1)*1000.
    fn build_uniform_index(n: u64) -> (irys_testing_utils::tempfile::TempDir, BlockIndex) {
        // Note: NOT calling `.with_tracing()` here — it installs a panic hook
        // that aborts the test process on `assert!`/`expect` failure, which
        // would prevent independent reporting of expected-fail edge cases.
        let tmp_dir = TempDirBuilder::new()
            .prefix("get_block_bounds_at_height_uniform")
            .build();
        let db = create_test_db(&tmp_dir.path().join("db"));
        let block_index = BlockIndex::new_for_testing(db);

        for h in 0..n {
            let item = BlockIndexItem {
                block_hash: H256::random(),
                num_ledgers: 2,
                ledgers: vec![
                    LedgerIndexItem {
                        total_chunks: (h + 1) * 100,
                        tx_root: H256::random(),
                        ledger: DataLedger::Publish,
                    },
                    LedgerIndexItem {
                        total_chunks: (h + 1) * 1000,
                        tx_root: H256::random(),
                        ledger: DataLedger::Submit,
                    },
                ],
            };
            block_index.push_item(&item, h).unwrap();
        }
        (tmp_dir, block_index)
    }

    /// `try_latest_height` must distinguish an empty index (`None`) from a
    /// populated one (`Some(max_height)`) — the distinction the deep-reorg
    /// rollback guard relies on to avoid treating "no blocks" and a masked DB
    /// error alike. `latest_height` collapses both empty and genesis-only to 0.
    #[test]
    fn try_latest_height_distinguishes_empty_from_populated() {
        let (_empty_tmp, empty) = build_uniform_index(0);
        assert_eq!(
            empty.try_latest_height().expect("empty index read"),
            None,
            "an empty index reports no latest height, not 0"
        );

        let (_tmp, populated) = build_uniform_index(3);
        assert_eq!(
            populated.try_latest_height().expect("populated index read"),
            Some(2),
            "3 blocks occupy heights 0..=2"
        );
    }

    /// Builds a `BlockIndex` where the Publish ledger is introduced at
    /// `publish_introduction_height`. Blocks below that height carry only the
    /// Submit ledger; blocks at and above carry both.
    ///
    /// Submit total_chunks grows by +100 per block from genesis.
    /// Publish total_chunks grows by +50 per block starting at introduction.
    fn build_index_with_late_publish(
        n: u64,
        publish_introduction_height: u64,
    ) -> (
        irys_testing_utils::tempfile::TempDir,
        BlockIndex,
        Vec<BlockIndexItem>,
    ) {
        // Note: NOT calling `.with_tracing()` here — see uniform helper.
        let tmp_dir = TempDirBuilder::new()
            .prefix("get_block_bounds_at_height_late_publish")
            .build();
        let db = create_test_db(&tmp_dir.path().join("db"));
        let block_index = BlockIndex::new_for_testing(db);

        let mut items = Vec::with_capacity(n as usize);
        for h in 0..n {
            let submit_total = (h + 1) * 100;
            let mut ledgers = vec![LedgerIndexItem {
                total_chunks: submit_total,
                tx_root: H256::random(),
                ledger: DataLedger::Submit,
            }];
            if h >= publish_introduction_height {
                let publish_total = (h - publish_introduction_height + 1) * 50;
                ledgers.push(LedgerIndexItem {
                    total_chunks: publish_total,
                    tx_root: H256::random(),
                    ledger: DataLedger::Publish,
                });
            }
            let item = BlockIndexItem {
                block_hash: H256::random(),
                num_ledgers: ledgers.len() as u8,
                ledgers,
            };
            block_index.push_item(&item, h).unwrap();
            items.push(item);
        }
        (tmp_dir, block_index, items)
    }

    /// Basic correctness: when `anchor_height == latest_height`, the anchored
    /// variant must agree with `get_block_bounds` across a range of offsets and
    /// both ledgers.
    #[test]
    fn get_block_bounds_at_height_matches_get_block_bounds_when_anchor_is_latest()
    -> eyre::Result<()> {
        let (_tmp, block_index) = build_uniform_index(5);
        let latest = block_index.latest_height();
        assert_eq!(latest, 4);

        let cases = [
            (DataLedger::Publish, 0_u64),
            (DataLedger::Publish, 50),
            (DataLedger::Publish, 100),
            (DataLedger::Publish, 250),
            (DataLedger::Publish, 499),
            (DataLedger::Submit, 0),
            (DataLedger::Submit, 999),
            (DataLedger::Submit, 1500),
            (DataLedger::Submit, 4999),
        ];

        for (ledger, offset) in cases {
            let unanchored =
                block_index.get_block_bounds(ledger, LedgerChunkOffset::from(offset))?;
            let anchored = block_index.get_block_bounds_at_height(
                ledger,
                LedgerChunkOffset::from(offset),
                latest,
            )?;
            assert_eq!(
                anchored, unanchored,
                "anchored vs unanchored differ at ledger={:?} offset={}",
                ledger, offset
            );
        }
        Ok(())
    }

    /// `anchor_height < latest_height` must restrict the search to the prefix
    /// `[0, anchor_height]` and return historical bounds for offsets contained
    /// within that prefix.
    #[test]
    fn get_block_bounds_at_height_returns_historical_bounds_for_lower_anchor() -> eyre::Result<()> {
        let (_tmp, block_index) = build_uniform_index(5);
        assert_eq!(block_index.latest_height(), 4);

        // At anchor=2, Publish totals are: h0=100, h1=200, h2=300. Querying
        // offset 150 must resolve to block 1 (covers [100, 200)).
        let bounds = block_index.get_block_bounds_at_height(
            DataLedger::Publish,
            LedgerChunkOffset::from(150),
            2,
        )?;
        assert_eq!(bounds.height, 1);
        assert_eq!(bounds.start_chunk_offset, 100);
        assert_eq!(bounds.end_chunk_offset, 200);
        assert_eq!(bounds.ledger, DataLedger::Publish);

        // Offsets at or beyond anchor's max must error (offset 300 == anchor_max).
        let err = block_index.get_block_bounds_at_height(
            DataLedger::Publish,
            LedgerChunkOffset::from(300),
            2,
        );
        assert!(
            err.is_err(),
            "offset at anchor's max should be rejected (exclusive upper bound)"
        );

        // Offset beyond anchor's max but within latest's range must still error
        // — anchored variant must NOT see those blocks.
        let err = block_index.get_block_bounds_at_height(
            DataLedger::Publish,
            LedgerChunkOffset::from(350),
            2,
        );
        assert!(
            err.is_err(),
            "offset beyond anchor's max must be rejected even if within latest's range"
        );

        Ok(())
    }

    /// Edge case (2): when the binary search lands on the block that FIRST
    /// introduces the target ledger, the predecessor has no entry for that
    /// ledger. The current code's `ok_or_else` on `prev_total` should treat
    /// this as `prev_total = 0` (the ledger's chunk count before introduction
    /// was, by definition, zero).
    ///
    /// In the index built here, Publish is first introduced at height 3 with
    /// total_chunks=50. Querying Publish offset 25 anchored at h=3 should
    /// resolve to `BlockBounds { height: 3, start: 0, end: 50, .. }`.
    #[test]
    fn get_block_bounds_at_height_handles_ledger_introduced_post_genesis() -> eyre::Result<()> {
        let (_tmp, block_index, items) = build_index_with_late_publish(5, 3);

        let bounds = block_index.get_block_bounds_at_height(
            DataLedger::Publish,
            LedgerChunkOffset::from(25),
            3,
        )?;

        assert_eq!(bounds.height, 3);
        assert_eq!(bounds.ledger, DataLedger::Publish);
        assert_eq!(
            bounds.start_chunk_offset, 0,
            "first-introducing block must report prev_total = 0"
        );
        assert_eq!(bounds.end_chunk_offset, 50);
        assert_eq!(
            bounds.tx_root,
            items[3]
                .ledgers
                .iter()
                .find(|l| l.ledger == DataLedger::Publish)
                .unwrap()
                .tx_root
        );
        Ok(())
    }

    /// Edge case (1): the binary search must tolerate probing heights below
    /// where the target ledger was introduced. Probed blocks without the
    /// ledger should be treated as `total_chunks = 0` (move right), not
    /// surface an error.
    ///
    /// Index here introduces Publish at height 2. Anchored at h=4 and querying
    /// Publish offset 25, the binary search starts with lo=0, hi=4 and probes
    /// mid=2 (Publish=50, descend left); then lo=0, hi=2 and probes mid=1
    /// (no Publish ledger) — this is the descent-below-introduction case.
    #[test]
    fn get_block_bounds_at_height_handles_search_descending_below_ledger_introduction()
    -> eyre::Result<()> {
        let (_tmp, block_index, _items) = build_index_with_late_publish(5, 2);

        let result = block_index.get_block_bounds_at_height(
            DataLedger::Publish,
            LedgerChunkOffset::from(25),
            4,
        );

        let bounds = result.expect(
            "binary search descending past ledger-introduction height must not error; \
             missing ledger should be treated as total_chunks = 0",
        );
        // Publish is introduced at h=2 with total=50, so offset 25 lives in
        // block 2 with start=0, end=50.
        assert_eq!(bounds.height, 2);
        assert_eq!(bounds.start_chunk_offset, 0);
        assert_eq!(bounds.end_chunk_offset, 50);
        assert_eq!(bounds.ledger, DataLedger::Publish);
        Ok(())
    }

    /// Edge case at genesis (height 0): `saturating_sub(1)` makes the
    /// "previous" lookup land on the found block itself. The implementation
    /// must still report `prev_total = 0` for the genesis block, not
    /// `prev_total == found_ledger.total_chunks` (which would collapse the
    /// bounds to an empty range).
    #[test]
    fn get_block_bounds_at_height_handles_genesis_offset() -> eyre::Result<()> {
        let (_tmp, block_index) = build_uniform_index(3);

        // Publish at h=0: total_chunks = 100. Offset 25 must resolve to
        // block 0 with start=0, end=100.
        let bounds = block_index.get_block_bounds_at_height(
            DataLedger::Publish,
            LedgerChunkOffset::from(25),
            2,
        )?;

        assert_eq!(bounds.height, 0);
        assert_eq!(bounds.ledger, DataLedger::Publish);
        assert_eq!(
            bounds.start_chunk_offset, 0,
            "genesis block must report prev_total = 0 (saturating_sub(1) collides with self)"
        );
        assert_eq!(bounds.end_chunk_offset, 100);

        // Same for Submit.
        let bounds = block_index.get_block_bounds_at_height(
            DataLedger::Submit,
            LedgerChunkOffset::from(500),
            2,
        )?;
        assert_eq!(bounds.height, 0);
        assert_eq!(bounds.start_chunk_offset, 0);
        assert_eq!(bounds.end_chunk_offset, 1000);

        Ok(())
    }

    #[test]
    fn migration_from_file_to_db() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("migration_from_file_to_db")
            .with_tracing()
            .build();
        let base_path = tmp_dir.path().to_path_buf();

        let mut node_config = NodeConfig::testing();
        node_config.base_directory = base_path.clone();

        let block_items = make_test_items();

        // Write items to legacy file
        save_block_index(&block_items, &node_config)?;

        // Create DB and trigger migration
        let db = create_test_db(&base_path.join("db"));
        let block_index = BlockIndex::new(&node_config, db)?;

        // Verify items migrated correctly
        assert_eq!(block_index.num_blocks(), 3);
        assert_eq!(block_index.get_item(0).unwrap(), block_items[0]);
        assert_eq!(block_index.get_item(1).unwrap(), block_items[1]);
        assert_eq!(block_index.get_item(2).unwrap(), block_items[2]);

        // Verify legacy file was renamed
        let index_dir = node_config.block_index_dir();
        assert!(
            !index_dir.join("index.dat").exists(),
            "index.dat should have been renamed"
        );
        assert!(
            index_dir.join("index.dat.migrated").exists(),
            "index.dat.migrated should exist"
        );

        Ok(())
    }
}
