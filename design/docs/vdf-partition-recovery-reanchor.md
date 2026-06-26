# VDF Re-anchor on Network-Partition Recovery

## Status

Accepted — implemented and verified. On a network-partition recovery the VDF re-anchors to
the canonical chain **TIP** and the recovering node converges to the canonical VDF lineage
automatically, instead of wedging until an operator restart.

The shipped solution is a **stack of layers** (detailed in
[Implementation: the complete heal](#implementation-the-complete-heal-2026-06)), each forced
by the end-to-end boundary-crossing test:

1. **Re-anchor at the tip** — `create_state_for_canonical_tip` rebuilds `VdfState` from the
   canonical chain (not the LCA; see the Correction under [Decision](#decision)).
2. **Catch-up reset-seed fix** — `canonical_vdf_info_at_or_below_step` so the
   incremental-adoption window cannot re-poison the steps past a boundary the re-anchor landed
   below.
3. **Fork-aware validation** — recompute-on-mismatch + fork-local step/recall views so the
   recovering node can adopt the canonical chain *during* the asynchronous re-anchor window.
4. **Mining rotation reset** — a `Reanchored` broadcast resets partition mining's stateful
   efficient-sampling rotation.

Mechanism: the VDF thread is a supervisor loop (`init_vdf_thread`, `crates/chain/src/chain.rs`);
`run_vdf` returns `VdfExit` and is restarted on `VdfExit::Reanchor`; `BlockTreeService` sends
the `vdf_reanchor` signal after `recover_from_network_partition`. Verified by
`heavy4_slow_partition_recovery_crosses_reset_boundary` — 20/20 across varied fork geometries.

## Context

When a node is isolated during a network partition, mines its own minority fork,
and later reconnects, it performs a **deep reorg** onto the canonical chain. This
is the "network partition recovery" path: triggered at
`crates/actors/src/block_tree_service.rs:1052` when the orphaned fork is strictly
deeper than `block_migration_depth`, which calls
`BlockMigrationService::recover_from_network_partition`
(`crates/actors/src/block_migration_service.rs:348`). That routine truncates the
block index, unassigns storage-module offsets, and rolls back supply state to the
fork-parent height.

**It does not touch VDF state.** This is the gap.

### How the reset seed becomes poisoned

At every VDF reset boundary (`global_step % reset_frequency == 0`,
`crates/vdf/src/vdf.rs:333`) the loop folds in `next_reset_seed` via
`apply_reset_seed` (`crates/vdf/src/lib.rs:166`). That seed is pinned by the
*rotation block* at `boundary − reset_frequency`:
`next_seed = parent_header.block_hash` (`crates/types/src/block.rs:145`). A
different fork has a different rotation-block hash, so it has a different reset
seed, so its VDF lineage **diverges** for every step after that boundary.

If the partition lasted long enough for the minority fork to cross a reset
boundary, the recovering node holds VDF steps — both in the running loop's
`hash`/`global_step` locals and in the shared `VdfState` buffer
(`crates/vdf/src/state.rs`) — that were computed with the **minority** reset seed.
This is the exact poisoned-buffer wedge reproduced by
`issue_1447_unconfirmed_reset_seed_poisons_buffer_and_wedges_block_validation`
(`crates/vdf/src/vdf.rs:1225`): `vdf_steps_are_valid` rejects the canonical block,
and the node cannot follow the chain it just reorged onto.

### Why the #1449 confirmation gate does not cover this

The reset-boundary confirmation gate (`is_reset_boundary_blocked`,
`crates/vdf/src/vdf.rs:365`) prevents the loop from crossing a boundary until the
*confirmed* chain (`block_migration_depth` deep) reaches the rotation point. That
makes the loop safe for reorgs **up to `block_migration_depth`**. Partition
recovery is, by definition, the one sanctioned path that reorgs *past* that depth:
on the minority fork the rotation block was confirmed (migrated) locally, so the
gate correctly let the loop cross — using the minority seed. The fast-forward path
comment at `crates/vdf/src/vdf.rs:136` acknowledges this residual concern and
relies on deeper forks being refused at p2p admission (`PartOfAPrunedFork`); but
the partition-recovery path explicitly *accepts* such a fork, so that assumption
does not hold here.

### Why nothing self-heals today

- **The forward seed already updates.** The loop re-reads
  `latest_canonical_vdf_info()` every iteration (`crates/vdf/src/vdf.rs:196`) and
  `BlockStatusProvider` reads the live canonical tip with no cache lag — the
  longest-chain cache is rebuilt synchronously on `mark_tip`. So `next_reset_seed`
  flips to the canonical value immediately. **This is necessary but not
  sufficient:** the `hash` fed *into* the next boundary is still on the poisoned
  lineage, so `apply_reset_seed(poisoned_hash, canonical_seed)` still diverges.
- **The buffer cannot rewind.** `VdfState::store_step`
  (`crates/vdf/src/state.rs:87`) is strictly forward-only and gap-rejecting
  (`global_step >= proposed → no-op`; `global_step + 1 != proposed → no-op`).
- **Fast-forward cannot overwrite it.** FF only fires when the canonical step is
  *ahead* of the local step (`crates/vdf/src/vdf.rs:121`). After a partition the
  local loop sits at the live VDF timeline (≈ wall clock), at or ahead of the
  older canonical blocks being replayed, so FF is skipped and the poisoned steps
  are never overwritten.

### Reachability

A recovery reorg can be as deep as `block_tree_depth` (beyond that,
`validate_reorg_within_cache_window` triggers controlled shutdown). Poisoning is
reachable whenever a `block_tree_depth`-block window can span a reset interval:

| Config (`crates/types/src/config/consensus.rs`) | `block_tree_depth` | reset interval | spans a boundary? |
| --- | --- | --- | --- |
| mainnet (`:594`, `:688`) | 100 blocks | 50 blocks | **always** (period < window) |
| testnet (`:790`, `:804`) | 50 blocks | 100 blocks | ~50% of windows |

On mainnet config this is reliably reachable for any deep recovery that approaches
the cache limit.

## Decision

On a network-partition recovery, **re-anchor the VDF**: rewind it to a
pre-divergence point on the (now canonical) chain and let the existing
fast-forward + local-stepping machinery rebuild it forward with the correct reset
entropy. Concretely, tear down and restart the `run_vdf` invocation — but keep the
OS thread.

The key realisation that makes this cheap: **the fast-forward path stores each
canonical block's step output verbatim** (`crates/vdf/src/vdf.rs:179`). So once the
loop's `global_step` is rewound below the canonical blocks' steps, FF replays the
entire canonical step range verbatim and the buffer becomes *exactly* canonical —
including correct boundary crossings, because we store canonical-provided values
rather than recomputing them. Only steps beyond the canonical tip (catching up to
the live timeline) are computed locally, and those use the now-canonical
`next_reset_seed`. The poisoning heals itself once we make the loop go backward,
which is the one thing it cannot do on its own.

> **Correction (2026-06, supersedes the LCA-anchor decision below).** The "FF
> replays the entire canonical step range verbatim … the poisoning heals itself"
> reasoning above does **not** hold when the recovered range crosses a reset
> boundary above the LCA — the node re-wedges. Three code facts break it:
>
> 1. **The gate does not re-gate.** `run_vdf` reads `confirmed_global_step_number`
>    from `latest_canonical_vdf_info()`, which sources it from the block tree's NEW
>    canonical tip (`BlockTree::confirmed_canonical_step`, far above the LCA) — not
>    from the re-anchored position. So `is_reset_boundary_blocked(LCA+1, …)` is
>    `false` and the loop runs ahead across the recovered range. (This refutes "the
>    #1449 gate naturally re-gates", below.)
> 2. **Mining is not paused on the reorg path,** so the loop local-steps LCA→tip
>    immediately, applying the canonical TIP's single `next_seed` at every
>    intermediate boundary instead of each boundary's own rotation-block seed —
>    diverging from the canonical lineage.
> 3. **Fast-forward is not re-supplied.** The canonical blocks' FF steps are drained
>    on re-anchor and never re-sent (the reorg handler only `reevaluate_priorities`,
>    never re-validating on-chain blocks), and `store_step` is forward-only, so FF
>    cannot overwrite the locally-stepped poison.
>
> Net: the buffer diverges and the node rejects the very canonical chain it reorged
> onto — the #1447 wedge, re-introduced. Reproduced deterministically by
> `partition_recovery_reanchor_repoisons_buffer_and_wedges_block_validation`
> (`crates/vdf/src/vdf.rs`).
>
> **Fix: re-anchor at the canonical TIP, not the LCA.** Rebuild `VdfState` from the
> block tree's canonical chain up to the tip via `create_state_for_canonical_tip`
> (`crates/vdf/src/state.rs`); each canonical block carries its own reset-boundary
> seed, so the rebuilt buffer matches the canonical lineage across the whole
> recovered range and the loop restarts **at the tip** (never re-crossing a
> historical boundary). The recovered range is always within the block tree (a reorg
> is only representable up to `block_tree_depth` deep), and block headers are
> persisted to the DB only at migration, so the un-migrated tail is read from the
> block tree rather than the DB. The `create_state(LCA)` approach described in the
> rest of this section is retained only as historical context.

### The anchor: the LCA (fork parent)

`recover_from_network_partition` truncates the block index to `fork_parent_height`
— the last common ancestor, which is shared between both forks and therefore
known-good. We re-anchor the VDF there by rebuilding `VdfState` from the truncated
index via the existing `create_state` (`crates/vdf/src/state.rs:295`), which reads
`get_latest_item()` (now the LCA) and walks back filling the step buffer. The
restarted `run_vdf` takes its `(global_step, hash, next_seed)` from that anchor.

Anchoring at the LCA — rather than the canonical tip — is deliberately the
simplest correct choice: the LCA's seed lineage is unambiguously shared, and every
step above it on the canonical chain arrives through FF (cached canonical blocks,
then live). It also keeps the re-anchor logic independent of whether canonical
re-migration has run yet.

### Mechanism: a VDF supervisor loop on the existing thread

Today `init_vdf_thread` (`crates/chain/src/chain.rs:2148`) spawns one OS thread
that calls `run_vdf` exactly once. We change that thread to run a **supervisor
loop**:

```
loop {
    run_vdf(.., &mut ff_receiver, current_anchor, shared_vdf_state, ..);
    // run_vdf returns when either the shutdown token OR a new reanchor token fires
    if shutdown_token.is_cancelled() { break; }
    // a reanchor was requested:
    current_anchor = rebuild_anchor_from_canonical_index();   // create_state + tip next_seed
    *shared_vdf_state.write() = current_anchor.state;          // overwrite buffer in place
    reanchor_token = CancellationToken::new();                 // fresh token for the next run
}
```

This is "teardown + restart `run_vdf`" without tearing down the OS thread, which
sidesteps the two costs of a full thread respawn:

1. **The fast-forward receiver.** `Receiver<Traced<VdfStep>>` is single-ownership
   and is *moved* into `run_vdf` today. The supervisor owns it across iterations
   and passes `&mut` (a small signature change to `run_vdf`), so the channel — and
   the `Sender` held in `ServiceSenders` (`crates/actors/src/services.rs:118`,
   cloned by the validation service at
   `crates/actors/src/validation_service.rs:1064`) — is never recreated and no
   sender swap is needed.
2. **Core pinning.** The pinning done in `init_vdf_thread` persists; we do not
   re-run it.

### Shared state handling

- **`VdfState` buffer:** the shared `Arc<RwLock<VdfState>>` handle is referenced by
  mining and validation, so we cannot replace the `Arc`. We overwrite its contents
  under the write lock: `*vdf_state.write() = create_state(...)`. Held only for the
  swap; readers that take the lock afterward see a consistent canonical buffer.
- **`atomic_vdf_global_step`** (`Arc<AtomicU64>`): reset to the anchor's global
  step so partition-mining services observe the rewind.

### Triggering the re-anchor

`BlockTreeService` already detects the deep reorg and calls
`recover_from_network_partition` (`crates/actors/src/block_tree_service.rs:1068`).
Immediately **after that call returns**, it sends a `reanchor` signal to the VDF
supervisor. Add a dedicated control channel (mirroring `vdf_fast_forward` in
`ServiceSenders`); the supervisor selects on it while `run_vdf` is parked/looping,
or `run_vdf` checks the `reanchor_token` on the same cadence as `shutdown_token`
(`crates/vdf/src/vdf.rs:112`) and returns when it fires.

Ordering is the critical invariant: the signal must be sent strictly **after** the
block index has been truncated, so `create_state` reads the canonical (LCA-rooted)
index, never the stale minority index.

### Interaction with mining and validation

- **Mining** is already paused during sync via `is_vdf_mining_enabled`
  (`crates/vdf/src/state.rs:162`). The reorg event invalidates any in-flight block
  solution built on poisoned steps; that solution is dropped on the reorg path
  regardless of this change.
- **The #1449 gate needs no special handling.** After re-anchor, `global_step`
  drops back to the anchor (the canonical tip), so `is_reset_boundary_blocked` naturally
  re-gates every forward boundary crossing against the (now canonical) confirmed step.
  > **Superseded in part (2026-06).** "No special handling" understated it: the re-anchor
  > can land *below* a boundary the canonical chain has since passed, and the catch-up
  > local-stepping then needs the *per-boundary* seed (not the tip's). The gate is still
  > untouched, but the seed source is — see
  > [§2 of Implementation](#2-the-catch-up-reset-seed-hole--and-canonical_vdf_info_at_or_below_step).

## Implementation: the complete heal (2026-06)

The shipped fix is the **canonical-TIP re-anchor** (per the Correction under [Decision](#decision))
plus four layers that together make a boundary-crossing recovery both heal correctly and avoid
wedging validation or mining in the window before the heal lands. Each layer was forced by the
end-to-end test `heavy4_slow_partition_recovery_crosses_reset_boundary`, which mines two real
forks across a reset boundary and reorgs one onto the other.

### 1. Re-anchor at the canonical tip (the heal)

`create_state_for_canonical_tip` (`crates/vdf/src/state.rs`) rebuilds `VdfState` from the block
tree's canonical chain (`get_canonical_chain`) up to the tip, via the shared, DB-free-testable
`build_vdf_seed_buffer`. Each canonical block contributes its own recorded steps (carrying its
own boundary seed), so the rebuilt buffer reproduces the canonical lineage across the whole
recovered range. `init_vdf_thread` takes the `block_tree_guard` (not the block index). It returns
`eyre::Result` (see [Rebuild failure](#risks--edge-cases)) plus the canonical `next_seed`.

### 2. The catch-up reset-seed hole — and `canonical_vdf_snapshot`

The tip re-anchor is correct **only if it anchors at, or above, every reset boundary the
recovered range crosses.** But a partition recovery adopts the canonical fork **incrementally**
(block-by-block over gossip), so the *first* deep reorg can fire while the canonical tip is still
**below** a poison boundary. The re-anchor correctly anchors at the canonical tip *of that
instant* — below the boundary — and the VDF then **local-steps the catch-up** forward across the
boundary on its own, before the canonical blocks that pin that boundary's seed have been adopted.

At the crossing, `run_vdf` applied `next_reset_seed = <tip>.next_seed` — the **tip's** next_seed.
Once the tip advances *past* the boundary (the rest of the fork arrives), that field is the seed for
the next, *higher* boundary, not the one being crossed. The wrong seed is XOR'd in and the
post-boundary steps are re-poisoned — and, because `store_step` is forward-only, the canonical steps
that arrive afterward can never overwrite them. The heal silently fails for that range (~1 in 15–20
boundary-crossing recoveries; the deterministic VDF would otherwise reproduce canonical exactly with
the right seed).

**Fix:** source the reset seed from the canonical block **ending at or before the boundary being
crossed**, not the tip — that block's `next_seed` pins the seed for that boundary. The rule is:
**boundary `B`'s reset seed is the `next_seed` of the canonical block ending at or before `B - 1`.**
Each path queries accordingly: local-stepping (which is at step `S` and resets the step it computes,
`S + 1`) queries at `S = B - 1`; the fast-forward path (which stores step `P` and resets *at* `P`)
queries at `P - 1`. Plumbing: `BlockProvider::canonical_vdf_snapshot(step)`
(`crates/types/src/block_provider.rs`) → `BlockTree::canonical_entry_at_or_below_step`
(`crates/domain/src/models/block_tree.rs`, an in-place binary search by `global_step_number` over
the canonical-chain cache) → implemented in `BlockStatusProvider` (`crates/p2p/...`).

`canonical_vdf_snapshot` returns the **whole** canonical snapshot the loop needs — tip info,
`confirmed_global_step_number`, and the per-step reset seed (`reset_seed_for_step`) — from **one**
block-tree read lock, so a reorg cannot land between a tip read and a separate seed read (it
replaced the earlier split `latest_canonical_vdf_info()` + `canonical_vdf_info_at_or_below_step()`
pair, which took two locks). When the VDF runs *ahead* of the tip the query returns the tip itself
(the greatest `global_step_number <= S` is the tip), yielding the tip's `next_seed` — equal to the
prior behavior, a no-op. The fallback to the tip's `next_seed` is a defensive default for the
distinct edge case where *no* canonical block ends at/before the step (the step is below the earliest
cached block in the bounded canonical-chain cache).

Two subtleties this resolves:

- **Boundary-spanning blocks.** A single block can straddle a reset boundary; its `next_seed`
  targets the boundary *above* its range. So the query is the block *ending at or before* the step
  (greatest `global_step_number <= S`), **not** the block whose range *covers* the step (which may
  be the spanning block, yielding the wrong seed). This off-by-one — covering vs ending-before —
  was a real bug in the first cut of this fix, caught by the boundary-crossing test.
- **The fast-forward path needs the same seed for its carried hash.** FF stores canonical step
  values **verbatim** (`store_step` runs *before* `process_reset`), so the FF'd *buffer* is canonical
  regardless of `next_reset_seed`. But the `process_reset` after an FF step folds the seed into the
  *carried* `hash` that seeds any subsequent **local** stepping — so if an FF step lands exactly on a
  boundary and local stepping resumes, that hash must carry the right boundary fold. The FF path
  therefore sources `reset_seed_for_step` too, querying at `P - 1` (the step before the FF step, since
  `process_reset` is applied *at* `P`, not `P + 1`). Without the `- 1` it would fold the seed for the
  boundary *above* `P` whenever a canonical block ends exactly on `P`.

This preserves the #1447/#1449 protection: the confirmation gate still keys on
`confirmed_global_step_number` (unchanged); only the seed *source* moves, and only when the VDF is
at/below the canonical tip (catch-up).

### 3. Validation must tolerate the asynchronous re-anchor window

The re-anchor is signalled *after* `recover_from_network_partition` but applied **asynchronously**
by the VDF supervisor on its next loop iteration (~one loop tick later). In that window the live
buffer still holds the pre-swap (poisoned) lineage, yet the canonical fork's blocks are already
being validated. Without care, an honest block validated in that window is rejected as terminal
`Invalid` and never retried (the gossip-seen cache dedups a re-gossip; an orphan pull needs a
child). Three validation seams were made **fork-aware** so the recovering node can adopt the
canonical chain *during* the window, all flowing through one primitive,
`irys_vdf::state::build_fork_local_view`:

- **VDF-step batch validation — recompute on mismatch.** `vdf_step_batch_is_valid`
  (`crates/vdf/src/state.rs`) no longer treats a live-buffer mismatch as invalidity; a mismatch
  falls through to the authoritative recompute from the block's own `prev_output`/seed. A heavier
  competing fork legitimately differs from this node's buffer past a boundary; recompute is the
  source of truth. (Verified not to weaken #1447, whose protection is the mining-loop gate, not
  this fast path.)
- **Recall-range validation — fork-local view, always.** The recall range is validated against a
  fork-local step view built from the block's **own** lineage (its ancestors via the block tree,
  DB fallback) — `build_fork_local_recall_view` (`crates/actors/src/block_validation.rs`) — **never**
  this node's live buffer. The recall range is a many-to-one reduction of the step window to an
  index, so a competing fork's range that is invalid for its own steps could otherwise coincide with
  the (per-node) live buffer's reduction and be wrongly accepted — and two honest nodes would then
  disagree on the same block (non-deterministic consensus). The block's steps are already validated
  by layer 1, so the fork-local view is the authoritative, node-independent source; a build failure
  is the soft `StepsUnavailable` (retry) lane, never a peer-attributed mismatch. (This *always*-path
  superseded an earlier "live buffer first, fork-local only on `Mismatch`" version that had the
  coincidence hole.)
- **Previous-step continuity — fork-local view.** The `ensure_vdf_is_valid` prev-step equality
  check (`crates/actors/src/validation_service.rs`) reads the live buffer as a fast path and treats
  an *absent* previous step and a *present-but-stale* one (the poisoned boundary value during the
  window) identically: both fall through to resolve the previous step from a fork-local view sized
  to include it (`build_fork_local_step_view`, which covers the prev step even for a boundary-crossing
  block) and accept iff that matches. (An absent-or-mismatched step with an unbuildable view requeues
  via `VdfPrevStepForkViewUnavailable`; there is no separate `VdfStepRewound` re-wait path.) A value that
  mismatches the block's own canonical ancestry is still rejected. The `VdfPrevStepForkViewUnavailable`
  requeue is **bounded** (`MAX_PREV_STEP_VIEW_RETRIES`): a few requeues cover the transient window (on
  retry the buffer has typically healed so the fast path works, or the evicted ancestor reappears),
  after which the block is parked as a peer-innocent SoftInternal (`VdfPrevStepViewUnavailable`) so the
  single VDF lane is freed — rather than spinning forever on a permanently-pruned ancestor.

Both view builders resolve each ancestor **block-tree-first, then by hash from the DB**, mirroring
`create_state_for_canonical_tip`. The tree-only lookup was insufficient: a deep recovery's
recall/step window can reach below the cached chain (mainnet `reset_frequency` ≈ 50 blocks vs
`block_tree_depth` 100, but lower steps-per-block widen the window), so a needed ancestor may be
migrated out of the tree yet still present in the DB. Without the fallback the build fails and the
honest canonical block is routed to `StepsUnavailable` / `VdfPrevStepViewUnavailable` and retried
(now *boundedly* — see above — then parked) instead of validating; the DB fallback is what lets the
build succeed in the first place so the honest block validates rather than parks.

### 4. Post-recovery mining — reset the efficient-sampling rotation

Partition mining's recall-range selection uses a **stateful** efficient-sampling rotation
(`irys_efficient_sampling::Ranges`) that assumes strictly consecutive *forward* VDF steps. A
re-anchor is a backward rewind, so the rotation must be discarded. On re-anchor the supervisor
broadcasts `MiningBroadcastEvent::Reanchored` (`MiningBus::send_reanchor`,
`crates/actors/src/mining_bus.rs`); each partition miner resets its rotation
(`PartitionMiningService::handle_reanchor`: `Ranges::reinitialize()` + `last_step_num = 0`,
`crates/actors/src/partition_mining_service.rs`) and rebuilds it from the re-anchored steps on the
next seed. `get_recall_range` also defends against a backward step jump.

> **Residual (non-safety, tracked separately).** Immediately after a re-anchor the rotation
> realigns over the next few steps; a node that *mines* into that window can compute a recall range
> off-by-one from the validator and self-reject the block (retried — nothing invalid is ever
> adopted, no safety impact). In production a recovered node **follows** the network (ungated
> fast-forward, no local recall-range computation) rather than mining into the window, so this does
> not arise on the real recovery path. The end-to-end test therefore drives continued production
> from the **peer** (the canonical-fork miner, whose rotation was never re-anchored and so is
> intact) and asserts the recovered node *follows* — the production path — rather than forcing the
> recovered node to mine into the turbulence.

### 5. The anchor hash itself must carry its boundary fold

The buffer stores **raw** step outputs: `process_reset` folds the reset entropy into the *carried*
hash only **after** a step is stored (the loop tail), and fast-forward stores values verbatim. So a
hash read back from the buffer via `get_last_step_and_seed` — exactly what the supervisor uses as
the next `run_vdf` anchor — is **unfolded**. `run_vdf` does not fold its initial hash; it goes
straight into `vdf_sha` for `anchor_step + 1`. If `anchor_step` is *itself* a reset boundary, that
first step skips boundary `anchor_step`'s fold and diverges from the canonical lineage.

Both anchor sites apply `irys_vdf::vdf::reset_applied_anchor_hash(reset_frequency, step, hash, seed)`
(a no-op unless `step` is a boundary): the **startup** anchor (`init_vdf_thread`) with
`latest_block`'s seed, and the **re-anchor** (the supervisor's `Reanchor` arm) with the rebuilt
canonical tip's seed. `seed` here is the anchoring block's own `vdf_limiter_info.seed` — *not*
`next_seed`: when a block's step range contains a reset boundary, `IrysBlockHeader::set_seeds` pins
that in-range boundary's entropy in `seed` while `next_seed` targets the *next* boundary above the
block. (This is the same value `canonical_vdf_snapshot(step - 1).reset_seed_for_step` would return,
since the parent's `next_seed` equals this block's `seed`.)

Rare (only when an anchor lands exactly on a boundary) and non-safety (validation recomputes from
each block's own seed, so a mis-stepped buffer cannot reject a canonical block), but it mis-steps
local mining for a range until it heals. The rebuild-*failure* fallback (resume from the live
buffer's current step) is left unfolded — it is already a degraded best-effort path that leans on
`store_step`'s behind-step rejection to realign.

## Alternatives considered

- **In-place re-anchor command (Strategy B).** Add a `VdfState::reanchor()` that
  clears/refills the buffer and a command channel that rewrites the running loop's
  `hash`/`global_step` without returning from `run_vdf`. Lighter, but introduces a
  deliberate backward mutation into a buffer whose entire invariant set assumes
  forward-only progress, and spreads re-anchor logic across the hot loop. Rejected
  in favour of restarting `run_vdf` from the well-tested `create_state` startup
  path, which yields a clean, fully-canonical buffer with no new buffer-rewind
  primitive.
- **Full OS-thread teardown + respawn (Strategy A1).** Conceptually identical but
  forces recreating the FF channel (and swapping the `Sender` inside the
  `ServiceSenders` hub) and re-running core pinning. The supervisor-loop variant
  keeps the thread and the channel, so it is strictly simpler.
- **Widen the confirmation gate to `block_tree_depth`.** Would prevent the loop
  from ever crossing a boundary that a recovery could later invalidate, but parks
  honest mining for an entire cache window of run-ahead budget and still leaves the
  buffer poisoned for a recovery that *does* happen within the window. Treats the
  symptom, not the cause.
- **Do nothing.** The node wedges (rejects canonical blocks) until manually
  restarted — at which point startup `create_state` performs exactly this
  re-anchor. The decision is, in effect, to perform that same recovery
  automatically at the moment it is needed.

## Addendum: cold restart is a mechanism change, not a trust-boundary change (2026-06)

Whenever this design is revisited, a natural question recurs: would it not be simpler to
abandon the in-place re-anchor and instead *reinitialise* — restart the process, or tear down
and respawn the VDF thread — and let the ordinary startup path rebuild correct state? The
intuition is sound, yet the conclusion does not follow, and the reason is worth recording.
A restart changes the recovery *mechanism*, but it leaves the *trust boundary* exactly where
it was: that boundary is fork-local VDF verification before discard or adoption, and
everything genuinely expensive about this work lives there. A restart therefore relocates the
cost rather than removing it.

To see why the boundary is irreducible, we must recall what a partition recovery actually asks
of us. The recovering node has to follow a competing chain whose VDF lineage diverges from its
own past a reset boundary, and to do so safely it must verify that chain against *its own*
lineage before it either discards local state or adopts the fork. This is precisely what the
recompute and fork-local paths provide: `vdf_step_batch_is_valid` treats a live-buffer
mismatch as non-terminal and recomputes from the block's own seed while still rejecting forged
steps (`crates/vdf/src/state.rs`), and recall-range and previous-step validation resolve from
the block's own ancestry rather than the live buffer (`crates/actors/src/block_validation.rs`,
`crates/actors/src/validation_service.rs`,
`crates/actors/src/validation_service/block_validation_task.rs`). The two ways to dodge this
verification are both worse than the ailment: trusting an unverified header's cumulative
difficulty opens an eclipse vector, and discarding local state before verification risks
throwing a valid chain away at a griefer's request.

One might hope that building the restart approach afresh against `master` would sidestep the
layered complexity, but it would not, for master is precisely the state in which the
verification primitive is absent. On master, `vdf_step_batch_is_valid` rejects a buffer
mismatch outright — `warn_mismatches(...)` followed by `Err("VDF steps are invalid!")`, with
no recompute path (`crates/vdf/src/state.rs:416-419` on master). The deep-recovery call is
validation-gated: `on_block_validation_finished` returns early on an invalid or internal
result and reaches reorg handling only for a fully validated block, marking the tip solely on
that path (`crates/actors/src/block_tree_service.rs:642-718`, `:925-960` on master). And
cumulative difficulty is taken directly from the header field as a block is added
(`crates/domain/src/models/block_tree.rs:553-560` on master), so it cannot by itself serve as
a safe discard or restart trigger. In the boundary-crossing case master therefore wedges
*before* adoption ever occurs; it is the broken baseline, not a cheaper starting point, and it
is why even the *Do nothing* alternative above heals only once the verification primitive is
present to let the fork be adopted in the first place.

The clean design boundary, then, separates the one thing we must keep from the one thing we
may choose. We keep the recompute and fork-local verification primitive unconditionally. We
decide only the *post-adoption healing mechanism*, and here two options are genuinely on the
table: the VDF-loop re-anchor that ships today, which incurs no downtime at the price of more
local mechanism; or a controlled process restart after a validated deep recovery, which
carries less VDF mechanism but more operational downtime and resync risk. The decisive
observation is that the cold-restart variant is simpler only when it *begins* after fork-local
validation has already proven the competing chain. Were it to trigger any earlier, it would
have to either trust unverified headers or discard local state before the peer chain is known
to be valid — and we have just seen that both are strictly worse.

The accounting of what a restart sheds is not uniform across the layers, and the distinctions
are what make the trade legible.

| Restart's effect | Components |
| --- | --- |
| **Deletes** | the VDF re-anchor supervisor mechanism, `create_state_for_canonical_tip`, the `MiningBroadcastEvent::Reanchored` broadcast, and the asynchronous-window handling (the bounded-requeue drain heal and the absent-versus-stale previous-step path) |
| **Reduces, but does not delete** | the catch-up reset-seed selection (§2). A restart that holds mining paused while fast-forward fills the buffer avoids the *local-stepping* crossing, but the *fast-forward* path still needs the per-step canonical reset seed for its carried hash when a fast-forward step lands on a boundary (`crates/vdf/src/vdf.rs:184`, `:224`). Deleting the seed-selection logic wholesale would require a separate proof. |
| **Keeps** | the recompute and fork-local validation primitive, the startup anchor fold (`VdfAnchor::at_startup`, `crates/vdf/src/vdf.rs`), and the validation-gated trigger itself |
| **Adds** | the cost of restarting the embedded Reth, peer, and mempool subsystems, the re-fetch of the un-migrated canonical tail (lost on a process restart), and the attendant downtime and resync risk |

The reduction in the second row is the subtle one, and it is why the catch-up seed logic cannot
simply be struck out. On a cold catch-up the buffer is filled verbatim by fast-forward, so an
intermediate boundary's carried hash is overwritten by the next stored step and never read; the
single place the seed still matters is the hand-off where fast-forward yields to local stepping,
when the catch-up tip happens to land exactly on a boundary. Rare though that is, it is real,
and master's behaviour there — folding the tip's `next_seed` rather than the boundary's own seed
— is wrong.

We therefore do not pivot. The thread-level supervisor is already the lighter form of
reinitialisation: it restarts `run_vdf` while preserving the fast-forward channel and core
pinning (`crates/chain/src/chain.rs:2233`), whereas a full OS-thread teardown would additionally
have to replace the fast-forward channel (see *Strategy A1* under
[Alternatives considered](#alternatives-considered)). A controlled process restart remains a
legitimate fallback were the operational requirement ever to relax in favour of tolerating
downtime, but the choice between it and the in-place re-anchor is a downtime-versus-mechanism
trade. It is not, and cannot be, a way to avoid the consensus-critical verification that defines
the trust boundary.

## Consequences

- A recovering node converges to the canonical VDF lineage automatically instead
  of wedging until an operator restarts it.
- The recovery reuses `create_state` and the FF path — no new buffer-mutation
  primitive, no change to the seed-derivation or validation logic.
- Cost: the re-anchored loop discards already-computed (poisoned) steps and
  recomputes the catch-up to the live timeline. Those steps were invalid anyway, so
  no useful work is lost.

### Risks / edge cases

- **Signal ordering.** A `reanchor` sent before index truncation would re-anchor to
  the stale index. The signal must originate after `recover_from_network_partition`
  returns.
- **Repeated/at-startup recovery.** A second deep reorg before the first re-anchor
  completes must coalesce (the supervisor should re-read the latest anchor, not
  queue stale ones). Restarting mid-`run_vdf` is safe because the next iteration
  always rebuilds from the current index.
- **Validation in flight.** The buffer swap is atomic (the write lock makes a reader
  see either the old or new buffer), but the rewind itself is observable across a
  read→wait or wait→read gap, so two seams in `ensure_vdf_is_valid` are hardened:
  - `VdfStateReadonly::wait_for_step` treats *any* change in `global_step` — including
    the backward jump from a re-anchor — as liveness (resets the stall baseline/timer).
    The stall detector only fires on a genuinely *constant* `global_step` (a dead
    writer); `store_step` is forward-only, so a decrease is always a re-anchor and must
    not be mislabelled as a stall (which would `panic!` the node per the never-mislabel
    rule).
  - The post-wait `get_step(prev_output_step_number)` is a fast path that may read absent or
    stale during a rewind; either way the prev-step check falls through to the authoritative
    fork-local view (no re-wait). If that view is also unbuildable it surfaces
    `VdfPrevStepForkViewUnavailable`, which routes to `Cancelled` (peer-innocent requeue)
    rather than `panic!` or `Invalid`.
- **Rebuild failure.** `create_state_for_canonical_tip` runs on the live VDF supervisor
  thread, so it returns `eyre::Result` instead of unwrapping DB/header reads. On failure
  (transient DB error, or an ancestor missing from both the block-tree cache and the DB)
  the supervisor logs, keeps the current buffer/anchor, and resumes `run_vdf` from the
  existing timeline; `BlockTreeService` re-emits `vdf_reanchor` on the next recovery, so
  a transient failure self-heals on retry. A panic here would otherwise fire the thread's
  `CancelOnDrop` and shut the node down mid-recovery.
- **Atomic/buffer ordering.** `atomic_global_step_number` is stored to the rewound
  (lower) value *under the same write lock and before* the buffer is published, so the
  invariant `atomic <= buffer.global_step` holds at every instant (no reader can observe
  the atomic pointing past the buffer's newest step).
- **Lock poisoning.** `run_vdf` already exits gracefully on a poisoned VDF lock
  (`crates/vdf/src/vdf.rs:83`); the supervisor must treat that as full shutdown, not
  a re-anchor.
- **Fast-forward drain strands a paused follower (drain stall).** The re-anchor drains the
  *entire* fast-forward queue after rebuilding to the canonical tip `T` (to discard stale
  orphaned-fork steps that would re-poison — `be55d7796`). It therefore also drops any *legitimate
  canonical* fast-forward step for a block above `T` that was in flight at the drain instant (the
  recovery adopts the canonical fork incrementally, so such steps exist). A mining-paused follower
  never local-steps, so `store_step` gap-rejects the next step and the buffer is stranded at `T`;
  validation's `wait_for_step` then stalls. This is **not** unrecoverable (correcting the earlier
  "fast-forward is not re-supplied … never re-sent" reasoning, which described the rejected
  LCA-anchor): the stall is handled in the validation layer by a bounded, canonicality-gated
  requeue — the canonical block is re-validated, which re-sends the verified-canonical step (`run_vdf`
  consumes fast-forward even while paused), healing the buffer. A fork block, or a buffer frozen
  past the bound, still crashes per the never-mislabel rule. See
  `design/docs/vdf-validation-stall-detection.md` and `ValidationCoordinator::handle_stalled_vdf_task`.

## Testing

Implemented:

- **Unit (vdf), control path:** `reanchor_signal_returns_reanchor_exit`
  (`crates/vdf/src/vdf.rs`) drives the real `run_vdf` loop, fires a `vdf_reanchor`
  signal, and asserts the loop returns `VdfExit::Reanchor` (and exits for that reason,
  not shutdown).
- **Unit (vdf), buffer mechanics:** the existing
  `issue_1447_unconfirmed_reset_seed_poisons_buffer_and_wedges_block_validation`
  already proves that crossing a boundary on a loser vs winner seed diverges the
  buffer, and that `vdf_steps_are_valid` rejects a poisoned buffer / accepts a clean
  one. The re-anchor heals by rebuilding to the **canonical-tip** lineage.
- **Unit (vdf), fork-aware validation:** `vdf_step_batch_recomputes_on_buffer_mismatch`
  (`crates/vdf/src/state.rs`) proves a poisoned buffer ACCEPTS an honest competing-fork block via
  recompute-on-mismatch while a forged block is still rejected (§3, layer 1); plus
  `build_vdf_seed_buffer_propagates_fetch_error` and `wait_for_step_tolerates_backward_reanchor_jump`
  cover the fallible tip-rebuild and the backward-jump liveness handling.
- **Integration (chain):** `heavy4_slow_network_partition_recovery`
  (`crates/chain-tests/src/multi_node/partition_recovery.rs`) now exercises the
  re-anchor wiring end-to-end — recovery fires the signal, the supervisor rebuilds
  from the index and restarts the loop, and the node both adopts the peer's canonical
  chain and continues mining afterward (the "continued operation after recovery"
  stage), proving the re-anchor does not wedge a live node.

- **End-to-end, boundary-spanning (the headline test):**
  `heavy4_slow_partition_recovery_crosses_reset_boundary`
  (`crates/chain-tests/src/multi_node/partition_recovery.rs`) is the live two-node test where a
  fork **crosses a VDF reset boundary** before the deep reorg — so the recovering node's buffer
  is genuinely poisoned and the re-anchor must reproduce the canonical reset seed. Both nodes
  mine real forks past the poison boundary via natural mining; the peer's (longer) fork is
  gossiped to genesis, triggering the deep reorg → re-anchor. It asserts:

  - **(A) the heal** — genesis's VDF buffer over the boundary-crossing block's step range equals
    the canonical chain's recorded steps. Because the re-anchor is applied **asynchronously**, this
    is a **poll-until-converged** check (a bare `global_step >= last` wait would race the heal:
    genesis's own free-ran buffer already sits past that step *before* the re-anchor lands). A
    *persistent* mismatch after the full timeout is a genuine wedge.
  - **(B) no wedge / continued operation** — after confirming genesis's VDF advances past the
    recovered tip and settles (live, not wedged) and still holds the full adopted canonical chain,
    the **peer** (canonical-fork miner) produces the next block and genesis **follows** it. This is
    the production recovery path and sidesteps the [§4 residual mining race](#4-post-recovery-mining--reset-the-efficient-sampling-rotation)
    (the peer's rotation is intact; genesis only *validates* the peer's block against its healed
    buffer and fast-forwards). Deterministic and binding.

  Verified at **20/20** across the geometries natural mining produces (`peer_tip` 14 & 16, poison
  boundary steps ~300–386). A clean run is ~28–31s.

  This was previously shelved as "structurally infeasible," which turned out to be **wrong**
  — an artifact of the test harness, not the protocol. The earlier attempt funded but never
  **staked/pledged** the peer, so the peer had no partition assignment, no packed mineable
  data, and natural `mine_blocks` stalled; the fallback to forced capacity solutions
  (`mine_block_without_gossip`) never calls `start_mining()`, so the peer's VDF never
  free-ran and parked at the gated boundary (its confirmed step stuck at the LCA). The fix:
  **provision the peer as a real miner** (stake → pledge → epoch assignment → pack) and use
  **natural mining** — `mine_blocks` enables `start_mining()`, the VDF free-runs, partition
  mining finds real PoA solutions, blocks validate → `mark_tip` → `confirmed_canonical_step`
  advances → the #1449 gate releases, exactly as production crosses boundaries. The assigned
  partition is entropy-packed, so its PoA solutions are data-independent and the recovering
  node validates the canonical fork on reorg by recomputing entropy (no chunk-data sync).

The heal is therefore covered both end-to-end (above) and in composition by the unit tests:
the buffer divergence + validation reject/accept (`issue_1447_*`), the re-anchor trigger
(`reanchor_signal_returns_reanchor_exit`), the DB-free tip-rebuild
(`build_vdf_seed_buffer_reproduces_canonical_steps_anchored_at_tip`), and the live reorg →
re-anchor → rebuild → restart → continued-operation wiring (`heavy4_slow_network_partition_recovery`).
