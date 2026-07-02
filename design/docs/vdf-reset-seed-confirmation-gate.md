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

### The residual deep-reorg concern, the admission guards, and the invariant relied upon

Leaving the fast-forward path ungated surfaces one residual, and purely theoretical, concern. The invariant we depend upon is that the step buffer is a single append-only sequence and validation rejects any block whose steps disagree with it. A competing fork whose post-boundary steps differed could, in principle, be validated (and thus fast-forwarded) first and so cause the canonical block to be rejected. Reaching that state, however, requires a reorg deep enough to re-pin the boundary's reset seed — that is, a reorg reaching back past step `boundary − reset_frequency`, on the order of a whole reset window. By the configuration floor above, a reset window is at least twice the confirmation lag, so such a reorg is far deeper than `block_migration_depth`.

Two admission guards refuse a fork of that depth long before any of its blocks reach validation or fast-forward, which is what allows the fast-forward path to remain ungated:

- **p2p block-pool admission — `PartOfAPrunedFork`** (`crates/p2p/src/block_status_provider.rs`). A block arriving at an already-indexed height but with a different hash, whose height is more than `max_reorg_depth` behind the latest indexed height, is rejected outright as a pruned fork. `max_reorg_depth` is the `block_tree_depth`; a block within that range is instead left to the block tree to adjudicate.
- **Block-tree reorg bound — `validate_reorg_within_migration_depth`** (`crates/actors/src/block_tree_service.rs`). A reorg whose old fork is deeper than `block_migration_depth` would revert an already-migrated block, which the fork-choice-update and downstream migration path cannot do safely; the caller treats this as a controlled-shutdown trigger rather than attempting the reorg.

Between them these guards bound admissible reorg depth to `block_migration_depth`, comfortably inside the reset window that a seed-repinning reorg would have to exceed. The poisoning fork is therefore turned away at the door, and the fast-forward path needs no gate of its own.

## Consequences

- The local loop never folds an unconfirmed reset seed; the confirmation gate and the configuration floor together guarantee it, and the guarantee holds across reorgs and node restarts without any per-seed bookkeeping.
- Honest mining retains a run-ahead budget equal to the confirmation lag. The gate bites only at a boundary, and only when the confirmed chain is still catching up to the rotation point — a transient that a healthy network clears within `block_migration_depth` blocks.
- The fast-forward path's safety is not intrinsic; it rests on the two admission guards. Any change that loosened either — admitting a reorg deeper than `block_migration_depth`, or widening `max_reorg_depth` past the reset window — would reopen the residual concern, and the reset window would then need to grow in step. This coupling is worth stating plainly so a future reader does not relax one bound in isolation.
- The mechanism is covered deterministically by the unit tests `gating_on_confirmed_step_not_canonical_tip_is_the_1447_fix` (the gate's verdict on the exact #1447 window) and `issue_1447_unconfirmed_reset_seed_poisons_buffer_and_wedges_block_validation` (the wedge the old rule caused), both in `crates/vdf/src/vdf.rs`.

## Source

Issue #1447; branch `jason/centralise_vdf_reanchor_fix`.
