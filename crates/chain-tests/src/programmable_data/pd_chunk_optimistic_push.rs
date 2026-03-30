//! PD chunk optimistic push integration tests.
//!
//! Tests the optimistic push feature across a 3-node network:
//! - Genesis (Node A): has partition assignments, stores chunks, source of push
//! - Block Producer (Node B): staked with assignments, no PD data for test offsets
//! - Observer (Node C): not staked, not pledged, validator-only push target
//!
//! Three scenarios:
//! 1. Happy path: push delivers chunk before block validation
//! 2. Cache-hit shortcut: duplicate push exits early without re-verification
//! 3. Pending fetch reconciliation: push wins over in-flight P2P fetch

use std::sync::Arc;

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

/// Context returned by [`setup_pd_push_test`] containing all 3 nodes and metadata.
#[expect(dead_code, reason = "fields available for future test assertions")]
pub(crate) struct PdPushTestContext {
    /// Genesis node: has partition assignments, stores chunks, source of optimistic pushes.
    pub genesis: IrysNodeTest<irys_chain::IrysNodeCtx>,
    /// Block producer peer: staked, has assignments. Does NOT store PD data for test offsets.
    pub block_producer: IrysNodeTest<irys_chain::IrysNodeCtx>,
    /// Observer node: not staked, not pledged, validator-only. Primary push target.
    pub observer: IrysNodeTest<irys_chain::IrysNodeCtx>,
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
    /// The raw data bytes that were uploaded to Genesis.
    pub data_bytes: Vec<u8>,
    /// Signer/account used to upload data on Genesis.
    pub data_signer: IrysSigner,
    /// Signer/account for the block producer (Node B).
    pub peer_signer: IrysSigner,
    /// Signer/account for the observer (Node C).
    pub observer_signer: IrysSigner,
    /// Signer/account dedicated to submitting PD transactions.
    pub pd_signer: IrysSigner,
    /// Second PD signer for tests needing two independent PD accounts.
    pub pd_signer_2: IrysSigner,
}

/// Start 3 nodes for PD chunk optimistic push testing.
///
/// 1. Starts Genesis with `chunk_size=32`, `block_migration_depth=2`,
///    `num_chunks_in_partition=10`, `pd_optimistic_push_fanout=fanout`.
/// 2. Uploads 16 chunks x 32 bytes = 512 bytes of data on Genesis.
/// 3. Mines blocks past migration depth so chunks migrate to storage modules.
/// 4. Starts Block Producer with partition assignments (staked + pledged + epoch).
/// 5. Starts Observer as a validator-only peer (not staked, not pledged).
/// 6. Syncs Observer to Genesis block index height.
pub(crate) async fn setup_pd_push_test(fanout: u32) -> eyre::Result<PdPushTestContext> {
    let chunk_size: u64 = 32;
    let num_chunks_in_partition: u64 = 10;
    let num_chunks_uploaded: u64 = 16;
    let seconds_to_wait = 30;

    // --- Configure Genesis (Node A) ---
    let mut config = NodeConfig::testing();
    config.consensus.get_mut().chunk_size = chunk_size;
    config.consensus.get_mut().block_migration_depth = 2;
    config.consensus.get_mut().num_chunks_in_recall_range = 2;
    config.consensus.get_mut().num_chunks_in_partition = num_chunks_in_partition;
    config.p2p_gossip.pd_optimistic_push_fanout = fanout;

    let data_signer = config.new_random_signer();
    let peer_signer = config.new_random_signer();
    let observer_signer = config.new_random_signer();
    let pd_signer = config.new_random_signer();
    let pd_signer_2 = config.new_random_signer();
    config.fund_genesis_accounts(vec![
        &data_signer,
        &peer_signer,
        &observer_signer,
        &pd_signer,
        &pd_signer_2,
    ]);

    let genesis = IrysNodeTest::new_genesis(config)
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // --- Upload real data on Genesis: 16 chunks x 32 bytes = 512 bytes ---
    let data_bytes: Vec<u8> = (0..num_chunks_uploaded * chunk_size)
        .map(|i| (i & 0xff) as u8)
        .collect();

    // Record Publish ledger total_chunks BEFORE posting — this is the data_start_offset.
    let offset_before = {
        let block_index = genesis.node_ctx.block_index_guard.read();
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
    let tx = genesis
        .post_publish_data_tx(&data_signer, data_bytes.clone())
        .await
        .map_err(|e| eyre::eyre!("Failed to post data tx: {:?}", e))?;

    // Upload chunks via HTTP so they are in the cache for migration.
    let client = reqwest::Client::new();
    let http_url = format!(
        "http://127.0.0.1:{}",
        genesis.node_ctx.config.node_config.http.bind_port
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
    genesis
        .wait_for_migrated_txs(vec![tx.header.clone()], seconds_to_wait)
        .await?;

    // Wait for chunk to be available in storage module.
    genesis
        .wait_for_chunk_in_storage(
            DataLedger::Publish,
            LedgerChunkOffset::from(offset_before),
            seconds_to_wait,
        )
        .await?;

    let data_start_offset = offset_before;
    info!(
        "Data uploaded and migrated on Genesis: {} chunks, data_start_offset={}",
        num_chunks_uploaded, data_start_offset
    );

    // --- Start Block Producer (Node B) with partition assignments ---
    let block_producer = genesis.testing_peer_with_assignments(&peer_signer).await?;
    info!("Block Producer started with partition assignments");

    // Record Genesis block index height for Observer sync.
    let genesis_index_height = genesis.get_block_index_height();
    info!("Genesis block index height: {}", genesis_index_height);

    // --- Start Observer (Node C) — validator-only, no staking/pledging ---
    let observer_config = genesis.testing_peer_with_signer(&observer_signer);
    let observer = IrysNodeTest::new(observer_config)
        .start_with_name("OBSERVER")
        .await;

    // Wait for Observer's block index to catch up to Genesis.
    observer
        .wait_until_block_index_height(genesis_index_height, seconds_to_wait)
        .await?;
    info!(
        "Observer block index synced to height {}",
        genesis_index_height
    );

    // Compute partition_index and local_offset from data_start_offset.
    let partition_index = data_start_offset / num_chunks_in_partition;
    let local_offset = (data_start_offset % num_chunks_in_partition) as u32;

    Ok(PdPushTestContext {
        genesis,
        block_producer,
        observer,
        data_start_offset,
        partition_index,
        local_offset,
        num_chunks_in_partition,
        chunk_size,
        num_chunks_uploaded,
        data_bytes,
        data_signer,
        peer_signer,
        observer_signer,
        pd_signer,
        pd_signer_2,
    })
}

/// Build a signed PD transaction referencing real chunk offsets and inject it.
///
/// Unlike `create_and_inject_pd_transaction_with_priority_fee` (which uses `U200::MAX`
/// sentinel to bypass chunk provisioning), this builds a PD tx with real
/// `partition_index` and `offset` values, forcing PdService to actually provision.
pub(crate) async fn build_and_inject_real_pd_tx(
    node: &IrysNodeTest<irys_chain::IrysNodeCtx>,
    signer: &IrysSigner,
    partition_index: u64,
    offset: u32,
    chunk_count: u16,
    nonce: u64,
) -> eyre::Result<alloy_primitives::FixedBytes<32>> {
    let local_signer = LocalSigner::from(signer.signer.clone());
    let chain_id = node.node_ctx.config.consensus.chain_id;

    let specs = vec![ChunkRangeSpecifier {
        partition_index: U200::from(partition_index),
        offset,
        chunk_count,
    }];
    let access_list = build_pd_access_list(specs.into_iter());

    let header = PdHeaderV1 {
        max_priority_fee_per_chunk: U256::from(10_000_000_000_000_000_u64),
        max_base_fee_per_chunk: U256::from(1_000_000_000_000_000_u64),
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

/// Genesis pushes PD chunks to Observer via optimistic push. Observer verifies
/// (block index + merkle + leaf hash) and caches. Block validates with cache hits.
///
/// Key assertion: chunks appear in Observer's ChunkDataIndex BEFORE any block
/// is gossiped, proving they arrived via push (not P2P fetch during validation).
#[test_log::test(tokio::test)]
async fn slow_heavy3_pd_chunk_optimistic_push_happy_path() -> eyre::Result<()> {
    let ctx = setup_pd_push_test(4).await?;

    let publish_ledger = DataLedger::Publish as u32;

    // Pre-test invariant: Observer should NOT have chunks at the target offset.
    for i in 0..2_u64 {
        let offset = ctx.data_start_offset + i;
        assert!(
            ctx.observer
                .node_ctx
                .chunk_data_index
                .get(&(publish_ledger, offset))
                .is_none(),
            "Observer should NOT have chunk at ({}, {}) before push",
            publish_ledger,
            offset,
        );
    }

    // Inject a PD tx on Genesis referencing 2 real chunks.
    // This triggers: handle_provision_chunks → cache hit (local storage) → schedule_outbound_push
    let tx_hash = build_and_inject_real_pd_tx(
        &ctx.genesis,
        &ctx.pd_signer,
        ctx.partition_index,
        ctx.local_offset,
        2, // chunk_count
        0, // nonce
    )
    .await?;
    info!("PD tx injected on Genesis: {:?}", tx_hash);

    // Wait for Genesis to provision and push. The push is fire-and-forget,
    // so we wait for the chunk to appear in Observer's cache.
    ctx.observer
        .wait_for_pd_chunk_in_cache(publish_ledger, ctx.data_start_offset, 30)
        .await?;
    ctx.observer
        .wait_for_pd_chunk_in_cache(publish_ledger, ctx.data_start_offset + 1, 30)
        .await?;
    info!("Chunks arrived in Observer cache via optimistic push (before block gossip)");

    // Verify byte-level correctness of pushed chunks.
    let chunk_size = ctx.chunk_size as usize;
    for i in 0..2_u64 {
        let global_offset = ctx.data_start_offset + i;
        let fetched_bytes = ctx
            .observer
            .node_ctx
            .chunk_data_index
            .get(&(publish_ledger, global_offset))
            .map(|r| Arc::clone(&*r));
        assert!(
            fetched_bytes.is_some(),
            "Chunk at ({}, {}) should be in Observer's ChunkDataIndex after push",
            publish_ledger,
            global_offset,
        );

        let fetched = fetched_bytes.unwrap();
        let chunk_byte_start = i as usize * chunk_size;
        let chunk_byte_end = chunk_byte_start + chunk_size;
        let expected = &ctx.data_bytes[chunk_byte_start..chunk_byte_end];
        assert_eq!(
            fetched.as_ref(),
            expected,
            "Pushed chunk at offset {} bytes should match uploaded data",
            global_offset,
        );
    }

    // Wait for Genesis to mark the PD tx as ready, then mine.
    ctx.genesis.wait_for_ready_pd_tx(&tx_hash, 30).await?;
    let (block, eth_payload, _) = ctx.genesis.mine_block_without_gossip().await?;

    // Verify PD tx is in the block.
    let pd_tx_included = eth_payload
        .block()
        .body()
        .transactions
        .iter()
        .any(|tx| tx.hash() == &tx_hash);
    assert!(
        pd_tx_included,
        "PD tx {:?} should be included in Genesis's mined block",
        tx_hash,
    );

    let block_height = block.height;
    info!("Genesis mined block at height {} with PD tx", block_height);

    // Gossip the block to Observer.
    ctx.genesis.gossip_block_to_peers(&block)?;
    ctx.genesis.gossip_eth_block_to_peers(eth_payload.block())?;

    // Wait for Observer to validate and accept the block.
    // Chunks are already cached from push — this should be a fast cache hit.
    ctx.observer.wait_until_height(block_height, 30).await?;

    let observer_height = ctx.observer.get_canonical_chain_height().await;
    assert_eq!(
        observer_height, block_height,
        "Observer canonical tip should match Genesis after validation",
    );

    info!(
        "Observer validated PD block at height {} — push delivered chunks before block arrived",
        block_height,
    );

    ctx.observer.stop().await;
    ctx.block_producer.stop().await;
    ctx.genesis.stop().await;
    Ok(())
}
