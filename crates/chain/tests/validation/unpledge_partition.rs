use std::sync::Arc;

use crate::utils::{read_block_from_state, solution_context, BlockValidationOutcome, IrysNodeTest};
use crate::validation::send_block_to_block_tree;
use eyre::WrapErr as _;
use irys_actors::{
    async_trait, block_producer::ledger_expiry::LedgerExpiryBalanceDelta,
    shadow_tx_generator::PublishLedgerWithTxs, BlockProdStrategy, BlockProducerInner,
    ProductionStrategy,
};
use irys_types::{CommitmentTransaction, NodeConfig};

#[test_log::test(actix_web::test)]
async fn heavy_block_unpledge_partition_not_owned_gets_rejected() -> eyre::Result<()> {
    struct EvilBlockProdStrategy {
        pub prod: ProductionStrategy,
        pub invalid_unpledge: CommitmentTransaction,
    }

    #[async_trait::async_trait]
    impl BlockProdStrategy for EvilBlockProdStrategy {
        fn inner(&self) -> &BlockProducerInner {
            &self.prod.inner
        }

        async fn get_mempool_txs(
            &self,
            _prev_block_header: &irys_types::IrysBlockHeader,
        ) -> eyre::Result<irys_actors::block_producer::MempoolTxsBundle> {
            let invalid_unpledge = self.invalid_unpledge.clone();
            Ok(irys_actors::block_producer::MempoolTxsBundle {
                commitment_txs: vec![invalid_unpledge.clone()],
                commitment_txs_to_bill: vec![invalid_unpledge],
                submit_txs: vec![],
                publish_txs: PublishLedgerWithTxs {
                    txs: vec![],
                    proofs: None,
                },
                aggregated_miner_fees: LedgerExpiryBalanceDelta::default(),
                commitment_refund_events: vec![],
            })
        }
    }

    let num_blocks_in_epoch = 4;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;

    let victim_signer = genesis_config.new_random_signer();
    let evil_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&victim_signer, &evil_signer]);

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    let victim_config = genesis_node.testing_peer_with_signer(&victim_signer);
    let victim_node = genesis_node
        .testing_peer_with_assignments_and_name(victim_config, "VICTIM")
        .await
        .wrap_err("failed to bootstrap victim peer with assignments")?;

    let evil_config = genesis_node.testing_peer_with_signer(&evil_signer);
    let evil_node = genesis_node
        .testing_peer_with_assignments_and_name(evil_config, "EVIL")
        .await
        .wrap_err("failed to bootstrap malicious peer with assignments")?;

    let victim_addr = victim_signer.address();
    let evil_addr = evil_signer.address();

    let victim_assignments = genesis_node.get_partition_assignments(victim_addr);
    eyre::ensure!(
        !victim_assignments.is_empty(),
        "victim peer must have at least one partition assignment"
    );

    let evil_assignments = genesis_node.get_partition_assignments(evil_addr);
    eyre::ensure!(
        !evil_assignments.is_empty(),
        "malicious peer must have at least one partition assignment"
    );

    let stolen_partition_hash = victim_assignments[0].partition_hash;

    let consensus_config = &genesis_node.node_ctx.config.consensus;
    let anchor = genesis_node.get_anchor().await?;

    let invalid_unpledge = CommitmentTransaction::new_unpledge(
        consensus_config,
        anchor,
        evil_node.node_ctx.mempool_pledge_provider.as_ref(),
        evil_addr,
        stolen_partition_hash,
    )
    .await;
    let invalid_unpledge = evil_signer.sign_commitment(invalid_unpledge)?;

    let block_prod_strategy = EvilBlockProdStrategy {
        invalid_unpledge: invalid_unpledge.clone(),
        prod: ProductionStrategy {
            inner: genesis_node.node_ctx.block_producer_inner.clone(),
        },
    };

    let (block, _stats, _payload) = block_prod_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .ok_or_else(|| eyre::eyre!("Block producer strategy returned no block"))?;

    send_block_to_block_tree(
        &genesis_node.node_ctx,
        Arc::clone(&block),
        vec![invalid_unpledge.clone()],
        false,
    )
    .await?;
    let genesis_outcome = read_block_from_state(&genesis_node.node_ctx, &block.block_hash).await;
    assert_eq!(
        genesis_outcome,
        BlockValidationOutcome::Discarded,
        "genesis node should discard block with unpledge referencing unowned partition"
    );

    send_block_to_block_tree(
        &victim_node.node_ctx,
        Arc::clone(&block),
        vec![invalid_unpledge.clone()],
        false,
    )
    .await?;
    let victim_outcome = read_block_from_state(&victim_node.node_ctx, &block.block_hash).await;
    assert_eq!(
        victim_outcome,
        BlockValidationOutcome::Discarded,
        "victim peer should also discard the malicious block"
    );

    send_block_to_block_tree(
        &evil_node.node_ctx,
        Arc::clone(&block),
        vec![invalid_unpledge],
        false,
    )
    .await?;
    let evil_outcome = read_block_from_state(&evil_node.node_ctx, &block.block_hash).await;
    assert_eq!(
        evil_outcome,
        BlockValidationOutcome::Discarded,
        "malicious peer must not accept its own invalid block"
    );

    evil_node.stop().await;
    victim_node.stop().await;
    genesis_node.stop().await;

    Ok(())
}
