use crate::utils::IrysNodeTest;
use alloy_core::primitives::U256;
use irys_types::{DataLedger, H256, NodeConfig};

/// The producer fills the Publish ledger up to `max_publish_txs_per_block` in
/// fee priority order, and the ready candidates above the cap stay eligible for
/// the next block.
#[test_log::test(tokio::test)]
async fn heavy_publish_selection_stops_at_cap() -> eyre::Result<()> {
    let seconds_to_wait = 30;
    // Three ready candidates with distinct term fees; the cap takes two.
    let publish_cap = 2;

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
            consensus.mempool.max_publish_txs_per_block = publish_cap;
        })
        .with_genesis_peer_discovery_timeout(1000);
    let signer = config.signer();

    let node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    node.mine_block().await?;

    let price = node.get_data_price(DataLedger::Publish, 3 * 32).await?;
    let anchor = node.get_anchor().await?;
    let sign_candidate = |seed: u8, extra_term_fee: u64| -> eyre::Result<_> {
        let chunks = vec![[seed; 32], [seed + 1; 32], [seed + 2; 32]];
        let data: Vec<u8> = chunks.iter().flatten().copied().collect();
        let tx = signer.create_publish_transaction(
            data,
            anchor,
            price.perm_fee.into(),
            (price.term_fee + U256::from(extra_term_fee)).into(),
        )?;
        Ok((signer.sign_transaction(tx)?, chunks))
    };
    let highest_fee = sign_candidate(10, 2)?;
    let middle_fee = sign_candidate(40, 1)?;
    // Selection by ascending tx id leaves out the greatest id. Give the lowest
    // fee to a tx whose id is not the greatest, so only fee order leaves it out.
    let greatest_other_id = highest_fee.0.header.id.max(middle_fee.0.header.id);
    let mut lowest_fee = None;
    for seed in (100_u8..=250).step_by(3) {
        let candidate = sign_candidate(seed, 0)?;
        if candidate.0.header.id < greatest_other_id {
            lowest_fee = Some(candidate);
            break;
        }
    }
    let lowest_fee = lowest_fee.expect("a seed gives an id below the greatest other id");

    let txs = [highest_fee, middle_fee, lowest_fee];
    for (tx, _) in &txs {
        node.post_data_tx_raw(&tx.header).await;
        node.wait_for_mempool(tx.header.id, seconds_to_wait).await?;
    }
    let tx_ids: Vec<H256> = txs.iter().map(|(tx, _)| tx.header.id).collect();

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

    // All candidates are ready. The first block takes only the cap, highest fee first.
    let first_block = node.mine_block().await?;
    assert_eq!(
        first_block.data_ledgers[DataLedger::Publish].tx_ids.0,
        vec![tx_ids[0], tx_ids[1]],
        "Publish ledger must hold the cap in fee priority order"
    );

    // The candidate left out is promoted in the next block.
    let second_block = node.mine_block().await?;
    assert_eq!(
        second_block.data_ledgers[DataLedger::Publish].tx_ids.0,
        vec![tx_ids[2]],
        "the candidate above the cap must be promoted in the next block"
    );

    node.stop().await;
    Ok(())
}
