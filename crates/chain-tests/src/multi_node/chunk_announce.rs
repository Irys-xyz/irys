//! Three real nodes. Genesis ingests a 3-chunk transaction over the API.
//! Peers must list each other: `testing_peer_with_signer` trusts only
//! genesis, and the later handshake does not give the earlier peer the
//! later one's address, so a relay would otherwise reach nobody.

use crate::utils::IrysNodeTest;
use irys_chain::IrysNodeCtx;
use irys_types::{NodeConfig, PeerListItem, PeerScore, ProtocolVersion};
use rand::Rng as _;

fn connect_peers(peers: &[IrysNodeTest<IrysNodeCtx>]) {
    for peer in peers {
        for other in peers {
            if peer.node_ctx.config.peer_id() == other.node_ctx.config.peer_id() {
                continue;
            }
            let node_config = &other.node_ctx.config.node_config;
            peer.node_ctx.peer_list.add_or_update_peer(
                PeerListItem {
                    peer_id: other.node_ctx.config.peer_id(),
                    mining_address: node_config.miner_address(),
                    reputation_score: PeerScore::new(PeerScore::INITIAL),
                    response_time: 0,
                    address: node_config.peer_address(),
                    last_seen: 0,
                    is_online: true,
                    protocol_version: ProtocolVersion::V2,
                    ..Default::default()
                },
                true,
            );
        }
    }
}

async fn run(chunk_announce: bool) -> eyre::Result<Vec<irys_p2p::ChunkGossipStatsSnapshot>> {
    let seconds = 30;
    let mut genesis_config = NodeConfig::testing();
    genesis_config.p2p_gossip.chunk_announce = chunk_announce;
    genesis_config.p2p_gossip.chunk_fetch_delay_ms = 3_000;
    let genesis = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds)
        .await;

    let mut peers = Vec::new();
    for name in ["PEER1", "PEER2"] {
        let signer = genesis_config.new_random_signer();
        let mut cfg = genesis.testing_peer_with_signer(&signer);
        cfg.p2p_gossip.chunk_announce = chunk_announce;
        cfg.p2p_gossip.chunk_fetch_delay_ms = 3_000;
        peers.push(IrysNodeTest::new(cfg).start_with_name(name).await);
    }
    connect_peers(&peers);

    let mut rng = rand::thread_rng();
    let chunks: [[u8; 32]; 3] = [[rng.r#gen(); 32], [rng.r#gen(); 32], [rng.r#gen(); 32]];
    let signer = genesis.node_ctx.config.irys_signer();
    let tx = genesis
        .post_data_tx(genesis.get_anchor().await?, chunks.concat(), &signer)
        .await;
    for peer in &peers {
        peer.wait_for_mempool(tx.header.id, seconds).await?;
    }
    for i in 0..3 {
        genesis.post_chunk_32b(&tx, i, &chunks).await;
    }
    for peer in &peers {
        peer.wait_for_chunk_cache_count(3, seconds).await?;
    }
    // Late relays and the fetch rounds settle before the counters are read.
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    let stats = peers
        .iter()
        .map(|peer| peer.node_ctx.gossip_data_handler.chunk_announce.stats())
        .collect();
    for peer in peers {
        peer.stop().await;
    }
    genesis.stop().await;
    Ok(stats)
}

#[test_log::test(tokio::test)]
async fn heavy_chunk_announce_delivers_one_body_per_chunk() -> eyre::Result<()> {
    for snapshot in run(true).await? {
        assert_eq!(
            snapshot.bodies_push + snapshot.bodies_fetch,
            3,
            "{snapshot:?}"
        );
        assert!(snapshot.announcements_received >= 1, "{snapshot:?}");
    }
    Ok(())
}

#[test_log::test(tokio::test)]
async fn heavy_chunk_push_only_shows_duplicates() -> eyre::Result<()> {
    let stats = run(false).await?;
    assert!(
        stats.iter().any(|snapshot| snapshot.bodies_push > 3),
        "push-only gossip should deliver duplicate bodies: {stats:?}"
    );
    Ok(())
}
