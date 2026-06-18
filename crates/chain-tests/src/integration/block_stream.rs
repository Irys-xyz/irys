//! Integration tests for the node-internal block-stream contract (`/internal/blocks/*`).
//!
//! These drive a real node and exercise the producer through the shared `BlockStreamHandle`
//! (`observed`, `finalized`, `reorged`) and the canonical read endpoints over HTTP.

use crate::utils::IrysNodeTest;
use alloy_core::primitives::ruint::aliases::U256;
use alloy_genesis::GenesisAccount;
use eyre::OptionExt as _;
use irys_types::block_stream::{StreamEvent, StreamFrame};
use irys_types::irys::IrysSigner;
use irys_types::{H256, NodeConfig};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;

const TEST_USER_BALANCE_IRYS: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);

/// Receives frames until `predicate` matches, or times out.
async fn next_frame_for(
    live: &mut UnboundedReceiver<StreamFrame>,
    predicate: impl Fn(&StreamFrame) -> bool,
) -> eyre::Result<StreamFrame> {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(30), live.recv())
            .await?
            .ok_or_eyre("block-stream channel closed")?;
        if predicate(&frame) {
            return Ok(frame);
        }
    }
}

#[test_log::test(tokio::test)]
async fn observed_emitted_with_data_roots_and_replays_without_gap() -> eyre::Result<()> {
    let mut node = IrysNodeTest::default_async();
    let user = IrysSigner::random_signer(&node.cfg.consensus_config());
    node.cfg.consensus.extend_genesis_accounts(vec![(
        user.address(),
        GenesisAccount {
            balance: TEST_USER_BALANCE_IRYS,
            ..Default::default()
        },
    )]);
    let node = node.start().await;
    let handle = node.node_ctx.block_stream_handle.clone();

    // Subscribe before producing.
    let (_replay0, mut live) = handle.subscribe(0)?;

    // Post a data tx and mine; an `observed` frame carrying its data_root must arrive.
    node.post_publish_data_tx(&user, b"alpha".to_vec()).await?;
    node.mine_blocks(2).await?;

    let frame = next_frame_for(&mut live, |f| {
        f.kind() == "observed" && !f.data_roots().is_empty()
    })
    .await?;
    assert_eq!(frame.kind(), "observed");
    assert!(
        frame.block_hash().is_some(),
        "observed frame carries a block_hash"
    );
    let seq_with_data = frame.seq;

    // A late subscriber replays history (including that frame at its original seq) with strictly
    // increasing, gapless seqs — the replay→live handover property.
    let (replay_late, _live_late) = handle.subscribe(0)?;
    assert!(
        replay_late.iter().any(|f| f.seq == seq_with_data),
        "replay must include the earlier frame at its original seq"
    );
    let seqs: Vec<u64> = replay_late.iter().map(|f| f.seq).collect();
    assert!(
        seqs.windows(2).all(|w| w[1] == w[0] + 1),
        "replayed seqs are contiguous and monotonic: {seqs:?}"
    );
    // De-dup: no two `observed` frames share a block_hash.
    let mut observed_hashes: Vec<_> = replay_late
        .iter()
        .filter(|f| f.kind() == "observed")
        .filter_map(StreamFrame::block_hash)
        .collect();
    let total = observed_hashes.len();
    observed_hashes.sort();
    observed_hashes.dedup();
    assert_eq!(
        total,
        observed_hashes.len(),
        "observed emitted once per block"
    );

    Ok(())
}

#[test_log::test(tokio::test)]
async fn finalized_emitted_on_migration() -> eyre::Result<()> {
    let mut node = IrysNodeTest::default_async();
    // Finalise quickly: a block migrates once the tip is this many blocks ahead.
    node.cfg.consensus.get_mut().block_migration_depth = 2;
    let node = node.start().await;
    let handle = node.node_ctx.block_stream_handle.clone();
    let (_replay, mut live) = handle.subscribe(0)?;

    let blk1 = node.mine_block().await?;
    node.wait_until_height(blk1.height, 10).await?;

    // Advance the tip well past the migration depth so blk1 migrates to the index.
    node.mine_blocks(3).await?;
    node.wait_until_block_index_height(blk1.height, 30).await?;

    // A `finalized` frame for blk1 must arrive.
    let finalized = next_frame_for(&mut live, |f| {
        f.kind() == "finalized" && f.block_hash() == Some(blk1.block_hash)
    })
    .await?;
    assert_eq!(finalized.kind(), "finalized");
    assert_eq!(finalized.block_hash(), Some(blk1.block_hash));

    Ok(())
}

#[test_log::test(tokio::test)]
async fn internal_reads_by_height_and_range() -> eyre::Result<()> {
    let mut node = IrysNodeTest::default_async();
    let user = IrysSigner::random_signer(&node.cfg.consensus_config());
    node.cfg.consensus.extend_genesis_accounts(vec![(
        user.address(),
        GenesisAccount {
            balance: TEST_USER_BALANCE_IRYS,
            ..Default::default()
        },
    )]);
    let node = node.start().await;
    let address = format!(
        "http://127.0.0.1:{}",
        node.node_ctx.config.node_config.http.bind_port
    );
    let client = reqwest::Client::new();

    node.post_publish_data_tx(&user, b"gamma".to_vec()).await?;
    let blk = node.mine_block().await?;
    node.wait_until_height(blk.height, 10).await?;

    // by-height: returns the canonical BlockEvent, tx count matching the block.
    let resp = client
        .get(format!("{address}/internal/blocks/{}", blk.height))
        .send()
        .await?;
    assert_eq!(resp.status(), 200);
    let event: serde_json::Value = resp.json().await?;
    assert_eq!(event["header"]["height"].as_u64(), Some(blk.height));
    let expected_txs: usize = blk.data_ledgers.iter().map(|l| l.tx_ids.0.len()).sum();
    assert_eq!(
        event["txs"].as_array().expect("txs array").len(),
        expected_txs
    );

    // range: ascending canonical BlockEvents.
    let resp = client
        .get(format!(
            "{address}/internal/blocks?from_height=0&to_height={}",
            blk.height
        ))
        .send()
        .await?;
    assert_eq!(resp.status(), 200);
    let events: serde_json::Value = resp.json().await?;
    let heights: Vec<u64> = events
        .as_array()
        .expect("events array")
        .iter()
        .map(|e| e["header"]["height"].as_u64().expect("height"))
        .collect();
    assert!(!heights.is_empty());
    assert!(
        heights.windows(2).all(|w| w[0] < w[1]),
        "range is ascending: {heights:?}"
    );

    // unknown height: 404.
    let resp = client
        .get(format!("{address}/internal/blocks/99999"))
        .send()
        .await?;
    assert_eq!(resp.status(), 404);

    Ok(())
}

/// A fork switch emits exactly one batched `reorged` frame whose `orphaned`/`new_fork`/`fork_parent`
/// mirror the authoritative `ReorgEvent`, and the winning-fork blocks are conveyed by that frame
/// alone — never re-emitted as `observed` (the de-dup the producer's ordering guarantee relies on).
#[test_log::test(tokio::test)]
async fn reorged_emitted_for_fork_switch_without_duplicate_observed() -> eyre::Result<()> {
    let seconds_to_wait = 20_usize;
    // Epoch size 10 (matching the other isolated-fork reorg tests) avoids epoch-boundary races
    // while the two nodes mine competing forks in isolation.
    let mut genesis_config = NodeConfig::testing_with_epochs(10);
    genesis_config.consensus.get_mut().chunk_size = 32;

    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer]);

    let genesis_node = IrysNodeTest::new_genesis(genesis_config)
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    let peer_config = genesis_node.testing_peer_with_signer(&peer_signer);
    let peer_node = genesis_node
        .testing_peer_with_assignments_and_name(peer_config, "PEER")
        .await?;

    // Subscribe to the genesis node's stream before the fork; setup frames are already durable, so
    // only post-subscribe frames arrive live.
    let handle = genesis_node.node_ctx.block_stream_handle.clone();
    let (_replay, mut live) = handle.subscribe(0)?;

    // Isolate both nodes so each builds an independent fork from the same base.
    genesis_node.gossip_disable();
    peer_node.gossip_disable();
    let base_height = genesis_node.get_canonical_chain_height().await;

    // Genesis mines the soon-to-be-orphaned block; the peer mines a strictly longer chain that will
    // win the fork choice once gossiped.
    genesis_node.mine_blocks_without_gossip(1).await?;
    genesis_node
        .wait_until_height(base_height + 1, seconds_to_wait)
        .await?;
    peer_node.mine_blocks_without_gossip(3).await?;
    peer_node
        .wait_until_height(base_height + 3, seconds_to_wait)
        .await?;
    let peer_block_1 = Arc::new(peer_node.get_block_by_height(base_height + 1).await?);
    let peer_block_2 = Arc::new(peer_node.get_block_by_height(base_height + 2).await?);
    let peer_block_3 = Arc::new(peer_node.get_block_by_height(base_height + 3).await?);

    // Arm reorg detection, then re-enable gossip and feed genesis the winning chain in order.
    let reorg_future = genesis_node.wait_for_reorg(seconds_to_wait);
    genesis_node.gossip_enable();
    peer_node.gossip_enable();
    for block in [&peer_block_1, &peer_block_2, &peer_block_3] {
        peer_node.gossip_block_to_peers(block)?;
        genesis_node
            .wait_for_block(&block.block_hash, seconds_to_wait)
            .await?;
    }
    let reorg_event = reorg_future.await?;

    // Block until the durable `reorged` frame lands (the producer consumes the signal async).
    let _ = next_frame_for(&mut live, |f| f.kind() == "reorged").await?;

    // Flush the producer past the post-reorg `Confirmed` signals for the winning fork (which must be
    // de-duped to nothing): mine one fresh canonical block and await ITS `observed`. Receiving a
    // frame the producer enqueued strictly after those signals proves they were all processed, so a
    // missing `observed` for a winning block is a real de-dup, not an unflushed queue.
    let flush = genesis_node.mine_block().await?;
    let _ = next_frame_for(&mut live, |f| {
        f.kind() == "observed" && f.block_hash() == Some(flush.block_hash)
    })
    .await?;

    // Snapshot the complete durable log and assert the whole-log invariants on it.
    let (log, _live_snapshot) = handle.subscribe(0)?;

    let reorged: Vec<&StreamFrame> = log.iter().filter(|f| f.kind() == "reorged").collect();
    assert_eq!(
        reorged.len(),
        1,
        "exactly one reorged frame for one fork switch"
    );

    let StreamEvent::Reorged {
        fork_parent,
        orphaned,
        new_fork,
    } = &reorged[0].event
    else {
        unreachable!("filtered to reorged frames");
    };

    // The frame's batch mirrors the authoritative ReorgEvent.
    assert_eq!(fork_parent.height, reorg_event.fork_parent.height);
    assert_eq!(fork_parent.block_hash, reorg_event.fork_parent.block_hash);
    let orphaned_hashes: Vec<H256> = orphaned.iter().map(|b| b.header.block_hash).collect();
    let old_fork_hashes: Vec<H256> = reorg_event
        .old_fork
        .iter()
        .map(|b| b.header().block_hash)
        .collect();
    assert_eq!(
        orphaned_hashes, old_fork_hashes,
        "orphaned == old fork, ascending"
    );
    let new_fork_hashes: Vec<H256> = new_fork.iter().map(|b| b.header.block_hash).collect();
    let new_fork_event_hashes: Vec<H256> = reorg_event
        .new_fork
        .iter()
        .map(|b| b.header().block_hash)
        .collect();
    assert_eq!(
        new_fork_hashes, new_fork_event_hashes,
        "new_fork == winning fork, ascending"
    );

    // seqs stay contiguous and monotonic across the reorg.
    let seqs: Vec<u64> = log.iter().map(|f| f.seq).collect();
    assert!(
        seqs.windows(2).all(|w| w[1] == w[0] + 1),
        "seqs contiguous across the reorg: {seqs:?}"
    );

    // De-dup: no `observed` frame carries a winning-fork hash — those blocks reach the consumer only
    // via the reorged frame's `new_fork`.
    let winning: HashSet<H256> = new_fork_hashes.into_iter().collect();
    let duplicate = log
        .iter()
        .filter(|f| f.kind() == "observed")
        .filter_map(StreamFrame::block_hash)
        .find(|h| winning.contains(h));
    assert!(
        duplicate.is_none(),
        "winning-fork block must not be re-emitted as observed: {duplicate:?}"
    );

    tokio::join!(genesis_node.stop(), peer_node.stop());
    Ok(())
}
