# Commitment-tx Anchor Expiry Depth Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give commitment transactions a separate, longer anchor-expiry window (~24h) than data txs, safely — which first requires durable, consensus-level commitment replay protection.

**Architecture:** Two phases. **Phase A (replay protection)** is safe standalone: commitment dedup today relies on the epoch-reset commitment snapshot, which only bounds replay because anchors expire within an epoch. Add a durable dedup mirroring the data-tx pattern — a branch-correct by-hash walk of the Commitment system ledger within the reorg window (`block_tree_depth`) plus an MBH-cross-checked `IrysCommitmentTxMetadata.included_height` lookup below the reorg floor. **Phase B (longer window)** adds `commitment_anchor_expiry_depth` and wires the four sites that gate commitment anchors (mempool ingress, mempool pruning, block production, block validation) so commitments use it while data txs keep `tx_anchor_expiry_depth`.

**Tech Stack:** Rust (edition 2024), tokio channel-based services, reth-db (MDBX) tables, `IrysNodeTest` integration harness in `crates/chain-tests/src/`, nextest.

## Global Constraints

- Consensus-critical. `MempoolConsensusConfig` feeds `ConsensusConfig::keccak256_hash()` → `consensus_config_hash` compared in the P2P handshake; adding a field is a **coordinated network upgrade** (all nodes need the new binary + config to peer).
- `MempoolConsensusConfig` is `#[serde(deny_unknown_fields)]` with **no serde default** — every `[consensus.Custom.mempool]` TOML must gain the new field or it fails to parse.
- Reorgs are bounded by `block_tree_depth`; anything at height `<= tip - block_tree_depth` is finalized and cannot fork. Above the floor, only by-hash (branch-correct) checks are valid; below it, MBH-verified DB lookups are branch-invariant. (See the `canonical_metadata_height` docblock, `crates/database/src/database.rs:250-269`.)
- A commitment applies at most once, ever; dedup is keyed on commitment `tx_id` (re-anchoring yields a different tx_id).
- Toolchain: Rust 1.93.0. Tests: `#[test_log::test(tokio::test)]` for integration; `#[test]` for pure/db units. Run via `cargo nextest run -p <crate> <name>`.
- Temp dirs in tests: `irys_testing_utils::utils::TempDirBuilder` only (never `tempfile`/`std::env::temp_dir`).
- Never remove pre-existing comments explaining *why*; match surrounding style.

**Profile reference (unchanged):** `block_migration_depth`=6 all profiles; `block_tree_depth`: mainnet 100, testing 50, testnet 50; `tx_anchor_expiry_depth`: mainnet 20, testing 20, testnet 50; `ingress_proof_anchor_expiry_depth`=200; `num_blocks_in_epoch`: mainnet 7200, testing 100.

---

## Phase A — Durable commitment replay protection

### Task A1: MBH-cross-checked finalized commitment-inclusion DB helper

**Files:**
- Modify: `crates/database/src/database.rs` (add fn adjacent to `canonical_submit_height` at :318; test in `mod tests` at :1104)

**Interfaces:**
- Consumes: `get_commitment_tx_metadata` (`crates/database/src/db_index.rs:8`) returning `Option<CommitmentTransactionMetadata>` with `included_height: Option<u64>`; `MigratedBlockHashes` table.
- Produces: `pub fn canonical_commitment_included_height<T: DbTx>(tx: &T, txid: &IrysTransactionId, max_height: u64) -> eyre::Result<Option<u64>>` — `Some(h)` iff `IrysCommitmentTxMetadata[txid].included_height = h <= max_height` AND `MigratedBlockHashes[h]` is `Some` (canonical/finalized). Used by Tasks A3, A4.

Rationale: `persist_block` (`crates/actors/src/block_migration_service.rs:676-682`) writes `included_height` at migration; it is only cleared when a block is orphaned (`persist_metadata` reorg phase). Below the reorg floor a block cannot be orphaned, so this value is reliable there. The MBH check rejects stranded writes, exactly like `canonical_submit_height`.

- [ ] **Step 1: Write the failing test** (append to `mod tests` in `crates/database/src/database.rs`)

```rust
#[test]
fn canonical_commitment_included_height_requires_mbh() -> eyre::Result<()> {
    use crate::{
        canonical_commitment_included_height, open_or_create_db,
        set_commitment_tx_included_height,
    };
    use crate::tables::MigratedBlockHashes;
    use irys_types::{H256, IrysDatabaseArgs as _};
    use reth_db::transaction::DbTxMut;

    let path = irys_testing_utils::utils::TempDirBuilder::new().build();
    let db = open_or_create_db(&path, crate::tables::IrysTables::ALL, irys_testing()?)?;
    let txid = H256::random();

    // Write inclusion height 10, but no MBH row yet -> not canonical.
    db.update(|tx| set_commitment_tx_included_height(tx, &txid, 10))??;
    assert_eq!(db.view(|tx| canonical_commitment_included_height(tx, &txid, 100))??, None);

    // Add the MBH row at height 10 -> canonical.
    db.update(|tx| tx.put::<MigratedBlockHashes>(10, H256::random()))??;
    assert_eq!(db.view(|tx| canonical_commitment_included_height(tx, &txid, 100))??, Some(10));

    // max_height below the inclusion height -> filtered out.
    assert_eq!(db.view(|tx| canonical_commitment_included_height(tx, &txid, 5))??, None);

    // Unknown tx -> None.
    assert_eq!(db.view(|tx| canonical_commitment_included_height(tx, &H256::random(), 100))??, None);
    Ok(())
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p irys-database canonical_commitment_included_height_requires_mbh`
Expected: FAIL — `canonical_commitment_included_height` not found (compile error).

- [ ] **Step 3: Write minimal implementation** (in `crates/database/src/database.rs`, after `canonical_promoted_height` at :356)

```rust
/// Returns the MBH-verified commitment inclusion height of `txid`, if any,
/// capped at `max_height`.
///
/// `Some(h)` means: `IrysCommitmentTxMetadata` carries `included_height = h ≤
/// max_height`, AND `MigratedBlockHashes[h]` is `Some` (canonical). Like the
/// data-tx `canonical_submit_height`, the existence check is only
/// branch-invariant BELOW the reorg floor — callers must pass `max_height ≤
/// tip - block_tree_depth` and cover the reorg window by-hash separately.
///
/// `None` = tx unknown / no `included_height` / hint > `max_height` / hint not
/// confirmed by MBH (stranded write from an orphaned block).
pub fn canonical_commitment_included_height<T: DbTx>(
    tx: &T,
    txid: &IrysTransactionId,
    max_height: u64,
) -> eyre::Result<Option<u64>> {
    let Some(metadata) = crate::db_index::get_commitment_tx_metadata(tx, txid)? else {
        return Ok(None);
    };
    let Some(height) = metadata.included_height else {
        return Ok(None);
    };
    if height > max_height {
        return Ok(None);
    }
    if tx.get::<MigratedBlockHashes>(height)?.is_none() {
        return Ok(None);
    }
    Ok(Some(height))
}
```

Ensure `MigratedBlockHashes` and `IrysTransactionId` are in scope (add to the existing `use` at the top of `database.rs` if missing — `MigratedBlockHashes` is already imported per `canonical_metadata_height`). Re-export the fn from the crate root if `canonical_submit_height` is (check `crates/database/src/lib.rs`; add `canonical_commitment_included_height` alongside it).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo nextest run -p irys-database canonical_commitment_included_height_requires_mbh`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/database/src/database.rs crates/database/src/lib.rs
git commit --no-gpg-sign -m "feat(db): MBH-verified canonical commitment inclusion lookup"
```

---

### Task A2: Ancestor commitment-tx-id collector (by-hash, branch-correct)

**Files:**
- Create: `crates/actors/src/commitment_dedup.rs`
- Modify: `crates/actors/src/lib.rs` (add `pub mod commitment_dedup;`)
- Test: unit test inside `commitment_dedup.rs`

**Interfaces:**
- Consumes: `BlockTreeReadGuard` (`irys_domain`), `DatabaseProvider`, `IrysBlockHeader::commitment_tx_ids()` (`crates/types/src/block.rs:555`), `irys_database::block_header_by_hash`.
- Produces: `pub fn ancestor_commitment_tx_ids(block_tree: &BlockTreeReadGuard, db: &DatabaseProvider, block_under_validation: &IrysBlockHeader, walk_depth: u64) -> eyre::Result<std::collections::HashSet<H256>>` — the set of commitment tx_ids in ancestors of `block_under_validation` with height in `[block_height - walk_depth, block_height)`, walked by-hash (block tree then DB fallback). Used by Tasks A3, A4.

Model on the existing by-hash ancestor walk in `crates/actors/src/block_discovery.rs:481-527` (block-tree lookup with DB fallback via `block_header_by_hash`).

- [ ] **Step 1: Write the failing test** (in `commitment_dedup.rs`, `#[cfg(test)] mod tests`)

Because the walk needs a populated block tree, drive this behavior primarily through the integration tests in A3/A5. Add one focused unit test that a zero-depth walk returns empty, guarding the boundary math:

```rust
// Full behavioral coverage is in the integration tests (Tasks A3, A5); this
// pins the boundary: walk_depth 0 inspects no ancestors.
#[test]
fn walk_depth_zero_returns_empty() {
    // Constructed via a minimal BlockTreeReadGuard fixture; see
    // block_discovery tests for the guard builder. Assert the returned set is empty.
    // (If no lightweight guard fixture exists, delete this unit test and rely on A3/A5.)
}
```

If `crates/actors` has no lightweight `BlockTreeReadGuard` fixture (grep `BlockTreeReadGuard` in `crates/actors/src/*tests*`), omit this unit test — the collector is fully exercised by the A3/A5 integration tests. Do not leave a stub.

- [ ] **Step 2: Write the implementation**

```rust
use irys_database::block_header_by_hash;
use irys_domain::BlockTreeReadGuard;
use irys_types::{H256, IrysBlockHeader, app_state::DatabaseProvider};
use irys_database::db::IrysDatabaseExt as _;
use std::collections::HashSet;

/// Commitment tx_ids included in `block_under_validation`'s ancestors within
/// `[height - walk_depth, height)`, resolved by-hash (block tree, DB fallback).
///
/// Branch-correct: only walks THIS block's own ancestry, so it never counts a
/// reorged-out sibling. Callers use it for the reorg window (`walk_depth =
/// block_tree_depth`) and cover finalized inclusions below the floor via
/// `canonical_commitment_included_height`.
pub fn ancestor_commitment_tx_ids(
    block_tree: &BlockTreeReadGuard,
    db: &DatabaseProvider,
    block_under_validation: &IrysBlockHeader,
    walk_depth: u64,
) -> eyre::Result<HashSet<H256>> {
    let mut ids = HashSet::new();
    let block_height = block_under_validation.height;
    let min_height = block_height.saturating_sub(walk_depth);

    let guard = block_tree.read();
    let mut cursor = block_under_validation.previous_block_hash;
    // Walk parents until we drop below min_height or hit genesis.
    loop {
        let header = match guard.get_block(&cursor) {
            Some(h) => h.clone(),
            None => match db.view_eyre(|tx| block_header_by_hash(tx, &cursor, false))? {
                Some(h) => h,
                None => break, // unknown ancestor: stop (validation elsewhere rejects orphans)
            },
        };
        if header.height < min_height {
            break;
        }
        ids.extend(header.commitment_tx_ids().iter().copied());
        if header.height == 0 {
            break;
        }
        cursor = header.previous_block_hash;
    }
    Ok(ids)
}
```

- [ ] **Step 3: Wire the module** — add `pub mod commitment_dedup;` to `crates/actors/src/lib.rs`.

- [ ] **Step 4: Compile**

Run: `cargo xtask check`
Expected: compiles clean.

- [ ] **Step 5: Commit**

```bash
git add crates/actors/src/commitment_dedup.rs crates/actors/src/lib.rs
git commit --no-gpg-sign -m "feat(actors): by-hash ancestor commitment tx_id collector"
```

---

### Task A3: Enforce replay dedup in block_discovery (prevalidation)

**Files:**
- Modify: `crates/actors/src/block_discovery.rs:613-624` (the non-epoch commitment loop) — reuses existing `BlockDiscoveryError::DuplicateTransaction(tx_id)` (:47).
- Test: `crates/chain-tests/src/validation/commitment_replay.rs` (new); register in `crates/chain-tests/src/validation/mod.rs`.

**Interfaces:**
- Consumes: `commitment_dedup::ancestor_commitment_tx_ids` (A2), `irys_database::canonical_commitment_included_height` (A1), `config.consensus.block_tree_depth`, `block_tree_guard`, `db`.
- Produces: block_discovery rejects any non-epoch block whose commitment tx_id was included in a canonical ancestor (reorg window) or finalized below the floor.

- [ ] **Step 1: Write the failing test** (`crates/chain-tests/src/validation/commitment_replay.rs`)

Uses the evil-block pattern (`validation/data_tx_pricing.rs`) to re-include an already-finalized pledge in a fresh block across an epoch boundary, and asserts rejection.

```rust
use crate::utils::*;
use irys_actors::block_producer::{BlockProdStrategy, BlockProducerInner, ProductionStrategy};
use irys_types::{CommitmentTransaction, NodeConfig};
use std::sync::Arc;

// Evil producer that force-includes `dup` in the commitment ledger regardless of dedup.
struct ReplayStrategy { prod: ProductionStrategy, dup: CommitmentTransaction }

#[async_trait::async_trait]
impl BlockProdStrategy for ReplayStrategy {
    fn inner(&self) -> &BlockProducerInner { &self.prod.inner }
    async fn get_mempool_txs(&self, prev: &irys_types::IrysBlockHeader, ts: irys_types::UnixTimestampMs)
        -> Result<irys_actors::MempoolTxsBundle, irys_actors::TxSelectorError> {
        let mut bundle = self.prod.get_mempool_txs(prev, ts).await?;
        bundle.commitment_txs = vec![self.dup.clone()];
        bundle.commitment_txs_to_bill = vec![self.dup.clone()];
        Ok(bundle)
    }
}

#[test_log::test(tokio::test)]
async fn heavy_commitment_replay_across_epoch_rejected() -> eyre::Result<()> {
    initialize_tracing();
    let num_blocks_in_epoch = 4;
    let mut config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    config.consensus.get_mut().block_migration_depth = 1;
    config.consensus.get_mut().block_tree_depth = 3; // small reorg window so inclusion finalizes fast
    config.consensus.get_mut().mempool.commitment_anchor_expiry_depth = 100; // long: survives past the epoch (Phase B; harmless default if run before)
    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&signer]);

    let node = IrysNodeTest::new_genesis(config).start_and_wait_for_packing("GENESIS", 10).await;

    // Stake+pledge, include them, then let them finalize below the reorg floor and cross an epoch.
    let _stake = node.post_stake_commitment_with_signer(&signer).await;
    let pledge = node.post_pledge_commitment_with_signer(&signer).await;
    node.mine_blocks(num_blocks_in_epoch * 2).await?; // cross an epoch boundary; snapshot resets
    node.wait_until_height_confirmed(num_blocks_in_epoch as u64 * 2, 20).await?;

    // Craft a block that re-includes the already-included pledge.
    let strat = ReplayStrategy {
        dup: pledge.clone(),
        prod: ProductionStrategy { inner: node.node_ctx.block_producer_inner.clone() },
    };
    let (block, _s, _p) = strat
        .fully_produce_new_block_without_gossip(&solution_context(&node.node_ctx).await?)
        .await?
        .expect("block produced");

    let outcome = send_block_and_read_state(&node.node_ctx, Arc::clone(&block), false).await?;
    assert!(matches!(outcome, BlockValidationOutcome::Discarded(_)),
        "re-included finalized commitment must be rejected, got {outcome:?}");
    node.stop().await;
    Ok(())
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p chain-tests heavy_commitment_replay_across_epoch_rejected`
Expected: FAIL — outcome is `StoredOnNode(...)` (block accepted; dedup absent). (If `commitment_anchor_expiry_depth` doesn't exist yet because Phase B is unmerged, delete that one config line for this phase; the test works without it.)

- [ ] **Step 3: Implement the dedup** — in `crates/actors/src/block_discovery.rs`, replace the non-epoch commitment anchor loop at :615-624 with an anchor check PLUS a prior-inclusion check:

```rust
// for commitments, only validate if we're not an epoch block
if !is_epoch_block {
    let block_tree_depth = config.consensus.block_tree_depth;
    let prior_commitment_ids = crate::commitment_dedup::ancestor_commitment_tx_ids(
        &block_tree_guard, &db, new_block_header, block_tree_depth,
    )?;
    let finalized_floor = block_height.saturating_sub(block_tree_depth);
    for tx in commitment_txs.iter() {
        // anchor validity (Phase B widens this set for commitments; see Task B6)
        if !valid_tx_anchor_blocks.contains(&tx.anchor()) {
            return Err(BlockDiscoveryError::InvalidAnchor {
                item_type: AnchorItemType::SystemTransaction { tx_id: tx.id() },
                anchor: tx.anchor(),
            });
        }
        // durable replay protection: reject if this commitment was already
        // included on this branch's ancestry (reorg window, by-hash) or in a
        // finalized block below the reorg floor (MBH-verified).
        let already_included = prior_commitment_ids.contains(&tx.id())
            || db_view_canonical_commitment(&db, &tx.id(), finalized_floor)?;
        if already_included {
            return Err(BlockDiscoveryError::DuplicateTransaction(tx.id()));
        }
    }
}
```

Add a small local helper near the top of the impl (or inline the `db.view_eyre`):

```rust
fn db_view_canonical_commitment(db: &DatabaseProvider, tx_id: &H256, floor: u64) -> eyre::Result<bool> {
    Ok(db.view_eyre(|tx| irys_database::canonical_commitment_included_height(tx, tx_id, floor))?.is_some())
}
```

Import `H256` / `DatabaseProvider` if not already in scope.

- [ ] **Step 4: Register the test module** — add `mod commitment_replay;` to `crates/chain-tests/src/validation/mod.rs`.

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo nextest run -p chain-tests heavy_commitment_replay_across_epoch_rejected`
Expected: PASS — outcome `Discarded(_)`.

- [ ] **Step 6: Commit**

```bash
git add crates/actors/src/block_discovery.rs crates/chain-tests/src/validation/
git commit --no-gpg-sign -m "feat(consensus): reject replayed commitments in prevalidation"
```

---

### Task A4: Enforce replay dedup in full validation (`commitment_txs_are_valid`)

**Files:**
- Modify: `crates/actors/src/block_validation.rs` — non-epoch commitment loop at :4938-4965; add `ValidationError::DuplicateCommitmentTransaction { tx_id: H256 }` variant near `CommitmentSnapshotRejected` (:977) and update its match arms (:1118, :1181, :1421).
- Test: extend `crates/chain-tests/src/validation/commitment_replay.rs`.

**Interfaces:**
- Consumes: A1, A2 helpers; the `block_tree`/`db` handles already available in `commitment_txs_are_valid`; `config.consensus.block_tree_depth`.
- Produces: full validation rejects a replayed commitment with `ValidationError::DuplicateCommitmentTransaction`.

- [ ] **Step 1: Write the failing assertion** — in the A3 test, tighten the assertion to require the precise error at full validation. Add a second test that routes the block through full validation (the A3 evil block already goes through both; assert the discard reason):

```rust
// In heavy_commitment_replay_across_epoch_rejected, replace the loose matches! with:
assert_validation_error(
    outcome,
    |e| matches!(e, irys_actors::ValidationError::DuplicateCommitmentTransaction { .. })
        // prevalidation may reject first with a discovery-layer duplicate:
        || format!("{e:?}").contains("DuplicateTransaction"),
    "replayed commitment must be rejected as duplicate",
);
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo nextest run -p chain-tests heavy_commitment_replay_across_epoch_rejected`
Expected: FAIL — `ValidationError::DuplicateCommitmentTransaction` does not exist (compile error) OR full-validation path does not produce it.

- [ ] **Step 3: Add the error variant** — in `crates/actors/src/block_validation.rs` near :977:

```rust
    /// A commitment tx_id was already included in a canonical ancestor (reorg
    /// window) or a finalized block below the reorg floor.
    DuplicateCommitmentTransaction { tx_id: H256 },
```

Update the three match sites that enumerate `CommitmentSnapshotRejected`:
- recoverability/category arms at :1118 and :1181 — add `| Self::DuplicateCommitmentTransaction { .. }` alongside `CommitmentSnapshotRejected { .. }` in the same arm.
- metric-name arm at :1421 — add `Self::DuplicateCommitmentTransaction { .. } => "duplicate_commitment_transaction",`.

- [ ] **Step 4: Enforce in the loop** — in `commitment_txs_are_valid` non-epoch loop (:4938-4965), before/after the `simulated_snapshot.add_commitment` check, add:

```rust
let block_tree_depth = config.consensus.block_tree_depth;
let prior_commitment_ids = crate::commitment_dedup::ancestor_commitment_tx_ids(
    &block_tree_read_guard, db, block, block_tree_depth,
)?;
let finalized_floor = block.height.saturating_sub(block_tree_depth);
for tx in commitment_txs.iter() {
    let already = prior_commitment_ids.contains(&tx.id())
        || db.view_eyre(|t| irys_database::canonical_commitment_included_height(t, &tx.id(), finalized_floor))?.is_some();
    if already {
        return Err(ValidationError::DuplicateCommitmentTransaction { tx_id: tx.id() });
    }
}
```

Match the exact block-tree-guard / db variable names in scope at that site (grep the enclosing fn signature; `commitment_txs`, `block`, and the guards are already bound). Keep the existing `add_commitment`/snapshot check intact — this is an additional, earlier reject.

- [ ] **Step 5: Run to verify it passes**

Run: `cargo nextest run -p chain-tests heavy_commitment_replay_across_epoch_rejected`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/actors/src/block_validation.rs crates/chain-tests/src/validation/commitment_replay.rs
git commit --no-gpg-sign -m "feat(consensus): reject replayed commitments in full validation"
```

---

### Task A5: Branch-safety regression test (reorged-out commitment stays includable)

**Files:**
- Test: `crates/chain-tests/src/validation/commitment_replay.rs` (add test) — adapts `multi_node/fork_recovery.rs:609` `heavy4_reorg_tip_moves_across_nodes_commitment_txs`.

**Interfaces:**
- Consumes: reorg harness (`wait_for_reorg`, `gossip_block_to_peers`/`send_full_block`, `mine_block_without_gossip`, `wait_for_mempool_commitment_txs`, `get_block_by_height`).
- Produces: proof the dedup does not false-positive on a commitment whose only prior inclusion was on a reorged-out branch (guards the branch-correctness of A2 + the MBH check of A1).

- [ ] **Step 1: Write the test** — two nodes; on the to-be-orphaned branch include a commitment (kept local via `_without_gossip`), then deliver a longer competing fork to trigger a reorg; assert the commitment (a) returns to mempool (`wait_for_mempool_commitment_txs`) and (b) is re-included in a subsequent canonical block via `get_block_by_height(h).commitment_tx_ids()`. Base it directly on `heavy4_reorg_tip_moves_across_nodes_commitment_txs` — copy its fork-building recipe and add an explicit assertion that the reorged-out commitment is accepted (NOT rejected as duplicate) on the surviving branch.

```rust
#[test_log::test(tokio::test)]
async fn heavy_commitment_reorged_out_stays_includable() -> eyre::Result<()> {
    // See multi_node/fork_recovery.rs:609 for the full template; the key new
    // assertion vs the existing test is that after reorg the commitment is
    // re-included in a canonical block and NOT rejected by the new dedup.
    // ... (adapt template: build two forks, orphan branch containing `pledge`,
    //      trigger reorg, then mine on surviving branch and assert `pledge.id()`
    //      appears in a canonical block's commitment_tx_ids()) ...
    Ok(())
}
```

Fill the body by adapting the referenced test — do not leave the `...` placeholder; the template lines are `fork_recovery.rs:700-922`.

- [ ] **Step 2: Run**

Run: `cargo nextest run -p chain-tests heavy_commitment_reorged_out_stays_includable`
Expected: PASS.

- [ ] **Step 3: Confirm the existing reorg test still passes** (regression):

Run: `cargo nextest run -p chain-tests heavy4_reorg_tip_moves_across_nodes_commitment_txs`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/chain-tests/src/validation/commitment_replay.rs
git commit --no-gpg-sign -m "test(consensus): reorged-out commitment remains includable"
```

---

## Phase B — Longer commitment anchor window

### Task B1: Config field, defaults, invariant, canonical hash

**Files:**
- Modify: `crates/types/src/config/consensus.rs` — struct :506-515, defaults at :711-722 (mainnet), :852-858 (testing), :993-999 (testnet).
- Modify: `crates/types/src/config/mod.rs:97` — add invariant after the `tx_anchor_expiry_depth >= block_migration_depth` check.
- Modify: `crates/types/src/canonical.rs:773` — add key-presence assertion.
- Test: config-validation unit test in `crates/types/src/config/mod.rs` `mod tests`.

**Interfaces:**
- Produces: `MempoolConsensusConfig::commitment_anchor_expiry_depth: u16`. Consumed by Tasks B3–B6.

- [ ] **Step 1: Write the failing test** (in `crates/types/src/config/mod.rs` `mod tests`, near `test_tx_anchor_expiry_depth_vs_block_tree_depth_validation` at :1364)

```rust
#[test]
fn commitment_anchor_expiry_depth_invariant() {
    // Must be >= block_migration_depth.
    let mut cfg = Config::new(NodeConfig::testing());
    cfg.consensus.mempool.commitment_anchor_expiry_depth = 3; // < block_migration_depth (6)
    cfg.consensus.block_migration_depth = 6;
    cfg.validate().expect_err("commitment_anchor_expiry_depth < block_migration_depth must fail");

    // May EXCEED block_tree_depth (unlike tx_anchor_expiry_depth): validated via
    // finalized index below the reorg floor.
    let mut ok = Config::new(NodeConfig::testing());
    ok.consensus.mempool.commitment_anchor_expiry_depth = 100; // > block_tree_depth (50)
    ok.validate().expect("commitment_anchor_expiry_depth may exceed block_tree_depth");
}
```

Match the exact `Config` construction pattern used by the neighboring tests (grep how `test_tx_anchor_expiry_depth_vs_block_tree_depth_validation` builds and mutates its config — reuse it verbatim so field access paths are correct).

- [ ] **Step 2: Run to verify it fails**

Run: `cargo nextest run -p irys-types commitment_anchor_expiry_depth_invariant`
Expected: FAIL — field does not exist (compile error).

- [ ] **Step 3: Add the struct field** (`consensus.rs`, after `ingress_proof_anchor_expiry_depth` at :512):

```rust
    /// The number of blocks a given anchor is valid for when anchoring a
    /// COMMITMENT transaction. Longer than `tx_anchor_expiry_depth` so custody
    /// workflows (multisig coordination) have time to broadcast a signed
    /// commitment. Unlike `tx_anchor_expiry_depth` it is NOT capped at
    /// `block_tree_depth`: commitment anchors below the reorg floor are
    /// resolved from the finalized block index, like ingress-proof anchors.
    pub commitment_anchor_expiry_depth: u16,
```

- [ ] **Step 4: Set defaults** — add `commitment_anchor_expiry_depth: 7200,` to the mainnet (:721) and testnet (:998) mempool blocks, and `commitment_anchor_expiry_depth: 100,` to the testing block (:857). (7200 ≈ 24h @ 12s; testing 100 > block_tree_depth 50 and > tx_anchor_expiry_depth 20, and = num_blocks_in_epoch so replay tests cross a boundary.)

- [ ] **Step 5: Add the invariant** (`config/mod.rs`, after :97):

```rust
        // Commitment anchors must survive at least until their anchor block
        // migrates; but they may outlive the reorg window (resolved from the
        // finalized index below the floor), so they are NOT capped at
        // block_tree_depth.
        ensure!(
            self.consensus.block_migration_depth
                <= u32::from(self.consensus.mempool.commitment_anchor_expiry_depth),
            "commitment_anchor_expiry_depth ({}) must be >= block_migration_depth ({})",
            self.consensus.mempool.commitment_anchor_expiry_depth,
            self.consensus.block_migration_depth,
        );
```

- [ ] **Step 6: Add the canonical assertion** (`canonical.rs`, after :773):

```rust
        assert!(v["mempool"].get("commitmentAnchorExpiryDepth").is_some());
```

- [ ] **Step 7: Run all touched tests**

Run: `cargo nextest run -p irys-types commitment_anchor_expiry_depth_invariant && cargo nextest run -p irys-types canonical`
Expected: PASS (the `#[case::mempool]` roundtrip case covers serialize/deserialize automatically).

- [ ] **Step 8: Commit**

```bash
git add crates/types/src/config/consensus.rs crates/types/src/config/mod.rs crates/types/src/canonical.rs
git commit --no-gpg-sign -m "feat(config): add commitment_anchor_expiry_depth"
```

---

### Task B2: Add the field to every Custom-consensus TOML

**Files (add `commitment_anchor_expiry_depth = 100` under `[consensus.Custom.mempool]`, beside `ingress_proof_anchor_expiry_depth`):**
- Modify: `crates/config/templates/testnet_config.toml:132`
- Modify: `docker/configs/irys-1.toml:122`, `docker/configs/irys-2.toml`
- Modify: `docker/agent_cluster/configs/irys-1.toml`, `irys-2.toml`, `irys-3.toml`
- Modify: `docker/tests/data-sync/configs/irys-1.toml`, `irys-2.toml`, `irys-3.toml`

- [ ] **Step 1: Enumerate every file with the table**

Run: `grep -rln "\[consensus.Custom.mempool\]" --include="*.toml" .`
Expected: the list above. Edit each, adding the line beneath `ingress_proof_anchor_expiry_depth`. Use `100` (these are test/dev configs).

- [ ] **Step 2: Verify each parses** (deny_unknown_fields + required field)

Run: `cargo nextest run -p irys-config 2>&1 | tail -5` (or the crate that loads these templates; grep tests that call `Config::from`/`toml::from_str` on the template)
Expected: no "missing field commitment_anchor_expiry_depth" / "unknown field" errors.

- [ ] **Step 3: Commit**

```bash
git add crates/config/templates/*.toml docker/**/*.toml
git commit --no-gpg-sign -m "chore(config): add commitment_anchor_expiry_depth to Custom-consensus TOMLs"
```

---

### Task B3: Mempool ingress uses the commitment depth

**Files:**
- Modify: `crates/actors/src/mempool_service.rs:361-415` (`validate_tx_anchor` — add depth param)
- Modify: `crates/actors/src/mempool_service/data_txs.rs:109` (pass tx depth)
- Modify: `crates/actors/src/mempool_service/commitment_txs.rs:124` (pass commitment depth)
- Test: `crates/chain-tests/src/multi_node/mempool_tests.rs` (add) or a new `crates/chain-tests/src/api/commitment_anchor_window.rs`.

**Interfaces:**
- Produces: `validate_tx_anchor(&self, tx, anchor_expiry_depth: u64) -> Result<u64, TxIngressError>`.

- [ ] **Step 1: Write the failing test** — a commitment anchored older than `tx_anchor_expiry_depth` but within `commitment_anchor_expiry_depth` is accepted at ingress, while a data tx at the same anchor age is rejected. Use `testing()` (tx depth 20, commitment 100). Mine ~30 blocks, capture an old block hash as the anchor, post a pledge with that anchor via `post_pledge_commitment(Some(old_anchor))` (expect Ok) and a data tx with the same anchor (expect the anchor-too-old rejection).

```rust
#[test_log::test(tokio::test)]
async fn heavy_commitment_accepts_old_anchor_data_tx_rejects() -> eyre::Result<()> {
    initialize_tracing();
    let mut config = NodeConfig::testing(); // tx depth 20, commitment depth 100
    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&signer]);
    let node = IrysNodeTest::new_genesis(config).start_and_wait_for_packing("N", 10).await;

    let old_anchor = node.mine_block().await?.block_hash; // height 1
    node.mine_blocks(25).await?;                           // now >20 deep (past tx window), <100

    // commitment with the old anchor: accepted
    node.post_stake_commitment_with_signer(&signer).await;
    let pledge = CommitmentTransaction::new_pledge(
        &node.node_ctx.config.consensus, old_anchor,
        &node.node_ctx /* PledgeDataProvider */, signer.address()).await;
    let mut pledge = pledge; signer.sign_commitment(&mut pledge)?;
    node.ingest_commitment_tx(pledge.clone()).await
        .expect("commitment with old-but-in-window anchor should be accepted");

    // data tx with the same old anchor: rejected (too old for tx window)
    // build a minimal data tx anchored at `old_anchor` and assert ingest errors InvalidAnchor.
    // (use the data-tx builder used elsewhere in chain-tests; assert Err)
    node.stop().await;
    Ok(())
}
```

Adapt the data-tx construction to the harness's existing builder (grep `new_data_tx`/`DataTransaction` in chain-tests). The assertion is: commitment Ok, data tx `Err(InvalidAnchor)`.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo nextest run -p chain-tests heavy_commitment_accepts_old_anchor_data_tx_rejects`
Expected: FAIL — commitment currently rejected (still uses tx depth 20).

- [ ] **Step 3: Parametrize `validate_tx_anchor`** (`mempool_service.rs:361`):

```rust
    pub async fn validate_tx_anchor(
        &self,
        tx: &impl IrysTransactionCommon,
        anchor_expiry_depth: u64,
    ) -> Result<u64, TxIngressError> {
```
and replace :397-398:
```rust
        let min_anchor_height = latest_height.saturating_sub(anchor_expiry_depth);
```

- [ ] **Step 4: Update callers**
- `data_txs.rs:109`: `let anchor_height = self.validate_tx_anchor(tx, self.config.consensus.mempool.tx_anchor_expiry_depth as u64).await?;`
- `commitment_txs.rs:124`: `self.validate_tx_anchor(commitment_tx, self.config.consensus.mempool.commitment_anchor_expiry_depth as u64).await?;`

- [ ] **Step 5: Run to verify it passes**

Run: `cargo nextest run -p chain-tests heavy_commitment_accepts_old_anchor_data_tx_rejects`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/actors/src/mempool_service.rs crates/actors/src/mempool_service/data_txs.rs crates/actors/src/mempool_service/commitment_txs.rs crates/chain-tests/src/
git commit --no-gpg-sign -m "feat(mempool): validate commitment anchors against commitment window"
```

---

### Task B4: Mempool pruning uses the commitment depth

**Files:**
- Modify: `crates/actors/src/mempool_service/lifecycle.rs:307-341` (`should_prune_tx` — add depth param) and its two callers in `prune_pending_txs` (:368 data, :380 commitment).

**Interfaces:**
- Produces: `should_prune_tx(&self, current_height: u64, tx: &impl IrysTransactionCommon, anchor_expiry_depth: u32) -> bool`.

- [ ] **Step 1: Write the failing test** — extend `heavy_commitment_accepts_old_anchor_data_tx_rejects` (or a new test): after accepting the old-anchor commitment, mine enough blocks that the anchor age exceeds `tx_anchor_expiry_depth + grace` but stays within `commitment_anchor_expiry_depth`, run a prune cycle, and assert the commitment is still in the mempool (`wait_for_mempool_commitment_txs`/`get_commitment_snapshot_status`), whereas a data tx at the same age would be pruned.

```rust
// after ingesting `pledge` at anchor age ~26:
node.mine_blocks(10).await?; // anchor now ~36 deep: past tx-window+grace(20+6+5=31), within 100
node.trigger_mempool_prune().await?; // grep utils.rs for the exact prune-trigger method name
node.wait_for_mempool_commitment_txs(vec![pledge.id()], 10).await
    .expect("commitment must survive pruning within its longer window");
```

Grep `prune_pending_txs`/`prune` exposure in `utils.rs` for the exact trigger (or advance height and rely on the lifecycle timer). If no direct trigger exists, drive pruning by height advance and a short wait.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo nextest run -p chain-tests <test name>`
Expected: FAIL — commitment pruned (uses tx depth).

- [ ] **Step 3: Parametrize `should_prune_tx`** (:307):

```rust
    pub fn should_prune_tx(
        &self,
        current_height: u64,
        tx: &impl IrysTransactionCommon,
        anchor_expiry_depth: u32,
    ) -> bool {
```
and replace :325-327:
```rust
        let effective_expiry_depth = anchor_expiry_depth
            + self.config.consensus.block_migration_depth
            + 5;
```

- [ ] **Step 4: Update the two call sites** in `prune_pending_txs`:
- data loop (:368): `if self.should_prune_tx(current_height, tx, self.config.consensus.mempool.tx_anchor_expiry_depth as u32) {`
- commitment loop (:380): `if self.should_prune_tx(current_height, tx, self.config.consensus.mempool.commitment_anchor_expiry_depth as u32) {`

- [ ] **Step 5: Run to verify it passes**

Run: `cargo nextest run -p chain-tests <test name>`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/actors/src/mempool_service/lifecycle.rs crates/chain-tests/src/
git commit --no-gpg-sign -m "feat(mempool): prune commitments against the commitment window"
```

---

### Task B5: Block production selects commitments over the commitment window

**Files:**
- Modify: `crates/actors/src/tx_selector/mod.rs:185-296` — add a commitment-specific min anchor height; use it in the commitment loop's `validate_anchor_for_inclusion` at :279-285.

**Interfaces:**
- Consumes: `commitment_anchor_expiry_depth`. Data-tx selection min is unchanged.

- [ ] **Step 1: Write the failing test** — mine so a commitment's anchor is older than `tx_anchor_expiry_depth` but within `commitment_anchor_expiry_depth`, ingest it, mine one block, assert it appears in `get_block_by_height(h).commitment_tx_ids()`. (Extends the B3 test: after ingest, `node.mine_block()` and assert inclusion.)

```rust
let blk = node.mine_block().await?;
assert!(blk.commitment_tx_ids().contains(&pledge.id()),
    "commitment with in-window anchor must be selected into a block");
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo nextest run -p chain-tests <test>`
Expected: FAIL — commitment not selected (anchor outside the tx-depth selection window).

- [ ] **Step 3: Implement** — after `max_anchor_height` (:190-191) add:

```rust
    // Commitments use a longer expiry window than data txs; keep the same
    // maturity upper bound (max_anchor_height).
    let commitment_min_anchor_height = current_height.saturating_sub(
        (ctx.config.consensus.mempool.commitment_anchor_expiry_depth as u64)
            .saturating_sub(ctx.config.consensus.block_migration_depth as u64),
    );
```
and in the commitment loop change the `validate_anchor_for_inclusion` call (:279-285) to pass `commitment_min_anchor_height` instead of `min_anchor_height`. Leave the data-tx (`submit_txs`) selection using `min_anchor_height`.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo nextest run -p chain-tests <test>`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/actors/src/tx_selector/mod.rs crates/chain-tests/src/
git commit --no-gpg-sign -m "feat(block-producer): select commitments over the commitment window"
```

---

### Task B6: Block validation accepts commitment anchors over the commitment window

**Files:**
- Modify: `crates/actors/src/block_discovery.rs:452-624` — the anchor-validation section. Currently commitments validate against `valid_tx_anchor_blocks` (the `tx_anchor_expiry_depth` by-hash set). They must validate against the commitment window `[block_height - commitment_anchor_expiry_depth, block_height]`, resolved branch-correctly by-hash within the reorg window and via the finalized block index below it — the same machinery that already builds `valid_ingress_anchor_blocks`.
- Test: `crates/chain-tests/src/validation/commitment_replay.rs` or `.../commitment_anchor_window.rs` — evil-block with a commitment whose anchor is within the commitment window but older than `tx_anchor_expiry_depth`: must VALIDATE (accepted); with an anchor older than `commitment_anchor_expiry_depth`: must be REJECTED (`InvalidAnchor`).

**Interfaces:**
- Consumes: `commitment_anchor_expiry_depth`, `ingress_proof_anchor_expiry_depth`, the existing by-hash-walk + block-index-fallback code (:476-591).
- Produces: commitment anchors accepted iff within `[block_height - commitment_anchor_expiry_depth, block_height]` and canonical for this branch.

Implementation approach (refactor, guided by the test — this is the most intricate task; make it surgical): the existing code builds `valid_tx_anchor_blocks` (by-hash, to `min_tx_anchor_height`) and extends it to `valid_ingress_anchor_blocks` (by-hash boundary + block index down to `min_ingress_proof_anchor_height`). Generalize the deep set to reach `min_deep = block_height.saturating_sub(max(commitment_anchor_expiry_depth, ingress_proof_anchor_expiry_depth) as u64)`, recording **height per hash** so each item type can be range-checked. Concretely, change the deep set from a `Vec<H256>` to a `HashMap<H256, u64>` (hash → height), extend the by-hash walk to `block_tree_depth` and the block-index loop to `min_deep`, then validate:
- submit txs: `anchor` present with height `>= min_tx_anchor_height` (unchanged behavior),
- commitments: `anchor` present with height `>= block_height - commitment_anchor_expiry_depth`,
- ingress proofs: `anchor` present with height `>= min_ingress_proof_anchor_height`.

Keep the epoch-block commitment exemption (:615) and the A3 replay check. Preserve the by-hash-vs-index branch-safety boundary (`bt_finished_height`, `last_bt_safe_parent_height` handoff at :566-591) — the block index is only consulted below the by-hash floor.

- [ ] **Step 1: Write the failing test**

```rust
#[test_log::test(tokio::test)]
async fn heavy_commitment_in_window_anchor_validates() -> eyre::Result<()> {
    initialize_tracing();
    let mut config = NodeConfig::testing(); // tx 20, commitment 100, block_tree_depth 50
    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&signer]);
    let node = IrysNodeTest::new_genesis(config).start_and_wait_for_packing("N", 10).await;

    let old_anchor = node.mine_block().await?.block_hash; // height 1
    node.mine_blocks(30).await?; // anchor 30 deep: > tx window (20), < commitment window (100), > block_tree floor? (tip~31, floor tip-50 => within reorg window)

    node.post_stake_commitment_with_signer(&signer).await;
    let mut pledge = CommitmentTransaction::new_pledge(
        &node.node_ctx.config.consensus, old_anchor, &node.node_ctx, signer.address()).await;
    signer.sign_commitment(&mut pledge)?;
    node.ingest_commitment_tx(pledge.clone()).await?;

    let blk = node.mine_block_and_wait_for_validation().await?;
    assert!(matches!(blk.3, BlockValidationOutcome::StoredOnNode(_)),
        "block with in-window commitment anchor must validate: {:?}", blk.3);
    assert!(blk.0.commitment_tx_ids().contains(&pledge.id()));
    node.stop().await;
    Ok(())
}
```

Add a companion evil-block test posting a commitment whose anchor is older than `commitment_anchor_expiry_depth` (mine >100 blocks past `old_anchor`) and assert `Discarded(InvalidAnchor{..})`.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo nextest run -p chain-tests heavy_commitment_in_window_anchor_validates`
Expected: FAIL — anchor rejected (commitment still validated against the tx-depth set).

- [ ] **Step 3: Implement the refactor** described above in `block_discovery.rs`. Keep changes surgical; do not alter the submit/ingress semantics. Verify branch-safety comments (:537-548, :567-571) remain accurate after switching to the hash→height map.

- [ ] **Step 4: Run to verify it passes** (both new tests)

Run: `cargo nextest run -p chain-tests heavy_commitment_in_window_anchor_validates && cargo nextest run -p chain-tests <old-anchor-rejected test>`
Expected: PASS.

- [ ] **Step 5: Regression — full prevalidation + ingress-proof anchor tests**

Run: `cargo nextest run -p chain-tests ingress_proof_reanchor_dedup && cargo nextest run -p chain-tests -E 'test(/block_validation/)'`
Expected: PASS (ingress-proof and submit anchor behavior unchanged).

- [ ] **Step 6: Commit**

```bash
git add crates/actors/src/block_discovery.rs crates/chain-tests/src/validation/
git commit --no-gpg-sign -m "feat(consensus): validate commitment anchors over the commitment window"
```

---

### Task B7: End-to-end acceptance test + full local checks

**Files:**
- Test: `crates/chain-tests/src/validation/commitment_anchor_window.rs` (a single end-to-end test tying ingress → not-pruned → selected → validated for a commitment anchored beyond `tx_anchor_expiry_depth`, contrasted with a data tx that is rejected at that age).

- [ ] **Step 1: Write the end-to-end test** — combine the B3–B6 assertions in one flow: post commitment with an anchor aged between `tx_anchor_expiry_depth` and `commitment_anchor_expiry_depth`; confirm it survives a prune cycle, is selected, the block validates, and the same-age data tx is rejected at ingress.

- [ ] **Step 2: Run**

Run: `cargo nextest run -p chain-tests -E 'test(/commitment_anchor/) + test(/commitment_replay/)'`
Expected: PASS.

- [ ] **Step 3: Full local checks**

Run: `cargo fmt --all && cargo xtask local-checks`
Expected: fmt clean, clippy clean, no unused deps, typos clean.

- [ ] **Step 4: Commit**

```bash
git add crates/chain-tests/src/
git commit --no-gpg-sign -m "test(consensus): end-to-end commitment anchor window"
```

---

## Self-Review

**Spec coverage:**
- Part 1 config field + defaults + invariant + uncapped-vs-block_tree_depth → B1. TOMLs → B2. consensus_config_hash implication is inherent (documented in Global Constraints; no code beyond B1).
- Part 1 four sites: ingress → B3, pruning → B4, production → B5, validation → B6.
- Part 2 durable replay protection: finalized DB helper → A1; by-hash reorg-window walk → A2; both validator paths → A3 (block_discovery) + A4 (commitment_txs_are_valid); branch-safety → A5. `commitment_txs_are_valid` same-tx_id discrepancy is subsumed by the explicit A4 dedup.
- Test plan (spec §Test plan): config validation → B1; anchor window → B3/B5/B6/B7; anchor beyond block_tree_depth → B6; replay across epoch → A3/A4; branch-safety → A5.
- Spec "open implementation detail" is RESOLVED: reuse `IrysCommitmentTxMetadata.included_height` (written at migration by `persist_block`, cleared only on orphan) + MBH cross-check — no new table. Update the spec's open-detail section to record this.

**Placeholder scan:** A2 Step 1 and A5 Step 1 intentionally defer body detail to referenced existing tests with explicit line numbers and a "do not leave placeholder" instruction; every other step carries concrete code or exact commands. B3/B4 note grep-for-exact-name where a harness method name must be confirmed against `utils.rs`.

**Type consistency:** `commitment_anchor_expiry_depth: u16` throughout; cast `as u64` (ingress/production/DB max_height) and `as u32` (pruning) matches existing `tx_anchor_expiry_depth` cast sites. `canonical_commitment_included_height(tx, txid, max_height) -> eyre::Result<Option<u64>>` used consistently in A1/A3/A4. `ancestor_commitment_tx_ids(...) -> eyre::Result<HashSet<H256>>` used consistently in A2/A3/A4. `ValidationError::DuplicateCommitmentTransaction { tx_id: H256 }` and `BlockDiscoveryError::DuplicateTransaction(tx_id)` used as defined.
