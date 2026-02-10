use std::collections::HashSet;

use crate::utils::IrysNodeTest;
use irys_types::ingress::generate_ingress_proof;
use irys_types::{
    DataTransactionHeader, DataTransactionHeaderV1, IrysTransactionCommon as _, NodeConfig, H256,
    U256,
};
use reth_db::{transaction::DbTxMut as _, Database as _};

/// Pre-populates the DB with duplicate ingress proofs (2 from signer A, 1 from signer B)
/// and asserts `get_publish_txs_and_proofs` deduplicates by signer address.
#[test_log::test(tokio::test)]
async fn heavy_mempool_dedup_ingress_proof_signers() -> eyre::Result<()> {
    let mut genesis_config = NodeConfig::testing();
    let chunk_size = 256_usize;
    genesis_config.consensus.get_mut().chunk_size = chunk_size as u64;
    genesis_config
        .consensus
        .get_mut()
        .hardforks
        .frontier
        .number_of_ingress_proofs_total = 2;

    let signer_a = genesis_config.new_random_signer();
    let signer_b = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&signer_a, &signer_b]);

    let genesis_node = IrysNodeTest::new_genesis(genesis_config)
        .start_and_wait_for_packing("GENESIS", 20)
        .await;

    // Mine a block so we have two valid anchors (otherwise identical inputs → identical proof → DUPSORT dedup)
    let blk1 = genesis_node.mine_block().await?;
    let anchor = genesis_node.get_block_by_height(0).await?.block_hash;
    let anchor2 = blk1.block_hash;
    assert_ne!(anchor, anchor2, "need two distinct anchors");

    // Build data root and chunks
    let data_bytes = vec![0_u8; chunk_size * 2];
    let chunks: Vec<Vec<u8>> = data_bytes.chunks(chunk_size).map(Vec::from).collect();
    let leaves =
        irys_types::generate_leaves(vec![data_bytes.clone()].into_iter().map(Ok), chunk_size)?;
    let data_root = H256(irys_types::generate_data_root(leaves)?.id);

    let data_tx = DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
        tx: DataTransactionHeaderV1 {
            id: H256::zero(),
            anchor,
            signer: signer_a.address(),
            data_root,
            data_size: data_bytes.len() as u64,
            header_size: 0,
            term_fee: U256::from(1000).into(),
            perm_fee: Some(U256::from(1000).into()),
            ledger_id: 0,
            bundle_format: Some(0),
            chain_id: 1,
            signature: Default::default(),
        },
        metadata: irys_types::DataTransactionMetadata::new(),
    })
    .sign(&signer_a)?;

    let chain_id = 1_u64;
    let mk_chunks = |cs: Vec<Vec<u8>>| cs.into_iter().map(Ok);

    // 2 proofs from signer A (same address, different anchors so signatures differ), 1 from signer B
    let proof_a1 = generate_ingress_proof(
        &signer_a,
        data_root,
        mk_chunks(chunks.clone()),
        chain_id,
        anchor,
    )?;
    let proof_a2 = generate_ingress_proof(
        &signer_a,
        data_root,
        mk_chunks(chunks.clone()),
        chain_id,
        anchor2,
    )?;
    let proof_b =
        generate_ingress_proof(&signer_b, data_root, mk_chunks(chunks), chain_id, anchor)?;

    genesis_node.node_ctx.db.update(|tx| {
        use irys_database::tables::{CompactTxHeader, IrysDataTxHeaders};
        tx.put::<IrysDataTxHeaders>(data_tx.id, CompactTxHeader(data_tx.clone()))?;
        irys_database::cache_data_root(tx, &data_tx, None)?;
        for (proof, address) in [
            (&proof_a1, signer_a.address()),
            (&proof_a2, signer_a.address()),
            (&proof_b, signer_b.address()),
        ] {
            irys_database::store_external_ingress_proof_checked(tx, proof, address)?;
        }
        // Verify the DB actually has 3 rows (including the duplicate signer)
        let stored = irys_database::ingress_proofs_by_data_root(tx, data_root)?;
        assert_eq!(
            stored.len(),
            3,
            "expected 3 ingress proofs in DB, got {}",
            stored.len()
        );

        Ok::<_, eyre::Report>(())
    })??;

    let (_submit, publish, _commit) = genesis_node
        .wait_for_mempool_best_txs_shape(0, 1, 0, 20)
        .await?;

    let proofs = publish.proofs.expect("publish txs should have proofs");
    let signers: HashSet<_> = proofs.iter().map(|p| p.recover_signer().unwrap()).collect();

    // 3 proofs inserted, but only 2 unique signers should remain after dedup
    assert_eq!(
        proofs.len(),
        2,
        "expected dedup to 2 proofs, got {}",
        proofs.len()
    );
    assert_eq!(
        signers.len(),
        2,
        "expected 2 unique signers, got {:?}",
        signers
    );

    genesis_node.stop().await;
    Ok(())
}
