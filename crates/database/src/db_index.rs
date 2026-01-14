/// Once data has been well confirmed by being part of a transaction that is
/// in a block with several confirmations, it can move out of the `db_cache`
/// and into a `db_index` that is able to store more properties (to support
/// mining) now that the data is confirmed.
const _X: u64 = 1;

use crate::tables::IrysTransactionMetadata;
use irys_types::{TransactionMetadata, H256};
use reth_db::transaction::{DbTx, DbTxMut};

/// Get transaction metadata by transaction ID
pub fn get_tx_metadata(
    tx: &impl DbTx,
    tx_id: &H256,
) -> Result<Option<TransactionMetadata>, reth_db::DatabaseError> {
    tx.get::<IrysTransactionMetadata>(*tx_id)
        .map(|opt| opt.map(|wrapper| wrapper.0))
}

/// Store or update transaction metadata
pub fn put_tx_metadata(
    tx: &impl DbTxMut,
    tx_id: &H256,
    metadata: TransactionMetadata,
) -> Result<(), reth_db::DatabaseError> {
    tx.put::<IrysTransactionMetadata>(*tx_id, metadata.into())
}

/// Set the included_height for a transaction
pub fn set_tx_included_height(
    tx: &(impl DbTxMut + DbTx),
    tx_id: &H256,
    height: u64,
) -> Result<(), reth_db::DatabaseError> {
    let mut metadata = get_tx_metadata(tx, tx_id)?.unwrap_or_default();
    metadata.included_height = Some(height);
    put_tx_metadata(tx, tx_id, metadata)
}

/// Set the promoted_height for a transaction
pub fn set_tx_promoted_height(
    tx: &(impl DbTxMut + DbTx),
    tx_id: &H256,
    height: u64,
) -> Result<(), reth_db::DatabaseError> {
    let mut metadata = get_tx_metadata(tx, tx_id)?.unwrap_or_default();
    metadata.promoted_height = Some(height);
    put_tx_metadata(tx, tx_id, metadata)
}

/// Clear the included_height for a transaction (used during re-orgs)
pub fn clear_tx_included_height(
    tx: &(impl DbTxMut + DbTx),
    tx_id: &H256,
) -> Result<(), reth_db::DatabaseError> {
    let mut metadata = get_tx_metadata(tx, tx_id)?.unwrap_or_default();
    metadata.included_height = None;
    put_tx_metadata(tx, tx_id, metadata)
}

/// Clear the promoted_height for a transaction (used during re-orgs)
pub fn clear_tx_promoted_height(
    tx: &(impl DbTxMut + DbTx),
    tx_id: &H256,
) -> Result<(), reth_db::DatabaseError> {
    let mut metadata = get_tx_metadata(tx, tx_id)?.unwrap_or_default();
    metadata.promoted_height = None;
    put_tx_metadata(tx, tx_id, metadata)
}

/// Batch operation: Set included_height for multiple transactions
pub fn batch_set_tx_included_height(
    tx: &(impl DbTxMut + DbTx),
    tx_ids: &[H256],
    height: u64,
) -> Result<(), reth_db::DatabaseError> {
    for tx_id in tx_ids {
        set_tx_included_height(tx, tx_id, height)?;
    }
    Ok(())
}

/// Batch set the promoted_height for a list of transactions
pub fn batch_set_tx_promoted_height(
    tx: &(impl DbTxMut + DbTx),
    tx_ids: &[H256],
    height: u64,
) -> Result<(), reth_db::DatabaseError> {
    for tx_id in tx_ids {
        set_tx_promoted_height(tx, tx_id, height)?;
    }
    Ok(())
}

/// Batch operation: Clear included_height for multiple transactions (re-org handling)
pub fn batch_clear_tx_included_height(
    tx: &(impl DbTxMut + DbTx),
    tx_ids: &[H256],
) -> Result<(), reth_db::DatabaseError> {
    for tx_id in tx_ids {
        clear_tx_included_height(tx, tx_id)?;
    }
    Ok(())
}

/// Batch clear the promoted_height for a list of transactions
pub fn batch_clear_tx_promoted_height(
    tx: &(impl DbTxMut + DbTx),
    tx_ids: &[H256],
) -> Result<(), reth_db::DatabaseError> {
    for tx_id in tx_ids {
        clear_tx_promoted_height(tx, tx_id)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_or_create_db;
    use crate::tables::IrysTables;
    use reth_db::Database as _;
    use tempfile::tempdir;

    #[test]
    fn test_tx_metadata_operations() {
        let temp_dir = tempdir().unwrap();
        let db = open_or_create_db(temp_dir.path(), IrysTables::ALL, None).unwrap();

        let tx_id = H256::random();

        // Initially no metadata
        let metadata = db.view(|tx| get_tx_metadata(tx, &tx_id)).unwrap().unwrap();
        assert!(metadata.is_none());

        // Set included height
        db.update(|tx| set_tx_included_height(tx, &tx_id, 100))
            .unwrap()
            .unwrap();

        // Verify it was set
        let metadata = db.view(|tx| get_tx_metadata(tx, &tx_id)).unwrap().unwrap();
        assert_eq!(metadata.unwrap().included_height, Some(100));

        // Clear it
        db.update(|tx| clear_tx_included_height(tx, &tx_id))
            .unwrap()
            .unwrap();

        // Verify it was cleared
        let metadata = db.view(|tx| get_tx_metadata(tx, &tx_id)).unwrap().unwrap();
        assert_eq!(metadata.unwrap().included_height, None);
    }

    #[test]
    fn test_batch_operations() {
        let temp_dir = tempdir().unwrap();
        let db = open_or_create_db(temp_dir.path(), IrysTables::ALL, None).unwrap();

        let tx_ids = vec![H256::random(), H256::random(), H256::random()];

        // Batch set
        db.update(|tx| batch_set_tx_included_height(tx, &tx_ids, 200))
            .unwrap()
            .unwrap();

        // Verify all were set
        for tx_id in &tx_ids {
            let metadata = db.view(|tx| get_tx_metadata(tx, tx_id)).unwrap().unwrap();
            assert_eq!(metadata.unwrap().included_height, Some(200));
        }

        // Batch clear
        db.update(|tx| batch_clear_tx_included_height(tx, &tx_ids))
            .unwrap()
            .unwrap();

        // Verify all were cleared
        for tx_id in &tx_ids {
            let metadata = db.view(|tx| get_tx_metadata(tx, tx_id)).unwrap().unwrap();
            assert_eq!(metadata.unwrap().included_height, None);
        }
    }
}
