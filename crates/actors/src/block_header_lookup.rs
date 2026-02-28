use irys_database::{block_header_by_hash, db::IrysDatabaseExt as _};
use irys_domain::BlockTreeReadGuard;
use irys_types::{DatabaseProvider, H256, IrysBlockHeader};

/// Look up a block header from the in-memory block tree, falling back to the database.
/// Set `include_chunk` to false to strip the PoA chunk field.
pub fn get_block_header(
    block_tree: &BlockTreeReadGuard,
    db: &DatabaseProvider,
    block_hash: H256,
    include_chunk: bool,
) -> eyre::Result<Option<IrysBlockHeader>> {
    // Try block tree first (in-memory, fast)
    let guard = block_tree.read();
    if let Some(block) = guard.get_block(&block_hash) {
        let mut block = block.clone();
        if !include_chunk {
            block.poa.chunk = None;
        }
        return Ok(Some(block));
    }
    drop(guard);

    // Fall back to database
    db.view_eyre(|tx| block_header_by_hash(tx, &block_hash, include_chunk))
}
