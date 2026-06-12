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

/// Clear commitment transaction metadata (used during re-orgs)
pub fn clear_commitment_tx_metadata(
    tx: &(impl DbTxMut + DbTx),
    tx_id: &H256,
) -> Result<(), reth_db::DatabaseError> {
    // Delete the metadata entry entirely since we can't store without included_height
    tx.delete::<IrysCommitmentTxMetadata>(*tx_id, None)?;
    Ok(())
}

/// Batch operation: Set included_height for multiple commitment transactions
pub fn batch_set_commitment_tx_included_height<'a>(
    tx: &(impl DbTxMut + DbTx),
    tx_ids: impl IntoIterator<Item = &'a H256>,
    height: u64,
) -> Result<(), reth_db::DatabaseError> {
    for tx_id in tx_ids {
        set_commitment_tx_included_height(tx, tx_id, height)?;
    }
    Ok(())
}

/// Batch operation: Clear commitment transaction metadata (re-org handling)
pub fn batch_clear_commitment_tx_metadata<'a>(
    tx: &(impl DbTxMut + DbTx),
    tx_ids: impl IntoIterator<Item = &'a H256>,
) -> Result<(), reth_db::DatabaseError> {
    for tx_id in tx_ids {
        clear_commitment_tx_metadata(tx, tx_id)?;
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
    if !metadata.is_included() {
        return Err(reth_db::DatabaseError::Other(
            "Cannot set promoted_height without included_height".to_string(),
        ));
    }
    metadata.promoted_height = Some(height);
    put_data_tx_metadata(tx, tx_id, metadata)
}

/// Clear data transaction metadata (used during re-orgs)
pub fn clear_data_tx_metadata(
    tx: &(impl DbTxMut + DbTx),
    tx_id: &H256,
) -> Result<(), reth_db::DatabaseError> {
    // Delete the metadata entry entirely since we can't store without included_height
    tx.delete::<IrysDataTxMetadata>(*tx_id, None)?;
    Ok(())
}

/// Clear the promoted_height for a data transaction (used during re-orgs)
///
/// If `included_height` is still set, the row is kept with only `promoted_height` cleared.
/// If neither field would remain set after the clear, the row is deleted entirely.
pub fn clear_data_tx_promoted_height(
    tx: &(impl DbTxMut + DbTx),
    tx_id: &H256,
) -> Result<(), reth_db::DatabaseError> {
    let Some(mut metadata) = get_data_tx_metadata(tx, tx_id)? else {
        // Row absent — no-op; delete is safe but unnecessary.
        return Ok(());
    };
    metadata.promoted_height = None;
    // Only keep the row if included_height is still meaningful.
    if metadata.is_included() {
        put_data_tx_metadata(tx, tx_id, metadata)
    } else {
        tx.delete::<IrysDataTxMetadata>(*tx_id, None)?;
        Ok(())
    }
}

/// Batch operation: Set included_height for multiple data transactions
pub fn batch_set_data_tx_included_height<'a>(
    tx: &(impl DbTxMut + DbTx),
    tx_ids: impl IntoIterator<Item = &'a H256>,
    height: u64,
) -> Result<(), reth_db::DatabaseError> {
    for tx_id in tx_ids {
        set_data_tx_included_height(tx, tx_id, height)?;
    }
    Ok(())
}

/// Batch set the promoted_height for a list of data transactions
pub fn batch_set_data_tx_promoted_height<'a>(
    tx: &(impl DbTxMut + DbTx),
    tx_ids: impl IntoIterator<Item = &'a H256>,
    height: u64,
) -> Result<(), reth_db::DatabaseError> {
    for tx_id in tx_ids {
        set_data_tx_promoted_height(tx, tx_id, height)?;
    }
    Ok(())
}

/// Batch operation: Clear data transaction metadata (re-org handling)
pub fn batch_clear_data_tx_metadata<'a>(
    tx: &(impl DbTxMut + DbTx),
    tx_ids: impl IntoIterator<Item = &'a H256>,
) -> Result<(), reth_db::DatabaseError> {
    for tx_id in tx_ids {
        clear_data_tx_metadata(tx, tx_id)?;
    }
    Ok(())
}

/// Batch clear the promoted_height for a list of data transactions
pub fn batch_clear_data_tx_promoted_height<'a>(
    tx: &(impl DbTxMut + DbTx),
    tx_ids: impl IntoIterator<Item = &'a H256>,
) -> Result<(), reth_db::DatabaseError> {
    for tx_id in tx_ids {
        clear_data_tx_promoted_height(tx, tx_id)?;
    }
    Ok(())
}

/// Clear the included_height for a data transaction (used during re-orgs of term-ledger blocks).
///
/// Always deletes the row. A row with `{included: None, promoted: Some}` is
/// semantically illegal — `promoted_height` requires the tx to have been
/// included at some prior height, and the reorg invariant in
/// `BlockMigrationService::persist_metadata` guarantees the matching Publish
/// block is also in `blocks_to_clear`. Its `clear_data_tx_promoted_height`
/// call becomes a no-op on the already-deleted row. If the tx is
/// re-canonical on the new fork, Phase 2 recreates the row via
/// `set_data_tx_included_height` and any matching Publish re-confirmation
/// restores `promoted_height` in the same transaction.
pub fn clear_data_tx_included_height(
    tx: &impl DbTxMut,
    tx_id: &H256,
) -> Result<(), reth_db::DatabaseError> {
    tx.delete::<IrysDataTxMetadata>(*tx_id, None)?;
    Ok(())
}

/// Batch operation: Clear included_height for multiple data transactions (re-org handling).
pub fn batch_clear_data_tx_included_height<'a>(
    tx: &impl DbTxMut,
    tx_ids: impl IntoIterator<Item = &'a H256>,
) -> Result<(), reth_db::DatabaseError> {
    for tx_id in tx_ids {
        clear_data_tx_included_height(tx, tx_id)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::ConsensusTables;
    use crate::{IrysDatabaseArgs as _, open_or_create_db};
    use reth_db::Database as _;
    use reth_db::mdbx::DatabaseArguments;

    #[test]
    fn test_commitment_metadata_operations() {
        let temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = open_or_create_db(
            temp_dir.path(),
            ConsensusTables::ALL,
            DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();

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
        db.update(|tx| clear_commitment_tx_metadata(tx, &tx_id))
            .unwrap()
            .unwrap();

        // Verify the metadata entry was deleted entirely
        let metadata = db
            .view(|tx| get_commitment_tx_metadata(tx, &tx_id))
            .unwrap()
            .unwrap();
        assert!(metadata.is_none());
    }

    #[test]
    fn test_data_metadata_operations() {
        let temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = open_or_create_db(
            temp_dir.path(),
            ConsensusTables::ALL,
            DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();

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

        // Clear metadata
        db.update(|tx| clear_data_tx_metadata(tx, &tx_id))
            .unwrap()
            .unwrap();

        // Verify the metadata entry was deleted entirely (since we can't store without included_height)
        let metadata = db
            .view(|tx| get_data_tx_metadata(tx, &tx_id))
            .unwrap()
            .unwrap();
        assert!(metadata.is_none());
    }

    #[test]
    fn test_batch_commitment_operations() {
        let temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = open_or_create_db(
            temp_dir.path(),
            ConsensusTables::ALL,
            DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();

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
        db.update(|tx| batch_clear_commitment_tx_metadata(tx, &tx_ids))
            .unwrap()
            .unwrap();

        // Verify all metadata entries were deleted entirely
        for tx_id in &tx_ids {
            let metadata = db
                .view(|tx| get_commitment_tx_metadata(tx, tx_id))
                .unwrap()
                .unwrap();
            assert!(metadata.is_none());
        }
    }

    // ---- clear_data_tx_included_height tests ----

    /// When only `included_height` is set, clearing it should delete the row entirely
    /// (since a row with neither field set cannot be stored).
    #[test]
    fn clear_included_height_only_set_deletes_row() {
        let temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = open_or_create_db(
            temp_dir.path(),
            ConsensusTables::ALL,
            DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();

        let tx_id = H256::random();
        db.update(|tx| set_data_tx_included_height(tx, &tx_id, 42))
            .unwrap()
            .unwrap();

        // Verify it's there
        let meta = db
            .view(|tx| get_data_tx_metadata(tx, &tx_id))
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(meta.included_height, Some(42));
        assert_eq!(meta.promoted_height, None);

        // Clear it
        db.update(|tx| clear_data_tx_included_height(tx, &tx_id))
            .unwrap()
            .unwrap();

        // Row should be gone
        let meta = db
            .view(|tx| get_data_tx_metadata(tx, &tx_id))
            .unwrap()
            .unwrap();
        assert!(
            meta.is_none(),
            "row should be deleted when no fields remain"
        );
    }

    /// Clearing `included_height` always deletes the row, even when
    /// `promoted_height` was set. A `{included: None, promoted: Some}` row is
    /// semantically illegal — `promoted_height` requires the tx to have been
    /// included at a prior height, and the reorg invariant guarantees the
    /// matching Publish block is also being cleared in the same Phase 1.
    #[test]
    fn clear_included_height_deletes_row_even_when_promoted_is_set() {
        let temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = open_or_create_db(
            temp_dir.path(),
            ConsensusTables::ALL,
            DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();

        let tx_id = H256::random();
        db.update(|tx| set_data_tx_included_height(tx, &tx_id, 10))
            .unwrap()
            .unwrap();
        db.update(|tx| set_data_tx_promoted_height(tx, &tx_id, 20))
            .unwrap()
            .unwrap();

        db.update(|tx| clear_data_tx_included_height(tx, &tx_id))
            .unwrap()
            .unwrap();

        let meta = db
            .view(|tx| get_data_tx_metadata(tx, &tx_id))
            .unwrap()
            .unwrap();
        assert!(
            meta.is_none(),
            "row must be deleted; {{None, Some}} is an illegal persistent state"
        );
    }

    /// Calling `clear_data_tx_included_height` on a missing row is a no-op and
    /// must not return an error.
    #[test]
    fn clear_included_height_missing_row_is_noop() {
        let temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = open_or_create_db(
            temp_dir.path(),
            ConsensusTables::ALL,
            DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();

        let tx_id = H256::random();
        // No setup — row doesn't exist.
        db.update(|tx| clear_data_tx_included_height(tx, &tx_id))
            .unwrap()
            .unwrap();

        // Still absent, no panic/error.
        let meta = db
            .view(|tx| get_data_tx_metadata(tx, &tx_id))
            .unwrap()
            .unwrap();
        assert!(meta.is_none());
    }

    /// Calling `clear_data_tx_promoted_height` on a missing row is a no-op and
    /// must not return an error.
    #[test]
    fn clear_promoted_height_missing_row_is_noop() {
        let temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = open_or_create_db(
            temp_dir.path(),
            ConsensusTables::ALL,
            DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();

        let tx_id = H256::random();
        // No setup — row doesn't exist.
        db.update(|tx| clear_data_tx_promoted_height(tx, &tx_id))
            .unwrap()
            .unwrap();

        // Still absent, no panic/error.
        let meta = db
            .view(|tx| get_data_tx_metadata(tx, &tx_id))
            .unwrap()
            .unwrap();
        assert!(meta.is_none());
    }

    /// `batch_clear_data_tx_included_height` clears all supplied tx_ids.
    #[test]
    fn batch_clear_included_height_happy_path() {
        let temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = open_or_create_db(
            temp_dir.path(),
            ConsensusTables::ALL,
            DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();

        let tx_ids: Vec<H256> = (0..3).map(|_| H256::random()).collect();

        // Set included height for all
        db.update(|tx| batch_set_data_tx_included_height(tx, &tx_ids, 55))
            .unwrap()
            .unwrap();

        // Batch-clear
        db.update(|tx| batch_clear_data_tx_included_height(tx, &tx_ids))
            .unwrap()
            .unwrap();

        // All rows should be gone (only included_height was set)
        for tx_id in &tx_ids {
            let meta = db
                .view(|tx| get_data_tx_metadata(tx, tx_id))
                .unwrap()
                .unwrap();
            assert!(meta.is_none(), "row for {tx_id:?} should be deleted");
        }
    }

    #[test]
    fn test_batch_data_operations() {
        let temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = open_or_create_db(
            temp_dir.path(),
            ConsensusTables::ALL,
            DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();

        let tx_ids = vec![H256::random(), H256::random(), H256::random()];

        // First set included height (required before setting promoted height)
        db.update(|tx| batch_set_data_tx_included_height(tx, &tx_ids, 100))
            .unwrap()
            .unwrap();

        // Batch set promoted height
        db.update(|tx| batch_set_data_tx_promoted_height(tx, &tx_ids, 200))
            .unwrap()
            .unwrap();

        // Verify all were set
        for tx_id in &tx_ids {
            let metadata = db
                .view(|tx| get_data_tx_metadata(tx, tx_id))
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(metadata.included_height, Some(100));
            assert_eq!(metadata.promoted_height, Some(200));
        }

        // Batch clear promoted height
        db.update(|tx| batch_clear_data_tx_promoted_height(tx, &tx_ids))
            .unwrap()
            .unwrap();

        // Verify the promoted height was cleared but included height remains
        for tx_id in &tx_ids {
            let metadata = db
                .view(|tx| get_data_tx_metadata(tx, tx_id))
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(metadata.included_height, Some(100));
            assert_eq!(metadata.promoted_height, None);
        }
    }
}
