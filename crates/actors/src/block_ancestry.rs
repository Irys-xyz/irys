use irys_database::{block_header_by_hash, db::IrysDatabaseExt as _};
use irys_domain::{BlockTree, BlockTreeReadGuard};
use irys_types::{H256, IrysBlockHeader, app_state::DatabaseProvider};
use std::ops::ControlFlow;

/// Resolve a block header by hash against an already-held tree read guard,
/// falling back to the DB for headers evicted from the retained tree window.
pub(crate) fn block_header_from_tree_then_db(
    tree: &BlockTree,
    db: &DatabaseProvider,
    hash: &H256,
) -> eyre::Result<Option<IrysBlockHeader>> {
    if let Some(header) = tree.get_block(hash) {
        return Ok(Some(header.clone()));
    }
    db.view_eyre(|tx| block_header_by_hash(tx, hash, false))
}

/// Walk ancestors by hash starting at `start_hash`, visiting every resolved
/// header with `height >= min_height`.
///
/// The walk is branch-correct: it resolves against the supplied block tree
/// first, then continues in the DB once the ancestry leaves the retained tree
/// window. An in-range ancestor missing from both sources is treated as a local
/// inconsistency and fails closed.
pub(crate) fn walk_ancestors_tree_then_db(
    block_tree: &BlockTreeReadGuard,
    db: &DatabaseProvider,
    start_hash: H256,
    min_height: u64,
    mut visit: impl FnMut(&IrysBlockHeader) -> eyre::Result<ControlFlow<()>>,
) -> eyre::Result<()> {
    let mut cursor = start_hash;
    let db_cursor = {
        let tree = block_tree.read();
        loop {
            let Some(header) = tree.get_block(&cursor) else {
                break Some(cursor);
            };
            if header.height < min_height {
                break None;
            }
            let is_floor = header.height == min_height;
            if matches!(visit(header)?, ControlFlow::Break(())) || header.height == 0 || is_floor {
                break None;
            }
            cursor = header.previous_block_hash;
        }
    };

    if let Some(mut cursor) = db_cursor {
        db.view_eyre(|tx| {
            loop {
                let Some(header) = block_header_by_hash(tx, &cursor, false)? else {
                    eyre::bail!(
                        "ancestor walk: ancestor {cursor} at or above min_height {min_height} \
                         is missing from both the block tree and the DB"
                    );
                };
                if header.height < min_height {
                    break;
                }
                let is_floor = header.height == min_height;
                if matches!(visit(&header)?, ControlFlow::Break(()))
                    || header.height == 0
                    || is_floor
                {
                    break;
                }
                cursor = header.previous_block_hash;
            }
            Ok(())
        })?;
    }

    Ok(())
}
