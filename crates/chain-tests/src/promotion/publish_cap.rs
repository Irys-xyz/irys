use crate::utils::IrysNodeTest;
use alloy_core::primitives::U256;
use irys_types::{DataLedger, H256, NodeConfig};

/// The producer fills the Publish ledger up to `max_publish_txs_per_block` in
/// fee priority order, and the ready candidates above the cap stay eligible for
/// the next block.
#[test_log::test(tokio::test)]
async fn heavy_publish_selection_stops_at_cap() -> eyre::Result<()> {
    let seconds_to_wait = 30;
    // One ready candidate more than the cap. Each entry is (data seed, extra
    // term fee above the quoted price). The first entry has the lowest fee.
    let candidates = [(10_u8, 0_u64), (40, 2), (70, 1)];
    let publish_cap = candidates.len() - 1;

    let config = NodeConfig::testing()
        .with_consensus(|consensus| {
            consensus.chunk_size = 32;
            consensus.num_partitions_per_slot = 1;
            consensus.block_migration_depth = 1;
            consensus.hardforks.frontier.number_of_ingress_proofs_total = 1;
            consensus
                .hardforks
                .frontier
                .number_of_ingress_proofs_from_assignees = 0;
            consensus.mempool.max_publish_txs_per_block = publish_cap as u64;
        })
        .with_genesis_peer_discovery_timeout(1000);
    let signer = config.signer();

    let node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    node.mine_block().await?;

    let mut txs = Vec::new();
    for (seed, extra_term_fee) in candidates {
        let chunks = vec![[seed; 32], [seed + 1; 32], [seed + 2; 32]];
        let data: Vec<u8> = chunks.iter().flatten().copied().collect();
        let price = node
            .get_data_price(DataLedger::Publish, data.len() as u64)
            .await?;
        let tx = signer.create_publish_transaction(
            data,
            node.get_anchor().await?,
            price.perm_fee.into(),
            (price.term_fee + U256::from(extra_term_fee)).into(),
        )?;
        let tx = signer.sign_transaction(tx)?;
        node.post_data_tx_raw(&tx.header).await;
        node.wait_for_mempool(tx.header.id, seconds_to_wait).await?;
        txs.push((tx, chunks));
    }
    let tx_ids: Vec<H256> = txs.iter().map(|(tx, _)| tx.header.id).collect();
    let lowest_fee_tx_id = tx_ids[0];

    // Submit inclusion, then chunks, then one ingress proof for each tx.
    let submit_block = node.mine_block().await?;
    for id in &tx_ids {
        assert!(
            submit_block.data_ledgers[DataLedger::Submit]
                .tx_ids
                .contains(id),
            "tx {id} must be in the Submit ledger before promotion"
        );
    }
    for (tx, chunks) in &txs {
        for chunk_index in 0..chunks.len() {
            node.post_chunk_32b(tx, chunk_index, chunks).await;
        }
    }
    node.wait_for_ingress_proofs_no_mining(tx_ids.clone(), seconds_to_wait)
        .await?;

    // All candidates are ready. The first block takes only the cap.
    let first_block = node.mine_block().await?;
    let first_publish = &first_block.data_ledgers[DataLedger::Publish].tx_ids;
    assert_eq!(
        first_publish.len(),
        publish_cap,
        "Publish ledger must hold exactly the cap when more candidates are ready"
    );

    assert!(
        !first_publish.contains(&lowest_fee_tx_id),
        "the lowest-fee candidate must be the one left out"
    );

    // The candidate left out is promoted in the next block.
    let second_block = node.mine_block().await?;
    assert_eq!(
        second_block.data_ledgers[DataLedger::Publish].tx_ids.0,
        vec![lowest_fee_tx_id],
        "the candidate above the cap must be promoted in the next block"
    );

    node.stop().await;
    Ok(())
}
