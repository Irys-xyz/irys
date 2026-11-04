//! Two real nodes. Genesis is the only assignee of Submit slot 0. An
//! unstaked peer, with no partition and no ingress proofs, alone holds an
//! upload: its broadcast is off while it uploads. Its cached bodies must
//! survive until genesis attests a persisted copy, and genesis must learn
//! of them from the peer's re-announcement.

use crate::utils::IrysNodeTest;
use irys_chain::IrysNodeCtx;
use irys_database::{db::DatabaseProviderCacheExt as _, get_cache_size, tables::CachedChunks};
use irys_domain::ChunkType;
use irys_types::{DataLedger, NodeConfig};
use rand::Rng as _;
use std::time::{Duration, Instant};

/// Chunks of Submit slot 0 that `node` labels `Data`.
fn data_chunks(node: &IrysNodeTest<IrysNodeCtx>) -> u64 {
    node.get_storage_module_intervals(DataLedger::Submit, 0, ChunkType::Data)
        .iter()
        .map(|interval| {
            let start: u32 = interval.start().into();
            let end: u32 = interval.end().into();
            u64::from(end - start + 1)
        })
        .sum()
}

fn cached_chunks(node: &IrysNodeTest<IrysNodeCtx>) -> eyre::Result<u64> {
    let chunk_size = node.node_ctx.config.consensus.chunk_size;
    Ok(node
        .node_ctx
        .db
        .view_cache_eyre(|tx| get_cache_size::<CachedChunks, _>(tx.inner(), chunk_size))?
        .0)
}

/// Wait until `broadcast_data` has run (including skips) and then stayed
/// idle, so queued chunk messages are not still sitting when broadcast is
/// turned back on.
async fn wait_until_broadcasts_idle(
    peer: &IrysNodeTest<IrysNodeCtx>,
    idle: Duration,
    seconds: usize,
) {
    let deadline =
        Instant::now() + Duration::from_secs(u64::try_from(seconds).expect("wait budget fits u64"));
    let mut last = peer
        .node_ctx
        .gossip_data_handler
        .chunk_announce
        .broadcasts_attempted();
    let mut quiet_since = Instant::now();
    loop {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let attempted = peer
            .node_ctx
            .gossip_data_handler
            .chunk_announce
            .broadcasts_attempted();
        if attempted != last {
            last = attempted;
            quiet_since = Instant::now();
        }
        if Instant::now().duration_since(quiet_since) >= idle {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "peer broadcast queue did not drain"
        );
    }
}

#[test_log::test(tokio::test)]
async fn heavy_an_unassigned_uploader_holds_bodies_until_the_assignee_persists_them()
-> eyre::Result<()> {
    let seconds = 30;
    let genesis_config = NodeConfig::testing();
    let genesis = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds)
        .await;
    let peer_signer = genesis_config.new_random_signer();
    let mut peer_config = genesis.testing_peer_with_signer(&peer_signer);
    // Any cached body puts the peer over its threshold, so every pass is a
    // `Reclaim` pass, which used to delete unprotected bodies on age alone.
    peer_config.cache.max_cache_size_bytes = 1024;
    peer_config.cache.prune_at_capacity_percent = 1.0;
    let peer = IrysNodeTest::new(peer_config).start_with_name("PEER").await;

    let mut rng = rand::thread_rng();
    let chunks: [[u8; 32]; 3] = [[rng.r#gen(); 32], [rng.r#gen(); 32], [rng.r#gen(); 32]];
    let tx = genesis
        .post_data_tx(
            genesis.get_anchor().await?,
            chunks.concat(),
            &genesis.node_ctx.config.irys_signer(),
        )
        .await;
    peer.wait_for_mempool(tx.header.id, seconds).await?;

    // Only the peer receives the bodies. The gossip service checks the
    // switch when it drains a message, so let the queued pushes drain
    // before switching it back on.
    peer.node_ctx.sync_state.set_gossip_broadcast_enabled(false);
    for i in 0..3 {
        peer.post_chunk_32b(&tx, i, &chunks).await;
    }
    peer.wait_for_chunk_cache_count(3, seconds).await?;
    wait_until_broadcasts_idle(&peer, Duration::from_millis(200), seconds).await;
    peer.node_ctx.sync_state.set_gossip_broadcast_enabled(true);
    assert_eq!(data_chunks(&genesis), 0, "genesis has no copy yet");

    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        genesis.mine_blocks(1).await?;
        let held = cached_chunks(&peer)?;
        let stored = data_chunks(&genesis);
        assert!(
            held + stored >= 3,
            "a body was lost: peer holds {held}, genesis stores {stored}"
        );
        if held == 0 && stored == 3 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "custody never handed over: peer holds {held}, genesis stores {stored}"
        );
    }

    // Genesis pulled every body after an announcement; none was pushed.
    let stats = genesis.node_ctx.gossip_data_handler.chunk_announce.stats();
    assert_eq!(stats.bodies_push, 0, "{stats:?}");
    assert!(stats.bodies_fetch >= 3, "{stats:?}");
    assert!(stats.announcements_received >= 1, "{stats:?}");

    peer.stop().await;
    genesis.stop().await;
    Ok(())
}
