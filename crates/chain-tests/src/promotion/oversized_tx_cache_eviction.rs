use crate::utils::IrysNodeTest;
use alloy_core::primitives::U256;
use alloy_genesis::GenesisAccount;
use irys_database::db::{IrysDatabaseExt as _, IrysDupCursorExt as _};
use irys_database::tables::CachedChunksIndex;
use irys_testing_utils::initialize_tracing;
use irys_types::{DataLedger, NodeConfig, irys::IrysSigner};
use reth_db::transaction::DbTx as _;
use std::time::Duration;

/// Exposes a flaw in the current promotion mechanism: a submit tx whose data is larger
/// than the bounded chunk cache can never be promoted, no matter how it is uploaded.
///
/// THIS TEST ASSERTS THE BUGGY BEHAVIOR ON PURPOSE. The assertions below encode what the
/// system does *today*, not what it should do. It is a characterization test guarding a
/// known defect so the behavior is visible and cannot change silently.
///
/// The flaw: ingress-proof generation
/// (`chunk_ingress_service::chunks::generate_ingress_proof`) reads *every* chunk of a
/// data_root out of the `CachedChunks` DB cache and requires the complete set to be
/// resident simultaneously (it asserts `actual_chunk_count == expected_chunk_count`).
/// That cache is size-bounded (`cache.max_cache_size_bytes`) and pruned on block
/// migration. So a tx whose data exceeds the cache can never have all its chunks resident
/// at once — early chunks are evicted before the last one arrives — no ingress proof is
/// ever produced, and the tx can never be promoted to the Publish ledger.
///
/// Scaled down here: chunk_size=32, a 3-chunk tx, and a cache sized below the tx. Two
/// chunks are uploaded, aged past `min_chunk_age`, and evicted by the migration-driven
/// prune before the third is uploaded, so the full set is never simultaneously present.
///
/// WHEN THE FLAW IS FIXED (e.g. ingress-proof generation streams chunks incrementally, or
/// promotion no longer requires the whole data_root cached at once), this test SHOULD
/// START FAILING. That failure is the signal the fix works. At that point invert it: the
/// oversized tx should now produce an ingress proof and promote, so flip the two
/// assertions at the end to expect `proof.is_ok()` and `get_is_promoted(..) == true`.
#[test_log::test(tokio::test)]
async fn heavy_test_oversized_tx_cache_eviction_blocks_promotion() -> eyre::Result<()> {
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
    config.storage.num_writes_before_sync = 1;
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

    // Upload only the first two chunks.
    node.post_chunk_32b(&tx, 0, &data_chunks).await;
    node.post_chunk_32b(&tx, 1, &data_chunks).await;

    // Age them past min_chunk_age, then mine to drive the migration-triggered prune
    // until the bounded cache has evicted the staged chunks.
    tokio::time::sleep(Duration::from_secs(min_chunk_age_secs + 2)).await;

    let cached_count = |root| -> eyre::Result<u32> {
        node.node_ctx.db.view_eyre(|tx| {
            let mut cursor = tx.cursor_dup_read::<CachedChunksIndex>()?;
            Ok(cursor.dup_count(root)?.unwrap_or(0))
        })
    };

    let mut evicted = false;
    for _ in 0..10 {
        node.mine_block().await?;
        if cached_count(data_root)? == 0 {
            evicted = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(
        evicted,
        "precondition unmet: bounded cache never evicted the staged chunks"
    );

    // Upload the final chunk. The complete 3-chunk set is never simultaneously
    // resident, so ingress-proof generation can never fire.
    node.post_chunk_32b(&tx, 2, &data_chunks).await;

    // BUG BEING CHARACTERIZED: no ingress proof is ever produced for this data_root,
    // because the full chunk set is never simultaneously cached. When the flaw is fixed
    // this assertion should flip to `proof.is_ok()`.
    let proof = node
        .wait_for_ingress_proofs_no_mining(vec![tx.header.id], 10)
        .await;
    assert!(
        proof.is_err(),
        "expected no ingress proof for the oversized tx (current flawed behavior), got: {proof:?}"
    );

    // BUG BEING CHARACTERIZED: consequently the tx is never promoted, even after further
    // mining. When the flaw is fixed this assertion should flip to expect promotion.
    node.mine_blocks(3).await?;
    assert!(
        !node.get_is_promoted(&tx.header.id).await?,
        "oversized submit tx does not promote today (current flawed behavior)"
    );

    node.stop().await;
    Ok(())
}
