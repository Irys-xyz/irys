//! Manages a list of `{block_hash, weave_size, tx_root}`entries, indexed by
//! block height.
use crate::data_ledger::Ledger;
use color_eyre::eyre::Result;
use irys_types::H256;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;

/// This struct represents the `Uninitialized` block_index type state.
#[derive(Debug)]
pub struct Uninitialized;

/// This struct represents the `Initialized`  block_index type state.
#[derive(Debug)]
pub struct Initialized;

/// Stores an index of `{block_hash, ledgers: Vec<LedgerIndexItem>
/// ]` entries for each of Irys' blocks. Implemented using the type state
/// pattern which has [`Initialized`] and [`Uninitialized`] states that are
/// checked at compile time and prevent trying to read block index data from
/// an uninitialized block index
#[derive(Debug)]
pub struct BlockIndex<State = Uninitialized> {
    #[allow(dead_code)]
    state: State,
    /// Stored as a fixed size array with an Arc to allow multithreaded access
    items: Arc<[BlockIndexItem]>,
}

const HASH_INDEX_ITEM_SIZE: u64 = 48 + 16 + 32;
const FILE_PATH: &str = "data/index.dat";

/// Use a Type State pattern for BlockIndex with two states, Uninitialized and Initialized
impl BlockIndex {
    /// Constructs a new uninitialized block index.
    pub fn new() -> Self {
        BlockIndex {
            items: Arc::new([]),
            state: Uninitialized,
        }
    }
}

//==============================================================================
// Uninitialized State
//------------------------------------------------------------------------------

impl Default for BlockIndex<Uninitialized> {
    fn default() -> Self {
        BlockIndex::new()
    }
}

impl BlockIndex<Uninitialized> {
    /// Initializes a block index from disk, if this was a multi node network
    /// it could also read the latest block information from the network.
    pub async fn init(mut self) -> Result<BlockIndex<Initialized>> {
        // Ensure the path exists
        let path = Path::new(FILE_PATH);

        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }

        // Try to load the block index from disk
        match load_index_from_file() {
            Ok(indexes) => self.items = indexes.into(),
            Err(err) => println!("Error encountered\n {:?}", err),
        }

        // Return the "Initialized" state of the BlockIndex type
        Ok(BlockIndex {
            items: self.items,
            state: Initialized,
        })
    }

    /// Saves an empty block index to disk, resetting any persisted block state
    pub async fn reset() -> eyre::Result<()> {
        let block_items: Vec<BlockIndexItem> = Vec::new();
        save_block_index(&block_items)?;
        Ok(())
    }
}

//==============================================================================
// Initialized State
//------------------------------------------------------------------------------

impl BlockIndex<Initialized> {
    /// Retrieves the number of blocks in the index
    pub fn num_blocks(&self) -> u64 {
        self.items.len() as u64
    }

    /// Retrieves a [BlockIndexItem] from the block index by block height
    pub fn get_item(&self, block_height: usize) -> Option<&BlockIndexItem> {
        self.items.get(block_height)
    }

    /// Pushes a new [BlockIndexItem] onto the items array
    pub fn push_item(&mut self, block_index_item: &BlockIndexItem) {
        let mut items_vec = self.items.to_vec();
        items_vec.push(block_index_item.clone());
        self.items = items_vec.into();
    }

    /// For a given byte offset in a ledger, what block was responsible for adding
    /// that byte to the data ledger?
    pub fn get_block_bounds(&self, ledger: Ledger, byte_offset: u128) -> BlockBounds {
        let mut block_bounds: BlockBounds = Default::default();
        block_bounds.ledger = ledger;

        let result = self.get_block_index_item(ledger, byte_offset);
        if let Ok((block_height, found_item)) = result {
            let previous_item = self.get_item(block_height - 1).unwrap();
            block_bounds.start_ledger_offset = previous_item.ledgers[ledger as usize].ledger_size;
            block_bounds.end_ledger_offset = found_item.ledgers[ledger as usize].ledger_size;
            block_bounds.tx_root = found_item.ledgers[ledger as usize].tx_root;
            block_bounds.height = block_height as u128;
        }
        block_bounds
    }

    fn get_block_index_item(
        &self,
        ledger: Ledger,
        byte_offset: u128,
    ) -> Result<(usize, &BlockIndexItem)> {
        let result = self.items.binary_search_by(|item| {
            if byte_offset < item.ledgers[ledger as usize].ledger_size {
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

        Ok((index, &self.items[index]))
    }
}

/// BlockBounds describe the size of a ledger at the start of a block
/// and then after the blocks transactions were applied to the ledger
#[derive(Debug, Default, Clone, PartialEq)]
pub struct BlockBounds {
    /// Height of the block
    pub height: u128,
    /// Ledger for which the bounds are described
    pub ledger: Ledger,
    /// The (inclusive) ledger byte offset before the blocks tx are applied
    pub start_ledger_offset: u128,
    /// The (non inclusive) ledger byte offset after the blocks tx are applied
    pub end_ledger_offset: u128,
    /// The tx root of the ledger in that block
    pub tx_root: H256,
}

/// A [BlockIndexItem] contains a vec of [LedgerIndexItem]s which store the size
/// and and the tx_root of the ledger in that block.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LedgerIndexItem {
    /// Size in bytes of the ledger
    pub ledger_size: u128, // 16 bytes
    /// The merkle root of the TX that apply to this ledger in the current block
    pub tx_root: H256, // 32 bytes
}

impl LedgerIndexItem {
    fn to_bytes(&self) -> [u8; 48] {
        // Fixed size of 48 bytes
        let mut bytes = [0u8; 48];
        bytes[0..16].copy_from_slice(&self.ledger_size.to_le_bytes()); // First 16 bytes
        bytes[16..48].copy_from_slice(self.tx_root.as_bytes()); // Next 32 bytes
        bytes
    }

    fn from_bytes(bytes: &[u8]) -> Self {
        let mut item = LedgerIndexItem::default();

        // Read ledger size (first 16 bytes)
        let mut size_bytes = [0u8; 16];
        size_bytes.copy_from_slice(&bytes[0..16]);
        item.ledger_size = u128::from_le_bytes(size_bytes);

        // Read tx root (next 32 bytes)
        item.tx_root = H256::from_slice(&bytes[16..48]);

        item
    }
}

/// Core metadata of the [BlockIndex] this struct tracks the ledger size and
/// tx root for each ledger per block. Enabling lookups to that find the tx_root
/// for a ledger at a particular byte offset in the ledger.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BlockIndexItem {
    /// The hash of the block
    pub block_hash: H256, // 32 bytes
    /// The number of ledgers this block tracks
    pub num_ledgers: u8, // 1 byte
    /// The metadata about each of the blocks ledgers
    pub ledgers: Vec<LedgerIndexItem>, // Vec of 48 byte items
}

impl BlockIndexItem {
    // Serialize the BlockIndexItem to bytes
    fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(33 + self.ledgers.len() * 48);

        // Write fixed fields
        bytes.extend_from_slice(self.block_hash.as_bytes()); // 32 bytes
        bytes.push(self.num_ledgers); // 1 byte

        // Write each ledger item
        for ledger_index_item in &self.ledgers {
            bytes.extend_from_slice(&ledger_index_item.to_bytes()); // 48 bytes each
        }

        bytes
    }

    // Deserialize bytes to BlockIndexItem
    fn from_bytes(bytes: &[u8]) -> Self {
        let mut item = BlockIndexItem::default();

        // Read fixed fields
        item.block_hash = H256::from_slice(&bytes[0..32]);
        item.num_ledgers = bytes[32];

        // Read ledger items
        let num_ledgers = item.num_ledgers as usize;
        item.ledgers = Vec::with_capacity(num_ledgers);

        for i in 0..num_ledgers {
            let start = 33 + (i * 48);
            let ledger_bytes = &bytes[start..start + 48];
            item.ledgers.push(LedgerIndexItem::from_bytes(ledger_bytes));
        }

        item
    }
}

#[allow(dead_code)]
fn save_block_index(block_index_items: &[BlockIndexItem]) -> io::Result<()> {
    let mut file = File::create(FILE_PATH)?;
    for item in block_index_items {
        let bytes = item.to_bytes();
        file.write_all(&bytes)?;
    }
    Ok(())
}

#[allow(dead_code)]
fn read_item_at(block_height: u64) -> io::Result<BlockIndexItem> {
    let mut file = File::open(FILE_PATH)?;
    let mut buffer = [0; HASH_INDEX_ITEM_SIZE as usize];
    file.seek(SeekFrom::Start(block_height * HASH_INDEX_ITEM_SIZE))?;
    file.read_exact(&mut buffer)?;
    Ok(BlockIndexItem::from_bytes(&buffer))
}

#[allow(dead_code)]
fn append_item(item: BlockIndexItem) -> io::Result<()> {
    let mut file = OpenOptions::new().append(true).open(FILE_PATH)?;
    file.write_all(&item.to_bytes())?;
    Ok(())
}

#[allow(dead_code)]
fn append_items_to_file(items: &Vec<BlockIndexItem>) -> io::Result<()> {
    let mut file = OpenOptions::new().append(true).open(FILE_PATH)?;

    for item in items {
        file.write_all(&item.to_bytes())?;
    }

    Ok(())
}

#[allow(dead_code)]
fn update_file_item_at(block_height: u64, item: BlockIndexItem) -> io::Result<()> {
    let mut file = OpenOptions::new().read(true).write(true).open(FILE_PATH)?;
    file.seek(SeekFrom::Start(block_height * HASH_INDEX_ITEM_SIZE))?;
    file.write_all(&item.to_bytes())?;
    Ok(())
}
fn load_index_from_file() -> io::Result<Vec<BlockIndexItem>> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(FILE_PATH)?;

    // Determine the file size
    let file_size = file.seek(SeekFrom::End(0))?;
    file.seek(SeekFrom::Start(0))?;

    let mut buffer = vec![0u8; file_size as usize];
    file.read_exact(&mut buffer)?;

    let mut block_index_items = Vec::new();
    let mut offset = 0;

    // Read until we can't get another complete item
    while offset + 33 <= buffer.len() {
        // Read num_ledgers to determine full item size
        let num_ledgers = buffer[offset + 32] as usize;
        let item_size = 33 + (num_ledgers * 48); // 33 bytes header + ledger items

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
    use crate::{
        block_index::save_block_index, data_ledger::Ledger, BlockBounds, BlockIndexItem,
        LedgerIndexItem,
    };
    use assert_matches::assert_matches;
    use irys_types::H256;

    #[tokio::test]
    async fn read_and_write_block_index() -> eyre::Result<()> {
        let block_items = vec![
            BlockIndexItem {
                block_hash: H256::random(),
                num_ledgers: 2,
                ledgers: vec![
                    LedgerIndexItem {
                        ledger_size: 100,
                        tx_root: H256::random(),
                    },
                    LedgerIndexItem {
                        ledger_size: 1000,
                        tx_root: H256::random(),
                    },
                ],
            },
            BlockIndexItem {
                block_hash: H256::random(),
                num_ledgers: 2,
                ledgers: vec![
                    LedgerIndexItem {
                        ledger_size: 200,
                        tx_root: H256::random(),
                    },
                    LedgerIndexItem {
                        ledger_size: 2000,
                        tx_root: H256::random(),
                    },
                ],
            },
            BlockIndexItem {
                block_hash: H256::random(),
                num_ledgers: 2,
                ledgers: vec![
                    LedgerIndexItem {
                        ledger_size: 300,
                        tx_root: H256::random(),
                    },
                    LedgerIndexItem {
                        ledger_size: 3000,
                        tx_root: H256::random(),
                    },
                ],
            },
        ];

        let save_result = save_block_index(&block_items);
        assert_matches!(save_result, Ok(()));

        // Load the items from disk
        let block_index = BlockIndex::new();
        let block_index = block_index.init().await.unwrap();

        println!("{:?}", block_index.items);

        assert_eq!(block_index.items.len(), 3);
        assert_eq!(*block_index.get_item(0).unwrap(), block_items[0]);
        assert_eq!(*block_index.get_item(1).unwrap(), block_items[1]);
        assert_eq!(*block_index.get_item(2).unwrap(), block_items[2]);

        let block_bounds = block_index.get_block_bounds(Ledger::Publish, 150);
        assert_eq!(
            block_bounds,
            BlockBounds {
                height: 1,
                ledger: Ledger::Publish,
                start_ledger_offset: 100,
                end_ledger_offset: 200,
                tx_root: block_items[1].ledgers[Ledger::Publish as usize].tx_root
            }
        );

        let block_bounds = block_index.get_block_bounds(Ledger::Submit, 1000);
        assert_eq!(
            block_bounds,
            BlockBounds {
                height: 1,
                ledger: Ledger::Submit,
                start_ledger_offset: 1000,
                end_ledger_offset: 2000,
                tx_root: block_items[1].ledgers[Ledger::Submit as usize].tx_root
            }
        );

        let item = block_index.get_item(2).unwrap();
        assert_eq!(*item, block_items[2]);

        Ok(())
    }
}
