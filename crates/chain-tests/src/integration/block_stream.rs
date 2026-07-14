//! Integration tests for the node-internal block-stream contract (`/internal/blocks/*`).
//!
//! These drive a real node and exercise the producer through the shared `BlockStreamHandle`
//! (`observed`, `finalized`, `reorged`) and the canonical read endpoints over HTTP.

use crate::utils::IrysNodeTest;
use alloy_core::primitives::ruint::aliases::U256;
use alloy_genesis::GenesisAccount;
use eyre::OptionExt as _;
use irys_actors::block_stream_service::BlockStreamHandle;
use irys_types::block_stream::{StreamEvent, StreamFrame};
use irys_types::irys::IrysSigner;
use irys_types::{Base64, DataLedger, H256, LedgerChunkOffset, NodeConfig, validate_path};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::Receiver;

const TEST_USER_BALANCE_IRYS: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);

/// Receives frames until `predicate` matches, or times out.
async fn next_frame_for(
    live: &mut Receiver<Arc<StreamFrame>>,
    predicate: impl Fn(&StreamFrame) -> bool,
) -> eyre::Result<Arc<StreamFrame>> {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(30), live.recv())
            .await?
            .ok_or_eyre("block-stream channel closed")?;
        if predicate(&frame) {
            return Ok(frame);
        }
    }
}

fn collect_replay(
    handle: &BlockStreamHandle,
    start: u64,
    end: u64,
) -> eyre::Result<Vec<StreamFrame>> {
    let mut cursor = start;
    let mut frames = Vec::new();
    while let Some((next, page)) = handle.replay_page(cursor, end)? {
        frames.extend(page);
        cursor = next;
    }
    Ok(frames)
}

/// Reads `count` frames from the real `/internal/blocks/stream?from_seq=` SSE endpoint over HTTP,
/// decoding each `data: {json}\n\n` event to JSON. The durable replay prefix is gapless and ordered,
/// so the first `count` frames are stable against any frame appended afterwards.
async fn read_sse_frames(
    address: &str,
    from_seq: u64,
    count: usize,
) -> eyre::Result<Vec<serde_json::Value>> {
    use futures::StreamExt as _;

    if count == 0 {
        return Ok(Vec::new());
    }
    let mut stream = reqwest::Client::new()
        .get(format!(
            "{address}/internal/blocks/stream?from_seq={from_seq}"
        ))
        .send()
        .await?
        .bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut frames = Vec::new();
    while frames.len() < count {
        let chunk = tokio::time::timeout(Duration::from_secs(30), stream.next())
            .await?
            .ok_or_eyre("SSE stream ended before all frames arrived")??;
        buf.extend_from_slice(&chunk);
        // Compact JSON carries no newlines, so each `data: {json}\n\n` event ends at the first `\n\n`.
        while let Some(end) = buf.windows(2).position(|w| w == b"\n\n") {
            let raw: Vec<u8> = buf.drain(..end + 2).collect();
            let event = std::str::from_utf8(&raw)?
                .strip_prefix("data: ")
                .and_then(|s| s.strip_suffix("\n\n"))
                .ok_or_eyre("malformed SSE frame")?;
            frames.push(serde_json::from_str::<serde_json::Value>(event)?);
            if frames.len() == count {
                break;
            }
        }
    }
    Ok(frames)
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
    let (_, _, mut live) = handle.subscribe(0)?;

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
    let (start, end, _live_late) = handle.subscribe(0)?;
    let replay_late = collect_replay(&handle, start, end)?;
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
    let (_, _, mut live) = handle.subscribe(0)?;

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

    // range span exceeding the cap: 400, rejected before any per-height work.
    let resp = client
        .get(format!(
            "{address}/internal/blocks?from_height=0&to_height=5000"
        ))
        .send()
        .await?;
    assert_eq!(resp.status(), 400);

    // inverted range: 400, never a silently empty 200 (a reconciling follower could not tell it
    // from a genuinely empty canonical span).
    let resp = client
        .get(format!(
            "{address}/internal/blocks?from_height=3&to_height=1"
        ))
        .send()
        .await?;
    assert_eq!(resp.status(), 400);

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
    let (_, _, mut live) = handle.subscribe(0)?;

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
    let (start, end, _live_snapshot) = handle.subscribe(0)?;
    let log = collect_replay(&handle, start, end)?;

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

/// `GET /internal/blocks/events` pages the same log over HTTP and honours the three cursor regimes:
/// in-window pagination, the caught-up empty page, and the beyond-tip clamp — plus the error/probe codes.
#[test_log::test(tokio::test)]
async fn internal_events_endpoint_pages_and_regimes() -> eyre::Result<()> {
    let node = IrysNodeTest::default_async().start().await;
    let address = format!(
        "http://127.0.0.1:{}",
        node.node_ctx.config.node_config.http.bind_port
    );
    let client = reqwest::Client::new();

    let blk = node.mine_block().await?;
    node.wait_until_height(blk.height, 10).await?;
    node.mine_blocks(2).await?;
    node.wait_until_height(blk.height + 2, 10).await?;

    // Page from 0 with a small limit, following next_seq/has_more to exhaustion.
    let mut from = 0_u64;
    let mut seqs = Vec::new();
    loop {
        let page: serde_json::Value = client
            .get(format!(
                "{address}/internal/blocks/events?from_seq={from}&limit=2"
            ))
            .send()
            .await?
            .json()
            .await?;
        for f in page["frames"].as_array().expect("frames array") {
            seqs.push(f["seq"].as_u64().expect("seq"));
        }
        from = page["next_seq"].as_u64().expect("next_seq");
        if !page["has_more"].as_bool().expect("has_more") {
            break;
        }
    }
    // At exhaustion (`has_more=false`), the last `next_seq` is the log's logical length.
    let logical_len = from;
    assert_eq!(seqs.first(), Some(&0), "log starts at seq 0");
    assert!(
        seqs.windows(2).all(|w| w[1] == w[0] + 1),
        "contiguous, each seq visited once: {seqs:?}"
    );
    assert!(logical_len >= 1);

    // Caught-up (from_seq == logical_len): a normal empty page, not a clamp.
    let resp = client
        .get(format!(
            "{address}/internal/blocks/events?from_seq={logical_len}"
        ))
        .send()
        .await?;
    assert_eq!(resp.status(), 200);
    let page: serde_json::Value = resp.json().await?;
    assert!(page["frames"].as_array().unwrap().is_empty());
    assert_eq!(page["next_seq"].as_u64(), Some(logical_len));
    assert_eq!(page["has_more"].as_bool(), Some(false));
    assert_eq!(page["truncated"].as_bool(), Some(false));

    // Beyond tip (from_seq > logical_len): clamp to floor 0 (log not pruned), truncated false, not empty.
    let page: serde_json::Value = client
        .get(format!(
            "{address}/internal/blocks/events?from_seq={}",
            logical_len + 5
        ))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(
        page["frames"].as_array().unwrap().first().unwrap()["seq"].as_u64(),
        Some(0)
    );
    assert_eq!(page["truncated"].as_bool(), Some(false));

    // Over-size limit is clamped, not rejected.
    assert_eq!(
        client
            .get(format!(
                "{address}/internal/blocks/events?from_seq=0&limit=100000"
            ))
            .send()
            .await?
            .status(),
        200
    );

    // Malformed query → 400.
    assert_eq!(
        client
            .get(format!("{address}/internal/blocks/events?from_seq=abc"))
            .send()
            .await?
            .status(),
        400
    );

    // `events` is the literal route, not captured as `{height}`: a valid page shape proves it.
    let page: serde_json::Value = client
        .get(format!(
            "{address}/internal/blocks/events?from_seq=0&limit=1"
        ))
        .send()
        .await?
        .json()
        .await?;
    assert!(
        page["frames"].is_array()
            && page["next_seq"].is_u64()
            && page["lowest_retained_seq"].is_u64()
    );

    Ok(())
}

/// The poll endpoint and the SSE stream carry byte-identical frames over the same range. Driven with
/// `observed` + `finalized` frames; `reorged` equivalence follows from the same kind-agnostic
/// `StreamFrame` serializer (unit-tested via `events_page_frames_match_sse_replay`) and is exercised
/// end-to-end by `reorged_emitted_for_fork_switch_without_duplicate_observed`.
#[test_log::test(tokio::test)]
async fn internal_events_equal_sse_over_same_range() -> eyre::Result<()> {
    let mut node = IrysNodeTest::default_async();
    node.cfg.consensus.get_mut().block_migration_depth = 2;
    let node = node.start().await;
    let address = format!(
        "http://127.0.0.1:{}",
        node.node_ctx.config.node_config.http.bind_port
    );
    let client = reqwest::Client::new();

    // Produce observed + finalized frames: mine, advance past the migration depth, await the index.
    let blk1 = node.mine_block().await?;
    node.wait_until_height(blk1.height, 10).await?;
    node.mine_blocks(3).await?;
    node.wait_until_block_index_height(blk1.height, 30).await?;

    // Poll side: page /events from 0 to exhaustion over HTTP.
    let mut poll_frames: Vec<serde_json::Value> = Vec::new();
    let mut from = 0_u64;
    loop {
        let page: serde_json::Value = client
            .get(format!(
                "{address}/internal/blocks/events?from_seq={from}&limit=3"
            ))
            .send()
            .await?
            .json()
            .await?;
        for f in page["frames"].as_array().expect("frames array") {
            poll_frames.push(f.clone());
        }
        from = page["next_seq"].as_u64().expect("next_seq");
        if !page["has_more"].as_bool().expect("has_more") {
            break;
        }
    }

    // SSE side: read the same range from the real `/internal/blocks/stream` endpoint over HTTP, so this
    // covers route dispatch, `from_seq` handling, and the `data: {json}\n\n` framing — not just the
    // in-process replay. The durable replay prefix is gapless and ordered, so its first
    // `poll_frames.len()` frames are exactly the range the poll endpoint returned.
    let sse_frames = read_sse_frames(&address, 0, poll_frames.len()).await?;
    for (i, poll) in poll_frames.iter().enumerate() {
        assert_eq!(
            *poll, sse_frames[i],
            "poll frame {i} must be byte-identical to the SSE frame"
        );
    }
    // The range genuinely exercised both frame kinds.
    assert!(
        poll_frames
            .iter()
            .any(|f| f["kind"].as_str() == Some("observed")),
        "expected an observed frame"
    );
    assert!(
        poll_frames
            .iter()
            .any(|f| f["kind"].as_str() == Some("finalized")),
        "expected a finalized frame"
    );

    Ok(())
}

/// A canonical range request spanning a fork point returns one consistent, parent-linked chain from the
/// winning fork — never a splice of the orphaned and adopted chains. The endpoint snapshots the canonical
/// chain, re-verifies it is unchanged across the per-height resolution (retrying), and rejects any result
/// that is not a contiguous parent-linked chain (`snapshot_canonical_range`). This drives a real 2-node
/// reorg and asserts that observable contract over HTTP after the switch settles.
///
/// Scope: the mid-request race the snapshot guard defends against — a reorg landing between the canonical
/// reads — cannot be forced deterministically from integration without fault injection, and a timing-based
/// probe would be flaky. This locks in the single-fork result the guard must always produce post-reorg.
#[test_log::test(tokio::test)]
async fn internal_range_after_reorg_is_single_parent_linked_fork() -> eyre::Result<()> {
    let seconds_to_wait = 20_usize;
    // Epoch size 10 (matching the other isolated-fork reorg tests) avoids epoch-boundary races while the
    // two nodes mine competing forks in isolation.
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

    // Isolate both nodes so each builds an independent fork from the same base.
    genesis_node.gossip_disable();
    peer_node.gossip_disable();
    let base_height = genesis_node.get_canonical_chain_height().await;

    // Genesis mines the soon-to-be-orphaned block; capture it before the reorg replaces it. The peer mines
    // a strictly longer chain that wins the fork choice once gossiped.
    genesis_node.mine_blocks_without_gossip(1).await?;
    genesis_node
        .wait_until_height(base_height + 1, seconds_to_wait)
        .await?;
    let orphan_block = genesis_node.get_block_by_height(base_height + 1).await?;

    peer_node.mine_blocks_without_gossip(3).await?;
    peer_node
        .wait_until_height(base_height + 3, seconds_to_wait)
        .await?;
    let peer_block_1 = Arc::new(peer_node.get_block_by_height(base_height + 1).await?);
    let peer_block_2 = Arc::new(peer_node.get_block_by_height(base_height + 2).await?);
    let peer_block_3 = Arc::new(peer_node.get_block_by_height(base_height + 3).await?);
    assert_ne!(
        orphan_block.block_hash, peer_block_1.block_hash,
        "the orphan and the winning fork must genuinely diverge at the fork height"
    );

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
    let _reorg_event = reorg_future.await?;
    // The fork choice has switched: the canonical tip is now the winning fork's tip.
    genesis_node
        .wait_until_height(base_height + 3, seconds_to_wait)
        .await?;

    // Request the canonical range spanning the fork point: the common ancestor at `base_height` through the
    // winning tip at `base_height + 3`, covering the divergence at `base_height + 1`.
    let address = format!(
        "http://127.0.0.1:{}",
        genesis_node.node_ctx.config.node_config.http.bind_port
    );
    let client = reqwest::Client::new();
    let to_height = base_height + 3;
    let resp = client
        .get(format!(
            "{address}/internal/blocks?from_height={base_height}&to_height={to_height}"
        ))
        .send()
        .await?;
    assert_eq!(resp.status(), 200, "a settled canonical range resolves");
    let events: serde_json::Value = resp.json().await?;
    let events = events.as_array().expect("events array");

    // The range is the full contiguous height span, ascending.
    let heights: Vec<u64> = events
        .iter()
        .map(|e| e["header"]["height"].as_u64().expect("height"))
        .collect();
    assert_eq!(
        heights,
        (base_height..=to_height).collect::<Vec<_>>(),
        "range is the full contiguous height span: {heights:?}"
    );

    // Every block links to its predecessor: one parent-linked fork, not a splice of two.
    for pair in events.windows(2) {
        assert_eq!(
            pair[1]["header"]["previous_block_hash"], pair[0]["header"]["block_hash"],
            "block at height {} must link to its predecessor",
            pair[1]["header"]["height"]
        );
    }

    // Every forked height reflects the winning (adopted) fork — never the orphaned chain.
    for (height, winner) in [
        (base_height + 1, &peer_block_1),
        (base_height + 2, &peer_block_2),
        (base_height + 3, &peer_block_3),
    ] {
        let block = events
            .iter()
            .find(|e| e["header"]["height"].as_u64() == Some(height))
            .expect("block at forked height present");
        assert_eq!(
            block["header"]["block_hash"],
            serde_json::to_value(winner.block_hash)?,
            "height {height} must reflect the winning fork",
        );
    }
    let at_fork = events
        .iter()
        .find(|e| e["header"]["height"].as_u64() == Some(base_height + 1))
        .expect("block at fork height present");
    assert_ne!(
        at_fork["header"]["block_hash"],
        serde_json::to_value(orphan_block.block_hash)?,
        "the forked height must never resolve to the orphaned block",
    );

    tokio::join!(genesis_node.stop(), peer_node.stop());
    Ok(())
}

/// The `/internal/chunks` contract the gateway's verification pipeline relies on: unpacked bytes
/// plus the `data_path` proof per stored chunk of an inclusive absolute-offset span, short reads
/// (not errors) for offsets the node does not hold, and 400s for malformed requests.
#[test_log::test(tokio::test)]
async fn internal_chunks_serves_unpacked_proven_range_with_short_reads() -> eyre::Result<()> {
    let chunk_size = 32_u64;
    let mut config = NodeConfig::testing().with_consensus(|c| {
        c.chunk_size = chunk_size;
        c.num_chunks_in_partition = 10;
        c.num_chunks_in_recall_range = 2;
        c.num_partitions_per_slot = 1;
        c.entropy_packing_iterations = 1_000;
        c.block_migration_depth = 1;
    });
    config.storage.num_writes_before_sync = 1;
    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&signer]);

    let node = IrysNodeTest::new_genesis(config).start().await;
    node.node_ctx
        .packing_waiter
        .wait_for_idle(Some(Duration::from_secs(10)))
        .await?;

    // 2 full chunks + 1 byte: the tail chunk must come back trimmed to the data, not padded to
    // chunk_size by the unpack.
    let data: Vec<u8> = (0..(2 * chunk_size + 1)).map(|i| i as u8).collect();
    let tx = node
        .post_data_tx(node.get_anchor().await?, data.clone(), &signer)
        .await;
    node.wait_for_mempool(tx.header.id, 20).await?;
    node.upload_chunks(&tx).await?;
    let inclusion_block = node.mine_block().await?;
    node.mine_block().await?;
    node.wait_for_block_in_index_height(inclusion_block.height, 20)
        .await?;
    // Sole data tx on the chain, Submit ledger: absolute offsets [0, 3). Wait for the last chunk
    // to land in a storage module so the range read below is deterministic.
    node.wait_for_chunk_in_storage(DataLedger::Submit, LedgerChunkOffset::from(2_u64), 30)
        .await?;

    let address = format!(
        "http://127.0.0.1:{}",
        node.node_ctx.config.node_config.http.bind_port
    );
    let client = reqwest::Client::new();

    // Mirrors the gateway's decode: bytes/proof as plain u8 arrays. A base64-string regression
    // fails deserialization here, exactly as it would on the gateway.
    #[derive(Debug, serde::Deserialize)]
    struct ChunkRead {
        ledger_id: u32,
        offset: u64,
        bytes: Vec<u8>,
        proof: Vec<u8>,
    }

    let submit_id = DataLedger::Submit.get_id();
    let chunks: Vec<ChunkRead> = client
        .get(format!(
            "{address}/internal/chunks?ledger={submit_id}&offset=0-2"
        ))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(chunks.len(), 3, "all three stored chunks are served");
    for (i, chunk) in chunks.iter().enumerate() {
        assert_eq!(chunk.ledger_id, submit_id);
        assert_eq!(chunk.offset, i as u64);
        let start = i * chunk_size as usize;
        let end = (start + chunk_size as usize).min(data.len());
        assert_eq!(
            chunk.bytes,
            &data[start..end],
            "unpacked bytes at offset {i} match the uploaded data"
        );
        // The served proof is the stored data_path, byte-identical to the one the signer built,
        // and it validates against the tx's committed data_root at this chunk's byte position.
        assert_eq!(chunk.proof, tx.proofs[i].proof, "data_path at offset {i}");
        validate_path(
            tx.header.data_root.0,
            &Base64(chunk.proof.clone()),
            start as u128,
        )?;
    }

    // Short read: a span reaching past the stored data returns only the stored chunks (the
    // gateway reads the shortfall as "this node lacks the rest" and fails over — never a 4xx).
    let short: Vec<ChunkRead> = client
        .get(format!(
            "{address}/internal/chunks?ledger={submit_id}&offset=0-7"
        ))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(short.len(), 3, "unstored offsets are omitted, not erred");
    let empty: Vec<ChunkRead> = client
        .get(format!(
            "{address}/internal/chunks?ledger={submit_id}&offset=5-7"
        ))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert!(empty.is_empty(), "a fully unstored span is an empty page");

    // Malformed requests are 400s, distinguishable from absence.
    for bad in [
        format!("{address}/internal/chunks?ledger={submit_id}&offset=2-1"),
        format!("{address}/internal/chunks?ledger={submit_id}&offset=abc"),
        format!("{address}/internal/chunks?ledger={submit_id}&offset=0-64"),
        format!("{address}/internal/chunks?ledger=999&offset=0-2"),
        format!("{address}/internal/chunks?ledger={submit_id}"),
    ] {
        let status = client.get(&bad).send().await?.status();
        assert_eq!(status, 400, "GET {bad} must be a bad request");
    }

    node.stop().await;
    Ok(())
}
