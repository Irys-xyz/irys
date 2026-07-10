# VDF Reset-Seed Confirmation Gate

## Status

Accepted

## Context

The VDF step buffer is a single append-only sequence shared between two writers: the local stepping loop in `run_vdf`, which advances the clock ~1 step/sec, and the validation fast-forward path, which replays the steps of a block under validation into the same buffer. At every reset boundary — a global step number that is a multiple of `reset_frequency` — a fresh reset seed is folded into the chain, and that seed is not free to choose: it is pinned by the *rotation block*, the block sitting at step `boundary − reset_frequency`. The seed a node folds at a boundary is therefore only as trustworthy as the rotation block that pinned it.

Herein lies the danger that issue #1447 exposed. The local loop runs ahead of consensus by design, so when it reached a boundary it would fold whichever reset seed the *current canonical tip* advertised — even if that tip was a zero-confirmation block that a competing fork was about to displace. Once the loop had folded a minority seed, the poison was durable: because the buffer is append-only and block validation rejects any block whose steps disagree with the local buffer, the node would go on to reject the very canonical block whose (correct) post-boundary steps differed from its own poisoned ones. A single premature seed choice thus wedged block validation indefinitely. The concrete fork window from the issue makes this vivid: with `reset_frequency = 8` and the boundary at step 16, the reset seed is pinned by the rotation block at step 8; that block could sit freshly at the canonical tip with zero confirmations while the confirmed chain still trailed at step 6, and the loop would cross regardless.

We needed a rule that let the loop keep its run-ahead head room for honest mining, yet never allowed it to commit to a reset seed that a reorg could still revoke.

## Decision

### Gate the crossing on the confirmed step, not the canonical tip

The loop consults `is_reset_boundary_blocked` (`crates/vdf/src/vdf.rs`) before it is permitted to cross the upcoming boundary:

```rust
next_global_step.is_multiple_of(reset_frequency)
    && next_global_step > confirmed_global_step_number.saturating_add(reset_frequency)
```

The pivotal argument is `confirmed_global_step_number`: the step number of the deepest block that has been confirmed past the migration threshold (`block_migration_depth` deep), as reported by the canonical snapshot the loop reads each iteration. The loop may cross boundary `B` only once the confirmed chain has itself reached the rotation point `B − reset_frequency`. Crossing then implies the rotation block is at least `block_migration_depth` blocks deep, and so beyond reorg — which is precisely the guarantee that a folded seed can never be revoked. Returning to the #1447 window, feeding the confirmed step (6) rather than the canonical tip (8) flips the gate's verdict from *cross* to *block*, and the loop parks until the rotation block is confirmed. The predicate is identical; only the argument changes, and that swap is the whole fix.

Gating on the confirmed step, rather than reintroducing the tip-readiness check the old rule used, folds readiness and confirmation into one comparison and is inherently reorg- and startup-safe. Since `confirmed_global_step_number` only advances as blocks migrate, it cannot be fooled by a fork-loser block briefly at the tip, by a seed re-pinned across a reorg, or by a freshly started node's tip. The policy fails closed: we always gate on the confirmed step and never on the still-forkable canonical tip. The price is a run-ahead budget equal to the confirmation lag — `canonical_step − confirmed_step`, roughly `block_migration_depth` blocks — which is negligible against any production reset window, so honest mining keeps its head room while the confirmation guarantee holds unconditionally.

### A configuration floor keeps honest mining from wedging on its own gate

The gate parks the loop at a boundary until the rotation block confirms; if a reset window were shorter than the confirmation lag, the gate would reopen only *after* the next boundary was already due, and honest mining would wedge at every boundary. `Config::validate` (`crates/types/src/config/mod.rs`) therefore rejects any configuration where

```
reset_frequency < 2 × (block_migration_depth × block_time)
```

Clocked at ~1 step/sec, a block spans about `block_time` steps, so the confirmation lag is `block_migration_depth` blocks ≈ `block_migration_depth × block_time` steps; we require twice that for headroom. This floor is necessary but, as the recovery testing bore out, not on its own sufficient for a pathologically small window during bootstrap — it is the standing invariant that keeps a correctly-configured network clear of the gate under steady state.

### The fast-forward path is deliberately left ungated

Only the local stepping loop can run ahead of consensus, and so only it can commit to an unconfirmed seed. The fast-forward path replays the steps of a block already under validation verbatim; it never runs the loop ahead, and it cannot reproduce the #1447 run-ahead bug. Accordingly the confirmation gate does not apply on that path — the snapshot's `confirmed_global_step_number` is discarded there — and a fast-forwarded block simply extends the append-only buffer with the steps it carries.

### The residual deep-reorg concern and how each depth band is handled

Leaving the fast-forward path ungated surfaces one residual concern. The fast-forward path appends the steps of whichever block is validated first, so a competing fork whose post-boundary steps differ could — if its blocks are validated ahead of the canonical ones — write minority steps into the buffer. For the forks' steps to differ at all, the fork must be deep enough to re-pin a boundary's reset seed: it must reach back past the rotation block at `boundary − reset_frequency`, on the order of a whole reset window. By the configuration floor above, a reset window is at least twice the confirmation lag, so such a fork is far deeper than `block_migration_depth`.

What happens to a fork then depends on how deep it is, and only the deepest band is refused outright:

- **Within `block_migration_depth` — no seed can be re-pinned.** The confirmation gate guarantees every locally folded seed is pinned by a rotation block at least `block_migration_depth` deep, so no reorg in this band can revoke one; the forks' steps cannot differ.
- **Deeper than `block_migration_depth`, within the block-tree window — adopted, and the poisoned buffer healed.** Such a reorg is a network partition recovery: the node rolls back the migrated minority-fork blocks (`recover_from_network_partition`, `crates/actors/src/block_migration_service.rs`) and adopts the canonical fork. Nothing pins the block-tree window below a reset window, so a fork in this band genuinely can re-pin a reset seed, and by the time it is adopted the local buffer may already hold minority post-boundary steps. The system tolerates that rather than trusting the buffer: validation is block-rooted, not buffer-rooted — `prev_output_is_valid` and `vdf_step_continuity_is_valid` anchor a block's steps to its parent in mandatory prevalidation, and recall-range validation falls back to a fork-local recompute from canonical ancestry whenever the local buffer disagrees — and the divergent-boundary gate re-anchors the buffer itself in process (see *Two Homes* below).
- **Beyond the block-tree window — refused.** p2p block-pool admission (`PartOfAPrunedFork`, `crates/p2p/src/block_status_provider.rs`) rejects a block arriving at an already-indexed height with a different hash more than `max_reorg_depth` (the `block_tree_depth`) behind the latest indexed height, and the block tree aborts, as a controlled shutdown, any reorg whose fork point predates its post-prune cache window (`validate_reorg_within_cache_window`, `crates/actors/src/block_tree_service.rs`). Together these bound admissible reorg depth to `block_tree_depth`.

A poisoning fork is therefore either too shallow to poison, adopted with its poisoning healed, or turned away at the door — which is what allows the fast-forward path to remain ungated.

## Consequences

- The local loop never folds an unconfirmed reset seed; the confirmation gate and the configuration floor together guarantee it, and the guarantee holds across reorgs and node restarts without any per-seed bookkeeping.
- Honest mining retains a run-ahead budget equal to the confirmation lag. The gate bites only at a boundary, and only when the confirmed chain is still catching up to the rotation point — a transient that a healthy network clears within `block_migration_depth` blocks.
- The fast-forward path's safety is not intrinsic; it rests on the admission bound at `block_tree_depth` and, inside that bound, on validation staying block-rooted plus the deep-reorg heal (the fork-local recompute and the in-process re-anchor). Any change that admitted a fork deeper than the block-tree cache window, or that reinstated the local buffer as a rejection authority in validation, would reopen the residual concern. This coupling is worth stating plainly so a future reader does not relax one bound in isolation.
- The mechanism is covered deterministically by the unit tests `gating_on_confirmed_step_not_canonical_tip_is_the_1447_fix` (the gate's verdict on the exact #1447 window) and `issue_1447_unconfirmed_reset_seed_poisons_buffer_and_wedges_block_validation` (the wedge the old rule caused), both in `crates/vdf/src/vdf.rs`.

## Two Homes for Fork-Related VDF Safety

This gate is the *shallow-reorg* half of fork-related VDF correctness: it prevents the local loop from ever folding a seed that a reorg inside the migration window could revoke. The *deep-reorg* half operates after the fact — once a partition-recovery reorg deeper than `block_migration_depth` has been adopted, the block tree's divergent-boundary gate (`maybe_reanchor_vdf_after_reorg`, `crates/actors/src/block_tree_service.rs`) decides whether the already-poisoned buffer must be healed, using the pure predicates `irys_vdf::first_divergent_boundary` and `irys_vdf::reorg_crossed_divergent_boundary`, and ships a `ReanchorRequest` that `run_vdf` applies in place. The two mechanisms guard different windows with different information — this gate needs only the confirmed step the loop already reads, while the re-anchor needs the reorg event that only the block tree observes synchronously — which is why they live apart. The division of labour is deliberate: the block tree owns *when* a heal is considered; `irys-vdf` owns *whether* the reorg crossed a divergent boundary and *what* the corrected buffer contains.

## Source

Issue #1447; branch `jason/vdf_original` (PR #1474, descending from the re-anchor work in PR #1466).
