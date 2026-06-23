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

### 2. The catch-up reset-seed hole — and `canonical_vdf_info_at_or_below_step`

The tip re-anchor is correct **only if it anchors at, or above, every reset boundary the
recovered range crosses.** But a partition recovery adopts the canonical fork **incrementally**
(block-by-block over gossip), so the *first* deep reorg can fire while the canonical tip is still
**below** a poison boundary. The re-anchor correctly anchors at the canonical tip *of that
instant* — below the boundary — and the VDF then **local-steps the catch-up** forward across the
boundary on its own, before the canonical blocks that pin that boundary's seed have been adopted.

At the crossing, `run_vdf` applied `next_reset_seed = latest_canonical_vdf_info().next_seed` — the
**tip's** next_seed. Once the tip advances *past* the boundary (the rest of the fork arrives), that
field is the seed for the next, *higher* boundary, not the one being crossed. The wrong seed is
XOR'd in and the post-boundary steps are re-poisoned — and, because `store_step` is forward-only,
the canonical steps that arrive afterward can never overwrite them. The heal silently fails for
that range (~1 in 15–20 boundary-crossing recoveries; the deterministic VDF would otherwise
reproduce canonical exactly with the right seed).

**Fix:** source the reset seed from the canonical block **ending at or before the VDF's current
step**, not the tip. That block's `next_seed` pins the seed for the next boundary *above the
current step* — exactly the boundary the VDF is about to cross. Plumbing:
`BlockProvider::canonical_vdf_info_at_or_below_step` (`crates/types/src/block_provider.rs`,
default `None`) → `BlockTree::canonical_entry_at_or_below_step`
(`crates/domain/src/models/block_tree.rs`, an in-place binary search by `global_step_number` over
the canonical-chain cache) → overridden in `BlockStatusProvider` (`crates/p2p/...`). `run_vdf`'s
local-stepping read uses it: when the VDF runs *ahead* of the tip the query returns the tip itself
(the greatest `global_step_number <= S` is the tip), yielding the tip's `next_seed` — equal to the
prior behavior, a no-op. The `unwrap_or` fallback to the tip's `next_seed` is a defensive default
for the distinct edge case where *no* canonical block ends at/before the step (the step is below the
earliest cached block in the bounded canonical-chain cache).

Two subtleties this resolves:

- **Boundary-spanning blocks.** A single block can straddle a reset boundary; its `next_seed`
  targets the boundary *above* its range. So the query is the block *ending at or before* the step
  (greatest `global_step_number <= S`), **not** the block whose range *covers* the step (which may
  be the spanning block, yielding the wrong seed). This off-by-one — covering vs ending-before —
  was a real bug in the first cut of this fix, caught by the boundary-crossing test.
- **The fast-forward path is unchanged.** FF stores canonical step values **verbatim**
  (`store_step` runs *before* `process_reset`), so an FF'd buffer is canonical regardless of
  `next_reset_seed`. Only local-stepping can poison the buffer, so the fix is local-path-only.

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
- **Recall-range validation — fork-local view.** On a recall `Mismatch` against the live buffer,
  rebuild a fork-local step view from the block's **own** lineage (its ancestors via the block
  tree) and re-validate against that: `build_fork_local_recall_view`
  (`crates/actors/src/block_validation.rs`).
- **Previous-step continuity — fork-local view.** The `ensure_vdf_is_valid` prev-step equality
  check (`crates/actors/src/validation_service.rs`) previously handled only an *absent* previous
  step (requeue via `VdfStepRewound`); a *present-but-stale* step — the poisoned boundary value
  during the window — fell through to terminal `Invalid`. On mismatch it now resolves the previous
  step from a fork-local view sized to include it (`build_fork_local_step_view`, which covers the
  prev step even for a boundary-crossing block) and accepts iff that matches. A value that
  mismatches the block's own canonical ancestry is still rejected.

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
  - The post-wait `get_step(prev_output_step_number)` re-waits once if the step is
    transiently absent (rewound after the wait returned); if still absent it surfaces
    the typed `VdfStepRewound` sentinel, which routes to `Cancelled` (peer-innocent
    requeue) rather than `panic!` or `Invalid`.
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
- **Integration (chain):** `heavy4_network_partition_recovery`
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
re-anchor → rebuild → restart → continued-operation wiring (`heavy4_network_partition_recovery`).
