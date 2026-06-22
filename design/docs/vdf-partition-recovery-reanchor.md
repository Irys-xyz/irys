# VDF Re-anchor on Network-Partition Recovery

## Status

Accepted — implemented. The VDF thread is a supervisor loop (`init_vdf_thread`,
`crates/chain/src/chain.rs`); `run_vdf` returns `VdfExit` and is restarted on
`VdfExit::Reanchor`; `BlockTreeService` sends the `vdf_reanchor` signal after
`recover_from_network_partition`; `create_state` borrows the index and returns the
anchor's `next_seed`.

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
  drops back to the LCA, so `is_reset_boundary_blocked` naturally re-gates every
  forward boundary crossing against the (now canonical) confirmed step.

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
- **Validation in flight.** Any `vdf_steps_are_valid` call reading the buffer during
  the swap must see either the old or new buffer atomically — guaranteed by holding
  the write lock for the assignment.
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
  one. The re-anchor heals by rebuilding to a clean (winner/LCA) lineage.
- **Integration (chain):** `heavy4_network_partition_recovery`
  (`crates/chain-tests/src/multi_node/partition_recovery.rs`) now exercises the
  re-anchor wiring end-to-end — recovery fires the signal, the supervisor rebuilds
  from the index and restarts the loop, and the node both adopts the peer's canonical
  chain and continues mining afterward (the "continued operation after recovery"
  stage), proving the re-anchor does not wedge a live node.

Attempted and shelved — a boundary-spanning multi-node test:

A live two-node test where the minority fork crosses a reset boundary before
deep-reorging was prototyped and abandoned as structurally infeasible within the test
suite's time budget (nextest terminates at 60s default / 120s for the `slow_` bucket).
The obstacle is the #1449 confirmation gate itself, which is designed for large
production reset windows:

- **Low `reset_frequency` wedges at startup.** The first block consumes ~45-76 VDF steps
  of run-up; if `2 × reset_frequency` (the gate's park point) is below that, the VDF
  parks before a second block can be produced, so no block migrates, `confirmed` never
  advances, and the gate never releases. Measured: `reset_frequency = 24` deadlocks at
  genesis; `reset_frequency = 60` is the rough floor.
- **The single-block test miner can't cross a boundary.** `IrysNodeTest::mine_block`
  (via `solution_context`) targets one future VDF step and waits; when the VDF parks at a
  boundary it advances its target past the buffer and panics, because releasing the gate
  requires producing *other* blocks first — a dependency one `mine_block` call cannot
  satisfy.
- **Continuous mining works but is too slow.** Switching to `mine_blocks_without_gossip`
  (real continuous mining) crosses boundaries correctly — exactly as production does — but
  the gate serializes each crossing behind migration confirmation, adding real latency per
  boundary. Crossing ~2 boundaries on each of two forks plus the deep reorg ran ~190s,
  well over the 120s ceiling.

The heal is nonetheless covered in composition by the three tests above: the buffer
divergence + validation reject/accept (`issue_1447_*`), the re-anchor trigger
(`reanchor_signal_returns_reanchor_exit`), and the live reorg → re-anchor → rebuild →
restart → continued-operation wiring (`heavy4_network_partition_recovery`). A dedicated
boundary-spanning test would need either a test-only fast path that crosses boundaries
without the migration-confirmation latency, or acceptance as a long-running
(non-default-suite) test.
