use crate::{api::pd_fee_history_request, utils::IrysNodeTest};
use irys_api_server::routes::pd_pricing::PdFeeHistoryResponse;
use irys_chain::IrysNodeCtx;
use irys_types::{storage_pricing::PRECISION_SCALE, NodeConfig, U256};

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
async fn setup_pd_fee_history_test_chain(
) -> eyre::Result<(IrysNodeTest<IrysNodeCtx>, String, Vec<BlockMetadata>, u64)> {
    use alloy_consensus::Transaction as _;
    use irys_actors::pd_pricing::base_fee::PD_BASE_FEE_INDEX;
    use irys_reth::shadow_tx::{ShadowTransaction, TransactionPacket};

    // Configure node with predictable PD parameters
    let mut config = NodeConfig::testing();
    config.consensus.get_mut().chunk_size = 32;
    config
        .consensus
        .get_mut()
        .programmable_data
        .max_pd_chunks_per_block = 100; // 100 chunks for easy percentage calculations

    // Create and fund a test account for PD transactions
    let pd_signer = config.new_random_signer();
    config.fund_genesis_accounts(vec![&pd_signer]);

    // Start node
    let node = IrysNodeTest::new_genesis(config.clone()).start().await;
    let address = format!(
        "http://127.0.0.1:{}",
        node.node_ctx.config.node_config.http.bind_port
    );

    // Define block specifications
    // Block utilizations: 0%, 50%, 80%, 0%, 100%, 25%
    let block_specs = vec![
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
            priority_fees: vec![
                1_000_000_000,
                1_000_000_000,
                5_000_000_000,
                5_000_000_000,
            ],
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
    let mut nonce = 0u64;
    let mut offset_base = 0u32;

    for (block_idx, spec) in block_specs.iter().enumerate() {
        // Inject PD transactions if this block has any
        if !spec.priority_fees.is_empty() {
            let num_txs = spec.priority_fees.len();
            let chunks_per_tx = (spec.chunks / num_txs as u64) as u16;

            for priority_fee in &spec.priority_fees {
                node.create_and_inject_pd_transaction_with_priority_fee(
                    &pd_signer,
                    chunks_per_tx,
                    *priority_fee,
                    nonce,
                    offset_base,
                )
                .await?;
                nonce += 1;
                offset_base += chunks_per_tx as u32;
            }
        }

        // Mine the block
        let (_irys_block, eth_payload) = node.mine_block_without_gossip().await?;

        // Extract the PD base fee from the block
        let sealed_block = eth_payload.block();
        let second_tx = sealed_block
            .body()
            .transactions
            .get(PD_BASE_FEE_INDEX)
            .ok_or_else(|| eyre::eyre!("Block {} missing PdBaseFeeUpdate tx", block_idx))?;

        let shadow_tx = ShadowTransaction::decode(&mut second_tx.input().as_ref())
            .map_err(|e| eyre::eyre!("Failed to decode shadow tx in block {}: {}", block_idx, e))?;

        let base_fee = match shadow_tx
            .as_v1()
            .ok_or_else(|| eyre::eyre!("Expected V1 shadow tx"))?
        {
            TransactionPacket::PdBaseFeeUpdate(update) => {
                // Convert alloy U256 to irys U256 using the same method as pd_pricing module
                U256::from(update.per_chunk)
            }
            other => {
                return Err(eyre::eyre!(
                    "Block {} 2nd tx is not PdBaseFeeUpdate: {:?}",
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

    let max_pd_chunks = config.consensus.get_mut().programmable_data.max_pd_chunks_per_block;
    Ok((node, address, block_metadata, max_pd_chunks))
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
    let (node, address, block_metadata, max_pd_chunks) = setup_pd_fee_history_test_chain().await?;

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
    assert_eq!(
        fee_history.reward.len(),
        6,
        "reward should have N elements"
    );

    // Response is oldest-first. Map block heights to response indices using oldest_block.
    let oldest = fee_history.oldest_block;
    let max_chunks = U256::from(max_pd_chunks);
    for (i, metadata) in block_metadata.iter().enumerate() {
        let block_height = (i + 1) as u64;
        if block_height < oldest {
            continue;
        }
        let idx = (block_height - oldest) as usize;
        if idx >= fee_history.gas_used_ratio.len() {
            continue;
        }

        assert_eq!(
            fee_history.base_fee_per_chunk_irys[idx].amount,
            metadata.base_fee,
            "Block {} base fee mismatch",
            block_height
        );

        let expected_ratio = U256::from(metadata.chunks_used) * PRECISION_SCALE / max_chunks;
        assert_eq!(
            fee_history.gas_used_ratio[idx].amount,
            expected_ratio,
            "Block {} utilization mismatch: expected {}%",
            block_height,
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
    let (node, address, block_metadata, _max_pd_chunks) = setup_pd_fee_history_test_chain().await?;

    let response = pd_fee_history_request(&address, 6).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let fee_history: PdFeeHistoryResponse = response.json().await?;
    let oldest = fee_history.oldest_block;

    // Verify percentiles for each block using block_metadata
    for (i, metadata) in block_metadata.iter().enumerate() {
        let block_height = (i + 1) as u64;
        if block_height < oldest {
            continue;
        }
        let idx = (block_height - oldest) as usize;
        if idx >= fee_history.reward.len() {
            continue;
        }

        let reward = &fee_history.reward[idx];

        if metadata.priority_fees.is_empty() {
            // Empty blocks should have no percentiles
            assert!(
                reward.percentiles.is_empty(),
                "Block {} should have no percentiles",
                block_height
            );
            assert_eq!(reward.pd_tx_count, 0);
        } else {
            // Compute expected percentiles from metadata
            let mut sorted_fees = metadata.priority_fees.clone();
            sorted_fees.sort();

            assert_eq!(reward.pd_tx_count, sorted_fees.len());

            for percentile in [25u8, 50, 75] {
                let expected = compute_percentile(&sorted_fees, percentile);
                let fee_data = reward
                    .percentiles
                    .get(&percentile)
                    .unwrap_or_else(|| panic!("Block {} missing percentile {}", block_height, percentile));

                assert_eq!(
                    fee_data.fee_irys.amount,
                    U256::from(expected),
                    "Block {} percentile {} mismatch: expected {}, got {:?}",
                    block_height,
                    percentile,
                    expected,
                    fee_data.fee_irys.amount
                );
            }
        }
    }

    node.stop().await;
    Ok(())
}

/// Test that gas_used_ratio accurately reflects block utilization.
///
/// Verifies utilization calculation: (chunks_used * PRECISION_SCALE) / max_chunks
#[test_log::test(tokio::test)]
async fn heavy_pd_fee_history_utilization_accuracy() -> eyre::Result<()> {
    let (node, address, _block_metadata, _max_pd_chunks) = setup_pd_fee_history_test_chain().await?;

    let response = pd_fee_history_request(&address, 6).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let fee_history: PdFeeHistoryResponse = response.json().await?;

    // Verify we got the expected number of utilization values
    assert_eq!(
        fee_history.gas_used_ratio.len(),
        6,
        "gas_used_ratio should have 6 elements"
    );

    // Verify all utilization values are within valid range (0% to 100%)
    let max_utilization = PRECISION_SCALE;
    for (i, ratio) in fee_history.gas_used_ratio.iter().enumerate() {
        assert!(
            ratio.amount <= max_utilization,
            "Utilization at index {} should be <= 100%, got {:?}",
            i,
            ratio.amount
        );
    }

    // Collect unique utilization values
    let actual_ratios: std::collections::HashSet<U256> = fee_history
        .gas_used_ratio
        .iter()
        .map(|r| r.amount)
        .collect();

    // We created blocks with utilization [0%, 50%, 80%, 0%, 100%, 25%]
    // Verify we have variety in utilization - at least 0% and some non-zero values
    let zero_util = U256::from(0);
    let has_zero = actual_ratios.contains(&zero_util);
    let has_nonzero = actual_ratios.iter().any(|&r| r > zero_util);

    assert!(has_zero, "Should have at least one 0% utilization block");
    assert!(
        has_nonzero,
        "Should have at least one block with non-zero utilization"
    );

    // Verify we have multiple distinct utilization values (blocks with different usage)
    assert!(
        actual_ratios.len() >= 3,
        "Should have at least 3 distinct utilization levels, got {} ({:?})",
        actual_ratios.len(),
        actual_ratios
    );

    // Verify we have at least one high utilization block (>= 50%)
    let fifty_percent = PRECISION_SCALE / U256::from(2);
    let has_high_util = actual_ratios.iter().any(|&r| r >= fifty_percent);
    assert!(
        has_high_util,
        "Should have at least one block with >= 50% utilization"
    );

    node.stop().await;
    Ok(())
}
