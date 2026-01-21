/// Once data has been well confirmed by being part of a transaction that is
/// in a block with several confirmations, it can move out of the `db_cache`
/// and into a `db_index` that is able to store more properties (to support
/// mining) now that the data is confirmed.
const _X: u64 = 1;

use crate::tables::{IrysCommitmentTxMetadata, IrysDataTxMetadata};
use irys_types::{CommitmentTransactionMetadata, DataTransactionMetadata, H256};
use reth_db::transaction::{DbTx, DbTxMut};

// ==================== Commitment Metadata Operations ====================

/// Get commitment transaction metadata by transaction ID
pub fn get_commitment_tx_metadata(
    tx: &impl DbTx,
    tx_id: &H256,
) -> Result<Option<CommitmentTransactionMetadata>, reth_db::DatabaseError> {
    tx.get::<IrysCommitmentTxMetadata>(*tx_id)
        .map(|opt| opt.map(|wrapper| wrapper.0))
}

/// Store or update commitment transaction metadata
pub fn put_commitment_tx_metadata(
    tx: &impl DbTxMut,
    tx_id: &H256,
    metadata: CommitmentTransactionMetadata,
) -> Result<(), reth_db::DatabaseError> {
    if !metadata.is_included() {
        return Err(reth_db::DatabaseError::Other(
            "Cannot store commitment transaction metadata without inclusion height".to_string(),
        ));
    }
    tx.put::<IrysCommitmentTxMetadata>(*tx_id, metadata.into())
}

/// Set the included_height for a commitment transaction
pub fn set_commitment_tx_included_height(
    tx: &(impl DbTxMut + DbTx),
    tx_id: &H256,
    height: u64,
) -> Result<(), reth_db::DatabaseError> {
    let mut metadata = get_commitment_tx_metadata(tx, tx_id)?.unwrap_or_default();
    metadata.included_height = Some(height);
    put_commitment_tx_metadata(tx, tx_id, metadata)
}

/// Clear the included_height for a commitment transaction (used during re-orgs)
pub fn clear_commitment_tx_included_height(
    tx: &(impl DbTxMut + DbTx),
    tx_id: &H256,
) -> Result<(), reth_db::DatabaseError> {
    let mut metadata = get_commitment_tx_metadata(tx, tx_id)?.unwrap_or_default();
    metadata.included_height = None;
    put_commitment_tx_metadata(tx, tx_id, metadata)
}

/// Batch operation: Set included_height for multiple commitment transactions
pub fn batch_set_commitment_tx_included_height(
    tx: &(impl DbTxMut + DbTx),
    tx_ids: &[H256],
    height: u64,
) -> Result<(), reth_db::DatabaseError> {
    for tx_id in tx_ids {
        set_commitment_tx_included_height(tx, tx_id, height)?;
    }
    Ok(())
}

/// Batch operation: Clear included_height for multiple commitment transactions (re-org handling)
pub fn batch_clear_commitment_tx_included_height(
    tx: &(impl DbTxMut + DbTx),
    tx_ids: &[H256],
) -> Result<(), reth_db::DatabaseError> {
    for tx_id in tx_ids {
        clear_commitment_tx_included_height(tx, tx_id)?;
    }
    Ok(())
}

// ==================== Data Metadata Operations ====================

/// Get data transaction metadata by transaction ID
pub fn get_data_tx_metadata(
    tx: &impl DbTx,
    tx_id: &H256,
) -> Result<Option<DataTransactionMetadata>, reth_db::DatabaseError> {
    tx.get::<IrysDataTxMetadata>(*tx_id)
        .map(|opt| opt.map(|wrapper| wrapper.0))
}

/// Store or update data transaction metadata
pub fn put_data_tx_metadata(
    tx: &impl DbTxMut,
    tx_id: &H256,
    metadata: DataTransactionMetadata,
) -> Result<(), reth_db::DatabaseError> {
    if !metadata.is_included() {
        return Err(reth_db::DatabaseError::Other(
            "Cannot store data transaction metadata without inclusion height".to_string(),
        ));
    }
    tx.put::<IrysDataTxMetadata>(*tx_id, metadata.into())
}

/// Set the included_height for a data transaction
pub fn set_data_tx_included_height(
    tx: &(impl DbTxMut + DbTx),
    tx_id: &H256,
    height: u64,
) -> Result<(), reth_db::DatabaseError> {
    let mut metadata = get_data_tx_metadata(tx, tx_id)?.unwrap_or_default();
    metadata.included_height = Some(height);
    put_data_tx_metadata(tx, tx_id, metadata)
}

/// Set the promoted_height for a data transaction
pub fn set_data_tx_promoted_height(
    tx: &(impl DbTxMut + DbTx),
    tx_id: &H256,
    height: u64,
) -> Result<(), reth_db::DatabaseError> {
    let mut metadata = get_data_tx_metadata(tx, tx_id)?.unwrap_or_default();
    metadata.promoted_height = Some(height);
    put_data_tx_metadata(tx, tx_id, metadata)
}

/// Clear the included_height for a data transaction (used during re-orgs)
pub fn clear_data_tx_included_height(
    tx: &(impl DbTxMut + DbTx),
    tx_id: &H256,
) -> Result<(), reth_db::DatabaseError> {
    let mut metadata = get_data_tx_metadata(tx, tx_id)?.unwrap_or_default();
    metadata.included_height = None;
    put_data_tx_metadata(tx, tx_id, metadata)
}

/// Clear the promoted_height for a data transaction (used during re-orgs)
pub fn clear_data_tx_promoted_height(
    tx: &(impl DbTxMut + DbTx),
    tx_id: &H256,
) -> Result<(), reth_db::DatabaseError> {
    let mut metadata = get_data_tx_metadata(tx, tx_id)?.unwrap_or_default();
    metadata.promoted_height = None;
    put_data_tx_metadata(tx, tx_id, metadata)
}

/// Batch operation: Set included_height for multiple data transactions
pub fn batch_set_data_tx_included_height(
    tx: &(impl DbTxMut + DbTx),
    tx_ids: &[H256],
    height: u64,
) -> Result<(), reth_db::DatabaseError> {
    for tx_id in tx_ids {
        set_data_tx_included_height(tx, tx_id, height)?;
    }
    Ok(())
}

/// Batch set the promoted_height for a list of data transactions
pub fn batch_set_data_tx_promoted_height(
    tx: &(impl DbTxMut + DbTx),
    tx_ids: &[H256],
    height: u64,
) -> Result<(), reth_db::DatabaseError> {
    for tx_id in tx_ids {
        set_data_tx_promoted_height(tx, tx_id, height)?;
    }
    Ok(())
}

/// Batch operation: Clear included_height for multiple data transactions (re-org handling)
pub fn batch_clear_data_tx_included_height(
    tx: &(impl DbTxMut + DbTx),
    tx_ids: &[H256],
) -> Result<(), reth_db::DatabaseError> {
    for tx_id in tx_ids {
        clear_data_tx_included_height(tx, tx_id)?;
    }
    Ok(())
}

/// Batch clear the promoted_height for a list of data transactions
pub fn batch_clear_data_tx_promoted_height(
    tx: &(impl DbTxMut + DbTx),
    tx_ids: &[H256],
) -> Result<(), reth_db::DatabaseError> {
    for tx_id in tx_ids {
        clear_data_tx_promoted_height(tx, tx_id)?;
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
    fn test_commitment_metadata_operations() {
        let temp_dir = tempdir().unwrap();
        let db = open_or_create_db(temp_dir.path(), IrysTables::ALL, None).unwrap();

        let tx_id = H256::random();

        // Initially no metadata
        let metadata = db
            .view(|tx| get_commitment_tx_metadata(tx, &tx_id))
            .unwrap()
            .unwrap();
        assert!(metadata.is_none());

        // Set included height
        db.update(|tx| set_commitment_tx_included_height(tx, &tx_id, 100))
            .unwrap()
            .unwrap();

        // Verify it was set
        let metadata = db
            .view(|tx| get_commitment_tx_metadata(tx, &tx_id))
            .unwrap()
            .unwrap();
        assert_eq!(metadata.unwrap().included_height, Some(100));

        // Clear it
        db.update(|tx| clear_commitment_tx_included_height(tx, &tx_id))
            .unwrap()
            .unwrap();

        // Verify it was cleared
        let metadata = db
            .view(|tx| get_commitment_tx_metadata(tx, &tx_id))
            .unwrap()
            .unwrap();
        assert_eq!(metadata.unwrap().included_height, None);
    }

    #[test]
    fn test_data_metadata_operations() {
        let temp_dir = tempdir().unwrap();
        let db = open_or_create_db(temp_dir.path(), IrysTables::ALL, None).unwrap();

        let tx_id = H256::random();

        // Initially no metadata
        let metadata = db
            .view(|tx| get_data_tx_metadata(tx, &tx_id))
            .unwrap()
            .unwrap();
        assert!(metadata.is_none());

        // Set included height
        db.update(|tx| set_data_tx_included_height(tx, &tx_id, 100))
            .unwrap()
            .unwrap();

        // Verify it was set
        let metadata = db
            .view(|tx| get_data_tx_metadata(tx, &tx_id))
            .unwrap()
            .unwrap();
        assert_eq!(metadata.unwrap().included_height, Some(100));

        // Set promoted height
        db.update(|tx| set_data_tx_promoted_height(tx, &tx_id, 200))
            .unwrap()
            .unwrap();

        // Verify both are set
        let metadata = db
            .view(|tx| get_data_tx_metadata(tx, &tx_id))
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(metadata.included_height, Some(100));
        assert_eq!(metadata.promoted_height, Some(200));

        // Clear included height
        db.update(|tx| clear_data_tx_included_height(tx, &tx_id))
            .unwrap()
            .unwrap();

        // Verify it was cleared but promoted height remains
        let metadata = db
            .view(|tx| get_data_tx_metadata(tx, &tx_id))
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(metadata.included_height, None);
        assert_eq!(metadata.promoted_height, Some(200));

        // Clear promoted height
        db.update(|tx| clear_data_tx_promoted_height(tx, &tx_id))
            .unwrap()
            .unwrap();

        // Verify it was cleared
        let metadata = db
            .view(|tx| get_data_tx_metadata(tx, &tx_id))
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(metadata.promoted_height, None);
    }

    #[test]
    fn test_batch_commitment_operations() {
        let temp_dir = tempdir().unwrap();
        let db = open_or_create_db(temp_dir.path(), IrysTables::ALL, None).unwrap();

        let tx_ids = vec![H256::random(), H256::random(), H256::random()];

        // Batch set
        db.update(|tx| batch_set_commitment_tx_included_height(tx, &tx_ids, 200))
            .unwrap()
            .unwrap();

        // Verify all were set
        for tx_id in &tx_ids {
            let metadata = db
                .view(|tx| get_commitment_tx_metadata(tx, tx_id))
                .unwrap()
                .unwrap();
            assert_eq!(metadata.unwrap().included_height, Some(200));
        }

        // Batch clear
        db.update(|tx| batch_clear_commitment_tx_included_height(tx, &tx_ids))
            .unwrap()
            .unwrap();

        // Verify all were cleared
        for tx_id in &tx_ids {
            let metadata = db
                .view(|tx| get_commitment_tx_metadata(tx, tx_id))
                .unwrap()
                .unwrap();
            assert_eq!(metadata.unwrap().included_height, None);
        }
    }

    #[test]
    fn test_batch_data_operations() {
        let temp_dir = tempdir().unwrap();
        let db = open_or_create_db(temp_dir.path(), IrysTables::ALL, None).unwrap();

        let tx_ids = vec![H256::random(), H256::random(), H256::random()];

        // Batch set promoted height
        db.update(|tx| batch_set_data_tx_promoted_height(tx, &tx_ids, 200))
            .unwrap()
            .unwrap();

        // Verify all were set
        for tx_id in &tx_ids {
            let metadata = db
                .view(|tx| get_data_tx_metadata(tx, tx_id))
                .unwrap()
                .unwrap();
            assert_eq!(metadata.unwrap().promoted_height, Some(200));
        }

        // Batch clear promoted height
        db.update(|tx| batch_clear_data_tx_promoted_height(tx, &tx_ids))
            .unwrap()
            .unwrap();

        // Verify all were cleared
        for tx_id in &tx_ids {
            let metadata = db
                .view(|tx| get_data_tx_metadata(tx, tx_id))
                .unwrap()
                .unwrap();
            assert_eq!(metadata.unwrap().promoted_height, None);
        }
    }
}
