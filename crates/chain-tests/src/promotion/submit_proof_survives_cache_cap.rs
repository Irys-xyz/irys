use crate::utils::IrysNodeTest;
use alloy_core::primitives::U256;
use alloy_genesis::GenesisAccount;
use irys_chain::IrysNodeCtx;
use irys_database::DatabaseProvider;
use irys_database::db::{DatabaseProviderCacheExt as _, IrysDupCursorExt as _};
use irys_database::tables::{CachedChunksIndex, CompactCachedIngressProof};
use irys_testing_utils::initialize_tracing;
use irys_types::{DataLedger, DataRoot, H256, IrysAddress, NodeConfig, irys::IrysSigner};
use reth_db::transaction::DbTx as _;
use std::time::Duration;

fn cached_chunk_count(db: &DatabaseProvider, data_root: DataRoot) -> eyre::Result<u32> {
    db.view_cache_eyre(|tx| {
        let mut cursor = tx.inner().cursor_dup_read::<CachedChunksIndex>()?;
        Ok(cursor.dup_count(data_root)?.unwrap_or(0))
    })
}

fn local_proof(
    db: &DatabaseProvider,
    data_root: DataRoot,
    address: IrysAddress,
) -> eyre::Result<Option<CompactCachedIngressProof>> {
    db.view_cache_eyre(|tx| {
        irys_database::ingress_proof_by_data_root_address(tx, data_root, address)
    })
}

fn local_anchor(
    db: &DatabaseProvider,
    data_root: DataRoot,
    address: IrysAddress,
) -> eyre::Result<Option<H256>> {
    Ok(local_proof(db, data_root, address)?.map(|compact| compact.0.proof.anchor))
}

/// Mine one block at a time, up to `max_blocks`, until `done` holds. The cache
/// prune runs on its own task after each migration, so each block polls for a
/// few seconds before mining the next.
async fn mine_until(
    node: &IrysNodeTest<IrysNodeCtx>,
    max_blocks: usize,
    mut done: impl FnMut() -> eyre::Result<bool>,
) -> eyre::Result<bool> {
    for _ in 0..max_blocks {
        node.mine_block().await?;
        for _ in 0..20 {
            if done()? {
                return Ok(true);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
    done()
}

/// Regression: an assigned Submit miner keeps re-signing its ingress proof
/// after its chunk bodies have left the capped chunk cache.
///
/// HTTP admission rejects when `cached_bytes >= max_cache_size_bytes`. A max
/// of 0 503s the three POSTs (empty cache is `0 >= 0`). The cap is the
/// 3-chunk body so those POSTs land, then the cache sits at the mark and
/// capacity prune reclaims the bodies once they are durable. Promotion needs
/// two proofs, so this single node's tx stays unpromoted.
///
/// Once the bodies are persisted to the Submit module and reclaimed, the
/// proof anchor expires. A live Submit copy must keep that proof (reanchor,
/// not delete). Then the row is deleted by hand, as a node that already lost
/// it would be. The restore sweep regenerates it from the persisted compact
/// leaves.
#[test_log::test(tokio::test)]
async fn heavy_test_submit_ingress_proof_survives_cache_cap() -> eyre::Result<()> {
    initialize_tracing();

    let anchor_expiry_depth = 5_u16;
    let mut config = NodeConfig::testing();
    {
        let c = config.consensus.get_mut();
        c.chunk_size = 32;
        c.num_chunks_in_partition = 10;
        c.num_chunks_in_recall_range = 2;
        c.num_partitions_per_slot = irys_types::ReplicaCount::new(1);
        c.block_migration_depth = 1;
        c.mempool.tx_anchor_expiry_depth = 3;
        c.mempool.ingress_proof_anchor_expiry_depth = anchor_expiry_depth;
        // One node signs one proof; the tx never promotes.
        c.hardforks.frontier.number_of_ingress_proofs_total = 2;
        c.hardforks.frontier.number_of_ingress_proofs_from_assignees = 0;
    }
    config.storage.num_writes_before_sync = 2;
    // HTTP watermark is `cached_bytes >= max`. Zero makes an empty cache
    // look full. Three bodies fill this cap, then capacity prune reclaims.
    config.cache.max_cache_size_bytes = 3 * config.consensus_config().chunk_size;
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
    let db = node.node_ctx.db.clone();
    let local_address = node.node_ctx.config.irys_signer().address();
    node.node_ctx
        .packing_waiter
        .wait_for_idle(Some(Duration::from_secs(10)))
        .await?;

    // A 3-chunk Publish tx, confirmed on Submit.
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
    node.post_data_tx_raw(&tx.header).await;
    node.wait_for_mempool(tx.header.id, 20).await?;
    node.mine_blocks(2).await?;

    for offset in 0..3 {
        node.post_chunk_32b(&tx, offset, &data_chunks).await;
    }
    node.wait_for_ingress_proofs_no_mining(vec![tx.header.id], 20)
        .await?;
    assert!(local_proof(&db, data_root, local_address)?.is_some());

    // Age the bodies, then mine until the capacity prune has reclaimed every
    // one. It reclaims only bodies the Submit module reports as persisted.
    tokio::time::sleep(Duration::from_secs(min_chunk_age_secs + 2)).await;
    let reclaimed = mine_until(&node, 10, || Ok(cached_chunk_count(&db, data_root)? == 0)).await?;
    assert!(reclaimed, "persisted chunk bodies were never reclaimed");

    // Mine past the anchor expiry. With no cached body left, the stored
    // Submit copy is what keeps the proof; it must be reanchored, not deleted.
    let first_anchor =
        local_anchor(&db, data_root, local_address)?.expect("local proof before expiry");
    let blocks = usize::from(anchor_expiry_depth) * 3;
    // A reanchor replaces the row in one write transaction, so a missing row
    // means the prune deleted it.
    let reanchored = mine_until(&node, blocks, || {
        let anchor = local_anchor(&db, data_root, local_address)?;
        eyre::ensure!(
            anchor.is_some(),
            "the local proof was deleted after expiry at cache capacity"
        );
        Ok(anchor.is_some_and(|anchor| anchor != first_anchor))
    })
    .await?;
    assert!(
        reanchored,
        "the local proof was not reanchored after expiry at cache capacity"
    );
    assert!(
        !node.get_is_promoted(&tx.header.id).await?,
        "one proof must not promote the tx"
    );
    assert_eq!(cached_chunk_count(&db, data_root)?, 0);

    // A node that already lost the row gets it back from the restore sweep.
    let current = local_proof(&db, data_root, local_address)?.expect("reanchored proof");
    let deleted = db.update_cache_eyre(|tx| {
        irys_database::delete_ingress_proof_if_unchanged(tx, data_root, current)
    })?;
    assert!(deleted);
    assert!(local_proof(&db, data_root, local_address)?.is_none());
    let restored = mine_until(&node, 5, || {
        Ok(local_proof(&db, data_root, local_address)?.is_some())
    })
    .await?;
    assert!(
        restored,
        "the restore sweep did not regenerate the missing local proof"
    );
    assert!(!node.get_is_promoted(&tx.header.id).await?);

    node.stop().await;
    Ok(())
}
