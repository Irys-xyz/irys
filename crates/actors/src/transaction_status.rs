use irys_database::db_index::get_tx_metadata;
use irys_domain::block_index::BlockIndex;
use irys_types::{TransactionStatusResponse, H256};
use reth_db::transaction::DbTx;

use crate::MempoolReadGuard;

/// Compute transaction status based on mempool state, metadata, and block index
/// 
/// Works with the wrapper pattern: checks mempool for wrapped transactions,
/// then falls back to database metadata for migrated/historical transactions.
pub fn compute_transaction_status(
    db_tx: &impl DbTx,
    tx_id: &H256,
    block_index: &BlockIndex,
    current_head_height: u64,
    mempool_guard: &MempoolReadGuard,
) -> eyre::Result<Option<TransactionStatusResponse>> {
    // First check mempool for metadata
    let mempool_metadata = mempool_guard.get_tx_metadata_blocking(tx_id);
    
    // Check if in mempool
    let in_mempool = mempool_metadata.is_some()
        || mempool_guard.is_recent_valid_tx_blocking(tx_id);

    // Try mempool metadata first, then database
    let metadata = if let Some(m) = mempool_metadata {
        Some(m)
    } else {
        get_tx_metadata(db_tx, tx_id).ok().flatten()
    };

    match (metadata, in_mempool) {
        (Some(metadata), _) if metadata.included_height.is_some() => {
            let included_height = metadata.included_height.unwrap();
            
            // Check if the block has been migrated to index
            let is_confirmed = block_index.contains_height(included_height);
            
            if is_confirmed {
                Ok(Some(TransactionStatusResponse::confirmed(
                    included_height,
                    current_head_height,
                )))
            } else {
                Ok(Some(TransactionStatusResponse::included(
                    included_height,
                    current_head_height,
                )))
            }
        }
        (_, true) => {
            // In mempool but not included in any block
            Ok(Some(TransactionStatusResponse::pending()))
        }
        _ => {
            // Not found anywhere
            Ok(None)
        }
    }
}
