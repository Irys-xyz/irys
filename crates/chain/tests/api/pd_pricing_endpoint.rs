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
async fn setup_pd_fee_history_test_chain(
) -> eyre::Result<(IrysNodeTest<IrysNodeCtx>, String, Vec<BlockMetadata>, u64)> {
    use alloy_consensus::Transaction as _;
    use irys_actors::pd_pricing::base_fee::PD_BASE_FEE_INDEX;
    use irys_reth::shadow_tx::{ShadowTransaction, TransactionPacket};

    // Configure node with predictable PD parameters
    let mut config = NodeConfig::testing();
    config.consensus.get_mut().chunk_size = 32;
    let sprite = config
        .consensus
        .get_mut()
        .hardforks
        .sprite
        .as_mut()
        .expect("Sprite hardfork must be configured for testing");
    sprite.max_pd_chunks_per_block = 100; // 100 chunks for easy percentage calculations
                                          // Set min_pd_transaction_cost to 0 to avoid rejections in this test
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

    // Define block specifications
    // Block utilizations: 0%, 50%, 80%, 0%, 100%, 25%
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
    let mut offset_base = 0_u32;

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
        let (irys_block, eth_payload, _block_txs) = node.mine_block_without_gossip().await?;

        // Wait for the block to be validated and tip updated
        node.wait_until_height(irys_block.height, 10).await?;

        // Extract the PD base fee from the block
        // On the first Sprite block, TreasuryDeposit is at index 1 and PdBaseFeeUpdate is at index 2.
        // For subsequent blocks, PdBaseFeeUpdate is at index 1.
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
            TransactionPacket::PdBaseFeeUpdate(update) => {
                // Normal case: PdBaseFeeUpdate is at index 1
                U256::from(update.per_chunk)
            }
            TransactionPacket::TreasuryDeposit(_) => {
                // First Sprite block: TreasuryDeposit at index 1, check index 2 for PdBaseFeeUpdate
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
    assert_eq!(fee_history.reward.len(), 6, "reward should have N elements");

    // Verify oldest_block is 1 (excludes genesis)
    assert_eq!(
        fee_history.oldest_block, 1,
        "oldest_block should be 1 (excludes genesis)"
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
    let (node, address, block_metadata, _max_pd_chunks) = setup_pd_fee_history_test_chain().await?;

    let response = pd_fee_history_request(&address, 6).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let fee_history: PdFeeHistoryResponse = response.json().await?;

    // Verify oldest_block is 1 (excludes genesis)
    assert_eq!(
        fee_history.oldest_block, 1,
        "oldest_block should be 1 (excludes genesis)"
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

    // Store min_cost before we borrow config for node creation
    let min_cost_usd = min_cost.amount;

    // Start node and mine initial block to establish pricing
    let node = IrysNodeTest::new_genesis(config).start().await;
    let address = format!(
        "http://127.0.0.1:{}",
        node.node_ctx.config.node_config.http.bind_port
    );
    node.mine_block().await?;

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

    // 2. Submit LOW-FEE transaction (below minimum)
    // Use fees well below min_cost_irys
    let low_priority_fee = 100_u64; // 100 wei per chunk
    let low_base_fee = 100_u64; // 100 wei per chunk
                                // Total for 1 chunk = 200 wei, far below min_cost_irys

    let low_fee_tx_hash = node
        .create_and_inject_pd_transaction_with_custom_fees(
            &low_fee_signer,
            1,                // 1 chunk
            low_priority_fee, // priority fee per chunk
            low_base_fee,     // base fee per chunk
            0,                // nonce
            0,                // offset_base
        )
        .await?;

    // Mine block - low-fee tx should NOT be included
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

    // 3. Submit ADEQUATE-FEE transaction (at or above minimum)
    // Use the base fee from the API response (predicted next block fee at index [1])
    // Only adjust priority fee to meet the minimum threshold
    let base_fee_from_api: u64 = fee_history.base_fee_per_chunk_irys[1]
        .amount
        .try_into()
        .expect("base_fee should fit in u64");

    // The EVM calculates min_cost_irys using on-chain price which may differ from API's EMA.
    // At $1/IRYS price: min_cost_irys = min_cost.amount (they're equal in internal representation)
    // For 1 chunk: total_fee = base_fee + priority_fee >= min_cost_irys
    // So: priority_fee >= min_cost_irys - base_fee
    let evm_min_cost_irys: u64 = min_cost_usd.try_into().expect("min_cost fits in u64");
    let required_priority_fee = evm_min_cost_irys.saturating_sub(base_fee_from_api);
    // Add buffer to ensure we exceed the threshold (same magnitude as min_cost)
    let priority_fee = required_priority_fee.saturating_add(evm_min_cost_irys);

    let adequate_tx_hash = node
        .create_and_inject_pd_transaction_with_custom_fees(
            &adequate_fee_signer,
            1,                 // 1 chunk
            priority_fee,      // priority fee per chunk (adjusted to meet minimum)
            base_fee_from_api, // base fee from API
            0,                 // nonce (fresh signer, starts at 0)
            100,               // offset_base (different from first tx)
        )
        .await?;

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
