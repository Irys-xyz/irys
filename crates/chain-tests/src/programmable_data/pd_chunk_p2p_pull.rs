//! PD chunk P2P pull integration tests.
//!
//! Provides `PdP2pTestContext` and `setup_pd_p2p_test()` which:
//! - Start a genesis Node A with real uploaded data (16 chunks x 32 bytes = 512 bytes)
//! - Mine blocks past migration depth so chunks are in storage modules
//! - Start a peer Node B that syncs to Node A's tip (validator-only, no mining)
//! - Return context with both nodes, data offsets, and signer accounts
//!
//! Test functions exercise the P2P chunk pull path: Node A has chunks in storage,
//! Node B must fetch them from Node A to validate PD transactions. Currently all tests
//! are `#[ignore]` because PdService cannot yet fetch chunks from peers (Stage 3).

use std::sync::Arc;
use std::time::Duration;

use alloy_consensus::{SignableTransaction as _, TxEip1559, TxEnvelope as EthereumTxEnvelope};
use alloy_eips::Encodable2718 as _;
use alloy_network::TxSignerSync as _;
use alloy_primitives::{aliases::U200, Address, U256};
use alloy_signer_local::LocalSigner;
use irys_reth::pd_tx::{build_pd_access_list, prepend_pd_header_v1_to_calldata, PdHeaderV1};
use irys_types::irys::IrysSigner;
use irys_types::range_specifier::ChunkRangeSpecifier;
use irys_types::{Base64, DataLedger, LedgerChunkOffset, NodeConfig, TxChunkOffset, UnpackedChunk};
use tracing::info;

use crate::utils::IrysNodeTest;

/// Context returned by [`setup_pd_p2p_test`] containing both nodes and metadata
/// about the uploaded data.
#[expect(dead_code, reason = "fields reserved for Stage 3 P2P pull tests")]
pub(crate) struct PdP2pTestContext {
    /// Genesis node with mining, storage modules, and uploaded chunk data.
    pub node_a: IrysNodeTest<irys_chain::IrysNodeCtx>,
    /// Peer node synced to Node A. Validator-only: not staked, not pledged, no mining.
    pub node_b: IrysNodeTest<irys_chain::IrysNodeCtx>,
    /// Global ledger offset where the uploaded data starts (Publish ledger).
    pub data_start_offset: u64,
    /// Partition index derived from `data_start_offset / num_chunks_in_partition`.
    pub partition_index: u64,
    /// Local offset within the partition: `(data_start_offset % num_chunks_in_partition) as u32`.
    pub local_offset: u32,
    /// Number of chunks in a partition (from consensus config).
    pub num_chunks_in_partition: u64,
    /// Chunk size in bytes (from consensus config).
    pub chunk_size: u64,
    /// Number of chunks uploaded.
    pub num_chunks_uploaded: u64,
    /// The raw data bytes that were uploaded to Node A.
    pub data_bytes: Vec<u8>,
    /// Signer/account used to upload data on Node A.
    pub data_signer: IrysSigner,
    /// Signer/account for the peer (Node B).
    pub peer_signer: IrysSigner,
    /// Signer/account dedicated to submitting PD transactions.
    pub pd_signer: IrysSigner,
    /// Second PD signer for tests that need two independent PD accounts (e.g., deduplication).
    pub pd_signer_2: IrysSigner,
}

/// Start two nodes for PD chunk P2P pull testing.
///
/// 1. Starts Node A as genesis with `chunk_size=32`, `block_migration_depth=2`,
///    `num_chunks_in_partition=10`, `num_chunks_in_recall_range=2`.
/// 2. Uploads 16 chunks x 32 bytes = 512 bytes of data on Node A.
/// 3. Mines blocks past migration depth so chunks migrate to storage modules.
/// 4. Records the `data_start_offset` from the block index.
/// 5. Starts Node B as a validator-only peer (not staked, not pledged, no mining).
/// 6. Waits for Node B to sync to Node A's tip.
/// 7. Returns [`PdP2pTestContext`] with both nodes and metadata.
pub(crate) async fn setup_pd_p2p_test() -> eyre::Result<PdP2pTestContext> {
    let chunk_size: u64 = 32;
    let num_chunks_in_partition: u64 = 10;
    let num_chunks_uploaded: u64 = 16;
    let seconds_to_wait = 30;

    // --- Configure and start Node A (genesis, mining, has chunks) ---
    let mut config = NodeConfig::testing();
    config.consensus.get_mut().chunk_size = chunk_size;
    config.consensus.get_mut().block_migration_depth = 2;
    config.consensus.get_mut().num_chunks_in_recall_range = 2;
    config.consensus.get_mut().num_chunks_in_partition = num_chunks_in_partition;

    let data_signer = config.new_random_signer();
    let peer_signer = config.new_random_signer();
    let pd_signer = config.new_random_signer();
    let pd_signer_2 = config.new_random_signer();
    config.fund_genesis_accounts(vec![&data_signer, &peer_signer, &pd_signer, &pd_signer_2]);

    let node_a = IrysNodeTest::new_genesis(config)
        .start_and_wait_for_packing("NODE_A", seconds_to_wait)
        .await;

    // Wait for HTTP server to be ready
    tokio::time::sleep(Duration::from_secs(2)).await;

    // --- Upload real data on Node A: 16 chunks x 32 bytes = 512 bytes ---
    let data_bytes: Vec<u8> = (0..num_chunks_uploaded * chunk_size)
        .map(|i| (i & 0xff) as u8)
        .collect();

    // Record the Publish ledger total_chunks BEFORE posting — this is the data_start_offset.
    let offset_before = {
        let block_index = node_a.node_ctx.block_index_guard.read();
        block_index
            .get_latest_item()
            .and_then(|item| {
                item.ledgers
                    .iter()
                    .find(|l| l.ledger == DataLedger::Publish)
                    .map(|l| l.total_chunks)
            })
            .unwrap_or(0)
    };

    // Post the data transaction via the mempool channel.
    let tx = node_a
        .post_publish_data_tx(&data_signer, data_bytes.clone())
        .await
        .map_err(|e| eyre::eyre!("Failed to post data tx: {:?}", e))?;

    // Upload chunks via HTTP so they are in the cache for migration.
    let client = reqwest::Client::new();
    let http_url = format!(
        "http://127.0.0.1:{}",
        node_a.node_ctx.config.node_config.http.bind_port
    );
    for (tx_chunk_offset, chunk_node) in tx.chunks.iter().enumerate() {
        let min = chunk_node.min_byte_range;
        let max = chunk_node.max_byte_range;
        let chunk = UnpackedChunk {
            data_root: tx.header.data_root,
            data_size: tx.header.data_size,
            data_path: Base64(tx.proofs[tx_chunk_offset].proof.clone()),
            bytes: Base64(data_bytes[min..max].to_vec()),
            tx_offset: TxChunkOffset::from(
                TryInto::<u32>::try_into(tx_chunk_offset).expect("Value exceeds u32::MAX"),
            ),
        };
        let resp = client
            .post(format!("{}/v1/chunk", &http_url))
            .json(&chunk)
            .send()
            .await?;
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::OK,
            "Failed to upload chunk {} for data tx",
            tx_chunk_offset
        );
    }

    // Mine blocks until the tx header appears in the block index (past migration depth).
    node_a
        .wait_for_migrated_txs(vec![tx.header.clone()], seconds_to_wait)
        .await?;

    // ChunkMigrationService writes chunk data to storage modules asynchronously
    // after block migration. Wait for it to complete.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let data_start_offset = offset_before;
    info!(
        "Data uploaded and migrated on Node A: {} chunks, data_start_offset={}",
        num_chunks_uploaded, data_start_offset
    );

    // Record Node A's block index height (migrated/confirmed blocks only).
    // Chain sync can only pull blocks that are in the source's block index —
    // NOT the canonical tip, which may be block_migration_depth ahead.
    let node_a_index_height = node_a.get_block_index_height();
    info!("Node A block index height: {}", node_a_index_height);

    // --- Start Node B (peer, validator-only, no staking/pledging/mining) ---
    let peer_config = node_a.testing_peer_with_signer(&peer_signer);
    let node_b = IrysNodeTest::new(peer_config)
        .start_with_name("NODE_B")
        .await;

    // Wait for Node B's block index to catch up to Node A's indexed height.
    // This ensures Node B has BlockIndex + DataTransactionHeaders in MDBX,
    // required for local data_root derivation during chunk verification.
    node_b
        .wait_until_block_index_height(node_a_index_height, seconds_to_wait)
        .await?;
    info!(
        "Node B block index synced to height {}",
        node_a_index_height
    );

    // Compute partition_index and local_offset from data_start_offset.
    let partition_index = data_start_offset / num_chunks_in_partition;
    let local_offset = (data_start_offset % num_chunks_in_partition) as u32;

    Ok(PdP2pTestContext {
        node_a,
        node_b,
        data_start_offset,
        partition_index,
        local_offset,
        num_chunks_in_partition,
        chunk_size,
        num_chunks_uploaded,
        data_bytes,
        data_signer,
        peer_signer,
        pd_signer,
        pd_signer_2,
    })
}

/// Build a signed PD transaction referencing real chunk offsets.
///
/// Unlike `create_and_inject_pd_transaction_with_priority_fee` (which uses `U200::MAX`
/// sentinel to bypass chunk provisioning), this helper builds a PD tx with real
/// `partition_index` and `offset` values. This forces PdService on the validating node
/// to actually locate (and eventually P2P-fetch) the referenced chunks.
///
/// Returns the signed transaction hash (B256) after injection into the target node.
async fn build_and_inject_real_pd_tx(
    node: &IrysNodeTest<irys_chain::IrysNodeCtx>,
    signer: &IrysSigner,
    partition_index: u64,
    offset: u32,
    chunk_count: u16,
    nonce: u64,
) -> eyre::Result<alloy_primitives::FixedBytes<32>> {
    let local_signer = LocalSigner::from(signer.signer.clone());
    let chain_id = node.node_ctx.config.consensus.chain_id;

    // Build access list referencing real partition/offset values.
    let specs = vec![ChunkRangeSpecifier {
        partition_index: U200::from(partition_index),
        offset,
        chunk_count,
    }];
    let access_list = build_pd_access_list(specs.into_iter());

    // PD header with fees high enough to pass min_pd_transaction_cost.
    let header = PdHeaderV1 {
        max_priority_fee_per_chunk: U256::from(10_000_000_000_000_000_u64), // 0.01 IRYS
        max_base_fee_per_chunk: U256::from(1_000_000_000_000_000_u64),      // 0.001 IRYS
    };
    let calldata = prepend_pd_header_v1_to_calldata(&header, &[]);

    let mut tx = TxEip1559 {
        access_list,
        chain_id,
        gas_limit: 1_000_000,
        input: calldata,
        max_fee_per_gas: 20_000_000_000,
        max_priority_fee_per_gas: 1_000_000_000,
        nonce,
        to: alloy_primitives::TxKind::Call(Address::random()),
        value: U256::ZERO,
    };
    let signature = local_signer
        .sign_transaction_sync(&mut tx)
        .expect("PD tx must be signable");

    let tx_envelope = EthereumTxEnvelope::Eip1559(tx.into_signed(signature))
        .encoded_2718()
        .into();
    let tx_hash = node
        .node_ctx
        .reth_node_adapter
        .rpc
        .inject_tx(tx_envelope)
        .await?;

    Ok(tx_hash)
}

// ---------------------------------------------------------------------------
// Integration tests — PD chunk P2P pull
//
// All tests below are #[ignore] because they require PdService to fetch chunks
// from peers over P2P, which is not yet implemented (Stage 3). They currently
// exercise the test harness, node topology, data upload, PD tx construction,
// and block gossip. Once the P2P pull path is wired, remove #[ignore].
// ---------------------------------------------------------------------------

/// Node A mines a block with 1 PD tx referencing 2 chunks. Node B receives the block
/// and attempts to validate it. Currently expected to fail because Node B can't fetch
/// chunks remotely.
///
/// Flow:
/// 1. setup_pd_p2p_test() — Node A has 16 chunks in storage, Node B synced to tip
/// 2. Pre-test invariant: Node B does NOT have the chunks locally
/// 3. Inject a PD tx on Node A referencing 2 real chunks at (partition_index, local_offset)
/// 4. Node A mines a block including the PD tx
/// 5. Gossip the block to Node B
/// 6. Assert Node B validates the block and its canonical tip matches Node A
/// 7. Assert the PD tx is present in the block
///
/// Expected failure: Node B's PdService logs "Chunk not found locally. P2P gossip not
/// yet implemented" and rejects the block during validation.
#[ignore = "requires PD chunk P2P pull implementation (Stage 3)"]
#[test_log::test(tokio::test)]
async fn test_pd_chunk_p2p_happy_path() -> eyre::Result<()> {
    let ctx = setup_pd_p2p_test().await?;

    // Pre-test invariant: Node B should NOT have chunks at the target offset.
    // Storage modules: Ok(None) or Err (no storage module) — but NOT Ok(Some(_)).
    let probe_result = ctx
        .node_b
        .node_ctx
        .chunk_provider
        .get_chunk_by_ledger_offset(
            DataLedger::Publish,
            LedgerChunkOffset::from(ctx.data_start_offset),
        );
    assert!(
        matches!(probe_result, Ok(None) | Err(_)),
        "Node B should NOT have chunk data at ledger offset {} before P2P fetch",
        ctx.data_start_offset,
    );

    // PD cache (ChunkDataIndex DashMap): must be empty for these offsets before P2P fetch.
    let publish_ledger = DataLedger::Publish as u32;
    for i in 0..2_u64 {
        let offset = ctx.data_start_offset + i;
        assert!(
            ctx.node_b
                .node_ctx
                .chunk_data_index
                .get(&(publish_ledger, offset))
                .is_none(),
            "Node B ChunkDataIndex should NOT have chunk at ({}, {}) before P2P fetch",
            publish_ledger,
            offset,
        );
    }

    // Inject a PD tx on Node A referencing 2 real chunks.
    let tx_hash = build_and_inject_real_pd_tx(
        &ctx.node_a,
        &ctx.pd_signer,
        ctx.partition_index,
        ctx.local_offset,
        2, // chunk_count
        0, // nonce
    )
    .await?;
    info!("PD tx injected on Node A: {:?}", tx_hash);

    // Wait for PD monitor on Node A to detect and provision chunks from local storage.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Mine a block on Node A (without auto-gossip so we control timing).
    let (block, eth_payload, _) = ctx.node_a.mine_block_without_gossip().await?;

    // Verify the PD tx was included in the block.
    let pd_tx_included = eth_payload
        .block()
        .body()
        .transactions
        .iter()
        .any(|tx| tx.hash() == &tx_hash);
    assert!(
        pd_tx_included,
        "PD tx {:?} should be included in Node A's mined block",
        tx_hash,
    );

    let block_height = block.height;
    info!("Node A mined block at height {} with PD tx", block_height);

    // Gossip the block to Node B.
    ctx.node_a
        .gossip_block_to_peers(&Arc::new(block.as_ref().clone()))?;
    ctx.node_a.gossip_eth_block_to_peers(eth_payload.block())?;

    // Wait for Node B to validate and accept the block.
    // This is where P2P chunk fetch would be triggered on Node B.
    ctx.node_b.wait_until_height(block_height, 30).await?;

    // Verify Node B's canonical tip matches Node A's.
    let node_b_height = ctx.node_b.get_canonical_chain_height().await;
    assert_eq!(
        node_b_height, block_height,
        "Node B canonical tip should match Node A after validation",
    );

    // Note: We do NOT assert ChunkDataIndex here. For block-only PD chunks,
    // the PdBlockGuard drops after validation completes, which sends
    // ReleaseBlockChunks → chunks are evicted from cache. The fact that
    // Node B accepted the block at the correct height IS the proof that
    // P2P chunk fetch worked — without the chunks, shadow_transactions_are_valid
    // would have failed and the block would have been rejected.

    info!(
        "Node B validated PD block at height {} (both nodes at same tip). \
         Block acceptance proves P2P chunk fetch succeeded.",
        block_height,
    );

    ctx.node_b.stop().await;
    ctx.node_a.stop().await;
    Ok(())
}

/// Node A mines a block with 3 PD txs referencing different chunk ranges.
/// Node B fetches all chunks and validates.
///
/// Flow:
/// 1. setup_pd_p2p_test() — Node A has 16 chunks in storage
/// 2. Inject 3 PD txs on Node A:
///    - T1: chunks at (partition_index, local_offset+0, count=2) — offsets 0-1
///    - T2: chunks at (partition_index, local_offset+2, count=2) — offsets 2-3
///    - T3: chunks at (partition_index, local_offset+4, count=2) — offsets 4-5
/// 3. Node A mines a block, gossips to Node B
/// 4. Assert Node B validates, all 3 txs in block
///
/// Expected failure: same as happy_path — Node B can't fetch chunks remotely yet.
#[ignore = "requires PD chunk P2P pull implementation (Stage 3)"]
#[test_log::test(tokio::test)]
async fn test_pd_chunk_p2p_multiple_txs() -> eyre::Result<()> {
    let ctx = setup_pd_p2p_test().await?;

    // Verify that all 6 chunks (3 txs x 2 chunks each) fit within the uploaded data.
    assert!(
        ctx.num_chunks_uploaded >= 6,
        "Need at least 6 uploaded chunks for 3 PD txs x 2 chunks, got {}",
        ctx.num_chunks_uploaded,
    );

    // Inject 3 PD txs on Node A, each referencing 2 contiguous chunks at different offsets.
    let tx1_hash = build_and_inject_real_pd_tx(
        &ctx.node_a,
        &ctx.pd_signer,
        ctx.partition_index,
        ctx.local_offset, // offsets 0-1 relative to data start
        2,                // chunk_count
        0,                // nonce
    )
    .await?;
    info!("PD tx1 injected (offsets +0..+1): {:?}", tx1_hash);

    let tx2_hash = build_and_inject_real_pd_tx(
        &ctx.node_a,
        &ctx.pd_signer,
        ctx.partition_index,
        ctx.local_offset + 2, // offsets 2-3 relative to data start
        2,                    // chunk_count
        1,                    // nonce
    )
    .await?;
    info!("PD tx2 injected (offsets +2..+3): {:?}", tx2_hash);

    let tx3_hash = build_and_inject_real_pd_tx(
        &ctx.node_a,
        &ctx.pd_signer,
        ctx.partition_index,
        ctx.local_offset + 4, // offsets 4-5 relative to data start
        2,                    // chunk_count
        2,                    // nonce
    )
    .await?;
    info!("PD tx3 injected (offsets +4..+5): {:?}", tx3_hash);

    // Wait for PD monitor on Node A to provision all chunks.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Mine a block on Node A.
    let (block, eth_payload, _) = ctx.node_a.mine_block_without_gossip().await?;

    // Verify all 3 PD txs are in the block.
    let block_txs = &eth_payload.block().body().transactions;
    let tx1_included = block_txs.iter().any(|tx| tx.hash() == &tx1_hash);
    let tx2_included = block_txs.iter().any(|tx| tx.hash() == &tx2_hash);
    let tx3_included = block_txs.iter().any(|tx| tx.hash() == &tx3_hash);

    assert!(tx1_included, "PD tx1 (offsets +0..+1) should be in block");
    assert!(tx2_included, "PD tx2 (offsets +2..+3) should be in block");
    assert!(tx3_included, "PD tx3 (offsets +4..+5) should be in block");

    let block_height = block.height;
    info!(
        "Node A mined block at height {} with 3 PD txs",
        block_height,
    );

    // Gossip the block to Node B.
    ctx.node_a
        .gossip_block_to_peers(&Arc::new(block.as_ref().clone()))?;
    ctx.node_a.gossip_eth_block_to_peers(eth_payload.block())?;

    // Wait for Node B to validate and accept the block.
    ctx.node_b.wait_until_height(block_height, 30).await?;

    let node_b_height = ctx.node_b.get_canonical_chain_height().await;
    assert_eq!(
        node_b_height, block_height,
        "Node B should have validated the block with 3 PD txs",
    );

    // Note: We do NOT assert ChunkDataIndex here. For block-only PD chunks,
    // the PdBlockGuard drops after validation, releasing all chunk references.
    // Block acceptance at the correct height proves all 6 chunks were fetched.

    info!(
        "Node B validated block with 3 PD txs at height {} — block acceptance proves all chunks fetched",
        block_height,
    );

    ctx.node_b.stop().await;
    ctx.node_a.stop().await;
    Ok(())
}

/// PD tx T1 enters Node B's mempool (needs chunk X). Node A mines a block with
/// PD tx T2 also needing chunk X. Single fetch should serve both waiters.
///
/// This test exercises the deduplication logic in PdService: when two PD txs
/// reference the same chunk, only one P2P fetch should be initiated, and both
/// waiters should be satisfied when the chunk arrives.
///
/// Flow:
/// 1. setup_pd_p2p_test() — Node A has chunks, Node B synced to tip
/// 2. Submit T1 to Node B's mempool referencing chunk at local_offset (1 chunk)
/// 3. Submit T2 to Node A referencing the same chunk at local_offset (1 chunk)
/// 4. Node A mines a block containing T2, gossips to Node B
/// 5. Assert Node B validates the block
/// 6. Assert chunk is now available locally on Node B (fetched once for both)
///
/// Expected failure: Node B can't fetch chunks remotely yet. PdService on Node B
/// will fail to provision both T1 (mempool path) and T2 (block validation path).
#[ignore = "requires PD chunk P2P pull implementation (Stage 3)"]
#[test_log::test(tokio::test)]
async fn test_pd_chunk_p2p_deduplication() -> eyre::Result<()> {
    let ctx = setup_pd_p2p_test().await?;

    // T1: inject a PD tx directly into Node B's mempool, referencing 1 chunk
    // at (partition_index, local_offset). Node B does NOT have this chunk locally,
    // so PdService will need to P2P-fetch it.
    let t1_hash = build_and_inject_real_pd_tx(
        &ctx.node_b,
        &ctx.pd_signer,
        ctx.partition_index,
        ctx.local_offset,
        1, // chunk_count — same chunk as T2
        0, // nonce
    )
    .await?;
    info!(
        "T1 injected into Node B mempool (offset {}): {:?}",
        ctx.local_offset, t1_hash,
    );

    // T2: inject a PD tx into Node A's mempool, referencing the SAME chunk.
    // Use a DIFFERENT signer (pd_signer_2) so that when T2 is validated on Node B,
    // T1 is not evicted from Node B's mempool (same-signer nonce conflict would
    // invalidate T1 otherwise, removing its chunk reference from the PD cache).
    let t2_hash = build_and_inject_real_pd_tx(
        &ctx.node_a,
        &ctx.pd_signer_2,
        ctx.partition_index,
        ctx.local_offset,
        1, // chunk_count — same chunk as T1
        0, // nonce
    )
    .await?;
    info!(
        "T2 injected into Node A mempool (offset {}): {:?}",
        ctx.local_offset, t2_hash,
    );

    // Wait for the PD monitor on Node A to detect T2 and provision chunks locally.
    // Node A has chunks in storage, so this just needs the mempool monitor cycle.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Mine a block on Node A containing T2.
    let (block, eth_payload, _) = ctx.node_a.mine_block_without_gossip().await?;

    let t2_included = eth_payload
        .block()
        .body()
        .transactions
        .iter()
        .any(|tx| tx.hash() == &t2_hash);
    assert!(t2_included, "T2 should be included in Node A's mined block",);

    let block_height = block.height;
    info!("Node A mined block at height {} with T2", block_height,);

    // Gossip the block to Node B. Node B must:
    // 1. Fetch the chunk for T2 (block validation path)
    // 2. The same chunk also satisfies T1 (mempool path) — deduplication
    ctx.node_a
        .gossip_block_to_peers(&Arc::new(block.as_ref().clone()))?;
    ctx.node_a.gossip_eth_block_to_peers(eth_payload.block())?;

    // Wait for Node B to validate the block.
    ctx.node_b.wait_until_height(block_height, 30).await?;

    let node_b_height = ctx.node_b.get_canonical_chain_height().await;
    assert_eq!(
        node_b_height, block_height,
        "Node B should have validated the block containing T2",
    );

    // After P2P fetch, the chunk should be in Node B's ChunkDataIndex (PD cache)
    // and T1 (mempool tx) should be in ready_pd_txs.
    //
    // The timing depends on whether T1's NewTransaction was processed before or
    // after the block's fetch completed — both orderings are valid:
    //   - T1 registered before block fetch → dedup path, chunk stays in cache
    //   - T1 registered after block fetch  → T1 triggers its own fetch
    // Either way, poll until the post-conditions are met rather than asserting
    // immediately, which is racy under parallel load.
    let publish_ledger = DataLedger::Publish as u32;
    ctx.node_b
        .wait_for_pd_chunk_in_cache(publish_ledger, ctx.data_start_offset, 30)
        .await?;
    ctx.node_b.wait_for_ready_pd_tx(&t1_hash, 30).await?;

    info!("Deduplication verified: single fetch served both T1 and T2");

    ctx.node_b.stop().await;
    ctx.node_a.stop().await;
    Ok(())
}

/// PD tx enters Node B's mempool. Node B fetches chunks, tx transitions to Ready.
///
/// This test exercises the mempool-only path: a PD tx is submitted directly to
/// Node B, which must fetch the referenced chunks from peers to mark the tx as
/// ready for inclusion in a future block.
///
/// Flow:
/// 1. setup_pd_p2p_test() — Node A has chunks, Node B synced to tip
/// 2. Submit a PD tx to Node B's mempool referencing 2 chunks
/// 3. Wait for PdService on Node B to process the tx
/// 4. Assert the tx hash appears in Node B's ready_pd_txs set (once P2P fetch works)
/// 5. Assert chunks are in Node B's ChunkDataIndex
///
/// Expected failure: Node B can't fetch chunks remotely yet. PdService marks the tx
/// as pending/partially-ready because it cannot locate the chunks locally.
#[ignore = "requires PD chunk P2P pull implementation (Stage 3)"]
#[test_log::test(tokio::test)]
async fn test_pd_chunk_p2p_mempool_path() -> eyre::Result<()> {
    let ctx = setup_pd_p2p_test().await?;

    // Submit a PD tx directly to Node B's mempool referencing 2 real chunks.
    // Node B does NOT have these chunks in storage — it must P2P-fetch them.
    let tx_hash = build_and_inject_real_pd_tx(
        &ctx.node_b,
        &ctx.pd_signer,
        ctx.partition_index,
        ctx.local_offset,
        2, // chunk_count
        0, // nonce
    )
    .await?;
    info!(
        "PD tx injected into Node B mempool (offset {}, 2 chunks): {:?}",
        ctx.local_offset, tx_hash,
    );

    // Wait for PdService on Node B to detect the tx from the mempool monitor
    // and attempt to provision chunks. With P2P fetch implemented, this should
    // trigger a pull request to Node A for the 2 chunks.
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Once P2P fetch is implemented:
    // - PdService will fetch chunks from Node A
    // - The tx will transition to Ready in the ready_pd_txs set
    // - Chunks will be stored in Node B's ChunkDataIndex
    //
    // For now, we verify the tx was at least submitted successfully and the node
    // is still running (no panics from the missing-chunk path).
    let node_b_height = ctx.node_b.get_canonical_chain_height().await;
    info!(
        "Node B still running at height {} after PD tx submission",
        node_b_height,
    );

    // Verify chunks are in Node B's ChunkDataIndex (PD cache) after P2P fetch.
    // Also verify byte-level correctness: fetched chunks must match uploaded data.
    let publish_ledger = DataLedger::Publish as u32;
    let chunk_size = ctx.chunk_size as usize;
    for i in 0..2_u64 {
        let global_offset = ctx.data_start_offset + i;
        // Clone the Arc<Bytes> out of the DashMap ref to avoid holding the borrow.
        let fetched_bytes = ctx
            .node_b
            .node_ctx
            .chunk_data_index
            .get(&(publish_ledger, global_offset))
            .map(|r| Arc::clone(&*r));
        assert!(
            fetched_bytes.is_some(),
            "Chunk at ({}, {}) (+{}) should be in Node B's ChunkDataIndex after P2P fetch",
            publish_ledger,
            global_offset,
            i,
        );

        // Byte-level verification: compare fetched chunk bytes against uploaded data.
        let fetched = fetched_bytes.unwrap();
        let chunk_byte_start = i as usize * chunk_size;
        let chunk_byte_end = chunk_byte_start + chunk_size;
        let expected = &ctx.data_bytes[chunk_byte_start..chunk_byte_end];
        assert_eq!(
            fetched.as_ref(),
            expected,
            "Chunk at offset {} (+{}) bytes should match uploaded data",
            global_offset,
            i,
        );
    }

    // Assert the tx hash is in ready_pd_txs (PdService marks it ready after all chunks arrive).
    assert!(
        ctx.node_b.node_ctx.ready_pd_txs.contains(&tx_hash),
        "PD tx {:?} should be in Node B's ready_pd_txs after chunks were fetched",
        tx_hash,
    );

    info!("Mempool path verified: PD tx ready and chunks cached on Node B");

    ctx.node_b.stop().await;
    ctx.node_a.stop().await;
    Ok(())
}
