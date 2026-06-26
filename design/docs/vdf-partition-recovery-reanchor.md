# VDF Recovery on Network Partition (Controlled Restart)

> The filename retains `reanchor` for history. The **shipped mechanism is a controlled process
> restart**, not an in-place VDF re-anchor. An earlier iteration — an in-place supervisor-loop
> re-anchor — was implemented and then superseded; it is recorded under
> [Alternatives considered](#alternatives-considered).

## Status

Accepted — implemented and verified. On a network-partition recovery whose recovered range crosses
a VDF reset boundary (the only case that poisons VDF state), the recovering node requests a
**controlled process restart**: `BlockTreeService` cancels `partition_recovery_restart`, the node
lifecycle maps that to `ShutdownReason::PartitionRecoveryRestart`, and the process exits with a
distinct code (`EX_TEMPFAIL`, 75) so a supervisor relaunches it. On relaunch the ordinary cold-start
path rebuilds correct VDF state from the truncated index and the node resyncs the canonical chain
from trusted peers.

This **supersedes** the in-place VDF re-anchor (a supervisor loop that rebuilt `VdfState` from the
canonical tip without restarting the process). The trade is deliberate: we accept a brief recovery
downtime and a deployment restart-policy dependency in exchange for deleting a body of
consensus-critical mechanism — the supervisor loop, the `run_vdf` re-anchor return path, the
`create_state_for_canonical_tip` tip rebuild, and the mining-rotation re-anchor broadcast. Crucially
this is a change of **mechanism only**: the trust boundary — fork-local VDF verification before
adoption — is untouched, because it is irreducible (see
[Preserved trust boundary](#preserved-trust-boundary-fork-local-verification)).

Mechanism summary (each step links to the section below):

1. `BlockTreeService` detects the deep reorg (orphaned fork deeper than `block_migration_depth`) and
   runs `recover_from_network_partition` (index truncation + storage/supply rollback) on **every**
   such reorg.
2. **Only if** the recovered range crossed a divergent-seed reset boundary does it cancel
   `partition_recovery_restart` (`crates/actors/src/block_tree_service.rs`) — see
   [Gating](#gating-restart-only-when-the-buffer-is-actually-poisoned).
3. The lifecycle `select!` maps that token to `ShutdownReason::PartitionRecoveryRestart`
   (`crates/chain/src/chain.rs`); `main` converts it to exit code 75
   (`crates/types/src/shutdown.rs`, `crates/chain/src/main.rs`).
4. On relaunch, `create_state` rebuilds the VDF buffer over the truncated (LCA-rooted) index and the
   trusted-peer fast-forward path fills it forward (`crates/vdf/src/state.rs`, `crates/vdf/src/vdf.rs`).

Verified by `heavy4_slow_partition_recovery_crosses_reset_boundary_restarts` (boundary-crossing →
restart → cold-sync heal) and `heavy4_slow_network_partition_recovery` (non-boundary deep reorg → no
restart, in-place continued operation) in
`crates/chain-tests/src/multi_node/partition_recovery.rs`.

## Context

When a node is isolated during a network partition, mines its own minority fork, and later
reconnects, it performs a **deep reorg** onto the canonical chain. This is the "network partition
recovery" path: triggered in `crates/actors/src/block_tree_service.rs` when the orphaned fork is
strictly deeper than `block_migration_depth`, which calls
`BlockMigrationService::recover_from_network_partition`
(`crates/actors/src/block_migration_service.rs`). That routine truncates the block index, unassigns
storage-module offsets, and rolls back supply state to the fork-parent height.

**It does not touch VDF state.** This is the gap.

### How the reset seed becomes poisoned

At every VDF reset boundary (`global_step % reset_frequency == 0`, `crates/vdf/src/vdf.rs`) the loop
folds in `next_reset_seed` via `apply_reset_seed` (`crates/vdf/src/lib.rs`). That seed is pinned by
the *rotation block* at `boundary − reset_frequency`: `next_seed = parent_header.block_hash`
(`crates/types/src/block.rs`). A different fork has a different rotation-block hash, so it has a
different reset seed, so its VDF lineage **diverges** for every step after that boundary.

If the partition lasted long enough for the minority fork to cross a reset boundary, the recovering
node holds VDF steps — both in the running loop's `hash`/`global_step` locals and in the shared
`VdfState` buffer (`crates/vdf/src/state.rs`) — that were computed with the **minority** reset seed.
This is the exact poisoned-buffer wedge reproduced by
`issue_1447_unconfirmed_reset_seed_poisons_buffer_and_wedges_block_validation`
(`crates/vdf/src/vdf.rs`): `vdf_steps_are_valid` rejects the canonical block, and a node that trusted
its live buffer could not follow the chain it just reorged onto.

### Why the #1449 confirmation gate does not cover this

The reset-boundary confirmation gate (`is_reset_boundary_blocked`, `crates/vdf/src/vdf.rs`) prevents
the loop from crossing a boundary until the *confirmed* chain (`block_migration_depth` deep) reaches
the rotation point. That makes the loop safe for reorgs **up to `block_migration_depth`**. Partition
recovery is, by definition, the one sanctioned path that reorgs *past* that depth: on the minority
fork the rotation block was confirmed (migrated) locally, so the gate correctly let the loop cross —
using the minority seed.

### Why nothing self-heals without intervention

- **The forward seed already updates.** The loop re-reads the canonical snapshot
  (`canonical_vdf_snapshot`) every iteration, so `next_reset_seed` flips to the canonical value
  immediately. **This is necessary but not sufficient:** the `hash` fed *into* the next boundary is
  still on the poisoned lineage, so `apply_reset_seed(poisoned_hash, canonical_seed)` still diverges.
- **The buffer cannot rewind.** `VdfState::store_step` (`crates/vdf/src/state.rs`) is strictly
  forward-only and gap-rejecting (`global_step >= proposed → no-op`; `global_step + 1 != proposed →
  no-op`).
- **Fast-forward cannot overwrite it.** FF only fires when the canonical step is *ahead* of the
  local step. After a partition the local loop sits at the live VDF timeline (≈ wall clock), at or
  ahead of the older canonical blocks being replayed, so FF is skipped and the poisoned steps are
  never overwritten in the running process.

A restart escapes all three: it discards the running locals and the in-memory buffer entirely and
rebuilds from the truncated, canonical-rooted index.

### Reachability

A recovery reorg can be as deep as `block_tree_depth` (beyond that,
`validate_reorg_within_cache_window` triggers controlled shutdown). Poisoning is reachable whenever a
`block_tree_depth`-block window can span a reset interval:

| Config (`crates/types/src/config/consensus.rs`) | `block_tree_depth` | reset interval | spans a boundary? |
| --- | --- | --- | --- |
| mainnet | 100 blocks | 50 blocks | **always** (period < window) |
| testnet | 50 blocks | 100 blocks | ~50% of windows |

On mainnet config this is reliably reachable for any deep recovery that approaches the cache limit.

## Decision

On a network-partition recovery that poisons VDF state, **restart the process** and let the ordinary
cold-start path rebuild correct VDF state, rather than re-anchoring the running VDF in place.

### Why a restart heals

Fork choice acts on validated blocks: `recover_from_network_partition` is reached only after the
competing fork has been validated and adopted, at which point the block index has been truncated to
the fork parent (the LCA). A restart from there lands the node in the **cold-start regime** —
`create_state` rebuilds the buffer at the LCA (behind the canonical tip) and the existing
trusted-peer fast-forward path fills it forward verbatim while mining is paused. There is no
poisoned buffer to fight, because the buffer is rebuilt *from* the canonical chain rather than
reconciled *against* a divergent one.

### Why a whole-process restart, not a thread restart

The un-migrated canonical tail lives only in the in-memory block tree (headers persist to the DB
only at migration). A **process** restart loses that tail and refetches it from peers, which
re-validates and re-sends its fast-forward steps. A thread-only restart would keep the tree but
strand the buffer at the LCA, because the already-validated tail would never be re-fast-forwarded —
the trap the in-place design had to work around. This is also why the restart depends on a
supervisor that relaunches the process (see [Consequences](#consequences)).

### Gating: restart only when the buffer is actually poisoned

VDF steps *between* reset boundaries are a block-independent SHA chain, so a deep reorg that crosses
no reset boundary leaves the buffer **bit-identical** to the canonical lineage — no heal is needed.
The seeds diverge only once a boundary's rotation block (at `boundary − reset_frequency`) lies
**above** the LCA, i.e. at the **second** reset boundary above the LCA (the first boundary's
rotation block is at/below the shared LCA, so both forks share its seed).

`BlockTreeService` therefore restarts only when the **live VDF step** has reached that second
boundary:

```
first_divergent_boundary = (lca_step / reset_frequency + 2) * reset_frequency
restart  iff  live_vdf_step + 1 >= first_divergent_boundary
```

It reads the *live* step (not the orphaned blocks) because the VDF free-runs ahead of blocks, so a
block-level check would miss free-run poison. The `+ 1` is a one-step TOCTOU margin: the live
counter is sampled `Relaxed` and can lag the VDF loop's committed reset-seed decision by up to a
step. Over-triggering costs a rare extra restart; under-triggering would strand the buffer poisoned
with no other heal.

Because the restart is a **whole-process** event (reth, P2P, mempool, the VDF thread, all services,
plus resync downtime), gating it to genuine poison is what keeps routine deep reorgs cheap —
`recover_from_network_partition` still runs on every deep reorg for storage/supply recovery; only
the *restart* is gated.

This gating is **non-safety**: validation never trusts the live buffer (see
[Preserved trust boundary](#preserved-trust-boundary-fork-local-verification)), so even a missed
poison would only degrade this node's mining until healed, never violate consensus.

## Mechanism

- **Signal.** `BlockTreeService`, immediately after `recover_from_network_partition` returns (so the
  index truncation is durable first), evaluates the gate above and, on a hit, calls
  `self.service_senders.partition_recovery_restart.cancel()`. The token is a `CancellationToken` in
  `ServiceSenders` (it took the slot of the removed `vdf_reanchor` channel); cancelling is infallible
  and idempotent.
- **Lifecycle mapping.** `node_lifecycle` holds a clone of that token and watches it in its steady
  `select!` (`crates/chain/src/chain.rs`), mapping `token.cancelled()` to
  `ShutdownReason::PartitionRecoveryRestart` — mirroring the existing `vdf_exit_token → VdfExited`
  arm. It then runs the ordinary ordered shutdown.
- **Exit code.** `ShutdownReason::exit_code()` (`crates/types/src/shutdown.rs`) returns
  `PARTITION_RECOVERY_RESTART_EXIT_CODE` (75, `EX_TEMPFAIL`) for this reason and `0` for every other
  reason; `main` returns `ExitCode::from(reason.exit_code())` (`crates/chain/src/main.rs`). 75 reads
  as a retryable restart rather than a crash.
- **Relaunch (cold start).** A supervisor relaunches the process. Startup `create_state` rebuilds the
  VDF buffer over the now-truncated (LCA-rooted) index; `init_vdf_thread` runs `run_vdf` exactly once
  (no supervisor loop); the trusted-peer fast-forward path then replays the canonical tail's steps
  verbatim as the node resyncs.

`run_vdf` is now a single call returning `()` (the `VdfExit` enum and the re-anchor return path are
gone). The startup anchor still folds its own reset boundary via `reset_applied_anchor_hash` (see
[Preserved trust boundary](#preserved-trust-boundary-fork-local-verification), item 3).

## Preserved trust boundary: fork-local verification

A partition recovery requires the node to **validate and adopt** the competing canonical chain
*before* it can restart (fork choice acts only on validated blocks). That validation must not trust
the node's own — possibly poisoned — live VDF buffer. These seams are therefore load-bearing for the
restart path exactly as they were for the in-place re-anchor, and are unchanged:

1. **Recompute-on-mismatch.** `vdf_step_batch_is_valid` (`crates/vdf/src/state.rs`) treats a
   live-buffer mismatch not as invalidity but as a fall-through to the authoritative recompute from
   the block's own `prev_output`/seed. A heavier competing fork legitimately differs from this
   node's buffer past a boundary; recompute is the source of truth. Covered by
   `vdf_step_batch_recomputes_on_buffer_mismatch` (accepts honest fork, rejects forged).
2. **Fork-local step/recall views.** `build_fork_local_view` (`crates/vdf/src/state.rs`) and the
   `build_fork_local_recall_view` / `build_fork_local_step_view` builders
   (`crates/actors/src/block_validation.rs`, `crates/actors/src/validation_service.rs`) resolve VDF
   steps from a block's **own** lineage (ancestors tree-first, then DB), never the live buffer, so a
   poisoned buffer cannot reject an honest canonical block. An unbuildable view requeues via
   `VdfPrevStepForkViewUnavailable → Cancelled` (peer-innocent), never `Invalid`.
3. **Cold-path seed correctness (still required).** Two pieces survive because the cold-start +
   fast-forward catch-up uses them:
   - `canonical_vdf_snapshot` (`crates/types/src/block_provider.rs`, backed by
     `BlockTree::canonical_entry_at_or_below_step`) supplies the **per-step** reset seed when
     fast-forward hands off to local stepping at a reset boundary — folding that boundary's own seed,
     not the tip's `next_seed`.
   - `reset_applied_anchor_hash` (`crates/vdf/src/vdf.rs`) folds the startup anchor's own reset
     boundary into the (raw) buffer hash before `run_vdf`'s first step, so an anchor that lands
     exactly on a boundary does not mis-step the first range.

The only escape from this verification primitive would be weak-subjectivity checkpoints — a far
larger protocol decision, out of scope. The primitive stays; only the post-adoption heal changed.

## Alternatives considered

- **In-place VDF re-anchor (the superseded approach).** The VDF OS thread ran a supervisor loop;
  `run_vdf` returned a `VdfExit::Reanchor` on a `vdf_reanchor` signal; the supervisor rebuilt
  `VdfState` from the canonical **tip** (`create_state_for_canonical_tip`), republished the buffer
  under the write lock, drained the fast-forward queue, re-applied the boundary fold, and broadcast
  `MiningBroadcastEvent::Reanchored` to reset partition mining's efficient-sampling rotation. It
  healed without process downtime, but it added a substantial body of consensus-critical mechanism —
  a backward buffer mutation into an otherwise forward-only invariant set, a tip-rebuild that had to
  source each boundary's own seed, an asynchronous-window validation race, and a mining-rotation
  reset — much of which exists *only* to make an in-place rewind safe. The restart deletes all of it
  by rebuilding from the cold-start path, which is already exercised on every node boot. Rejected in
  favour of the restart once the cold-start heal was verified end-to-end (see Testing).
- **Widen the confirmation gate to `block_tree_depth`.** Would prevent the loop from ever crossing a
  boundary a recovery could later invalidate, but parks honest mining for an entire cache window of
  run-ahead budget and still leaves the buffer poisoned for a recovery that *does* happen within the
  window. Treats the symptom, not the cause.
- **Do nothing.** With the fork-local verification in place the node would not *wedge* (it still
  validates and follows canonical), but its live buffer would stay poisoned and it could not mine
  valid blocks until the poison aged out of the buffer window. The restart restores correct mining
  promptly.

## Consequences

- A recovering node converges to the canonical VDF lineage automatically — via relaunch + cold-sync
  — instead of mining on a poisoned buffer until an operator intervenes.
- The heal reuses the cold-start path and the fast-forward path: no in-place buffer-rewind
  primitive, no supervisor loop, no mining-rotation re-anchor broadcast.
- **Cost — downtime.** A boundary-crossing recovery now incurs the downtime of restarting the
  embedded reth, peer, and mempool subsystems plus the resync of the un-migrated tail. Gating the
  restart to genuine poison keeps non-boundary deep reorgs (which need no heal) free of this cost.
- **Cost — deployment dependency.** The node must run under a supervisor with a restart policy keyed
  to the exit code (systemd `Restart=on-failure` covers the non-zero 75; a Kubernetes/Docker restart
  policy equivalently). Without it the node stays down after a boundary-crossing deep reorg, which is
  a regression against the in-place heal — so the restart policy is part of the deliverable.

### Risks / edge cases

- **Signal ordering.** The restart must be requested strictly *after* `recover_from_network_partition`
  has truncated the index, so the post-restart `create_state` reads the LCA-rooted index, never the
  stale minority index. The `cancel()` is placed immediately after that call returns.
- **Restart-gate TOCTOU.** The live-step sample is `Relaxed` and can lag the VDF loop's committed
  seed by a step; the `+ 1` margin in the gate covers it conservatively (over-trigger over
  under-trigger). Non-safety regardless (validation is buffer-independent).
- **Cold-sync needs reachable peers.** The relaunched node must reach a peer within its startup-sync
  window to refetch the canonical tail; otherwise it comes up on the truncated index and waits. The
  deployment must keep peers (or trusted peers) configured — the integration test models this by
  injecting `trusted_peers` before relaunch.
- **Lock poisoning.** `run_vdf` still exits gracefully on a poisoned VDF lock; that path is a plain
  thread exit (surfaced as `VdfExited`), independent of the restart trigger.

## Testing

- **Unit (vdf), buffer mechanics:**
  `issue_1447_unconfirmed_reset_seed_poisons_buffer_and_wedges_block_validation`
  (`crates/vdf/src/vdf.rs`) proves that crossing a boundary on a loser vs winner seed diverges the
  buffer and that `vdf_steps_are_valid` rejects a poisoned buffer / accepts a clean one — the
  poisoning the restart heals.
- **Unit (vdf), fork-aware validation:** `vdf_step_batch_recomputes_on_buffer_mismatch`
  (`crates/vdf/src/state.rs`) proves a poisoned buffer ACCEPTS an honest competing-fork block via
  recompute-on-mismatch while a forged block is still rejected; the `build_vdf_seed_buffer_*` tests
  cover the shared walk-back (verbatim reproduction, capacity off-by-one, fetch-error propagation);
  `anchor_hash_folds_its_own_reset_boundary_*` and the per-step seed tests
  (`uses_per_step_seed_from_single_snapshot_when_crossing_boundary`,
  `fast_forward_exact_boundary_uses_pre_boundary_seed`) cover the cold-path seed correctness.
- **Unit (types), exit code:** `only_partition_recovery_restart_exits_nonzero`
  (`crates/types/src/shutdown.rs`) asserts only `PartitionRecoveryRestart` exits non-zero (75) and
  every other reason exits 0.
- **Integration (chain), non-boundary deep reorg:** `heavy4_slow_network_partition_recovery`
  (`crates/chain-tests/src/multi_node/partition_recovery.rs`) does a deep reorg that crosses no reset
  boundary; the gate does **not** restart and the node continues operating in place (surgical
  storage recovery + continued mining). This is the regression guard that non-boundary recoveries
  stay cheap.
- **Integration (chain), boundary-crossing heal-after-restart (the headline test):**
  `heavy4_slow_partition_recovery_crosses_reset_boundary_restarts`
  (`crates/chain-tests/src/multi_node/partition_recovery.rs`). Two real miners fork past a reset
  boundary; the peer's heavier fork is gossiped to genesis, which validates and adopts it (deep reorg
  crossing the boundary). The test asserts: (1) genesis requests the restart
  (`partition_recovery_restart` is cancelled); (2) after a `stop()`/`start()` relaunch from the same
  data directory (with the peer configured as a trusted peer so it can resync), genesis cold-syncs
  the canonical chain to the tip; (3) its rebuilt VDF buffer matches the canonical recorded steps
  over the boundary-crossing range; and (4) the recovered node produces the next canonical block and
  the peer follows it (no wedge).
  The `trusted_peers` injection is the in-harness analogue of the production supervisor-plus-peers
  requirement.
