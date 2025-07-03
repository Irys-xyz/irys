//! Manages a list of `{block_hash, weave_size, tx_root}`entries, indexed by
//! block height.
use actix::dev::MessageResponse;
use base58::ToBase58 as _;
use eyre::Result;
use irys_types::{
    BlockIndexItem, DataLedger, IrysBlockHeader, IrysTransactionHeader, LedgerIndexItem,
    NodeConfig, H256,
};
use std::fs::OpenOptions;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug)]
pub struct BlockIndex {
    /// Stored as a fixed size array with an Arc to allow multithreaded access
    pub items: Arc<[BlockIndexItem]>,
    pub block_index_file: PathBuf,
}

const FILE_NAME: &str = "index.dat";

impl Default for BlockIndex {
    fn default() -> Self {
        unreachable!("do not rely on the default implementation")
    }
}

impl BlockIndex {
    /// Initializes a block index from disk, if this was a multi node network
    /// it could also read the latest block information from the network.
    pub async fn new(config: &NodeConfig) -> Result<Self> {
        let block_index_dir = config.block_index_dir();
        tokio::fs::create_dir_all(&block_index_dir).await?;
        let block_index_file = block_index_dir.join(FILE_NAME);

        // Try to load the block index from disk
        let index = load_index_from_file(&block_index_file)?;

        // Return the "Initialized" state of the BlockIndex type
        Ok(Self {
            items: index.into(),
            block_index_file,
        })
    }

    /// Retrieves the number of blocks in the index
    pub fn num_blocks(&self) -> u64 {
        self.items.len() as u64
    }

    /// Returns the latest block height stored by the block index
    pub fn latest_height(&self) -> u64 {
        (self.items.len().saturating_sub(1)) as u64
    }

    /// Retrieves a [`BlockIndexItem`] from the block index by block height
    pub fn get_item(&self, block_height: u64) -> Option<&BlockIndexItem> {
        // Check if block_height can fit into a usize
        let index = if block_height <= usize::MAX as u64 {
            block_height as usize
        } else {
            return None; // Block height too large for this platform
        };

        self.items.get(index)
    }

    /// Retrieves the most recent [`BlockIndexItem`] from the block index by block height
    pub fn get_latest_item(&self) -> Option<&BlockIndexItem> {
        if self.items.is_empty() {
            return None;
        };
        self.items.last()
    }

    /// Pushes a new [`BlockIndexItem`] onto the items array
    pub fn push_item(&mut self, block_index_item: &BlockIndexItem) -> eyre::Result<()> {
        let mut items_vec = self.items.to_vec();
        // TODO: improve this, storing in file each item
        append_item(block_index_item, &self.block_index_file)?;
        items_vec.push(block_index_item.clone());
        self.items = items_vec.into();
        Ok(())
    }

    pub fn push_block(
        &mut self,
        block: &IrysBlockHeader,
        all_txs: &[IrysTransactionHeader],
        chunk_size: u64,
    ) -> eyre::Result<()> {
        /// Inner function: Calculates the total number of full chunks needed to store transactions
        /// Each transaction's data is padded to the next full chunk boundary
        fn calculate_chunks_added(txs: &[IrysTransactionHeader], chunk_size: u64) -> u64 {
            let bytes_added = txs.iter().fold(0, |acc, tx| {
                acc + tx.data_size.div_ceil(chunk_size) * chunk_size
            });

            bytes_added / chunk_size
        }

        // Extract just the transactions referenced in the submit ledger
        let submit_tx_count = block.data_ledgers[DataLedger::Submit].tx_ids.len();
        let submit_txs = &all_txs[..submit_tx_count];

        // Extract just the transactions referenced in the publish ledger
        let publish_txs = &all_txs[submit_tx_count..];

        // Calculate chunk counts for both ledger types
        let sub_chunks_added = calculate_chunks_added(submit_txs, chunk_size);
        let pub_chunks_added = calculate_chunks_added(publish_txs, chunk_size);

        // Get previous ledger sizes or default to 0 for genesis
        let (max_publish_chunks, max_submit_chunks) = if self.num_blocks() == 0 && block.height == 0
        {
            (0, sub_chunks_added)
        } else {
            let prev_block = self.get_item(block.height.saturating_sub(1));
            if let Some(prev_block) = prev_block {
                if prev_block.block_hash != block.previous_block_hash {
                    // Use println! here because errors and tracing are not getting propagated
                    println!(
                        "Panic: prev_block at index {} does not match current block's prev_block_hash",
                        block.height.saturating_sub(1)
                    );
                    return Err(eyre::eyre!(
                        "prev_block at index {} does not match current block's prev_block_hash",
                        block.height.saturating_sub(1)
                    ));
                }
                (
                    prev_block.ledgers[DataLedger::Publish].max_chunk_offset + pub_chunks_added,
                    prev_block.ledgers[DataLedger::Submit].max_chunk_offset + sub_chunks_added,
                )
            } else {
                // Use println! here because errors and tracing are not getting propagated
                println!(
                    "Panic: prev_block at index {} not found in block_index",
                    block.height.saturating_sub(1)
                );
                return Err(eyre::eyre!(
                    "prev_block at index {} not found in block_index",
                    block.height.saturating_sub(1)
                ));
            }
        };

        let block_index_item = BlockIndexItem {
            block_hash: block.block_hash,
            num_ledgers: 2,
            ledgers: vec![
                LedgerIndexItem {
                    max_chunk_offset: max_publish_chunks,
                    tx_root: block.data_ledgers[DataLedger::Publish].tx_root,
                },
                LedgerIndexItem {
                    max_chunk_offset: max_submit_chunks,
                    tx_root: block.data_ledgers[DataLedger::Submit].tx_root,
                },
            ],
        };

        self.push_item(&block_index_item)
    }

    /// For a given byte offset in a ledger, what block was responsible for adding
    /// that byte to the data ledger?
    pub fn get_block_bounds(&self, ledger: DataLedger, chunk_offset: u64) -> BlockBounds {
        let mut block_bounds = BlockBounds {
            ledger,
            ..Default::default()
        };

        let result = self.get_block_index_item(ledger, chunk_offset);
        if let Ok((block_height, found_item)) = result {
            let previous_item = self.get_item(block_height - 1).unwrap();
            block_bounds.start_chunk_offset =
                previous_item.ledgers[ledger as usize].max_chunk_offset;
            block_bounds.end_chunk_offset = found_item.ledgers[ledger as usize].max_chunk_offset;
            block_bounds.tx_root = found_item.ledgers[ledger as usize].tx_root;
            block_bounds.height = block_height as u128;
        }
        block_bounds
    }

    pub fn get_block_index_item(
        &self,
        ledger: DataLedger,
        chunk_offset: u64,
    ) -> Result<(u64, &BlockIndexItem)> {
        let result = self.items.binary_search_by(|item| {
            if chunk_offset < item.ledgers[ledger as usize].max_chunk_offset {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Less
            }
        });

        // It's the nature of binary_search_by to return Err if it doesn't find
        // an exact match. We are looking for the position of the closest element
        // so we ignore the Result enum values and extract the pos return val.
        let index = match result {
            Ok(pos) => pos,
            Err(pos) => pos,
        };

        Ok((index as u64, &self.items[index]))
    }

    pub fn print_items(&self) {
        for height in 0..self.num_blocks() {
            println!(
                "height: {} hash: {}",
                height,
                self.get_item(height).unwrap().block_hash.0.to_base58()
            );
        }
    }
}

/// `BlockBounds` describe the size of a ledger at the start of a block
/// and then after the blocks transactions were applied to the ledger
#[derive(Debug, Default, Clone, PartialEq, Eq, MessageResponse)]
pub struct BlockBounds {
    /// Block height where these bounds apply
    pub height: u128,
    /// Target ledger (Publish or Submit)
    pub ledger: DataLedger,
    /// First chunk offset included in this block (inclusive)
    pub start_chunk_offset: u64,
    /// Final chunk offset after processing block transactions
    pub end_chunk_offset: u64,
    /// Merkle root (`tx_root`) of all transactions this block applied to the ledger
    pub tx_root: H256,
}

fn append_item(item: &BlockIndexItem, file_path: &Path) -> eyre::Result<()> {
    match OpenOptions::new().append(true).open(file_path) {
        Ok(mut file) => {
            file.write_all(&item.to_bytes())?;
            Ok(())
        }
        Err(err) => Err(eyre::eyre!(
            "While trying to open file :{:?} got error: {}",
            file_path,
            err
        )),
    }
}

#[tracing::instrument(skip_all, err)]
fn load_index_from_file(file_path: &Path) -> eyre::Result<Vec<BlockIndexItem>> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(file_path)?;

    // Determine the file size
    let file_size = file.seek(SeekFrom::End(0))?;
    file.seek(SeekFrom::Start(0))?;

    let mut buffer = vec![0_u8; file_size as usize];
    file.read_exact(&mut buffer)?;

    let mut block_index_items = Vec::new();
    let mut offset = 0;

    // Read until we can't get another complete item
    while offset + 33 <= buffer.len() {
        // Read num_ledgers to determine full item size
        let num_ledgers = buffer[offset + 32] as usize;
        let item_size = 33 + (num_ledgers * 40); // 33 bytes header + ledger items

        // Ensure we have enough bytes for the full item
        if offset + item_size > buffer.len() {
            break;
        }

        // Deserialize the item
        let item = BlockIndexItem::from_bytes(&buffer[offset..offset + item_size]);
        block_index_items.push(item);

        offset += item_size;
    }

    Ok(block_index_items)
}

#[cfg(test)]
mod tests {
    use super::BlockIndex;
    use super::*;
    use crate::BlockBounds;
    use irys_testing_utils::utils::setup_tracing_and_temp_dir;
    use irys_types::H256;
    use std::fs::{self, File};

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

    #[test_log::test(tokio::test)]
    async fn read_and_write_block_index() -> eyre::Result<()> {
        let tmp_dir = setup_tracing_and_temp_dir(Some("read_and_write_block_index"), false);
        let base_path = tmp_dir.path().to_path_buf();
        let mut node_config = NodeConfig::testnet();
        node_config.base_directory = base_path;

        let block_items = vec![
            BlockIndexItem {
                block_hash: H256::random(),
                num_ledgers: 2,
                ledgers: vec![
                    LedgerIndexItem {
                        max_chunk_offset: 100,
                        tx_root: H256::random(),
                    },
                    LedgerIndexItem {
                        max_chunk_offset: 1000,
                        tx_root: H256::random(),
                    },
                ],
            },
            BlockIndexItem {
                block_hash: H256::random(),
                num_ledgers: 2,
                ledgers: vec![
                    LedgerIndexItem {
                        max_chunk_offset: 200,
                        tx_root: H256::random(),
                    },
                    LedgerIndexItem {
                        max_chunk_offset: 2000,
                        tx_root: H256::random(),
                    },
                ],
            },
            BlockIndexItem {
                block_hash: H256::random(),
                num_ledgers: 2,
                ledgers: vec![
                    LedgerIndexItem {
                        max_chunk_offset: 300,
                        tx_root: H256::random(),
                    },
                    LedgerIndexItem {
                        max_chunk_offset: 3000,
                        tx_root: H256::random(),
                    },
                ],
            },
        ];

        save_block_index(&block_items, &node_config)?;

        // Load the items from disk
        let block_index = BlockIndex::new(&node_config).await?;

        println!("{:?}", block_index.items);

        assert_eq!(block_index.items.len(), 3);
        assert_eq!(*block_index.get_item(0).unwrap(), block_items[0]);
        assert_eq!(*block_index.get_item(1).unwrap(), block_items[1]);
        assert_eq!(*block_index.get_item(2).unwrap(), block_items[2]);

        let block_bounds = block_index.get_block_bounds(DataLedger::Publish, 150);
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

        let block_bounds = block_index.get_block_bounds(DataLedger::Submit, 1000);
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
        assert_eq!(*item, block_items[2]);

        Ok(())
    }
}
