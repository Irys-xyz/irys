use crate::{api::pd_fee_history_request, utils::IrysNodeTest};
use irys_api_server::routes::pd_pricing::PdFeeHistoryResponse;
use irys_chain::IrysNodeCtx;
use irys_types::{
    storage_pricing::{Amount, PRECISION_SCALE},
    NodeConfig, U256,
};

/// Block specification for test chain setup
struct BlockSpec {
    /// Total chunks to use in this block (controls utilization)
    chunks: u64,
    /// Priority fees for each PD transaction (one tx per fee value)
    /// Chunks will be distributed evenly across transactions
    priority_fees: Vec<u64>,
}

/// Metadata tracked for each block during setup
struct BlockMetadata {
    /// Actual base fee extracted from the block
    base_fee: U256,
    /// Number of chunks used in the block
    chunks_used: u64,
    /// Priority fees of PD transactions in the block
    priority_fees: Vec<u64>,
}

/// Sets up a test chain with blocks having varying PD utilization patterns.
///
/// Creates a 6-block chain with the following utilization:
/// - Block 1: 0% (no PD txs)
/// - Block 2: 50% (5 txs × 10 chunks each, priority fees 1-5 × 1e9)
/// - Block 3: 80% (4 txs × 20 chunks each, priority fees [1,1,5,5] × 1e9)
/// - Block 4: 0% (no PD txs)
/// - Block 5: 100% (5 txs × 20 chunks each, priority fees [1,2,5,8,10] × 1e9)
/// - Block 6: 25% (5 txs × 5 chunks each, all priority fee 1e9)
async fn setup_pd_fee_history_test_chain() -> eyre::Result<(
    IrysNodeTest<IrysNodeCtx>,
    String,
    Vec<BlockMetadata>,
    u64,
    u64,
)> {
    use alloy_consensus::Transaction as _;
    use irys_actors::pd_pricing::base_fee::PD_BASE_FEE_INDEX;
    use irys_reth::shadow_tx::{ShadowTransaction, TransactionPacket};

    // Configure node with predictable PD parameters
    let mut config = NodeConfig::testing();
    config.consensus.get_mut().chunk_size = 32;
    // All 255 chunks must fit in a single partition so the genesis node's storage
    // modules can store them (default num_chunks_in_partition=10 would span 26 partitions).
    config.consensus.get_mut().num_chunks_in_partition = 256;
    let sprite = config
        .consensus
        .get_mut()
        .hardforks
        .sprite
        .as_mut()
        .expect("Sprite hardfork must be configured for testing");
    sprite.max_pd_chunks_per_block = 100;
    sprite.min_pd_transaction_cost = Amount::new(U256::from(0));

    // Create and fund a test account for PD transactions
    let pd_signer = config.new_random_signer();
    config.fund_genesis_accounts(vec![&pd_signer]);

    // Start node
    let node = IrysNodeTest::new_genesis(config.clone()).start().await;
    let address = format!(
        "http://127.0.0.1:{}",
        node.node_ctx.config.node_config.http.bind_port
    );

    // === Phase 1: Upload all data needed for Phase 2 ===
    // Block specs: 0 + 50 + 80 + 0 + 100 + 25 = 255 total chunks
    // At chunk_size=32, that's 255 * 32 = 8160 bytes
    let total_chunks: u64 = 255;
    let chunk_size = config.consensus.get_mut().chunk_size as usize;
    let data = vec![0xAB_u8; total_chunks as usize * chunk_size];
    let data_start_offset = node.upload_data_for_pd(&pd_signer, &data, 60).await?;

    // Record the tip height after Phase 1 completes (before Phase 2 blocks).
    // Must use block_tree (not block_index, which is 6 blocks behind tip).
    let phase2_start_height = node.get_canonical_chain_height().await;

    // === Phase 2: Mine 6 blocks with varying PD utilization ===
    let block_specs = [
        // Block 1: 0% utilization - no PD txs
        BlockSpec {
            chunks: 0,
            priority_fees: vec![],
        },
        // Block 2: 50% utilization - 5 txs × 10 chunks, fees [1,2,3,4,5] × 1e9
        BlockSpec {
            chunks: 50,
            priority_fees: vec![
                1_000_000_000,
                2_000_000_000,
                3_000_000_000,
                4_000_000_000,
                5_000_000_000,
            ],
        },
        // Block 3: 80% utilization - 4 txs × 20 chunks, fees [1,1,5,5] × 1e9
        BlockSpec {
            chunks: 80,
            priority_fees: vec![1_000_000_000, 1_000_000_000, 5_000_000_000, 5_000_000_000],
        },
        // Block 4: 0% utilization - no PD txs
        BlockSpec {
            chunks: 0,
            priority_fees: vec![],
        },
        // Block 5: 100% utilization - 5 txs × 20 chunks, fees [1,2,5,8,10] × 1e9
        BlockSpec {
            chunks: 100,
            priority_fees: vec![
                1_000_000_000,
                2_000_000_000,
                5_000_000_000,
                8_000_000_000,
                10_000_000_000,
            ],
        },
        // Block 6: 25% utilization - 5 txs × 5 chunks, all fee 1e9
        BlockSpec {
            chunks: 25,
            priority_fees: vec![
                1_000_000_000,
                1_000_000_000,
                1_000_000_000,
                1_000_000_000,
                1_000_000_000,
            ],
        },
    ];

    let mut block_metadata = Vec::new();
    let mut nonce = 0_u64;
    let mut cursor = data_start_offset;

    // Large base fee cap — we don't want base fee rejection in this test
    const LARGE_BASE_FEE_CAP: u64 = 1_000_000_000_000_000;

    for (block_idx, spec) in block_specs.iter().enumerate() {
        if !spec.priority_fees.is_empty() {
            let num_txs = spec.priority_fees.len();
            let chunks_per_tx = spec.chunks / num_txs as u64;

            let mut tx_hashes = Vec::new();
            for priority_fee in &spec.priority_fees {
                let tx_hash = node
                    .inject_pd_tx_at_real_offsets(
                        &pd_signer,
                        cursor,
                        chunks_per_tx,
                        *priority_fee,
                        LARGE_BASE_FEE_CAP,
                        nonce,
                    )
                    .await?;
                tx_hashes.push(tx_hash);
                nonce += 1;
                cursor += chunks_per_tx;
            }

            // Wait for PdService to provision all chunks and mark txs ready
            for tx_hash in &tx_hashes {
                node.wait_for_ready_pd_tx(tx_hash, 30).await?;
            }
        }

        // Mine the block
        let (irys_block, eth_payload, _block_txs) = node.mine_block_without_gossip().await?;
        node.wait_until_height(irys_block.height, 10).await?;

        // Extract the PD base fee from the block
        let sealed_block = eth_payload.block();
        let tx_at_index_1 = sealed_block
            .body()
            .transactions
            .get(PD_BASE_FEE_INDEX)
            .ok_or_else(|| eyre::eyre!("Block {} missing tx at index 1", block_idx))?;

        let shadow_tx_1 = ShadowTransaction::decode(&mut tx_at_index_1.input().as_ref())
            .map_err(|e| eyre::eyre!("Failed to decode shadow tx in block {}: {}", block_idx, e))?;

        let base_fee = match shadow_tx_1
            .as_v1()
            .ok_or_else(|| eyre::eyre!("Expected V1 shadow tx"))?
        {
            TransactionPacket::PdBaseFeeUpdate(update) => U256::from(update.per_chunk),
            TransactionPacket::TreasuryDeposit(_) => {
                let tx_at_index_2 = sealed_block
                    .body()
                    .transactions
                    .get(PD_BASE_FEE_INDEX + 1)
                    .ok_or_else(|| {
                        eyre::eyre!(
                            "Block {} missing tx at index 2 (first Sprite block)",
                            block_idx
                        )
                    })?;

                let shadow_tx_2 = ShadowTransaction::decode(&mut tx_at_index_2.input().as_ref())
                    .map_err(|e| {
                        eyre::eyre!(
                            "Failed to decode shadow tx at index 2 in block {}: {}",
                            block_idx,
                            e
                        )
                    })?;

                match shadow_tx_2
                    .as_v1()
                    .ok_or_else(|| eyre::eyre!("Expected V1 shadow tx at index 2"))?
                {
                    TransactionPacket::PdBaseFeeUpdate(update) => U256::from(update.per_chunk),
                    other => {
                        return Err(eyre::eyre!(
                            "Block {} 3rd tx is not PdBaseFeeUpdate: {:?}",
                            block_idx,
                            other
                        ))
                    }
                }
            }
            other => {
                return Err(eyre::eyre!(
                    "Block {} 2nd tx is not PdBaseFeeUpdate or TreasuryDeposit: {:?}",
                    block_idx,
                    other
                ))
            }
        };

        block_metadata.push(BlockMetadata {
            base_fee,
            chunks_used: spec.chunks,
            priority_fees: spec.priority_fees.clone(),
        });
    }

    let max_pd_chunks = config
        .consensus
        .get_mut()
        .hardforks
        .sprite
        .as_ref()
        .expect("Sprite hardfork must be configured")
        .max_pd_chunks_per_block;
    Ok((
        node,
        address,
        block_metadata,
        max_pd_chunks,
        phase2_start_height,
    ))
}

/// Test that the fee history API returns valid data with correct structure.
///
/// Verifies:
/// - Base fee arrays have correct length (N+1 for historical + prediction)
/// - Gas used ratio arrays have correct length (N)
/// - Base fees match the actual values extracted during block creation
/// - Utilization percentages match expected values based on chunks used
/// - Arrays follow eth_feeHistory ordering (oldest-to-newest)
#[test_log::test(tokio::test)]
async fn heavy_pd_fee_history_base_fee_progression() -> eyre::Result<()> {
    let (node, address, block_metadata, max_pd_chunks, phase2_start_height) =
        setup_pd_fee_history_test_chain().await?;

    // Request fee history for all 6 blocks
    let response = pd_fee_history_request(&address, 6).await;
    assert_eq!(
        response.status(),
        reqwest::StatusCode::OK,
        "Fee history request should succeed"
    );

    let fee_history: PdFeeHistoryResponse = response.json().await?;

    // Verify array lengths (N+1 for base fees, N for others)
    assert_eq!(
        fee_history.base_fee_per_chunk_irys.len(),
        7,
        "base_fee_per_chunk_irys should have N+1 elements (6 historical + 1 prediction)"
    );
    assert_eq!(
        fee_history.base_fee_per_chunk_usd.len(),
        7,
        "base_fee_per_chunk_usd should have N+1 elements"
    );
    assert_eq!(
        fee_history.gas_used_ratio.len(),
        6,
        "gas_used_ratio should have N elements"
    );
    assert_eq!(fee_history.reward.len(), 6, "reward should have N elements");

    assert_eq!(
        fee_history.oldest_block,
        phase2_start_height + 1,
        "oldest_block should be phase2_start_height + 1 (first Phase 2 block)"
    );

    // Response is oldest-first. block_metadata[i] corresponds to response[i].
    let max_chunks = U256::from(max_pd_chunks);
    for (i, metadata) in block_metadata.iter().enumerate() {
        assert_eq!(
            fee_history.base_fee_per_chunk_irys[i].amount,
            metadata.base_fee,
            "Block {} base fee mismatch",
            i + 1
        );

        let expected_ratio = U256::from(metadata.chunks_used) * PRECISION_SCALE / max_chunks;
        assert_eq!(
            fee_history.gas_used_ratio[i].amount,
            expected_ratio,
            "Block {} utilization mismatch: expected {}%",
            i + 1,
            metadata.chunks_used
        );
    }

    // Verify all base fees are non-zero (including the N+1 prediction)
    for (i, fee) in fee_history.base_fee_per_chunk_irys.iter().enumerate() {
        assert!(
            fee.amount > U256::from(0),
            "Base fee at index {} should be non-zero",
            i
        );
    }

    // Verify all USD fees are non-zero
    for (i, fee) in fee_history.base_fee_per_chunk_usd.iter().enumerate() {
        assert!(
            fee.amount > U256::from(0),
            "USD fee at index {} should be non-zero",
            i
        );
    }

    node.stop().await;
    Ok(())
}

/// Compute percentile value from sorted fees (nearest-rank method)
fn compute_percentile(sorted_fees: &[u64], percentile: u8) -> u64 {
    let index =
        ((percentile as usize * sorted_fees.len()) / 100).min(sorted_fees.len().saturating_sub(1));
    sorted_fees[index]
}

/// Test that priority fee percentiles are returned correctly.
///
/// Verifies:
/// - Blocks with PD transactions have percentile data
/// - Empty blocks have empty percentile maps
/// - Percentile values match expected computation
#[test_log::test(tokio::test)]
async fn heavy_pd_fee_history_priority_fee_percentiles() -> eyre::Result<()> {
    let (node, address, block_metadata, _max_pd_chunks, phase2_start_height) =
        setup_pd_fee_history_test_chain().await?;

    let response = pd_fee_history_request(&address, 6).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let fee_history: PdFeeHistoryResponse = response.json().await?;

    assert_eq!(
        fee_history.oldest_block,
        phase2_start_height + 1,
        "oldest_block should be phase2_start_height + 1 (first Phase 2 block)"
    );

    // Verify percentiles for each block. block_metadata[i] corresponds to response[i].
    for (i, metadata) in block_metadata.iter().enumerate() {
        let reward = &fee_history.reward[i];

        if metadata.priority_fees.is_empty() {
            assert!(
                reward.percentiles.is_empty(),
                "Block {} should have no percentiles",
                i + 1
            );
            assert_eq!(reward.pd_tx_count, 0);
        } else {
            let mut sorted_fees = metadata.priority_fees.clone();
            sorted_fees.sort();

            assert_eq!(reward.pd_tx_count, sorted_fees.len());

            for percentile in [25_u8, 50, 75] {
                let expected = compute_percentile(&sorted_fees, percentile);
                let fee_data = reward
                    .percentiles
                    .get(&percentile)
                    .unwrap_or_else(|| panic!("Block {} missing percentile {}", i + 1, percentile));

                assert_eq!(
                    fee_data.fee_irys.amount,
                    U256::from(expected),
                    "Block {} percentile {} mismatch",
                    i + 1,
                    percentile
                );
            }
        }
    }

    node.stop().await;
    Ok(())
}

/// Test that the fee history API returns min_pd_transaction_cost and that
/// transactions must respect this minimum to be included in blocks.
///
/// Verifies the full flow:
/// 1. API returns min_pd_transaction_cost in response
/// 2. PD transaction with fees < min is rejected (not included in block)
/// 3. PD transaction with fees >= min is accepted (included in block)
#[test_log::test(tokio::test)]
async fn heavy_pd_fee_history_returns_min_transaction_cost() -> eyre::Result<()> {
    // Configure node with high min_pd_transaction_cost
    let mut config = NodeConfig::testing();
    config.consensus.get_mut().chunk_size = 32;

    // Create and fund two signers for PD transactions (separate to avoid nonce conflicts)
    let low_fee_signer = config.new_random_signer();
    let adequate_fee_signer = config.new_random_signer();
    config.fund_genesis_accounts(vec![&low_fee_signer, &adequate_fee_signer]);

    let sprite = config
        .consensus
        .get_mut()
        .hardforks
        .sprite
        .as_mut()
        .expect("Sprite hardfork must be configured for testing");

    // Set minimum to 1 gwei (1e9 wei). At $1/IRYS, low fees (200 wei) won't meet this.
    let min_cost = Amount::new(U256::from(10_u64.pow(9)));
    sprite.min_pd_transaction_cost = min_cost;
    sprite.base_fee_floor = Amount::new(U256::from(10_u64.pow(8)));
    sprite.max_pd_chunks_per_block = 100;

    let min_cost_usd = min_cost.amount;

    // Start node
    let node = IrysNodeTest::new_genesis(config).start().await;
    let address = format!(
        "http://127.0.0.1:{}",
        node.node_ctx.config.node_config.http.bind_port
    );

    // === Phase 1: Upload 2 chunks of real data ===
    let chunk_size = 32_usize;
    let data = vec![0xCD_u8; 2 * chunk_size]; // 2 chunks × 32 bytes
    let data_start_offset = node.upload_data_for_pd(&low_fee_signer, &data, 60).await?;

    // Mine one more block to establish pricing (same as original test)
    node.mine_block().await?;

    // === Phase 2: Test fee threshold behavior ===

    // 1. Query API and verify min_pd_transaction_cost is returned
    let response = pd_fee_history_request(&address, 1).await;
    assert_eq!(
        response.status(),
        reqwest::StatusCode::OK,
        "Fee history request should succeed"
    );

    let fee_history: PdFeeHistoryResponse = response.json().await?;

    assert_eq!(
        fee_history.min_pd_transaction_cost_usd.amount, min_cost_usd,
        "API should return configured min_pd_transaction_cost_usd"
    );
    let min_cost_irys = fee_history.min_pd_transaction_cost_irys.amount;
    assert!(
        min_cost_irys > U256::from(0),
        "min_pd_transaction_cost_irys should be non-zero"
    );

    // 2. Submit LOW-FEE transaction (below minimum) at first real offset
    let low_priority_fee = 100_u64; // 100 wei per chunk
    let low_base_fee = 100_u64; // 100 wei per chunk
                                // Total for 1 chunk = 200 wei, far below min_cost_irys

    let low_fee_tx_hash = node
        .inject_pd_tx_at_real_offsets(
            &low_fee_signer,
            data_start_offset, // 1 chunk at first real offset
            1,
            low_priority_fee,
            low_base_fee,
            0, // nonce
        )
        .await?;

    // Wait for PdService to provision the chunk
    node.wait_for_ready_pd_tx(&low_fee_tx_hash, 30).await?;

    // Mine block - low-fee tx should NOT be included (rejected by block producer)
    let (_, eth_payload, _) = node.mine_block_without_gossip().await?;
    let low_fee_included = eth_payload
        .block()
        .body()
        .transactions
        .iter()
        .any(|tx| *tx.tx_hash() == low_fee_tx_hash);

    assert!(
        !low_fee_included,
        "Low-fee PD tx (fees below min_pd_transaction_cost) should be REJECTED"
    );

    // 3. Submit ADEQUATE-FEE transaction at second real offset
    let base_fee_from_api: u64 = fee_history.base_fee_per_chunk_irys[1]
        .amount
        .try_into()
        .map(|v: u64| v.max(1))
        .expect("base_fee should fit in u64");

    let evm_min_cost_irys: u64 = min_cost_usd.try_into().expect("min_cost fits in u64");
    let required_priority_fee = evm_min_cost_irys.saturating_sub(base_fee_from_api);
    let priority_fee = required_priority_fee.saturating_add(evm_min_cost_irys);

    let adequate_tx_hash = node
        .inject_pd_tx_at_real_offsets(
            &adequate_fee_signer,
            data_start_offset + 1, // 1 chunk at second real offset
            1,
            priority_fee,
            base_fee_from_api,
            0, // nonce (fresh signer, starts at 0)
        )
        .await?;

    // Wait for PdService to provision the chunk
    node.wait_for_ready_pd_tx(&adequate_tx_hash, 30).await?;

    // Mine block - adequate-fee tx should be included
    let (_, eth_payload, _) = node.mine_block_without_gossip().await?;
    let adequate_included = eth_payload
        .block()
        .body()
        .transactions
        .iter()
        .any(|tx| *tx.tx_hash() == adequate_tx_hash);

    assert!(
        adequate_included,
        "Adequate-fee PD tx (fees >= min_pd_transaction_cost) should be ACCEPTED"
    );

    node.stop().await;
    Ok(())
}
