//! Integration test verifying that a PD transaction flows end-to-end on a single node
//! using mock chunks (zeroed data):
//!
//!   mempool → mempool monitor → PdChunkManager provisioning
//!           → payload builder readiness gate → block inclusion → EVM execution
//!
//! This is the Step 4 test from docs/pd-mock-wiring-plan.md.

use crate::utils::IrysNodeTest;
use irys_types::NodeConfig;

/// Test that a PD transaction flows end-to-end on a single genesis node with mock chunks.
///
/// Flow:
/// 1. Start a single genesis node (Sprite hardfork active from genesis in testing config)
/// 2. Wait for packing to be ready
/// 3. Create a funded signer and construct a minimal PD transaction (1 chunk, partition 0)
/// 4. Inject the transaction into the node's mempool
/// 5. Wait for the mempool monitor to detect and provision the PD tx (monitor polls at 100 ms)
/// 6. Mine a block
/// 7. Verify the block contains at least the PD transaction
#[test_log::test(tokio::test)]
async fn heavy_test_pd_mock_e2e_single_node() -> eyre::Result<()> {
    let seconds_to_wait = 120;

    // 1. Configure a genesis node with a funded signer for PD transactions.
    //    NodeConfig::testing() enables Sprite from genesis (block 0), which is required
    //    for PD transactions to be accepted by the mempool.
    let mut config = NodeConfig::testing();
    let pd_tx_signer = config.new_random_signer();
    config.fund_genesis_accounts(vec![&pd_tx_signer]);

    // 2. Start the genesis node and wait for the packing service to be idle.
    let ctx = IrysNodeTest::new_genesis(config)
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // 3. Inject a minimal PD transaction:
    //    - 1 chunk, partition_index = 0 (U200::ZERO), offset = 0
    //    - Fees set well above the minimum to ensure acceptance
    //    - nonce = 0 (first transaction from this signer)
    //    - offset_base = 0 (start of the partition)
    //
    // create_and_inject_pd_transaction_with_priority_fee builds a TxEip1559 with a PD
    // header prepended to the calldata and a PdDataRead access list entry,
    // signs it with the provided signer, and injects it via rpc.inject_tx().
    let tx_hash = ctx
        .create_and_inject_pd_transaction_with_priority_fee(
            &pd_tx_signer,
            1,                          // chunks_per_tx: single chunk — minimal access list
            10_000_000_000_000_000_u64, // priority_fee_per_chunk: 0.01 IRYS — meets min_pd_transaction_cost
            0,                          // nonce: first tx from this signer
            0,                          // offset_base: start at chunk offset 0
        )
        .await?;

    tracing::info!("PD transaction injected: {:?}", tx_hash);

    // 4. Give the mempool monitor time to detect the PD tx and trigger PdChunkManager
    //    provisioning.  The monitor polls every 100 ms; 500 ms is comfortably beyond that.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // 5. Mine a block.  mine_block_without_gossip() produces a block via the block producer
    //    service (bypassing gossip) and returns the Irys block header, the Reth EVM payload,
    //    and the block transaction list.
    let (_, eth_payload, _) = ctx.mine_block_without_gossip().await?;

    // 6. Verify the PD transaction was included in the EVM block.
    let pd_tx_included = eth_payload
        .block()
        .body()
        .transactions
        .iter()
        .any(|tx| tx.hash() == &tx_hash);

    assert!(
        pd_tx_included,
        "PD transaction {:?} should be included in the mined block",
        tx_hash
    );

    tracing::info!(
        "PD transaction successfully included in block: {:?}",
        tx_hash
    );

    ctx.stop().await;
    Ok(())
}
