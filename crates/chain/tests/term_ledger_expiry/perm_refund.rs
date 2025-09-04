use crate::utils::IrysNodeTest;
use actix_web::test::{call_service, TestRequest};
use alloy_genesis::GenesisAccount;
use awc::http::StatusCode;
use irys_reth_node_bridge::irys_reth::shadow_tx::{ShadowTransaction, TransactionPacket};
use irys_types::{
    chunk::{TxChunkOffset, UnpackedChunk},
    fee_distribution::{PublishFeeCharges, TermFeeCharges},
    irys::IrysSigner,
    Base64,
    DataLedger, 
    IrysTransactionCommon, 
    NodeConfig, 
    U256
};
use reth::rpc::types::TransactionTrait as _;
use tracing::info;

#[test_log::test(actix_web::test)]
async fn heavy_perm_fee_refund_for_unpromoted_tx() -> eyre::Result<()> {
    info!("Testing permanent fee refunds for non-promoted transactions");
    
    // Configure node with appropriate epoch settings
    let mut config = NodeConfig::testing();
    config.consensus.get_mut().block_migration_depth = 1;
    config.consensus.get_mut().chunk_size = 256;
    config.consensus.get_mut().num_chunks_in_partition = 4; // 1024 bytes per partition
    config.consensus.get_mut().epoch.submit_ledger_epoch_length = 2; // Expires after 2 epochs
    config.consensus.get_mut().epoch.num_blocks_in_epoch = 3; // 3 blocks per epoch
    
    // Create two user signers
    let user1_signer = IrysSigner::random_signer(&config.consensus_config());
    let user2_signer = IrysSigner::random_signer(&config.consensus_config());
    let user1_address = user1_signer.address();
    let user2_address = user2_signer.address();
    
    // Fund both users
    let initial_balance = U256::from(10_000_000_000_000_000_000_u128); // 10 IRYS
    config.consensus.extend_genesis_accounts(vec![
        (
            user1_address,
            GenesisAccount {
                balance: initial_balance.into(),
                ..Default::default()
            },
        ),
        (
            user2_address,
            GenesisAccount {
                balance: initial_balance.into(),
                ..Default::default()
            },
        ),
    ]);
    
    // Start node
    let node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("perm_refund_test", 30)
        .await;
    
    // Get genesis block for anchor
    let genesis_block = node.get_block_by_height(0).await?;
    let anchor = genesis_block.block_hash;
    
    info!("Posting two transactions with perm_fee");
    
    // Create and post first transaction
    info!("Creating transaction 1 from user1");
    let data1 = vec![1_u8; 512];
    let tx1 = node.post_data_tx(anchor, data1, &user1_signer).await;
    let tx1_perm_fee = tx1.header.perm_fee.expect("Transaction should have perm_fee");
    let tx1_term_fee = tx1.header.term_fee;
    info!("Posted tx1 with perm_fee: {}, term_fee: {}", tx1_perm_fee, tx1_term_fee);
    assert!(tx1_perm_fee > U256::from(0), "Transaction 1 should have non-zero perm_fee");
    
    // Analyze fee distribution using fee structs
    let consensus_config = config.consensus_config();
    let term_charges = TermFeeCharges::new(tx1_term_fee, &consensus_config)?;
    info!("Tx1 TermFeeCharges breakdown:");
    info!("  - Block producer reward: {}", term_charges.block_producer_reward);
    info!("  - Term fee treasury: {}", term_charges.term_fee_treasury);
    
    let publish_charges = PublishFeeCharges::new(tx1_perm_fee, tx1_term_fee, &consensus_config)?;
    info!("Tx1 PublishFeeCharges breakdown:");
    info!("  - Ingress proof reward: {}", publish_charges.ingress_proof_reward);
    info!("  - Perm fee treasury: {}", publish_charges.perm_fee_treasury);
    
    // Calculate total immediate charge to user
    let total_immediate_charge = tx1_term_fee.saturating_add(term_charges.block_producer_reward);
    info!("Total immediate charge to user (including block producer reward): {}", total_immediate_charge);
    
    // Wait for mempool
    node.wait_for_mempool(tx1.header.id, 10).await?;
    
    // Create and post second transaction
    info!("Creating transaction 2 from user2");
    let data2 = vec![2_u8; 512];
    let tx2 = node.post_data_tx(anchor, data2, &user2_signer).await;
    let tx2_perm_fee = tx2.header.perm_fee.expect("Transaction should have perm_fee");
    let tx2_term_fee = tx2.header.term_fee;
    info!("Posted tx2 with perm_fee: {}, term_fee: {}", tx2_perm_fee, tx2_term_fee);
    assert!(tx2_perm_fee > U256::from(0), "Transaction 2 should have non-zero perm_fee");
    
    // Wait for mempool
    node.wait_for_mempool(tx2.header.id, 10).await?;
    
    // Start public API for chunk uploads
    info!("Starting public API for chunk uploads");
    let app = node.start_public_api().await;
    
    // Upload chunks for user2's transaction to trigger promotion
    info!("Uploading chunks for user2's transaction to trigger promotion");
    
    // User2's transaction has 512 bytes with chunk_size=256, so 2 chunks
    let tx2_data = vec![2_u8; 512];
    let chunk_size = 256;
    let tx2_chunks: Vec<Vec<u8>> = tx2_data.chunks(chunk_size)
        .map(|chunk| chunk.to_vec())
        .collect();
    
    // Post both chunks for tx2
    for (chunk_index, chunk_data) in tx2_chunks.iter().enumerate() {
        info!("Posting chunk {} for tx2", chunk_index);
        
        let unpacked_chunk = UnpackedChunk {
            data_root: tx2.header.data_root,
            data_size: tx2.header.data_size,
            data_path: Base64(tx2.proofs[chunk_index].proof.clone()),
            bytes: Base64(chunk_data.clone()),
            tx_offset: TxChunkOffset::from(chunk_index as u32),
        };
        
        let req = TestRequest::post()
            .uri("/v1/chunk")
            .set_json(&unpacked_chunk)
            .to_request();
        
        let resp = call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK, "Failed to post chunk {}", chunk_index);
    }
    
    info!("All chunks uploaded for tx2, waiting for promotion");
    
    // Wait for tx2 to be promoted
    let promotion_result = node.wait_for_ingress_proofs(vec![tx2.header.id], 20).await;
    assert!(promotion_result.is_ok(), "Tx2 should be promoted after uploading chunks");
    info!("✓ Tx2 has been promoted");
    
    // Mine some blocks to confirm the transactions
    info!("Mining initial blocks to confirm transactions");
    for i in 1..=3 {
        node.mine_block().await?;
        info!("Mined block {}", i);
    }
    
    // Verify initial balances after transactions are posted
    let block = node.get_block_by_height(1).await?;
    let user1_balance_initial = U256::from_be_bytes(
        node.get_balance(user1_address, block.evm_block_hash).to_be_bytes()
    );
    let user2_balance_initial = U256::from_be_bytes(
        node.get_balance(user2_address, block.evm_block_hash).to_be_bytes()
    );
    
    info!("Initial balance check:");
    info!("  User1 initial: {}", initial_balance);
    info!("  User1 current: {}", user1_balance_initial);
    info!("  User1 charged: {}", initial_balance.saturating_sub(user1_balance_initial));
    info!("  User1 expected charge (total_cost): {}", tx1.header.total_cost());
    info!("  Difference: {}", initial_balance.saturating_sub(user1_balance_initial).saturating_sub(tx1.header.total_cost()));
    info!("  User2 initial: {}", initial_balance);
    info!("  User2 current: {}", user2_balance_initial);
    info!("  User2 charged: {}", initial_balance.saturating_sub(user2_balance_initial));
    info!("  User2 expected charge (total_cost): {}", tx2.header.total_cost());
    info!("  Difference: {}", initial_balance.saturating_sub(user2_balance_initial).saturating_sub(tx2.header.total_cost()));
    
    // Calculate target height for expiry
    // Expiry happens after submit_ledger_epoch_length epochs
    let submit_ledger_epoch_length = config.consensus_config().epoch.submit_ledger_epoch_length;
    let num_blocks_in_epoch = config.consensus_config().epoch.num_blocks_in_epoch;
    let target_expiry_height = (submit_ledger_epoch_length + 1) * num_blocks_in_epoch;
    
    info!("Mining to expiry at block {}", target_expiry_height);
    
    // Mine blocks to reach expiry
    for height in 4..=target_expiry_height {
        let block = node.mine_block().await?;
        info!("Mined block {}", height);
        
        // Check for expired transactions
        let tx_ids = block.get_data_ledger_tx_ids();
        if let Some(publish_txs) = tx_ids.get(&DataLedger::Publish) {
            if !publish_txs.is_empty() {
                info!("Block {} has {} transactions in Publish ledger (expired)", 
                    height, publish_txs.len());
            }
        }
    }
    
    info!("Reached expiry block at height {}", target_expiry_height);
    info!("Checking shadow transactions for PermFeeRefund");
    
    // Track refunds found
    let mut user1_refund_amount = U256::from(0);
    let mut user2_refund_amount = U256::from(0);
    
    // Check all blocks for PermFeeRefund transactions
    for height in 0..=target_expiry_height {
        let block = node.get_block_by_height(height).await?;
        
        if let Ok(evm_block) = node.get_evm_block_by_hash(block.evm_block_hash) {
            for tx in evm_block.body.transactions {
                if tx.input().len() >= 4 {
                    if let Ok(shadow_tx) = ShadowTransaction::decode(&mut tx.input().as_ref()) {
                        if let Some(packet) = shadow_tx.as_v1() {
                            match packet {
                                TransactionPacket::PermFeeRefund(refund) => {
                                    info!("Found PermFeeRefund in block {}: target={}, amount={}, ref={}", 
                                        height, refund.target, refund.amount, refund.irys_ref);
                                    
                                    if refund.target == user1_address {
                                        user1_refund_amount = user1_refund_amount.saturating_add(
                                            U256::from_le_bytes(refund.amount.to_le_bytes())
                                        );
                                        info!("Found refund for user1: {}", refund.amount);
                                    } else if refund.target == user2_address {
                                        user2_refund_amount = user2_refund_amount.saturating_add(
                                            U256::from_le_bytes(refund.amount.to_le_bytes())
                                        );
                                        info!("Found refund for user2: {}", refund.amount);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
    }
    
    // Verify refunds match expectations
    // User1 (unpromoted) should receive perm_fee refund
    assert_eq!(
        user1_refund_amount, tx1_perm_fee,
        "User1 (unpromoted) should receive exactly their perm_fee back. Got {}, expected {}",
        user1_refund_amount, tx1_perm_fee
    );
    
    // User2 (promoted) should NOT receive perm_fee refund
    assert_eq!(
        user2_refund_amount, U256::from(0),
        "User2 (promoted) should NOT receive perm_fee refund. Got {}, expected 0",
        user2_refund_amount
    );
    
    info!("✓ User1 (unpromoted): Received refund of {}", user1_refund_amount);
    info!("✓ User2 (promoted): No refund (as expected)");
    
    // Verify final balances
    let final_block = node.get_block_by_height(target_expiry_height).await?;
    let user1_final = U256::from_be_bytes(
        node.get_balance(user1_address, final_block.evm_block_hash).to_be_bytes()
    );
    let user2_final = U256::from_be_bytes(
        node.get_balance(user2_address, final_block.evm_block_hash).to_be_bytes()
    );
    
    // Calculate expected final balances
    // User1 (unpromoted): pays total_cost and gets perm_fee refunded
    let user1_expected = initial_balance
        .saturating_sub(tx1.header.total_cost())
        .saturating_add(tx1_perm_fee); // Refund added back
    
    // User2 (promoted): pays total_cost and does NOT get perm_fee refunded
    let user2_expected = initial_balance
        .saturating_sub(tx2.header.total_cost()); // No refund for promoted tx
    
    info!("Final balance verification:");
    info!("  User1: {} (expected: {})", user1_final, user1_expected);
    info!("  User2: {} (expected: {})", user2_final, user2_expected);
    
    assert_eq!(
        user1_final, user1_expected,
        "User1 (unpromoted) final balance should include perm_fee refund"
    );
    assert_eq!(
        user2_final, user2_expected,
        "User2 (promoted) final balance should NOT include perm_fee refund"
    );
    
    info!("✅ Permanent fee refund test passed successfully!");
    info!("  - User1 (unpromoted): Received full perm_fee refund");
    info!("  - User2 (promoted): Did NOT receive perm_fee refund (as expected)");
    
    Ok(())
}