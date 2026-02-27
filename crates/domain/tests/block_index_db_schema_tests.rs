use irys_database::{
    block_index_item_by_height, db::IrysDatabaseExt as _, insert_block_index_items_for_block,
    open_or_create_db, prune_ledger_range, tables::IrysTables,
};
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_types::{DataLedger, DataTransactionLedger, H256, H256List, IrysBlockHeader, hash_sha256};
use tracing::info;

#[test_log::test(test)]
fn index_and_prune_block_index() -> eyre::Result<()> {
    let tmp_dir = setup_tracing_and_temp_dir(Some("index_and_prune_block_index"), false);
    let base_path = tmp_dir.path().to_path_buf();
    info!("temp_dir:{:?}\nbase_path:{:?}", tmp_dir, base_path);
    let db = open_or_create_db(tmp_dir, IrysTables::ALL, None).unwrap();

    let tx_ids = H256List::new();

    // Create a sequence of blocks that simulate chunks being added to each ledger
    let headers: Vec<IrysBlockHeader> = (0..10)
        .map(|i| {
            let mut header = IrysBlockHeader::new_mock_header();
            // Give each block its own height
            header.height = (i + 1) as u64;

            // Generate an artificial block hash
            header.block_hash = H256(hash_sha256(&header.height.to_le_bytes()));

            // Add the new term ledgers to the block (mock header includes publish & submit)
            let total_chunks = (i * 20) as u64;
            header.data_ledgers.push(DataTransactionLedger {
                ledger_id: DataLedger::OneYear.into(),
                tx_root: H256(hash_sha256(&total_chunks.to_le_bytes())),
                tx_ids: tx_ids.clone(),
                total_chunks,
                expires: Some(365),
                proofs: None,
                required_proof_count: None,
            });

            let total_chunks = (i * 11) as u64;
            header.data_ledgers.push(DataTransactionLedger {
                ledger_id: DataLedger::ThirtyDay.into(),
                tx_root: H256(hash_sha256(&total_chunks.to_le_bytes())),
                tx_ids: tx_ids.clone(),
                total_chunks,
                expires: Some(30),
                proofs: None,
                required_proof_count: None,
            });

            header
        })
        .collect();

    // Add the blocks to the db creating LedgerIndexItems for each ledger in each block
    db.update_eyre(|tx| {
        for header in &headers {
            insert_block_index_items_for_block(tx, header)?;
        }
        Ok(())
    })?;

    // Validate the update by reading back the results and comparing to the original block headers
    db.view_eyre(|tx| {
        for i in 0..10 {
            let item = block_index_item_by_height(tx, &(i as u64 + 1))?;
            assert_eq!(item.block_hash, headers[i].block_hash);
            assert_eq!(item.num_ledgers, 4);

            // Loop though each ledger_item in the BlockIndexItem
            for ledger_item in &item.ledgers {
                // Compare the ledger_item to the data_ledgers in the block header
                let block = &headers[i];

                // The database may return the ledger_items in a different order
                let ledger = &block
                    .data_ledgers
                    .iter()
                    .find(|l| l.ledger_id == ledger_item.ledger);

                // So make sure we find it
                assert!(ledger.is_some());
                let ledger = ledger.unwrap();

                // Then validate the fields
                assert_eq!(ledger_item.ledger, ledger.ledger_id);
                assert_eq!(ledger_item.total_chunks, ledger.total_chunks);
                assert_eq!(ledger_item.tx_root, ledger.tx_root);
            }
        }
        Ok(())
    })?;

    // Test pruning 6 blocks of the ThirtyDay ledger
    db.update_eyre(|tx| prune_ledger_range(tx, DataLedger::ThirtyDay, 1..7))?;

    // Verify that the ledger items are indeed pruned
    db.view_eyre(|tx| {
        for i in 0..10 {
            let item = block_index_item_by_height(tx, &(i as u64 + 1))?;

            // Create a helper closure to reduce code duplication
            let has_ledger = |ledger: DataLedger| item.ledgers.iter().any(|l| l.ledger == ledger);

            // ThirtyDay should be pruned for first 6 blocks
            assert_eq!(has_ledger(DataLedger::ThirtyDay), i >= 6);

            // All other ledgers should remain
            assert!(has_ledger(DataLedger::OneYear));
            assert!(has_ledger(DataLedger::Submit));
            assert!(has_ledger(DataLedger::Publish));
        }
        Ok(())
    })?;

    // Now prune 3 blocks of the OneYear ledger
    db.update_eyre(|tx| prune_ledger_range(tx, DataLedger::OneYear, 1..4))?;

    // Verify that the ledger OneYear and ThirtyDay ledger items are indeed pruned
    db.view_eyre(|tx| {
        for i in 0..10 {
            let item = block_index_item_by_height(tx, &(i as u64 + 1))?;

            // Create a helper closure to reduce code duplication
            let has_ledger = |ledger: DataLedger| item.ledgers.iter().any(|l| l.ledger == ledger);

            // ThirtyDay should be pruned for first 6 blocks
            assert_eq!(has_ledger(DataLedger::ThirtyDay), i >= 6);

            // OneYear should be pruned for the first 3 blocks
            assert_eq!(has_ledger(DataLedger::OneYear), i >= 3);

            // All other ledgers should remain
            assert!(has_ledger(DataLedger::Submit));
            assert!(has_ledger(DataLedger::Publish));
        }
        Ok(())
    })?;

    // Try to prune and already pruned range
    db.update_eyre(|tx| prune_ledger_range(tx, DataLedger::OneYear, 1..4))?;

    // Try to prune a range that overlaps pruned and unpruned BlockIndexItems
    db.update_eyre(|tx| prune_ledger_range(tx, DataLedger::OneYear, 1..5))?;

    // Verify that the ledger OneYear and ThirtyDay ledger items are indeed pruned
    db.view_eyre(|tx| {
        for i in 0..10 {
            let item = block_index_item_by_height(tx, &(i as u64 + 1))?;

            // Create a helper closure to reduce code duplication
            let has_ledger = |ledger: DataLedger| item.ledgers.iter().any(|l| l.ledger == ledger);

            // ThirtyDay should be pruned for first 6 blocks
            assert_eq!(has_ledger(DataLedger::ThirtyDay), i >= 6);

            // OneYear should be pruned for the first 4 blocks
            assert_eq!(has_ledger(DataLedger::OneYear), i >= 4);

            // All other ledgers should remain
            assert!(has_ledger(DataLedger::Submit));
            assert!(has_ledger(DataLedger::Publish));
        }
        Ok(())
    })?;

    Ok(())
}
