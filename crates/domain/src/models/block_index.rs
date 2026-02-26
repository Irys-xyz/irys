//! Manages a list of `{block_hash, weave_size, tx_root}`entries, indexed by
//! block height.
//!
//! Block index items are stored in MDBX via the `IrysBlockIndexItems` and
//! `MigratedBlockHashes` tables.

use eyre::Result;
use irys_database::{
    block_index_item_by_height, block_index_latest_height, block_index_num_blocks,
    db::IrysDatabaseExt as _, insert_block_index_item,
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

            // Publish ledger has no chunks at genesis (genesis block only contains Submit data).
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
                prev_total + chunks_added
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
    pub fn get_block_bounds(
        &self,
        ledger: DataLedger,
        chunk_offset: LedgerChunkOffset,
    ) -> eyre::Result<BlockBounds> {
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

            let chunk_offset_val: u64 = chunk_offset.into();
            eyre::ensure!(
                chunk_offset_val < last_max,
                "chunk_offset {} beyond last block's max_chunk_offset {}, last block height {}",
                chunk_offset_val,
                last_max,
                latest_height + 1
            );

            // Binary search for the block containing this chunk offset
            let (block_height, found_item) = {
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
                    if chunk_offset_val < total {
                        hi = mid;
                    } else {
                        lo = mid + 1;
                    }
                }

                let item = block_index_item_by_height(tx, &lo)?;
                (lo, item)
            };

            let previous_item = block_index_item_by_height(tx, &block_height.saturating_sub(1))?;

            let prev_total = previous_item
                .ledgers
                .iter()
                .find(|l| l.ledger == ledger)
                .map(|l| l.total_chunks)
                .ok_or_else(|| {
                    eyre::eyre!(
                        "Ledger {:?} not found in block at height {}",
                        ledger,
                        block_height.saturating_sub(1)
                    )
                })?;

            let found_ledger = found_item
                .ledgers
                .iter()
                .find(|l| l.ledger == ledger)
                .ok_or_else(|| {
                    eyre::eyre!(
                        "Ledger {:?} not found in block at height {}",
                        ledger,
                        block_height
                    )
                })?;

            Ok(BlockBounds {
                height: block_height,
                ledger,
                start_chunk_offset: prev_total,
                end_chunk_offset: found_ledger.total_chunks,
                tx_root: found_ledger.tx_root,
            })
        })
    }

    /// Returns the block height + block index item containing the given chunk offset
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
    /// Target ledger (Publish or Submit)
    pub ledger: DataLedger,
    /// First chunk offset included in this block (inclusive)
    pub start_chunk_offset: u64,
    /// Final chunk offset after processing block transactions
    pub end_chunk_offset: u64,
    /// Merkle root (`tx_root`) of all transactions this block applied to the ledger
    pub tx_root: H256,
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
    use irys_database::open_or_create_db;
    use irys_database::tables::IrysTables;
    use irys_testing_utils::utils::setup_tracing_and_temp_dir;
    use irys_types::H256;
    use std::fs::{self, File};
    use std::io::Write as _;
    use std::sync::Arc;

    fn create_test_db(path: &std::path::Path) -> DatabaseProvider {
        let db = open_or_create_db(path, IrysTables::ALL, None).unwrap();
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
        let tmp_dir = setup_tracing_and_temp_dir(Some("read_and_write_block_index_db"), false);
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

        // check an invalid byte offset causes get_block_bounds to return an error
        let invalid_block_bounds =
            block_index.get_block_bounds(DataLedger::Publish, LedgerChunkOffset::from(300));
        assert!(
            invalid_block_bounds.is_err(),
            "expected invalid block bound to generate an error"
        );

        // check valid block bound
        let block_bounds = block_index
            .get_block_bounds(DataLedger::Publish, LedgerChunkOffset::from(150))
            .expect("expected valid block bounds");
        assert_eq!(
            block_bounds,
            BlockBounds {
                height: 1,
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

    #[test]
    fn migration_from_file_to_db() -> eyre::Result<()> {
        let tmp_dir = setup_tracing_and_temp_dir(Some("migration_from_file_to_db"), false);
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
