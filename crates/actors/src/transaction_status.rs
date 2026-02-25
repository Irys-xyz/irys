use irys_domain::BlockIndexReadGuard;
use irys_types::{
    CommitmentTransactionMetadata, DataTransactionMetadata, H256, TransactionStatusResponse,
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
    let mempool_metadata = mempool_guard.get_tx_metadata(tx_id).await;
    let in_mempool = mempool_metadata.is_some() || mempool_guard.is_recent_valid_tx(tx_id).await;

    // Prefer DB metadata over mempool: BlockMigrationService writes included_height
    // to DB synchronously at confirmation time, so it is always authoritative.
    // Mempool metadata is the fallback for pending txs not yet in the DB.
    let metadata = db_metadata.or(mempool_metadata);

    match (metadata, in_mempool) {
        (Some(metadata), _) if metadata.included_height().is_some() => {
            let included_height = metadata.included_height().unwrap();

            let block_index = block_index_guard.read();
            // Check if the block has been migrated to index
            let is_finalized = block_index.get_item(included_height).is_some();

            if is_finalized {
                Ok(Some(TransactionStatusResponse::finalized(
                    included_height,
                    current_head_height,
                )))
            } else {
                Ok(Some(TransactionStatusResponse::confirmed(
                    included_height,
                    current_head_height,
                )))
            }
        }
        (_, true) => Ok(Some(TransactionStatusResponse::pending())),
        _ => Ok(None),
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
