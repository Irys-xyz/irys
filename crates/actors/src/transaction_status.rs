use irys_domain::BlockIndexReadGuard;
use irys_types::{
    CommitmentTransactionMetadata, DataTransactionMetadata, TransactionStatusResponse, H256,
};

use crate::{MempoolReadGuard, TxMetadata};

/// Compute transaction status based on mempool state, metadata, and block index
/// The db_metadata parameter can contain either commitment or data metadata from the database
pub async fn compute_transaction_status(
    db_metadata: Option<TxMetadata>,
    tx_id: &H256,
    block_index_guard: &BlockIndexReadGuard,
    current_head_height: u64,
    mempool_guard: &MempoolReadGuard,
) -> eyre::Result<Option<TransactionStatusResponse>> {
    // First check mempool for metadata
    let mempool_metadata = mempool_guard.get_tx_metadata(tx_id).await;

    // Check if in mempool
    let in_mempool = mempool_metadata.is_some() || mempool_guard.is_recent_valid_tx(tx_id).await;

    // Try mempool metadata first, then database
    let metadata = mempool_metadata.or(db_metadata);

    match (metadata, in_mempool) {
        (Some(metadata), _) if metadata.included_height().is_some() => {
            let included_height = metadata.included_height().unwrap();

            let block_index = block_index_guard.read();
            // Check if the block has been migrated to index
            let is_confirmed = block_index.get_item(included_height).is_some();

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

/// Helper to convert database metadata to TxMetadata enum
pub fn db_metadata_to_tx_metadata(
    commitment: Option<CommitmentTransactionMetadata>,
    data: Option<DataTransactionMetadata>,
) -> Option<TxMetadata> {
    data.map(TxMetadata::Data)
        .or_else(|| commitment.map(TxMetadata::Commitment))
}
