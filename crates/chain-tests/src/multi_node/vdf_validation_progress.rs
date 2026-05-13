//! Integration test verifying that a peer whose local VDF state cannot
//! reach a block's required `prev_output_step_number` surfaces a
//! `WaitForStepError::Stalled` (turned into `Invalid(VdfValidationFailed)`
//! at the validation-service boundary) within `progress_timeout_secs`,
//! rather than hanging the validation pipeline indefinitely.
//!
//! Why a normal "gossip the head only" setup doesn't exercise this:
//! when peer receives a head whose parent isn't in its tree,
//! `block_pool` runs the orphan-fetch cascade — each fetched ancestor is
//! validated and `fast_forward_vdf_steps_from_block` bridges the peer's
//! VDF state over the gap. By the time the head reaches VDF validation,
//! peer's `global_step` has caught up and the progress check correctly
//! does not fire.
//!
//! Fix 2 (`BlockStatus::InTreePendingValidation`) is necessary for this
//! test to be expressible, but on its own it isn't sufficient — even
//! with Fix 2, a head whose parent is `NotProcessed` still triggers the
//! cascade. We also need a way to deliver the head whose parent is in
//! peer's tree *but whose validated steps were never fast-forwarded into
//! peer's VDF state*. The cleanest way to do that in an integration test
//! is to inject the parent block into peer's `block_tree` directly via
//! the `test-utils`-gated write guard, marking it as `Validated`. The
//! head is then delivered via `send_full_block`, which calls
//! `block_discovery.handle_block` directly and goes through normal
//! prevalidation + validation_service — without going through
//! `block_pool` and without re-validating the parent.
//!
//! Peer's `run_vdf` thread is left running so the fast-forward channel
//! is still drained (otherwise validation would panic on a closed/full
//! channel, masking the progress-check failure), but local mining is
//! stopped via `stop_mining` so the peer cannot advance the step itself.
//! The injected parent is *not* sent through validation, so the
//! parent's VDF steps never enter peer's `vdf_state` buffer — the head's
//! `prev_output_step_number` is therefore beyond peer's `global_step`,
//! and the wait stalls.

use crate::utils::IrysNodeTest;
use irys_actors::{block_tree_service::ValidationResult, block_validation::ValidationError};
use irys_domain::{BlockState, ChainState};
use irys_types::NodeConfig;
use std::time::{Duration, Instant};

/// Aggressively short progress timeout so the test runs quickly. Must be
/// > 1s so the wait doesn't false-positive while the peer is receiving
/// the gossiped block header / payload.
const SHORT_PROGRESS_TIMEOUT_SECS: u64 = 3;

/// Slack on top of the progress timeout for event-channel + scheduling latency.
const FAILURE_DEADLINE_SECS: u64 = SHORT_PROGRESS_TIMEOUT_SECS + 20;

#[test_log::test(tokio::test)]
async fn heavy_test_vdf_progress_check_fails_stalled_peer() -> eyre::Result<()> {
    // -- Genesis (Peer A) ------------------------------------------------
    let mut genesis_cfg = NodeConfig::testing();
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

    // Capture peer's "frozen" baseline VDF step. After this point the
    // peer is not allowed to mine new steps locally, so anything past
    // this value would have to arrive via fast-forward — and the
    // injected parent below is never sent through validation, so its
    // steps will never reach the fast-forward channel.
    peer.stop_mining();
    let peer_frozen_step = peer.node_ctx.vdf_steps_guard.read().global_step;
    tracing::info!(
        peer.frozen_global_step = peer_frozen_step,
        "peer mining stopped"
    );

    // -- Mine the parent privately on genesis ----------------------------
    // This block (height 3) will be injected directly into peer's tree
    // as `Validated(ValidBlock)`, bypassing block_discovery /
    // block_tree_service / validation_service entirely. Its VDF steps
    // are therefore NEVER fast-forwarded into peer's `vdf_state`.
    let (parent_header, _parent_payload, _parent_txs) = genesis.mine_block_without_gossip().await?;
    let parent_hash = parent_header.block_hash;
    let parent_height = parent_header.height;
    tracing::info!(
        block.hash = %parent_hash,
        block.height = parent_height,
        vdf.global_step_number = parent_header.vdf_limiter_info.global_step_number,
        "private parent block mined on genesis"
    );

    // Inject the parent into peer's block_tree. We reuse the parent's
    // own parent's snapshots (a no-commitment, non-epoch block inherits
    // them) so the entry is coherent enough for the child's
    // `on_block_prevalidated` to derive snapshots from it. The parent's
    // chain state is then promoted Unknown → ValidationScheduled →
    // ValidBlock so it is observable as `ProcessedButCanBeReorganized`
    // by `block_status`, and `block_pool` would not enter Fix 2's
    // wait-for-parent-validation path (this test still delivers the
    // child via `send_full_block`, which bypasses `block_pool`).
    {
        // Build a SealedBlock with an empty body; the body isn't read
        // along the failure path we want to exercise.
        let sealed_parent = std::sync::Arc::new(irys_types::SealedBlock::new(
            parent_header.as_ref().clone(),
            irys_types::BlockBody {
                block_hash: parent_hash,
                ..Default::default()
            },
        )?);

        // Pull cloned snapshots from the parent's parent (height 2 in
        // peer's tree). For a non-epoch, no-commitment block these are
        // identical to what `on_block_prevalidated` would derive itself.
        let (grandparent_commitment, grandparent_epoch, grandparent_ema, grandparent_header) = {
            let tree = peer.node_ctx.block_tree_guard.read();
            let grandparent_hash = parent_header.previous_block_hash;
            (
                tree.get_commitment_snapshot(&grandparent_hash)
                    .map_err(|e| eyre::eyre!("grandparent commitment snapshot missing: {e}"))?,
                tree.get_epoch_snapshot(&grandparent_hash)
                    .ok_or_else(|| eyre::eyre!("grandparent epoch snapshot missing"))?,
                tree.get_ema_snapshot(&grandparent_hash)
                    .ok_or_else(|| eyre::eyre!("grandparent ema snapshot missing"))?,
                tree.get_block(&grandparent_hash)
                    .ok_or_else(|| eyre::eyre!("grandparent header missing"))?
                    .clone(),
            )
        };
        let next_ema = grandparent_ema.next_snapshot(
            &parent_header,
            &grandparent_header,
            &peer.node_ctx.config.consensus,
        )?;

        let mut tree = peer.node_ctx.block_tree_guard.write();
        tree.add_block(
            &sealed_parent,
            grandparent_commitment,
            grandparent_epoch,
            next_ema,
        )?;
        tree.mark_block_as_validation_scheduled(&parent_hash)?;
        tree.mark_block_as_valid(&parent_hash)?;
        // Sanity: the injected parent is now in a validated state.
        // `mark_block_as_valid` from `NotOnchain(Unknown)` →
        // `NotOnchain(ValidationScheduled)` → `NotOnchain(ValidBlock)` (not
        // `Validated(...)`, which is reached via `mark_tip` walking back
        // through `mark_on_chain`). Either way, the block is "validated" as
        // far as `block_status` and `wait_for_parent_validation` are
        // concerned (both bucket `NotOnchain(ValidBlock)` into
        // `ProcessedButCanBeReorganized`).
        let (_h, state) = tree
            .get_block_and_status(&parent_hash)
            .expect("parent must be in peer's tree after injection");
        assert!(
            matches!(
                state,
                ChainState::Validated(BlockState::ValidBlock)
                    | ChainState::NotOnchain(BlockState::ValidBlock)
                    | ChainState::Onchain
            ),
            "injected parent must be in a validated state, got {state:?}"
        );
    }
    tracing::info!(
        block.hash = %parent_hash,
        block.height = parent_height,
        "parent injected into peer's tree as Validated(ValidBlock)"
    );

    // -- Mine the head privately on genesis ------------------------------
    // Height 4. Its `prev_output_step_number` equals the parent's last
    // VDF step, which is strictly greater than peer's frozen step. Its
    // own steps are in `vdf_info.steps`, so once validation reaches
    // Stage D it would fast-forward — but we never get past Stage A
    // (`wait_for_step(prev_output_step_number)`), because peer cannot
    // reach the parent's last step without local mining and the
    // parent's steps were never fast-forwarded into peer's state.
    let (head_header, head_payload, head_txs) = genesis.mine_block_without_gossip().await?;
    let head_hash = head_header.block_hash;
    tracing::info!(
        block.hash = %head_hash,
        block.height = head_header.height,
        vdf.prev_output_step = head_header.vdf_limiter_info.first_step_number().saturating_sub(1),
        vdf.global_step_number = head_header.vdf_limiter_info.global_step_number,
        peer.frozen_step = peer_frozen_step,
        "head block mined; expect peer Stage A to stall"
    );
    let head_prev_output_step = head_header
        .vdf_limiter_info
        .first_step_number()
        .saturating_sub(1);
    assert!(
        head_prev_output_step > peer_frozen_step,
        "test invariant: head's prev_output_step ({head_prev_output_step}) must exceed peer's frozen step ({peer_frozen_step})"
    );

    // -- Subscribe to the peer's block-state events *before* delivering
    //    the head, so we'd catch a spurious Valid event if the gap
    //    somehow got bridged (regression detector).
    let mut event_rx = peer
        .node_ctx
        .service_senders
        .subscribe_block_state_updates();

    // Snapshot peer's vdf step *before* delivery; we'll assert it
    // doesn't advance during the wait (peer is supposed to be unable
    // to reach the parent's last step).
    let pre_delivery_step = peer.node_ctx.vdf_steps_guard.read().global_step;

    // -- Deliver the head via direct block_discovery + payload injection.
    //    This bypasses block_pool entirely, so the orphan-fetch cascade
    //    cannot trigger. block_discovery.handle_block runs prevalidation
    //    on the head, on_block_prevalidated adds it to the tree as
    //    NotOnchain(ValidationScheduled), and validation_service then
    //    picks it up and runs ensure_vdf_is_valid.
    genesis
        .send_full_block(&peer, &head_header, head_payload, head_txs)
        .await?;
    tracing::info!(block.hash = %head_hash, "head delivered to peer via send_full_block");

    // -- Wait for peer's validation_service to die from the Stalled panic.
    //
    // `ensure_vdf_is_valid` Stage A polls `vdf_state.global_step` and
    // returns `WaitForStepError::Stalled` once `progress_timeout_secs`
    // elapses without local progress. `PreemptibleVdfTask::execute` then
    // panics with "VDF wait stalled" (see
    // `active_validations.rs` ~line 150 and
    // `design/docs/vdf-validation-stall-detection.md` — never-mislabel
    // rule: a local liveness failure must not be reported as block-Invalid
    // to the block tree). The validation_service task ends, which drops
    // its `mpsc` receiver and flips
    // `service_senders.validation_service.is_closed()` to true.
    //
    // That sender-closed transition is the cleanest in-test signal that
    // the progress check fired. We also assert (separately) that no
    // `Valid` event was emitted for the head — that would mean peer's
    // VDF state somehow got bridged across the gap, which would be a
    // regression of the very scenario this test is meant to lock in.
    let deadline = Instant::now() + Duration::from_secs(FAILURE_DEADLINE_SECS);
    let mut peer_validation_dead = false;
    while Instant::now() < deadline {
        // Drain any pending state events with a small timeout so we
        // don't sleep through the whole wait. A `Valid` event for the
        // head would mean fast-forward bridged the gap — fail loudly.
        if let Ok(Ok(event)) =
            tokio::time::timeout(Duration::from_millis(100), event_rx.recv()).await
            && event.block_hash == head_hash
        {
            tracing::info!(
                block.hash = ?event.block_hash,
                state = ?event.state,
                discarded = event.discarded,
                result = ?event.validation_result,
                "peer block-state event for head"
            );
            if matches!(event.validation_result, ValidationResult::Valid) {
                panic!(
                    "head was unexpectedly validated as Valid — peer's VDF must \
                     have caught up across the gap (regression of the stalled-peer \
                     scenario)"
                );
            }
            if let ValidationResult::Invalid(err) = &event.validation_result {
                // Defensive: if the wait stage ever gets converted into
                // an Invalid result via a different code path (rather
                // than the current "panic on Stalled" behaviour), still
                // accept it — but require the reason to mention VDF.
                let reason = err.to_string();
                assert!(
                    matches!(err, ValidationError::VdfValidationFailed(_))
                        || reason.contains("VDF")
                        || reason.contains("Stalled")
                        || reason.contains("did not advance"),
                    "head failed validation for an unexpected non-VDF reason: {reason}"
                );
                peer_validation_dead = true;
                break;
            }
        }

        if peer.node_ctx.service_senders.validation_service.is_closed() {
            peer_validation_dead = true;
            break;
        }
    }
    assert!(
        peer_validation_dead,
        "expected peer's validation_service to die within {FAILURE_DEADLINE_SECS}s \
         (either via Stalled panic or Invalid(VdfValidationFailed) on the legacy \
         path) — Stage A's progress check did not fire"
    );

    // Peer's local VDF must not have advanced (mining is stopped, no
    // fast-forward arrived). Any advancement would mean a different
    // code path bridged the gap.
    let post_step = peer.node_ctx.vdf_steps_guard.read().global_step;
    assert_eq!(
        post_step, pre_delivery_step,
        "peer's local global_step changed during the wait \
         (pre={pre_delivery_step}, post={post_step}) — fast-forward must not run \
         on the stalled path"
    );

    // Cluster teardown. Peer's validation_service has already panicked,
    // and the panic hook in testing-utils raised SIGINT — peer is
    // already mid-shutdown. We swallow any teardown errors here; the
    // important assertions above have already passed.
    let _ = peer.stop().await;
    let _ = genesis.stop().await;
    Ok(())
}
