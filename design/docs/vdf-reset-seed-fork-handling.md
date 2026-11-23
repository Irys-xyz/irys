# VDF Reset-Seed Fork Handling

## Status

Accepted. Replaces the reset-seed confirmation gate.

## Context

The VDF step buffer is one sequence with two writers: the local stepping loop in `run_vdf`, which advances ~1 step/sec, and the validation fast-forward path, which writes the steps of a validated block when they are ahead of the loop. At every reset boundary `B` (a step number that is a multiple of `reset_frequency`) the loop folds a reset seed into the running hash before it computes step `B + 1`. Validation decides which seed is correct: a block folds its own `vdf_limiter_info.seed` at every boundary `b` with `b + 1` in its step range, and `VDFLimiterInfo::calculate_seeds` sets that seed from the parent — the parent's `next_seed` when the block's range holds a reset step, else the parent's `seed`.

So the correct seed for a boundary is a property of the canonical chain, and the chain can change after the loop has folded it:

- A reorg can replace the block whose hash pins the seed, so the loop may have folded a fork-loser seed.
- When the loop runs past the tip, no block covers the boundary yet. The loop folds the seed a child of the tip would carry. A block that arrives later and ends between two boundaries makes the next boundary's seed its own `next_seed`, not the old tip's.
- When the tip ends exactly on a boundary `b`, a child that also holds the next boundary carries the tip's `next_seed`, and it folds that seed at `b` too. A shorter child folds the tip's `seed` at `b`. The seed at `b` therefore depends on how far the next block reaches.

The earlier fix parked the loop at `B` until the block `block_migration_depth` deep had reached `B - reset_frequency`. That made every folded seed final, but it tied VDF progress to block arrival: when blocks became slower than the reset window divided by the migration depth (mainnet, after a hashrate drop), the loop parked, no new steps were mined, no block could confirm, and the network halted. Liveness must not depend on block arrival.

## Decision

The loop never waits for a block. It records which seed it folded at each boundary, keeps checking those seeds against the canonical chain, and rewrites the steps after any boundary whose seed changed: it copies the steps canonical blocks hold and recomputes only free-run steps past the canonical tip, which no block holds yet. A fork costs at most that recompute, never a halt.

### Seed-tagged buffer

`VdfState` keeps `reset_seed_by_boundary`: for every reset boundary from the step before the oldest buffered step up to the current step, the seed folded there to compute the next buffered step. Every writer records it:

- Local stepping and fast-forward folds record the seed they apply (`store_step` in `crates/vdf/src/vdf.rs`).
- A fast-forwarded step also records the validated block's `seed` for the boundary just before it (`VdfStep::reset_seed`), because that block computed the step.

Fast-forward stores a validated step only when the buffered step before it is the block's own predecessor (`VdfStep::predecessor`). A block on another lineage, such as another fork or the canonical chain while this buffer still holds a stale seed, never joins the buffer. So every buffered step is the VDF of the one before it under the recorded seed, which the validation fast paths rely on. If the buffer is the stale side, the reset-seed check below rewrites it in the same loop iteration, and fast-forward continues from there. A refused step is still owed to whoever waits for it (validation waits for the counter to reach a block's last step), so the loop computes up to the highest refused step on its own lineage, even while mining is paused. The same holds for a step past the next one (a gap): validation sends a block's steps in batches, so the later batches of a refused block arrive as gaps. A node that only advances by fast-forward therefore never stops on a refused block; it reaches the block's last step at its own VDF speed instead of at once.
- Bootstrap (`crates/chain/src/vdf_seed_source.rs`) and the block-tree re-anchor take the seeds from the block headers (`irys_vdf::reset_seeds_folded_by_block`), and the anchor fold at a bootstrap tip that ends on a boundary records the tip's own `seed`.

A seed identifies the block it came from (it is a block hash), so the buffer does not store the block separately.

### Canonical seed of a boundary

`irys_vdf::canonical_reset_seed` mirrors validation, from one canonical snapshot (`CanonicalVdfSnapshot`, which carries the confirmed block's last output and the step range and `seed` of every canonical block above it):

- If a canonical block holds step `b + 1`, its `seed` is the canonical seed of `b`.
- Otherwise `b` is at or above the tip, and the seed is the one a child of the tip reaching the live step would carry (`irys_vdf::tip_child_reset_seed`): the tip's `next_seed` when that child's range holds a reset step, else the tip's `seed`.

The loop uses this rule both to choose the seed it folds and to check the seeds it already folded, so the two can never disagree.

### Free-run crosses every boundary

There is no reset-boundary gate. Free-run folds the canonical seed of each boundary as it reaches it, including boundaries above the tip. When the tip does not move for many reset windows, the loop keeps stepping on the child-of-tip seed; this is also what an honest miner's next block will carry, because that block spans every step since the tip.

### The loop re-checks its seeds and rewrites the tail

Every iteration, unless a block-tree re-anchor is pending, the loop first compares the buffered step at `confirmed_global_step_number` with `confirmed_output`. A mismatch marks the buffer suspect, even if the wrong fold has fallen below the seed-check window. A pruned or not-yet-reached confirmed step is ignored.

The loop then compares the recorded seed of each buffered boundary from the confirmed step up to the live step with the canonical seed (`first_non_canonical_reset_seed`). Boundaries at or below the canonical tip are always checked. Boundaries above the tip are checked only while local VDF mining is enabled: with mining off, those steps come from fast-forward or catch-up, and replacing them with a child-of-tip guess would cause needless rewrites during sync. The loop follows only the canonical tip and stores no non-canonical block provenance. Turning mining on checks the full window on the next iteration, so mining uses steps aligned with the canonical parent. Validation still trusts the buffer only when it matches a block exactly. A missing seed record counts as a mismatch. At the first boundary that differs it rewrites the steps from that boundary to the live step in place (`VdfState::rewrite_tail`); the step counter does not move. It reads the canonical blocks' steps (`BlockProvider::canonical_vdf_snapshot_with_steps`) and copies the steps they hold, when their output at the boundary matches the buffered step (same lineage up to there). When the boundary equals the confirmed step, `confirmed_output` supplies that anchor; copying starts at boundary + 1. Only the steps past the canonical tip, which free-run computed and no block holds yet, are recomputed with canonical seeds, starting from the last copied step. If the canonical blocks hold the step at the boundary and it is not the buffered one, the buffer left the canonical lineage below the boundary, where this check cannot see; the loop then marks the buffer suspect, and the block-tree re-anchor rewrites it from the canonical blocks. While it rewrites, the step counter stops; the loop sets `ReanchorSignals::is_recompute_in_progress`, and validation's VDF waits hold their stall timer open while it is set, as they do while the buffer is suspect, so a long recompute is not read as a dead VDF thread. When steps were rewritten it bumps the re-anchor generation and tells miners to rebuild their recall ranges, as a block-tree re-anchor does; it leaves the suspect flag alone, since that belongs to a pending block-tree re-anchor. If only the fold at the live step changed, it re-folds the running hash and rewrites nothing.

This covers a reorg within the migration depth (including a fork at the block that pins the seed, with the loop already past the boundary), a late block after a free-run guess, and the boundary whose seed depends on the next block's reach. The recompute covers only free-run steps past the canonical tip. With normal block times and the configuration floor `reset_frequency >= 2 × block_migration_depth × block_time` (`Config::validate`), a reorg within the migration depth cannot change a seed the loop already folded; that needs a chain much slower than `block_time`.

### Deep reorgs keep the block-tree re-anchor

A reorg deeper than `block_migration_depth` can change seeds below the confirmed step, outside the loop's seed-check window. The block tree keeps handling those (`maybe_reanchor_vdf_after_reorg` in `crates/actors/src/block_tree_service.rs`, with `irys_vdf::first_divergent_boundary` / `reorg_crossed_divergent_boundary`): it sends a `ReanchorRequest` with the canonical steps and their reset seeds from the block headers, and `apply_reanchor` rewrites the buffer and recomputes the free-run tail with the child-of-tip rule. The re-anchor no longer skips a tail that crosses a second boundary past the tip: like free-run, it must not wait for a block.

The VDF also marks the buffer suspect when the confirmed output or a rewrite anchor disagrees. `ReanchorSignals::wait_for_suspect` wakes the block tree immediately to publish a request for the current canonical tip, using the watchdog's publish path and tip/live-step deduplication. The wait tracks the handled generation so a persistent flag cannot spin the service, and a completed heal permits a new request even with identical inputs. While the VDF applies a queued request it sets the rewrite-in-progress flag before re-marking suspect, so consuming that request cannot trigger another heal. A skipped request clears the in-progress flag and allows the listener to request usable inputs. The watchdog remains a backstop for skipped requests with changed inputs. A skipped request leaves the buffer suspect and local stepping paused; a later tip does not by itself guarantee a usable anchor.

### Validation trusts the buffer only for the same lineage

The VDF step fast path (`vdf_step_batch_is_valid`) and the recall-range fast path (`recall_recall_range_is_valid`) use `VdfState::holds_block_steps`. The buffer counts for a block only when its step before the block's steps equals the block's predecessor, its steps equal the block's, and every boundary the block folds was recorded with the block's `seed`. Each buffered step is the VDF of the one before it under the recorded seed, so these checks prove the block's steps without recomputing them. Any other buffer state, such as another fork's seed or a free-run guess on an older tip, makes validation recompute from the block's own data. A node with a stale buffer therefore rejects exactly the blocks a clean node rejects, including blocks it produced from its own stale steps.

## Consequences

- VDF progress never depends on block arrival. The deadlock that the confirmation gate caused at slow block times cannot occur.
- A seed change costs a recompute of the free-run steps past the canonical tip that were computed on the old seed, at ~1 step/sec. Local mining pauses during the recompute. Between recomputed steps, the loop drains fast-forward input: it drops already-buffered steps and stale generations, and defers future steps to the normal drain after the rewrite. Validation's fast-forward stage watchdog excludes rewrite time. Every node that sees the same canonical change recomputes at about the same time, so network-wide mining pauses for that long. When the canonical tip ends exactly on a boundary `b` and no block arrives for a whole reset window, every node recomputes `(b, b + rf]` when its live step reaches `b + rf`: about 10 minutes at `rf = 600` and ~1 step/sec. If a short child found before `b + rf` arrives during that recompute, the seed at `b` flips back and the nodes recompute again. The rough frequency is `(1 / steps per block) × e^(-rf / steps per block)` per boundary, so this matters only on a very slow chain. The cause is in consensus: the seed folded at `b` depends on how far the child reaches. Removing this cost needs a hardfork.
- Validation correctness does not depend on the buffer: the fast paths accept only what a recompute would accept.
- A node with free-run disabled still computes VDF steps locally when fast-forward refuses a block's steps (at most up to that block's last step). A node that is behind therefore validates a block from another lineage (a fork loser, or the canonical chain during a deep reorg until the block-tree re-anchor applies) at its own VDF speed, about 1 step/sec, instead of at once. Nodes whose free-run is ahead are not affected: they already hold those step numbers.
- Covered by the unit tests in `crates/vdf/src/vdf.rs` (`free_run_keeps_stepping_while_tip_is_stalled`, `late_block_after_free_run_guess_recomputes_the_tail`, `crosses_on_fork_loser_seed_then_recomputes_when_fork_resolves`, `shallow_fork_at_rotation_block_after_loop_crossed_the_boundary`, `heals_when_tail_crosses_several_boundaries`, `fork_switch_copies_steps_canonical_blocks_hold`, `fast_forward_refuses_a_step_off_the_buffered_lineage`), `crates/vdf/src/state.rs` (`fast_path_requires_the_blocks_reset_seed`, `fast_path_requires_recorded_seed_and_predecessor`) and `crates/vdf/src/vdf_utils.rs` (the seed-rule tests).

## Source

Replaces the reset-seed confirmation gate, which halted a network whose blocks became slower than the reset window allowed.
