//! Integration test (currently `#[ignore]`d — see "Why ignored" below).
//!
//! Goal: verify end-to-end that a peer whose local VDF state cannot reach a
//! block's required `global_step_number` surfaces `VdfValidationFailed`
//! within `progress_timeout_secs` rather than hanging the validation
//! pipeline.
//!
//! Setup:
//! - Genesis (Peer A) and Peer B reach a common baseline height via gossip.
//! - Peer B stops mining (its `run_vdf` thread halts; `is_mining_enabled = false`).
//! - Peer A privately mines a divergent chain (no gossip), advancing its
//!   global_step well beyond Peer B's.
//! - Peer A gossips only the head block to Peer B and is then stopped.
//! - Expectation: Peer B cannot reach the head's `prev_output_step_number`,
//!   `wait_for_step` bails on the progress check, and Peer B emits
//!   `BlockStateUpdated { validation_result: Invalid(VdfValidationFailed(_)) }`.
//!
//! Why ignored: the assumption that Peer B "cannot reach `prev_output_step`"
//! does not hold under the current `block_pool`. When Peer B receives the
//! head block whose parent is unknown, it walks the orphan-fetch cascade:
//! `AttemptReprocessingBlock` + `RequestBlockFromTheNetwork` pull each
//! intermediate block from genesis (or from any other peer that still has
//! them). Each fetched block carries its own `vdf_info.steps`, and
//! `ensure_vdf_is_valid` calls `fast_forward_vdf_steps_from_block` for
//! every block it validates. By the time the head block reaches VDF
//! validation, Peer B's `global_step` has already been bridged across the
//! gap and the progress check correctly does not fire. The test still
//! observes a hang in shadow_tx_validation's EVM-payload fetch, which is
//! an out-of-scope path.
//!
//! For this test to actually exercise the VDF progress check, we need
//! either:
//! 1. The orphan-storm fix (Defect 2 in `claude/issue.md`) plus a way to
//!    deliver only the head without triggering the cascade — i.e. a block
//!    whose parent is in the tree but unvalidated, so the new
//!    `InTreePendingValidation` path applies and no parent fetch is
//!    triggered; OR
//! 2. A direct injection helper that posts a `ValidateBlock` message to a
//!    peer's `validation_service` without going through `block_pool`,
//!    paired with a synthetic block builder that produces a properly
//!    signed block with malicious `vdf_info`.
//!
//! Both prerequisites are out of scope for this branch (see
//! `docs/superpowers/plans/2026-05-08-vdf-validation-progress-check.md`,
//! "Deferred / out of scope"). The unit tests in `crates/vdf/src/state.rs`
//! (`wait_for_step_bails_when_no_progress`,
//! `wait_for_step_bails_on_cancel`,
//! `wait_for_step_completes_when_state_advances`) are the canonical
//! coverage of the progress-check semantics until one of those
//! prerequisites lands.

use crate::utils::IrysNodeTest;
use irys_actors::{
    block_tree_service::ValidationResult, block_validation::ValidationError,
};
use irys_types::NodeConfig;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Aggressively short progress timeout so the test runs quickly.
/// Must be > 1s so Stage A doesn't false-positive while the peer is
/// receiving the gossiped block header.
const SHORT_PROGRESS_TIMEOUT_SECS: u64 = 3;

/// Slack for event-channel + scheduling latency on top of the progress
/// timeout. The full deadline must comfortably exceed
/// `SHORT_PROGRESS_TIMEOUT_SECS`.
const FAILURE_DEADLINE_SECS: u64 = SHORT_PROGRESS_TIMEOUT_SECS + 30;

#[ignore = "see file-level docs: orphan-fetch cascade fast-forwards the peer's VDF state, so the progress check correctly does not fire under the current block_pool semantics; re-enable once Defect 2 lands or direct ValidateBlock injection helpers exist"]
#[test_log::test(tokio::test)]
async fn heavy_test_vdf_progress_check_fails_stalled_peer() -> eyre::Result<()> {
    // -- Genesis (Peer A) ------------------------------------------------
    let mut genesis_cfg = NodeConfig::testing();
    // Both nodes share the short progress timeout. The peer is the one
    // we expect to bail; setting the genesis to the same value is harmless.
    genesis_cfg.vdf.progress_timeout_secs = SHORT_PROGRESS_TIMEOUT_SECS;
    let genesis = IrysNodeTest::new_genesis(genesis_cfg.clone())
        .start_and_wait_for_packing("GENESIS", 30)
        .await;

    // -- Peer B ----------------------------------------------------------
    let mut peer_cfg = genesis.testing_peer();
    peer_cfg.vdf.progress_timeout_secs = SHORT_PROGRESS_TIMEOUT_SECS;
    let peer = IrysNodeTest::new(peer_cfg).start_with_name("PEER").await;

    // -- Common baseline -------------------------------------------------
    // Genesis mines, peer follows via gossip + fast-forward.
    genesis.mine_block().await?;
    genesis.mine_block().await?;
    peer.wait_until_height(2, 30).await?;

    // -- Freeze the peer's VDF ------------------------------------------
    // After this point, peer.global_step is pinned at whatever was last
    // fast-forwarded from genesis. NB: we must not call `wait_until_height`
    // on the peer after this point, since that helper auto-restarts the
    // peer's VDF for sync purposes.
    peer.stop_mining();

    // -- Subscribe to the peer's block-state events *before* delivering
    //    the block, so we don't miss the failure event.
    let mut event_rx = peer
        .node_ctx
        .service_senders
        .subscribe_block_state_updates();

    // -- Genesis privately advances --------------------------------------
    // Mining without gossip: blocks land on genesis only. Genesis's VDF
    // jumps several steps; the peer's `global_step` does not budge.
    genesis.mine_blocks_without_gossip(5).await?;
    let private_tip_height = genesis.get_canonical_chain_height().await;
    assert!(
        private_tip_height >= 7,
        "genesis should have advanced privately, got height {private_tip_height}"
    );
    let private_tip = genesis.get_block_by_height(private_tip_height).await?;
    let target_hash = private_tip.block_hash;
    let head = Arc::new(private_tip);

    // -- Deliver only the head, then take genesis offline ----------------
    // The gossip is fire-and-forget; the peer's gossip task picks up the
    // header and routes it through block_pool -> block_discovery ->
    // block_tree_service -> validation_service. We must take genesis
    // offline *after* the gossip has had a chance to enqueue, otherwise
    // the broadcast itself can race the shutdown.
    genesis.gossip_block_to_peers(&head)?;
    // Give the gossip a moment to be received before we kill genesis,
    // so the peer can't fetch missing parents from the now-dead trusted
    // peer. Without this, the peer's block_pool would walk the parent
    // cascade (each parent block carries its own VDF checkpoints, so
    // fast-forward would let the peer reach `global_step_number`
    // legitimately, and the progress guard would never fire).
    tokio::time::sleep(Duration::from_millis(500)).await;
    genesis.stop().await;

    // -- Wait for the failure event --------------------------------------
    let deadline = Instant::now() + Duration::from_secs(FAILURE_DEADLINE_SECS);
    let mut saw_failure = false;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, event_rx.recv()).await {
            Ok(Ok(event)) => {
                if event.block_hash != target_hash {
                    continue;
                }
                tracing::info!(
                    block.hash = ?event.block_hash,
                    block.height = event.height,
                    discarded = event.discarded,
                    state = ?event.state,
                    result = ?event.validation_result,
                    "peer block-state event for target block",
                );
                if let ValidationResult::Invalid(err) = &event.validation_result {
                    let reason = err.to_string();
                    let is_progress_check = matches!(err, ValidationError::VdfValidationFailed(_))
                        || reason.contains("did not advance")
                        || reason.contains("VDF validation failed");
                    assert!(
                        is_progress_check,
                        "validation failed for unexpected reason: {reason}"
                    );
                    saw_failure = true;
                    break;
                }
            }
            // Channel closed or recv error: the peer has shut down before us;
            // treat as missed event and exit the loop so the assert below
            // produces a clear diagnostic.
            Ok(Err(_)) | Err(_) => break,
        }
    }
    assert!(
        saw_failure,
        "expected VdfValidationFailed for {target_hash} within {FAILURE_DEADLINE_SECS}s"
    );

    peer.stop().await;
    Ok(())
}
