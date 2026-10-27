//! End-to-end data tree v2 behaviour. Every test uses public node APIs only, so
//! the tests compile and run on either data tree.

use crate::utils::{IrysNodeTest, TimeMode};
use irys_chain::IrysNodeCtx;
use irys_domain::ChunkType;
use irys_p2p::{GossipResponse, RejectionReason};
use irys_testing_utils::initialize_tracing;
use irys_types::{
    DataLedger, DataTransaction, H256, NodeConfig, TxChunkOffset, UnpackedChunk, irys::IrysSigner,
    serialization::Base64,
};
use std::time::Duration;

const CHUNK_SIZE: usize = 32;
const WAIT_SECONDS: usize = 30;

/// One partition slot with a single replica, a small partition and one ingress
/// proof to promote: the smallest chain that still promotes and migrates data.
fn single_node_config() -> NodeConfig {
    NodeConfig::testing()
        .with_consensus(|consensus| {
            consensus.chunk_size = CHUNK_SIZE as u64;
            consensus.num_partitions_per_slot = 1;
            consensus.num_chunks_in_partition = 10;
            consensus.num_chunks_in_recall_range = 2;
            consensus.entropy_packing_iterations = 1_000;
            consensus.block_migration_depth = 1;
            consensus.hardforks.frontier.number_of_ingress_proofs_total = 1;
            consensus.hardforks.cascade = None;
        })
        .with_genesis_peer_discovery_timeout(1000)
}

fn chunk_bytes(data: &[u8], chunk_index: usize) -> &[u8] {
    let start = chunk_index * CHUNK_SIZE;
    &data[start..(start + CHUNK_SIZE).min(data.len())]
}

/// POST one chunk to the node's public API. The caller picks the claimed
/// `data_size` and the bytes, so a test can post a chunk that does not match
/// its header.
async fn post_chunk_claiming(
    node: &IrysNodeTest<IrysNodeCtx>,
    tx: &DataTransaction,
    chunk_index: usize,
    bytes: &[u8],
    claimed_data_size: u64,
) -> reqwest::StatusCode {
    let tx_offset = TxChunkOffset::from(u32::try_from(chunk_index).expect("chunk index fits u32"));
    let chunk = UnpackedChunk {
        data_tree_root: tx.header.data_tree_root,
        data_size: claimed_data_size,
        data_path: tx.chunk_proof(tx_offset).expect("chunk proof"),
        bytes: Base64(bytes.to_vec()),
        tx_offset,
    };
    let url = format!(
        "{}/v1/chunk",
        node.node_ctx.config.node_config.local_api_url()
    );
    reqwest::Client::new()
        .post(&url)
        .json(&chunk)
        .send()
        .await
        .expect("chunk POST")
        .status()
}

/// Upload every chunk of `data`, short last chunk included.
async fn upload_all_chunks(node: &IrysNodeTest<IrysNodeCtx>, tx: &DataTransaction, data: &[u8]) {
    for chunk_index in 0..data.len().div_ceil(CHUNK_SIZE) {
        let status = post_chunk_claiming(
            node,
            tx,
            chunk_index,
            chunk_bytes(data, chunk_index),
            tx.header.data_size,
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::OK, "chunk {chunk_index}");
    }
}

async fn publish_block_ids(
    node: &IrysNodeTest<IrysNodeCtx>,
    tx_id: H256,
) -> eyre::Result<Vec<H256>> {
    let block = node
        .future_or_mine_on_timeout(
            node.wait_for_block_containing_tx(tx_id, DataLedger::Publish, WAIT_SECONDS),
            Duration::from_secs(5),
        )
        .await??;
    Ok(block.data_ledgers[DataLedger::Publish].tx_ids.0.clone())
}

/// A multi-chunk upload with a short last chunk promotes. A peer takes the
/// published chunks into its own partition. A block with a data PoA then
/// validates on both nodes.
#[test_log::test(tokio::test)]
async fn slow_heavy_two_nodes_upload_promote_migrate_sync_and_mine_v2_data() -> eyre::Result<()> {
    initialize_tracing();
    let mut config = single_node_config().with_consensus(|consensus| {
        // Two replicas per slot: the peer gets a replica of the slot with the data.
        consensus.num_partitions_per_slot = 2;
        // The 4-chunk upload fills the partition, so every recall range holds data.
        consensus.num_chunks_in_partition = 4;
        consensus.epoch.num_blocks_in_epoch = 4;
        consensus.epoch.submit_ledger_epoch_length = 1000;
    });
    let peer_signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&peer_signer]);

    let genesis = IrysNodeTest::new_genesis(config)
        .with_time_mode(TimeMode::Accelerated)
        .start_and_wait_for_packing("GENESIS", WAIT_SECONDS)
        .await;
    let genesis_signer = genesis.node_ctx.config.irys_signer();
    let peer = genesis.testing_peer_with_assignments(&peer_signer).await?;
    // The next epoch assigns the peer to the data ledger slots.
    genesis.mine_until_next_epoch().await?;
    let epoch_height = genesis.get_canonical_chain_height().await;
    peer.wait_until_height(epoch_height, WAIT_SECONDS).await?;
    peer.wait_for_packing(WAIT_SECONDS).await;

    // Three full chunks and a short last chunk.
    let data: Vec<u8> = (0..(3 * CHUNK_SIZE + 13))
        .map(|i| (i % 251) as u8)
        .collect();
    let tx = genesis
        .post_publish_data_tx(&genesis_signer, data.clone())
        .await?;
    upload_all_chunks(&genesis, &tx, &data).await;
    genesis
        .wait_for_ingress_proofs(vec![tx.header.id], WAIT_SECONDS)
        .await?;
    assert!(
        publish_block_ids(&genesis, tx.header.id)
            .await?
            .contains(&tx.header.id),
        "the tx is promoted into the Publish ledger"
    );

    // Published chunks reach the Publish slots when the promoting block migrates.
    for _ in 0..2 {
        let block = genesis.mine_block().await?;
        peer.wait_until_height(block.height, WAIT_SECONDS).await?;
    }

    // The peer takes the published chunks into its own Publish slot.
    let mut peer_has_data = false;
    for _ in 0..(WAIT_SECONDS * 2) {
        if !peer
            .get_storage_module_intervals(DataLedger::Publish, 0, ChunkType::Data)
            .is_empty()
        {
            peer_has_data = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        peer_has_data,
        "the peer holds published data chunks. peer publish {:?} submit {:?} genesis publish {:?} submit {:?}",
        peer.get_storage_module_intervals(DataLedger::Publish, 0, ChunkType::Data),
        peer.get_storage_module_intervals(DataLedger::Submit, 0, ChunkType::Data),
        genesis.get_storage_module_intervals(DataLedger::Publish, 0, ChunkType::Data),
        genesis.get_storage_module_intervals(DataLedger::Submit, 0, ChunkType::Data)
    );

    // The partition is full, so a Publish data PoA is likely each block. Other
    // partitions can still win a block, hence the bound.
    const MAX_BLOCKS_FOR_DATA_POA: usize = 96;
    let mut data_poa_block = None;
    for _ in 0..MAX_BLOCKS_FOR_DATA_POA {
        let block = genesis.mine_block().await?;
        if block.poa.is_data_poa() && block.poa.ledger_id == Some(u32::from(DataLedger::Publish)) {
            data_poa_block = Some(block);
            break;
        }
    }
    let block = data_poa_block.expect("a Publish data PoA occurs within the block budget");

    // Ownership by construction: the Publish ledger holds exactly this tx's
    // chunks, the PoA names a chunk of the Publish slot that holds them, and
    // the offset lies inside those chunks. The bytes are packed, so they are
    // not compared with the data.
    let tx_chunk_count = data.len().div_ceil(CHUNK_SIZE) as u64;
    assert_eq!(
        block.ledger_total_chunks(DataLedger::Publish),
        tx_chunk_count,
        "the test tx is the only data in the Publish ledger"
    );
    let publish_slot_zero = genesis
        .node_ctx
        .storage_modules_guard
        .read()
        .iter()
        .find_map(|module| {
            let assignment = module.partition_assignment()?;
            (assignment.ledger_id == Some(u32::from(DataLedger::Publish))
                && assignment.slot_index == Some(0))
            .then_some(assignment.partition_hash)
        })
        .expect("genesis holds Publish slot 0");
    assert_eq!(block.poa.partition_hash, publish_slot_zero);
    assert!(
        u64::from(block.poa.partition_chunk_offset) < tx_chunk_count,
        "the PoA offset is a chunk of the test tx"
    );

    peer.wait_until_height(block.height, WAIT_SECONDS).await?;
    let on_peer = peer.get_block_by_hash(&block.block_hash)?;
    assert_eq!(on_peer.block_hash, block.block_hash);
    assert_eq!(on_peer.poa, block.poa, "the peer validated the same PoA");

    peer.stop().await;
    genesis.stop().await;
    Ok(())
}

/// Chunks that are all the same bytes still make an upload that completes.
#[test_log::test(tokio::test)]
async fn heavy_identical_chunk_upload_completes() -> eyre::Result<()> {
    initialize_tracing();
    let config = single_node_config();
    let signer = config.signer();
    let node = IrysNodeTest::new_genesis(config)
        .start_and_wait_for_packing("GENESIS", WAIT_SECONDS)
        .await;

    let data = vec![0_u8; 4 * CHUNK_SIZE];
    let tx = node.post_publish_data_tx(&signer, data.clone()).await?;
    upload_all_chunks(&node, &tx, &data).await;

    node.wait_for_ingress_proofs(vec![tx.header.id], WAIT_SECONDS)
        .await?;
    assert!(
        publish_block_ids(&node, tx.header.id)
            .await?
            .contains(&tx.header.id)
    );

    node.stop().await;
    Ok(())
}

/// A second header reuses the first header's `data_tree_root` with another
/// `data_size`. Whatever the node does with that header, the honest upload
/// completes and promotes, and the other header never completes.
#[test_log::test(tokio::test)]
async fn heavy_same_root_two_sizes_do_not_block_each_other() -> eyre::Result<()> {
    initialize_tracing();
    let config = single_node_config();
    let signer = config.signer();
    let node = IrysNodeTest::new_genesis(config)
        .start_and_wait_for_packing("GENESIS", WAIT_SECONDS)
        .await;

    let honest_data: Vec<u8> = (0..(2 * CHUNK_SIZE)).map(|i| i as u8).collect();
    let honest = node
        .post_publish_data_tx(&signer, honest_data.clone())
        .await?;

    // Same root, one chunk more: the claimed size does not match the tree.
    let other_size = honest.header.data_size + CHUNK_SIZE as u64;
    let price = node.get_data_price(DataLedger::Publish, other_size).await?;
    let mut other = honest.clone();
    other.header.data_size = other_size;
    other.header.perm_fee = Some(price.perm_fee.into());
    other.header.term_fee = price.term_fee.into();
    let other = signer.sign_transaction(other)?;
    assert_ne!(other.header.id, honest.header.id);
    assert_eq!(other.header.data_tree_root, honest.header.data_tree_root);
    // The node may refuse this header at ingress. Both outcomes are allowed.
    let header_post = reqwest::Client::new()
        .post(format!(
            "{}/v1/tx",
            node.node_ctx.config.node_config.local_api_url()
        ))
        .json(&other.header)
        .send()
        .await?;
    tracing::info!(status = ?header_post.status(), "other-size header POST");

    // Offer the other header its chunks first, then upload the honest data.
    // The statuses are not asserted: only the outcome for each header is.
    for chunk_index in 0..honest_data.len().div_ceil(CHUNK_SIZE) {
        post_chunk_claiming(
            &node,
            &honest,
            chunk_index,
            chunk_bytes(&honest_data, chunk_index),
            other_size,
        )
        .await;
    }
    upload_all_chunks(&node, &honest, &honest_data).await;

    node.wait_for_ingress_proofs(vec![honest.header.id], WAIT_SECONDS)
        .await?;
    let published = publish_block_ids(&node, honest.header.id).await?;
    assert!(published.contains(&honest.header.id));
    assert!(
        !published.contains(&other.header.id),
        "the other-size header never completes"
    );
    // The other header stays unpromoted in every later block too.
    for _ in 0..2 {
        let later = node.mine_block().await?;
        assert!(
            !later.data_ledgers[DataLedger::Publish]
                .tx_ids
                .0
                .contains(&other.header.id),
            "the other-size header is not promoted at height {}",
            later.height
        );
    }

    node.stop().await;
    Ok(())
}

/// A gossiped chunk whose length is not the chunk size is a fault of the
/// sending peer: the receiver rejects it and cuts the peer's score.
#[test_log::test(tokio::test)]
async fn heavy_wrong_length_chunk_is_rejected_as_a_peer_fault() -> eyre::Result<()> {
    initialize_tracing();
    let mut config = single_node_config();
    let peer_signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&peer_signer]);
    let signer = config.signer();

    let genesis = IrysNodeTest::new_genesis(config)
        .start_and_wait_for_packing("GENESIS", WAIT_SECONDS)
        .await;
    let peer = IrysNodeTest::new(genesis.testing_peer_with_signer(&peer_signer))
        .start_with_name("PEER")
        .await;
    let peer_id = peer.node_ctx.config.peer_id();
    let peer_miner_address = peer.node_ctx.config.node_config.miner_address();

    // The receiver knows the header, so the chunk is checked, not parked.
    let data: Vec<u8> = (0..(3 * CHUNK_SIZE)).map(|i| i as u8).collect();
    let tx = genesis.post_publish_data_tx(&signer, data.clone()).await?;

    let mut score_before = None;
    for _ in 0..(WAIT_SECONDS * 2) {
        if let Some(item) = genesis.node_ctx.peer_list.get_peer(&peer_id) {
            score_before = Some(item.reputation_score.get());
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let score_before = score_before.expect("the receiver lists the sending peer");

    let tx_offset = TxChunkOffset::from(0_u32);
    let data_path = tx.chunk_proof(tx_offset)?;
    let honest_bytes = chunk_bytes(&data, 0);
    let gossip_chunk = async |bytes: &[u8]| -> eyre::Result<GossipResponse<()>> {
        let chunk = UnpackedChunk {
            data_tree_root: tx.header.data_tree_root,
            data_size: tx.header.data_size,
            data_path: data_path.clone(),
            bytes: Base64(bytes.to_vec()),
            tx_offset,
        };
        let request = serde_json::json!({
            "peer_id": peer_id,
            "miner_address": peer_miner_address,
            "data": chunk,
        });
        let url = format!("http://{}/gossip/v2/chunk", genesis.get_gossip_addr());
        Ok(reqwest::Client::new()
            .post(&url)
            .json(&request)
            .send()
            .await?
            .json()
            .await?)
    };
    let score_of = || {
        genesis
            .node_ctx
            .peer_list
            .get_peer(&peer_id)
            .expect("peer still listed")
            .reputation_score
            .get()
    };

    // The honest proof with the honest bytes one byte short. The wrong-length
    // chunk goes first: a stored duplicate would be accepted without a check.
    let response = gossip_chunk(&honest_bytes[..CHUNK_SIZE - 1]).await?;
    assert!(
        matches!(
            response,
            GossipResponse::Rejected(RejectionReason::InvalidData)
        ),
        "the wrong-length chunk is rejected as invalid data: {response:?}"
    );
    let score_after = score_of();
    assert!(
        score_after < score_before,
        "peer score is cut: {score_before} -> {score_after}"
    );

    // Control: the same proof with the correctly sized bytes is accepted, and
    // it costs no score. Only the length differs from the rejected chunk.
    let response = gossip_chunk(honest_bytes).await?;
    assert!(
        matches!(response, GossipResponse::Accepted(())),
        "the correctly sized chunk is accepted: {response:?}"
    );
    assert!(score_of() >= score_after, "an honest chunk costs no score");

    peer.stop().await;
    genesis.stop().await;
    Ok(())
}
