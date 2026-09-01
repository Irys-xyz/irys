use crate::utils::IrysNodeTest;
use alloy_core::primitives::U256;
use alloy_genesis::GenesisAccount;
use irys_database::db::{IrysDatabaseExt as _, IrysDupCursorExt as _};
use irys_database::tables::CachedChunksIndex;
use irys_testing_utils::initialize_tracing;
use irys_types::{
    DataLedger, DataRoot, DatabaseProvider, IrysAddress, NodeConfig, irys::IrysSigner,
};
use reth_db::transaction::DbTx as _;
use std::time::Duration;

fn cached_chunk_count(db: &DatabaseProvider, data_root: DataRoot) -> eyre::Result<u32> {
    db.view_eyre(|tx| {
        let mut cursor = tx.cursor_dup_read::<CachedChunksIndex>()?;
        Ok(cursor.dup_count(data_root)?.unwrap_or(0))
    })
}

fn ingress_leaf_count(
    db: &DatabaseProvider,
    data_root: DataRoot,
    address: IrysAddress,
    expected_chunks: u32,
) -> eyre::Result<usize> {
    db.view_eyre(|tx| {
        (0..expected_chunks).try_fold(0_usize, |count, offset| {
            Ok(count
                + usize::from(
                    irys_database::cached_ingress_leaf(tx, data_root, address, offset.into())?
                        .is_some(),
                ))
        })
    })
}

/// Regression: a Submit transaction larger than the bounded chunk cache can
/// generate its ingress proof from persisted compact leaves and promote, even
/// though its chunk bodies never coexist in the cache.
///
/// Also pins the two invariants that make that safe: capacity pruning reclaims
/// only bodies a storage module reports as fsynced, and Publish migration falls
/// back to the durable Submit replica once a body has been reclaimed.
#[test_log::test(tokio::test)]
async fn heavy_test_oversized_tx_cache_eviction_allows_promotion() -> eyre::Result<()> {
    initialize_tracing();

    let mut config = NodeConfig::testing();
    {
        let c = config.consensus.get_mut();
        c.chunk_size = 32;
        c.num_chunks_in_partition = 10;
        c.num_chunks_in_recall_range = 2;
        c.num_partitions_per_slot = 1;
        c.block_migration_depth = 1;
        // Single node self-promotes with a single proof.
        c.hardforks.frontier.number_of_ingress_proofs_total = 1;
    }
    // Keep batching enabled: chunks stripe together and the tail is flushed by
    // the storage module's idle drain, so nothing on the write path force-syncs.
    config.storage.num_writes_before_sync = 2;
    // Cache far below the 3-chunk (96 B) tx, and prune eagerly.
    config.cache.max_cache_size_bytes = 32;
    config.cache.cache_clean_lag = 0;

    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.consensus.extend_genesis_accounts(vec![(
        signer.address(),
        GenesisAccount {
            balance: U256::from(690000000000000000_u128),
            ..Default::default()
        },
    )]);

    // min_chunk_age = (block_migration_depth + cache_clean_lag) * block_time
    let consensus = config.consensus_config();
    let min_chunk_age_secs =
        u64::from(consensus.block_migration_depth + u32::from(config.cache.cache_clean_lag))
            * consensus.difficulty_adjustment.block_time;

    let node = IrysNodeTest::new_genesis(config.clone()).start().await;
    let ingress_address = node.node_ctx.config.irys_signer().address();
    node.node_ctx
        .packing_waiter
        .wait_for_idle(Some(Duration::from_secs(10)))
        .await?;

    // A 3-chunk Publish tx.
    let data_chunks: [[u8; 32]; 3] = [[10; 32], [20; 32], [30; 32]];
    let data: Vec<u8> = data_chunks.iter().flatten().copied().collect();
    let price = node
        .get_data_price(DataLedger::Publish, data.len() as u64)
        .await
        .expect("price");
    let tx = signer.create_publish_transaction(
        data,
        node.get_anchor().await?,
        price.perm_fee.into(),
        price.term_fee.into(),
    )?;
    let tx = signer.sign_transaction(tx)?;
    let data_root = tx.header.data_root;

    // Confirm the tx in the Submit ledger so it becomes a promotion candidate.
    node.post_data_tx_raw(&tx.header).await;
    node.wait_for_mempool(tx.header.id, 20).await?;
    node.mine_blocks(2).await?;

    // Each validated chunk records its leaf immediately, independent of any
    // storage assignment, and keeps its cached body until the body is durable.
    node.post_chunk_32b(&tx, 0, &data_chunks).await;
    assert_eq!(cached_chunk_count(&node.node_ctx.db, data_root)?, 1);
    assert_eq!(
        ingress_leaf_count(&node.node_ctx.db, data_root, ingress_address, 3)?,
        1
    );

    node.post_chunk_32b(&tx, 1, &data_chunks).await;
    assert_eq!(cached_chunk_count(&node.node_ctx.db, data_root)?, 2);
    assert_eq!(
        ingress_leaf_count(&node.node_ctx.db, data_root, ingress_address, 3)?,
        2
    );

    // Age the bodies past min_chunk_age and mine to drive the capacity-pruning
    // pass. The cache is configured far below the tx size, so the pass runs;
    // it may reclaim only what a storage module reports as fsynced.
    tokio::time::sleep(Duration::from_secs(min_chunk_age_secs + 2)).await;
    let mut early_chunks_reclaimed = false;
    for _ in 0..10 {
        node.mine_block().await?;
        if cached_chunk_count(&node.node_ctx.db, data_root)? == 0 {
            early_chunks_reclaimed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        early_chunks_reclaimed,
        "durable early chunk bodies were never capacity-reclaimed"
    );
    assert_eq!(
        ingress_leaf_count(&node.node_ctx.db, data_root, ingress_address, 3)?,
        2,
        "reclaiming a body must not discard the leaf derived from it"
    );

    // Leaves live in the main DB, so a restart keeps proof readiness without
    // any cached body present.
    let node = node.stop().await.start().await;
    node.node_ctx
        .packing_waiter
        .wait_for_idle(Some(Duration::from_secs(10)))
        .await?;
    let leaves_after_restart =
        ingress_leaf_count(&node.node_ctx.db, data_root, ingress_address, 3)?;
    assert_eq!(
        leaves_after_restart, 2,
        "compact leaves must survive a restart"
    );
    let cached_after_restart = cached_chunk_count(&node.node_ctx.db, data_root)?;
    assert_eq!(
        cached_after_restart, 0,
        "proof readiness must not depend on cached chunk bodies"
    );

    // Upload the final chunk. The complete 3-chunk body set never needs to be
    // resident: proof generation uses the three persisted compact leaves.
    node.post_chunk_32b(&tx, 2, &data_chunks).await;

    let proof = node
        .wait_for_ingress_proofs_no_mining(vec![tx.header.id], 10)
        .await;
    assert!(
        proof.is_ok(),
        "expected an ingress proof built from persisted compact leaves, got: {proof:?}"
    );

    node.mine_blocks(3).await?;
    assert!(
        node.get_is_promoted(&tx.header.id).await?,
        "oversized submit tx should promote after compact ingress proof generation"
    );

    // Cached Submit bodies were reclaimed before promotion. Publish migration
    // must therefore read the validated durable Submit copy, not silently
    // leave holes when CachedChunks no longer contains the body.
    let app = node.start_public_api().await;
    for publish_offset in 0..3 {
        node.future_or_mine_on_timeout(
            node.wait_for_chunk(&app, DataLedger::Publish, publish_offset, 60),
            Duration::from_secs(5),
        )
        .await??;
    }
    drop(app);

    node.stop().await;
    Ok(())
}
