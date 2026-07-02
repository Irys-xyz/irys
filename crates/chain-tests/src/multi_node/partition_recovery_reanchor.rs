use crate::utils::IrysNodeTest;
use irys_config::submodules::StorageSubmodulesConfig;
use irys_storage::ii;
use irys_types::{NodeConfig, UnixTimestamp, hardfork_config::Cascade};
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

/// Slow, `#[ignore]`d two-node end-to-end proof of the in-process VDF re-anchor.
///
/// Scenario: genesis and peer share a base, then partition (gossip off). Each mines an
/// independent fork that crosses a VDF reset boundary, folding a *different* reset seed
/// past it — genesis's buffer becomes poisoned relative to the canonical (peer) chain.
/// Peer's fork is taller, so its cumulative difficulty is higher. On reconnect the
/// peer's chain is gossiped to genesis; the Tier-1 validation heal lets the poisoned
/// buffer accept the canonical blocks (recover runs), the partition-recovery gate fires,
/// and the VDF thread re-anchors genesis's step buffer onto the canonical steps in place
/// — no node restart.
///
/// Assertion: after recovery, genesis's live VDF buffer over a canonical block's step
/// window equals that block's own (canonical) steps. Since the probe window is past the
/// divergent reset boundary, a poisoned-and-unhealed buffer would disagree there; equality
/// is the proof that the in-process re-anchor healed it. Genesis then keeps mining.
///
/// Marked `#[ignore]`: the boundary crossing must be paced past the issue #1447
/// confirmation gate, and the steps-per-block ratio is harness-dependent, so this is a
/// timing-fragile slow test intended for manual runs (`--ignored`), not a CI gate.
///
/// STATUS (manual runs): reliably drives node setup → shared base → partition, and the
/// genesis node then mines its minority fork cleanly across two reset boundaries, folding a
/// divergent reset seed — the poison the re-anchor must heal. The peer's majority fork does
/// not yet build. The harness VDF runs at tens of steps per second while a reset window is
/// only `reset_frequency` (40) steps, so a node that merely *follows* the shared base
/// free-runs its VDF a full reset window ahead of its confirmed chain and parks at the next
/// boundary (the issue #1447 gate). From there it cannot mine the very step that would advance
/// its confirmed chain and reopen the gate — a deadlock. The genesis node escapes only because
/// it mines continuously, keeping its confirmed chain within a block of its VDF.
///
/// Two fixes were tried and did NOT work, so avoid repeating them. Mining both forks
/// concurrently does not help: the setup gap between the shared base and the first mined block
/// still lets both VDFs drift past a boundary before either can mine. Raising
/// `vdf.sha_1s_difficulty` does not help either: at these tiny iteration counts the step rate
/// is overhead-bound, so the clock does not measurably slow. A genuine fix needs whatever
/// actually governs the test VDF's step rate slowed until idle drift falls far below
/// `reset_frequency`, or a harness structured with no idle windows at all. Because the
/// re-anchor mechanism is already covered deterministically by the `irys-vdf` unit tests
/// (`vdf::tests::reanchor_buffer::*`, `run_vdf_applies_reanchor_request_in_process`), this
/// end-to-end proof is belt-and-braces and is kept `#[ignore]`d.
#[ignore = "slow 2-node boundary-crossing e2e; paced crossing is timing-fragile — run manually with --ignored"]
#[test_log::test(tokio::test)]
async fn heavy_partition_recovery_reanchors_vdf_across_reset_boundary() -> eyre::Result<()> {
    let seconds_to_wait = 60;
    // migration_depth=1 so a fork of 2+ orphaned blocks triggers recovery.
    let block_migration_depth: u32 = 1;
    // reset_frequency must comfortably exceed a block's worth of VDF steps plus the
    // confirmation lag (block_migration_depth blocks), or the issue #1447 gate parks the
    // VDF before the first block can be mined and confirmed advances (a smaller window
    // wedges honest mining at genesis). At ~7-10 steps/block here, 40 crosses a boundary
    // every ~5 blocks, so a ~12-block fork spans the second boundary above the LCA — the
    // divergence point the partition-recovery gate detects.
    let reset_frequency: usize = 40;

    let mut genesis_config = NodeConfig::testing().with_consensus(|c| {
        // Small partitions/chunks so packing completes and PoA finds solutions in test time.
        c.chunk_size = 32;
        c.num_chunks_in_partition = 10;
        c.num_chunks_in_recall_range = 2;
        c.num_partitions_per_slot = 1;
        c.num_partitions_per_term_ledger_slot = 1;
        c.epoch.num_blocks_in_epoch = 2;
        c.block_migration_depth = block_migration_depth;
        c.vdf.reset_frequency = reset_frequency;
        c.entropy_packing_iterations = 1_000;
        // Keep initial difficulty above 1 so cumulative_diff grows meaningfully per block
        // (fork choice must distinguish the taller, heavier peer chain), but low enough that a
        // block spans few VDF steps — the confirmed chain then advances quickly per block, so
        // the issue #1447 boundary gate reopens after only a handful of paced blocks.
        c.genesis.initial_packed_partitions = Some(2.0);
        c.hardforks.frontier.number_of_ingress_proofs_total = 1;
        c.hardforks.cascade = Some(Cascade {
            activation_timestamp: UnixTimestamp::from_secs(0),
            one_year_epoch_length: 365,
            thirty_day_epoch_length: 30,
            annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
        });
    });
    genesis_config.storage.num_writes_before_sync = 1;

    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer]);

    let genesis_test = IrysNodeTest::new_genesis(genesis_config.clone());
    StorageSubmodulesConfig::load_for_test(genesis_test.cfg.base_directory.clone(), 10)?;
    let genesis = genesis_test
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    let peer_test = IrysNodeTest::new(genesis.testing_peer_with_signer(&peer_signer));
    StorageSubmodulesConfig::load_for_test(peer_test.cfg.base_directory.clone(), 10)?;
    let peer = peer_test
        .start_and_wait_for_packing("PEER", seconds_to_wait)
        .await;

    IrysNodeTest::announce_between(&genesis, &peer).await?;
    // Peer only follows during the shared base — stop it producing competing blocks.
    peer.stop_mining();

    // ─── Stage 1: shared base (gossip ON), peer follows ───
    let fork_height = 4_u64;
    for h in 1..=fork_height {
        genesis.mine_block().await?;
        let block = genesis.get_block_by_height(h).await?;
        peer.wait_for_block(&block.block_hash, seconds_to_wait)
            .await?;
    }
    info!(fork_height, "shared base complete");

    // ─── Stage 2: partition — each node mines an independent fork (gossip OFF) ───
    genesis.gossip_disable();
    peer.gossip_disable();
    peer.wait_for_packing(seconds_to_wait).await;
    info!("gossip disabled; mining divergent forks");

    // Genesis minority fork: mine one block per iteration (each with its own fresh timeout)
    // until it crosses at least two reset boundaries past the LCA, or a block cap. Genesis
    // mines continuously, so its confirmed chain stays within a block of its VDF and the issue
    // #1447 boundary gate reopens as it advances — it never wedges. (See the STATUS block above
    // for why the peer, which only follows the shared base, does.)
    let cap = 24_u64;
    loop {
        genesis.mine_block().await?;
        let h = genesis.get_canonical_chain_height().await;
        let resets = genesis
            .get_blocks_with_vdf_resets(fork_height + 1, h)
            .await?
            .len();
        info!(height = h, resets, "genesis minority fork progress");
        if resets >= 2 || (h - fork_height) >= cap {
            break;
        }
    }
    let genesis_height = genesis.get_canonical_chain_height().await;

    // Peer majority fork: taller than genesis (higher cumulative_diff → reorg), crossing the
    // divergent boundary with its own (different) reset seed.
    loop {
        peer.mine_block().await?;
        let h = peer.get_canonical_chain_height().await;
        let resets = peer
            .get_blocks_with_vdf_resets(fork_height + 1, h)
            .await?
            .len();
        info!(height = h, resets, "peer majority fork progress");
        if (resets >= 2 && h >= genesis_height + 3) || (h - fork_height) >= cap + 6 {
            break;
        }
    }
    let peer_height = peer.get_canonical_chain_height().await;
    assert!(
        peer_height > genesis_height,
        "peer fork must be taller (heavier) to trigger the reorg: peer={peer_height} genesis={genesis_height}"
    );

    // Precondition: genesis's minority fork actually crossed a reset boundary, so its buffer
    // folded a minority reset seed (the poison the re-anchor must heal).
    let genesis_resets = genesis
        .get_blocks_with_vdf_resets(fork_height + 1, genesis_height)
        .await?;
    assert!(
        !genesis_resets.is_empty(),
        "genesis minority fork must cross a VDF reset boundary to poison its buffer"
    );
    info!(
        genesis_height,
        peer_height,
        genesis_reset_blocks = genesis_resets.len(),
        "forks built; genesis buffer poisoned across a reset boundary"
    );

    // ─── Stage 3: reconnect — gossip the heavier peer chain to genesis, forcing a reorg ───
    genesis.gossip_enable();
    peer.gossip_enable();
    IrysNodeTest::announce_between(&genesis, &peer).await?;

    let peer_blocks = peer.get_blocks(fork_height + 1, peer_height).await?;
    for block in &peer_blocks {
        peer.gossip_block_to_peers(&Arc::new(block.clone()))?;
        genesis
            .wait_for_block(&block.block_hash, seconds_to_wait)
            .await?;
    }
    genesis
        .wait_until_height(peer_height, seconds_to_wait)
        .await?;
    info!(peer_height, "genesis adopted the canonical (peer) chain");

    // ─── Stage 4: assert the in-process re-anchor healed genesis's VDF buffer ───
    // Probe a canonical block past the divergent boundary. Its own `steps` are the canonical
    // VDF outputs for its window; genesis's live buffer must match them after the re-anchor.
    let probe = peer.get_block_by_height(peer_height - 1).await?;
    let first = probe.vdf_limiter_info.first_step_number();
    let last = probe.vdf_limiter_info.global_step_number;
    let expected = probe.vdf_limiter_info.steps.clone();

    let mut converged = false;
    for _ in 0..seconds_to_wait {
        if let Ok(steps) = genesis.node_ctx.vdf_steps_guard.get_steps(ii(first, last))
            && steps == expected
        {
            converged = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(
        converged,
        "genesis VDF buffer over [{first}, {last}] must converge to the canonical steps after the in-process re-anchor"
    );
    info!(
        first,
        last, "genesis VDF buffer converged to canonical steps in-process (no restart)"
    );

    // ─── Stage 5: genesis keeps mining on the healed buffer ───
    genesis.mine_block().await?;
    info!("genesis continued mining after in-process re-anchor");

    genesis.stop().await;
    peer.stop().await;
    Ok(())
}
