// NC-0042 integration coverage for `resolve_submit_inclusion`'s SLOW path.
//
// The slow path (by-hash parent-ancestry walk) is what resolves a tx's Submit
// inclusion when that inclusion is NOT yet in the migrated block index. It is
// covered by unit fixtures (`resolve_submit_inclusion_slow_path_walks_untracked_tree`,
// `find_block_range_resolves_unmigrated_tail`, the `walk_*` C1 tests), but the
// §4b/§4c chain-tests all run with `block_migration_depth = 1`, so a real node
// in those tests always migrates the inclusion before expiry and only ever takes
// the O(1) fast path. In production `block_migration_depth` is large, so the
// slow path IS reached. This test pins that integration: it raises
// `block_migration_depth` so the expired tx's Submit inclusion is still
// UN-migrated at the verdict block, then asserts (a) the slow path is genuinely
// taken (the migrated-index lookup returns `None`) and (b) the §4c validator
// still correctly rejects a block promoting that already-expired tx — proving the
// by-hash walk resolved the un-migrated inclusion to the correct offsets.

use crate::utils::{assert_validation_error, solution_context};
use crate::validation::{
    EvilPublishStrategy, is_nc_0042_expiry_rejection, send_block_and_read_state,
    submit_expiry_two_node_setup,
};
use irys_actors::{BlockProdStrategy as _, ProductionStrategy};
use irys_database::{canonical_submit_height, db::IrysDatabaseExt as _};
use irys_types::ingress::generate_ingress_proof;
use irys_types::{DataLedger, IngressProofsList};

#[test_log::test(tokio::test)]
async fn heavy_submit_expiry_resolves_unmigrated_inclusion_via_slow_path() -> eyre::Result<()> {
    // --- 1 + 2. Shared setup, but with a LARGE block_migration_depth. The data
    //         block sits at height 1 and the slot expires at blocks_per_cycle (10),
    //         so a depth > (expiry_height - 1) keeps the inclusion un-migrated
    //         through expiry. 12 is comfortably above that 9 threshold and below
    //         the testing block_tree_depth (>= 50), so the inclusion still lives in
    //         the block tree for the by-hash walk to find. ---
    const BLOCK_MIGRATION_DEPTH: u32 = 12;
    let setup = submit_expiry_two_node_setup(BLOCK_MIGRATION_DEPTH).await?;
    let genesis_config = setup.genesis_config;
    let genesis_node = setup.genesis_node;
    let peer_node = setup.peer_node;
    let txs = setup.txs;
    let blocks_per_cycle = setup.blocks_per_cycle;
    let seconds_to_wait = setup.seconds_to_wait;

    // --- 3. Mine honestly to the expiry epoch. The honest producer drops the
    //         slot-0 txs from promotion (§4b) and refunds them (Pipeline B); both
    //         now resolve the un-migrated inclusion via the slow path. ---
    let expiry_height = blocks_per_cycle; // = 10
    let height_before_loop = genesis_node.get_canonical_chain_height().await;
    while genesis_node.get_canonical_chain_height().await < expiry_height {
        genesis_node.mine_block().await?;
    }
    assert!(
        genesis_node.get_canonical_chain_height().await > height_before_loop,
        "chain must advance past the expiry epoch (slow-path §4b filter must not stall the producer)"
    );

    // --- 4. Pick a target tx expired at the cross-block evil height. ---
    let evil_height = expiry_height + 1; // E+1
    let evil_parent_block = genesis_node.get_block_by_height(expiry_height).await?;
    let tip_hash = evil_parent_block.block_hash;
    let tip_snapshot = genesis_node
        .node_ctx
        .block_tree_guard
        .read()
        .get_epoch_snapshot(&tip_hash)
        .expect("epoch snapshot for the current tip");
    let evil_cascade_active = genesis_node
        .node_ctx
        .config
        .consensus
        .hardforks
        .is_cascade_active_at(evil_parent_block.timestamp_secs());
    let expired_set = irys_actors::block_producer::ledger_expiry::expired_submit_tx_ids(
        &tip_snapshot,
        &evil_parent_block,
        evil_height,
        &genesis_node.node_ctx.config,
        evil_cascade_active,
        genesis_node
            .node_ctx
            .block_producer_inner
            .block_index
            .clone(),
        &genesis_node.node_ctx.block_tree_guard,
        &genesis_node.node_ctx.mempool_guard,
        &genesis_node.node_ctx.db,
    )
    .await?;
    let target = txs
        .iter()
        .find(|tx| expired_set.contains(&tx.header.id))
        .expect("at least one posted tx must be expired at the cross-block evil height");
    let expired_tx = target.header.clone();

    // --- 5. THE SLOW-PATH PRECONDITION. `resolve_submit_inclusion` takes the slow
    //         path exactly when the migrated-index lookup returns `None`. Assert
    //         that for the target tx at the verdict's `max_height` (evil_height - 1)
    //         so this test cannot silently pass via the fast path. (If this ever
    //         fails, BLOCK_MIGRATION_DEPTH is too low for the expiry timing.) ---
    let max_height = evil_height - 1;
    let migrated_lookup = genesis_node
        .node_ctx
        .db
        .view_eyre(|tx| canonical_submit_height(tx, &expired_tx.id, max_height))?;
    assert_eq!(
        migrated_lookup, None,
        "the expired tx's Submit inclusion must be UN-migrated at max_height {max_height} so \
         resolve_submit_inclusion takes the by-hash slow path (block_migration_depth = {BLOCK_MIGRATION_DEPTH})"
    );

    // --- 6. Evil block at E+1 promotes the expired tx; build a valid ingress proof
    //         so the block fails ONLY on the §4c expiry rule. ---
    let genesis_signer = genesis_config.signer();
    let proof_anchor = genesis_node.get_anchor().await?;
    // Fail fast on a missing payload rather than silently building the ingress
    // proof over an empty chunk — that would change what is proven and could
    // mask a fixture regression (the posted tx's data must still be retained).
    let target_data = target.data.clone().expect(
        "posted tx payload must be retained to build the ingress proof (fixture invariant)",
    );
    let chunks: Vec<Vec<u8>> = vec![target_data.into()];
    let proof = generate_ingress_proof(
        &genesis_signer,
        expired_tx.data_root,
        chunks.iter().map(|c| Ok(c.as_slice())),
        genesis_config.consensus_config().chain_id,
        proof_anchor,
    )?;
    let evil_strategy = EvilPublishStrategy {
        publish_tx: expired_tx.clone(),
        proofs: IngressProofsList(vec![proof]),
        prod: ProductionStrategy {
            inner: genesis_node.node_ctx.block_producer_inner.clone(),
        },
        block_tree_guard: genesis_node.node_ctx.block_tree_guard.clone(),
    };
    let (block, _adj, _payload) = evil_strategy
        .fully_produce_new_block_without_gossip(&solution_context(&genesis_node.node_ctx).await?)
        .await?
        .expect("evil block should be produced (the guard fires at validation, not production)");
    assert_eq!(block.header().height, evil_height, "evil block at E+1");
    assert!(
        block.header().data_ledgers[DataLedger::Publish]
            .tx_ids
            .contains(&expired_tx.id),
        "evil block must promote the expired tx"
    );

    // --- 7. Both nodes must reject it (§4c) — only possible if the slow path
    //         resolved the un-migrated inclusion to the correct offsets. The
    //         shared `is_nc_0042_expiry_rejection` keys on the NC-0042 marker so
    //         we reject for the expiry rule, not an unrelated shadow-tx error. ---
    let genesis_outcome =
        send_block_and_read_state(&genesis_node.node_ctx, block.clone(), false).await?;
    assert_validation_error(
        genesis_outcome,
        is_nc_0042_expiry_rejection,
        "Genesis must reject a block promoting an already-expired Submit tx resolved via the slow path (NC-0042 §4c)",
    );

    peer_node
        .wait_until_height(expiry_height, seconds_to_wait)
        .await?;
    let peer_outcome = send_block_and_read_state(&peer_node.node_ctx, block.clone(), false).await?;
    assert_validation_error(
        peer_outcome,
        is_nc_0042_expiry_rejection,
        "Peer must reject a block promoting an already-expired Submit tx resolved via the slow path (NC-0042 §4c)",
    );

    genesis_node
        .assert_evm_block_absent(block.header().evm_block_hash, 2)
        .await?;
    peer_node
        .assert_evm_block_absent(block.header().evm_block_hash, 2)
        .await?;

    peer_node.stop().await;
    genesis_node.stop().await;
    Ok(())
}
