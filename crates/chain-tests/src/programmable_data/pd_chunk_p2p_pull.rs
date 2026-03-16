//! Shared test setup helper for PD chunk P2P pull integration tests.
//!
//! Provides `PdP2pTestContext` and `setup_pd_p2p_test()` which:
//! - Start a genesis Node A with real uploaded data (16 chunks x 32 bytes = 512 bytes)
//! - Mine blocks past migration depth so chunks are in storage modules
//! - Start a peer Node B that syncs to Node A's tip (validator-only, no mining)
//! - Return context with both nodes, data offsets, and signer accounts

use std::time::Duration;

use irys_types::irys::IrysSigner;
use irys_types::{Base64, DataLedger, NodeConfig, TxChunkOffset, UnpackedChunk};
use tracing::info;

use crate::utils::IrysNodeTest;

/// Context returned by [`setup_pd_p2p_test`] containing both nodes and metadata
/// about the uploaded data.
#[allow(dead_code)]
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
#[allow(dead_code)]
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
    config.fund_genesis_accounts(vec![&data_signer, &peer_signer, &pd_signer]);

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

    // Record the current canonical chain height so we can wait for Node B to sync.
    let node_a_height = node_a.get_canonical_chain_height().await;
    info!("Node A canonical chain height: {}", node_a_height);

    // --- Start Node B (peer, validator-only, no staking/pledging/mining) ---
    let peer_config = node_a.testing_peer_with_signer(&peer_signer);
    let node_b = IrysNodeTest::new(peer_config)
        .start_with_name("NODE_B")
        .await;

    // Wait for Node B to sync and confirm to Node A's tip.
    // Uses wait_until_height_confirmed so that blocks are migrated to MDBX (BlockIndex populated).
    // Node B needs BlockIndex + DataTransactionHeaders for local data_root derivation.
    node_b
        .wait_until_height_confirmed(node_a_height, seconds_to_wait)
        .await?;
    info!(
        "Node B synced and confirmed to Node A height {}",
        node_a_height
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
    })
}
