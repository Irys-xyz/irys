# Longer anchor expiry depth for commitment transactions

Status: proposed
Date: 2026-07-01

## Goal

Give commitment transactions (stake / pledge / unpledge / unstake / update_reward_address) a **separate, longer anchor-expiry window** than data transactions, so a signed commitment stays includable for at least ~24h. This supports custody workflows (multisig coordination) where a commitment may be signed and held before broadcast.

This is a **consensus rule change**: it changes which blocks are valid (commitment anchor acceptance) and requires a coordinated network upgrade.

## Background: current behavior

Today data txs and commitment txs share one field: `MempoolConsensusConfig::tx_anchor_expiry_depth` (u8; mainnet 20 ≈ 4 min, testing 50). Ingress proofs already have their own, longer window (`ingress_proof_anchor_expiry_depth`, u16, 200), so a per-item-type window is an established pattern.

The shared depth gates commitment txs at four consensus-relevant sites:

1. **Mempool ingress** — `mempool_service.rs::validate_tx_anchor` rejects anchors older than `latest_height - tx_anchor_expiry_depth`. Called by both `data_txs.rs` and `commitment_txs.rs`.
2. **Mempool pruning** — `lifecycle.rs::should_prune_tx` drops txs once `anchor_height < current - (tx_anchor_expiry_depth + block_migration_depth + 5)` (grace window). Called per-tx for data and commitment txs.
3. **Block validation** — `block_discovery.rs` validates each commitment's anchor is in `valid_tx_anchor_blocks`, the by-hash set over `[height - tx_anchor_expiry_depth, height]`.
4. **Block production** — `tx_selector/mod.rs` selects commitments whose anchor is in `[current - (tx_anchor_expiry_depth - block_migration_depth), current - block_migration_depth]` via `validate_anchor_for_inclusion`. The upper bound (`- block_migration_depth`) is a **maturity** requirement (anchor must be confirmed), independent of expiry.

### Why the naive change is unsafe

Commitment **replay** protection is layered but short-lived, and every layer implicitly assumes the anchor expires within one epoch:

- The permanent DB check (`is_known_commitment_in_db` → `IrysCommitments`) runs **only at mempool ingress**. Validators never consult it for a gossiped block.
- Consensus-level dedup during block validation relies **entirely on the commitment snapshot**, which is **reset to empty at every epoch boundary** (`block_tree.rs` `create_commitment_snapshot_for_block`) and only covers the current epoch. There is no cross-epoch, consensus-level commitment dedup (nothing analogous to `get_previous_tx_inclusions`, which handles data-tx duplicates).
- Epoch rollup (`epoch_snapshot.rs::compute_commitment_state`) **does not dedup** — it `push`es pledges.

Concrete replay vector: a Pledge applied in epoch N loses its snapshot record at the epoch boundary. In epoch N+1, if its anchor is still valid (exactly what a ~24h ≈ one-epoch window enables), a block can re-include the same Pledge tx_id. Both validator paths accept it (fresh snapshot + already-staked signer → `Unknown`/`Accepted`), and the next rollup applies it **again** — double partition assignment + double charge. The only thing preventing this today is that the anchor expires first.

**Conclusion:** the anchor-expiry window currently doubles as the de-facto replay bound. Extending it *requires* adding durable, consensus-level replay protection. These are decoupled below.

## Design

### Part 1 — the longer commitment anchor window

**New config field.** Add to `MempoolConsensusConfig` (`crates/types/src/config/consensus.rs`), after `ingress_proof_anchor_expiry_depth`:

```rust
/// Number of blocks a given anchor is valid for when anchoring a commitment
/// transaction. Longer than tx_anchor_expiry_depth so custody workflows
/// (multisig coordination) have time to broadcast a signed commitment.
pub commitment_anchor_expiry_depth: u16,
```

`u16` (max 65 535 ≈ 9 days @ 12s) comfortably covers the 24h (7 200-block) benchmark and matches `ingress_proof_anchor_expiry_depth`.

Defaults (confirm at review):
- `mainnet()`: `7200` (24h @ 12s block time).
- `testnet()`: `7200`.
- `testing()`: `100` — larger than `tx_anchor_expiry_depth` (50) and `block_tree_depth` (50) so tests exercise the finalized path, and equal to `num_blocks_in_epoch` (100) so replay tests can cross an epoch boundary.

**Config invariant** (`crates/types/src/config/mod.rs::validate`): require `commitment_anchor_expiry_depth >= block_migration_depth` (so a commitment survives until its anchor block migrates — same safety floor as `tx_anchor_expiry_depth`). Do **not** cap it at `block_tree_depth`: like ingress-proof anchors, commitment anchors below the reorg floor are resolved from the finalized index/DB, which is branch-invariant. No ordering relation to `tx_anchor_expiry_depth` is enforced.

**Wire the four sites** to use `commitment_anchor_expiry_depth` for commitment txs only; data txs keep `tx_anchor_expiry_depth`:

1. `validate_tx_anchor` — parametrize with the effective expiry depth (add an argument); `data_txs.rs` passes `tx_anchor_expiry_depth`, `commitment_txs.rs` passes `commitment_anchor_expiry_depth`.
2. `should_prune_tx` — parametrize with the effective depth; the data-tx loop in `prune_pending_txs` passes the tx depth, the commitment loop passes the commitment depth. Grace formula unchanged in shape (`depth + block_migration_depth + 5`).
3. `block_discovery.rs` — validate commitment anchors against a set covering `[height - commitment_anchor_expiry_depth, height]`, computed with the **same by-hash-walk + finalized-block-index-fallback machinery already used for `valid_ingress_anchor_blocks`** (since the window exceeds `block_tree_depth`). The epoch-block exemption (epoch blocks roll up commitments with anchors from anywhere in the epoch) is unchanged. Factor the "valid anchor blocks down to height H" computation so the commitment and ingress windows share it rather than duplicating the walk.
4. `tx_selector/mod.rs` — compute a separate `commitment_min_anchor_height = current - (commitment_anchor_expiry_depth - block_migration_depth)` for the commitment selection loop; keep `max_anchor_height` (maturity) unchanged; data-tx selection keeps the tx-depth min.

### Part 2 — durable, consensus-level commitment replay protection

Make "a commitment applies at most once" hold **independent of the epoch-reset snapshot and of the anchor window**. Uses the fact that reorgs are physically bounded by `block_tree_depth` — anything deeper is finalized and cannot fork — so the check splits into two bounded halves:

- **Within the reorg window** (`[tip - block_tree_depth, tip]`): a **branch-correct by-hash walk** of the `SystemLedger::Commitment` tx_ids over the validated block's ancestors. Rejects any commitment tx_id already included on this branch. This fills the gap the snapshot misses immediately after an epoch boundary (prior-epoch commitments still inside the reorg window but no longer in the reset snapshot). The existing ancestor walk in `block_discovery.rs` (currently Submit-ledger dedup to `tx_anchor_expiry_depth`) is the natural place to also dedup the Commitment ledger, extended to `block_tree_depth` depth.
- **Below the reorg floor** (finalized): a **branch-safe canonical lookup** of prior commitment inclusion. Because there is no forking below `block_tree_depth`, a finalized inclusion is authoritative — reject on presence.

Enforce this in both validator paths: the non-epoch commitment handling in `block_discovery.rs` and `block_validation.rs::commitment_txs_are_valid`. Also align a known discrepancy: `commitment_txs_are_valid` currently accepts a same-tx_id duplicate that `add_commitment` reports as `Accepted`, whereas `block_discovery` rejects it — make both reject.

Mempool ingress keeps its existing `is_known_commitment_in_db` check.

## Config surface & upgrade implications

- **`consensus_config_hash` changes.** The new field flows into `ConsensusConfig::keccak256_hash()` (canonical JSON) → `consensus_config_hash`, which peers compare in the P2P handshake (`version.rs`, `gossip_data_handler.rs`). Nodes on old vs new config will not peer. This is inherently a **coordinated network upgrade** (all nodes need the new binary + config).
- **TOML configs must add the field.** `MempoolConsensusConfig` is `#[serde(deny_unknown_fields)]` with no serde default, and several configs pin the full `[consensus.Custom.mempool]` table. Add `commitment_anchor_expiry_depth` to each:
  - `crates/config/templates/testnet_config.toml`
  - `docker/configs/irys-1.toml`, `docker/configs/irys-2.toml`
  - `docker/agent_cluster/configs/irys-1.toml`, `irys-2.toml`, `irys-3.toml`
  - `docker/tests/data-sync/configs/irys-1.toml`, `irys-2.toml`, `irys-3.toml`
  - (enumerate exhaustively during implementation; any file with a `[consensus.Custom.mempool]` table)
- **Canonical serialization test.** `crates/types/src/canonical.rs` asserts specific mempool keys are present; add a `commitmentAnchorExpiryDepth` assertion. The mempool roundtrip case (`#[case::mempool]`) covers serialize/deserialize automatically.

## Non-goals

- Changing the data-tx (`tx_anchor_expiry_depth`) or ingress-proof windows.
- Changing commitment maturity (`- block_migration_depth` upper bound in selection).
- Re-anchoring / re-broadcast tooling for custody (out of scope; the longer window is the mechanism).
- Bounding mempool growth from longer-lived commitments (commitments are fee-gated and whitelist-gated; note as a follow-up if memory becomes a concern).

## Test plan

- **Config validation:** `commitment_anchor_expiry_depth < block_migration_depth` rejected; a value `> block_tree_depth` accepted (unlike `tx_anchor_expiry_depth`).
- **Anchor window (Part 1):** a commitment anchored older than `tx_anchor_expiry_depth` but within `commitment_anchor_expiry_depth` is accepted at ingress, not pruned, selected into a block, and validates — while a data tx at the same anchor age is rejected/pruned.
- **Anchor beyond block_tree_depth:** a commitment whose anchor is below the reorg floor validates via the finalized index (exercises the reused ingress-proof machinery).
- **Replay protection (Part 2) — the critical tests:**
  - Re-including a Pledge tx_id across an epoch boundary (anchor still valid) is rejected by both validator paths and never double-applied at rollup.
  - Branch-safety: a commitment included only on a reorged-out sibling within the reorg window is still includable on the surviving branch (no false-positive finalized rejection).
- **Handshake:** old vs new `consensus_config_hash` mismatch behaves as expected (documented upgrade).

## Open implementation detail

The finalized (below-reorg-floor) lookup in Part 2 must be **branch-safe** and must not require walking the full window each validation. Candidate primitives:
- `IrysCommitments` (permanent, keyed by tx_id) — but it is not pruned on reorg, so presence alone is not branch-safe within the reorg window (handled by the by-hash walk above; below the floor there is no fork, so presence is safe *if* we can confirm the inclusion is below the floor).
- `IrysCommitmentTxMetadata.included_height` — carries inclusion height but is **cleared on reorg and on migration** (`db_index.rs::clear_commitment_tx_metadata`), so it is not durable post-migration.

The plan must settle how to establish "included in a canonical block below the reorg floor" — likely a durable canonical `tx_id → included_height` index (not cleared on migration) or a block-index/`MigratedBlockHashes` cross-check analogous to `canonical_block_height_by_hash`. This is a bounded addition, not a new subsystem.
