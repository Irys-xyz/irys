use crate::utils::IrysNodeTest;
use irys_types::ingress::generate_ingress_proof;
use irys_types::{
    DataTransactionHeader, DataTransactionHeaderV1, IrysTransactionCommon as _, NodeConfig, H256,
    U256,
};
use reth_db::{transaction::DbTxMut as _, Database as _};

/// Pre-populates the DB with two data roots:
/// - data_root_1: 3 ingress proofs (2 from signer A with different anchors, 1 from signer B) → dedup to 2
/// - data_root_2: 2 ingress proofs (1 from signer A, 1 from signer B) → no dedup needed
///
/// Asserts dedup only removes true duplicates and doesn't affect the clean data root.
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

    let chain_id = 1_u64;
    let mk_chunks = |cs: Vec<Vec<u8>>| cs.into_iter().map(Ok);

    // --- Data root 1: has duplicate signer proofs ---
    let data_bytes_1 = vec![0_u8; chunk_size * 2];
    let chunks_1: Vec<Vec<u8>> = data_bytes_1.chunks(chunk_size).map(Vec::from).collect();
    let leaves_1 =
        irys_types::generate_leaves(vec![data_bytes_1.clone()].into_iter().map(Ok), chunk_size)?;
    let data_root_1 = H256(irys_types::generate_data_root(leaves_1)?.id);

    let data_tx_1 = DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
        tx: DataTransactionHeaderV1 {
            id: H256::zero(),
            anchor,
            signer: signer_a.address(),
            data_root: data_root_1,
            data_size: data_bytes_1.len() as u64,
            header_size: 0,
            term_fee: U256::from(1000).into(),
            perm_fee: Some(U256::from(1000).into()),
            ledger_id: 0,
            bundle_format: Some(0),
            chain_id,
            signature: Default::default(),
        },
        // promoted_height intentionally unset so the publish path falls through
        // to the tx_header_by_txid DB lookup instead of skipping as already-promoted.
        metadata: irys_types::DataTransactionMetadata::new(),
    })
    .sign(&signer_a)?;

    // 2 proofs from signer A (different anchors), 1 from signer B
    let proof_1a1 = generate_ingress_proof(
        &signer_a,
        data_root_1,
        mk_chunks(chunks_1.clone()),
        chain_id,
        anchor,
    )?;
    let proof_1a2 = generate_ingress_proof(
        &signer_a,
        data_root_1,
        mk_chunks(chunks_1.clone()),
        chain_id,
        anchor2,
    )?;
    let proof_1b = generate_ingress_proof(
        &signer_b,
        data_root_1,
        mk_chunks(chunks_1),
        chain_id,
        anchor,
    )?;

    // --- Data root 2: no duplicates (1 proof per signer) ---
    let data_bytes_2 = vec![1_u8; chunk_size * 2];
    let chunks_2: Vec<Vec<u8>> = data_bytes_2.chunks(chunk_size).map(Vec::from).collect();
    let leaves_2 =
        irys_types::generate_leaves(vec![data_bytes_2.clone()].into_iter().map(Ok), chunk_size)?;
    let data_root_2 = H256(irys_types::generate_data_root(leaves_2)?.id);
    assert_ne!(data_root_1, data_root_2);

    let data_tx_2 = DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
        tx: DataTransactionHeaderV1 {
            id: H256::zero(),
            anchor,
            signer: signer_a.address(),
            data_root: data_root_2,
            data_size: data_bytes_2.len() as u64,
            header_size: 0,
            term_fee: U256::from(1000).into(),
            perm_fee: Some(U256::from(1000).into()),
            ledger_id: 0,
            bundle_format: Some(0),
            chain_id,
            signature: Default::default(),
        },
        // promoted_height intentionally unset so the publish path falls through
        // to the tx_header_by_txid DB lookup instead of skipping as already-promoted.
        metadata: irys_types::DataTransactionMetadata::new(),
    })
    .sign(&signer_a)?;

    let proof_2a = generate_ingress_proof(
        &signer_a,
        data_root_2,
        mk_chunks(chunks_2.clone()),
        chain_id,
        anchor,
    )?;
    let proof_2b = generate_ingress_proof(
        &signer_b,
        data_root_2,
        mk_chunks(chunks_2),
        chain_id,
        anchor,
    )?;

    // Store both data txs and all proofs in the DB
    genesis_node.node_ctx.db.update(|tx| {
        use irys_database::tables::{
            CompactCachedIngressProof, CompactTxHeader, IngressProofs, IrysDataTxHeaders,
        };
        use irys_types::ingress::CachedIngressProof;

        // Data root 1: tx + 3 proofs (2 from signer A, 1 from signer B)
        // Insert directly into the DB table to bypass the dedup logic in
        // store_external_ingress_proof_checked (which removes prior proofs
        // from the same signer for the same data root).
        tx.put::<IrysDataTxHeaders>(data_tx_1.id, CompactTxHeader(data_tx_1.clone()))?;
        irys_database::cache_data_root(tx, &data_tx_1, None)?;
        for (proof, address) in [
            (&proof_1a1, signer_a.address()),
            (&proof_1a2, signer_a.address()),
            (&proof_1b, signer_b.address()),
        ] {
            tx.put::<IngressProofs>(
                proof.data_root,
                CompactCachedIngressProof(CachedIngressProof {
                    address,
                    proof: proof.clone(),
                }),
            )?;
        }
        let stored_1 = irys_database::ingress_proofs_by_data_root(tx, data_root_1)?;
        assert_eq!(
            stored_1.len(),
            3,
            "expected 3 ingress proofs for data_root_1"
        );

        // Data root 2: tx + 2 proofs (1 from each signer, no duplicates)
        tx.put::<IrysDataTxHeaders>(data_tx_2.id, CompactTxHeader(data_tx_2.clone()))?;
        irys_database::cache_data_root(tx, &data_tx_2, None)?;
        for (proof, address) in [
            (&proof_2a, signer_a.address()),
            (&proof_2b, signer_b.address()),
        ] {
            irys_database::store_external_ingress_proof_checked(tx, proof, address)?;
        }
        let stored_2 = irys_database::ingress_proofs_by_data_root(tx, data_root_2)?;
        assert_eq!(
            stored_2.len(),
            2,
            "expected 2 ingress proofs for data_root_2"
        );

        Ok::<_, eyre::Report>(())
    })??;

    // Both txs should appear as publish candidates
    let (_submit, publish, _commit) = genesis_node
        .wait_for_mempool_best_txs_shape(0, 2, 0, 20)
        .await?;

    let proofs = publish.proofs.expect("publish txs should have proofs");

    // Total proofs: 2 (deduped from 3) for data_root_1 + 2 (unchanged) for data_root_2 = 4
    assert_eq!(
        proofs.len(),
        4,
        "expected 4 total proofs, got {}",
        proofs.len()
    );

    let signers: Vec<_> = proofs.iter().map(|p| p.recover_signer().unwrap()).collect();
    // Each tx contributes 2 proofs with unique signers; same signer across txs is fine
    let signer_a_count = signers.iter().filter(|&&s| s == signer_a.address()).count();
    let signer_b_count = signers.iter().filter(|&&s| s == signer_b.address()).count();
    assert_eq!(
        signer_a_count, 2,
        "signer A should have 1 proof per data root"
    );
    assert_eq!(
        signer_b_count, 2,
        "signer B should have 1 proof per data root"
    );

    genesis_node.stop().await;
    Ok(())
}
