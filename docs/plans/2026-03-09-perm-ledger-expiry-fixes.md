# Publish Ledger Expiry — Security Review Fixes

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Fix all 4 findings from the Codex + Claude security review (`docs/plans/2026-03-09-perm-ledger-expiry-codex-review.md`)

**Architecture:** 4 independent fixes ordered by severity. Finding 1 fixes cross-ledger coupling by filtering before lookup. Finding 2 strengthens the integration test. Finding 3 adds overflow protection. Finding 4 moves the dead-code guard to a useful position.

**Tech Stack:** Rust, `eyre`, `checked_mul`, existing test harness (`IrysNodeTest`)

---

### Task 1: Fix cross-ledger coupling in `collect_expired_partitions` (Finding 1 — Medium-High)

**Files:**
- Modify: `crates/actors/src/block_producer/ledger_expiry.rs:290-333`

**Problem:** The loop calls `partition_assignments.get_assignment()` for **every** expired partition (both Publish and Submit) before filtering by ledger type. A missing Publish partition would bail the entire function and block Submit fee distribution.

**Step 1: Move the ledger type filter before the assignment lookup**

In `crates/actors/src/block_producer/ledger_expiry.rs`, replace lines 290-333 with:

```rust
    for expired_partition in expired_partition_info {
        let ledger_id = expired_partition.ledger_id;
        let slot_index = SlotIndex::new(expired_partition.slot_index as u64);

        // Filter by ledger type FIRST — before any lookup that could fail.
        // This prevents a Publish partition state inconsistency from blocking
        // Submit fee distribution (or vice versa).
        if ledger_id != target_ledger_type {
            tracing::debug!(
                "Skipping partition with ledger_id={:?} (looking for {:?})",
                ledger_id,
                target_ledger_type
            );
            continue;
        }

        let partition = partition_assignments
            .get_assignment(expired_partition.partition_hash)
            .ok_or_eyre("could not get expired partition")?;

        tracing::info!(
            "Found expired partition for {:?} ledger at slot_index={}, miner={:?}",
            ledger_id,
            slot_index.0,
            partition.miner_address
        );

        // Store miner_address (not reward_address) to preserve unique miner identities
        // for correct fee distribution. Reward address resolution is deferred to
        // aggregate_balance_deltas to ensure pooled miners (sharing a reward address)
        // are counted individually for fee splitting.
        expired_ledger_slot_indexes
            .entry(slot_index)
            .and_modify(|miners: &mut Vec<IrysAddress>| {
                miners.push(partition.miner_address);
            })
            .or_insert(vec![partition.miner_address]);
    }
```

This also removes the dead bail guard from Finding 4 — it lived inside the old `if ledger_id == target_ledger_type` block which is now an early `continue`. The guard is replaced in Task 4 below.

**Step 2: Run compile check**

Run: `cargo check -p irys-actors`
Expected: PASS

**Step 3: Run existing tests**

Run: `cargo nextest run -p irys-actors --filter-expr 'test(ledger_expiry)'`
Expected: PASS (existing tests should still work)

**Step 4: Commit**

```
fix: filter by ledger type before partition lookup in collect_expired_partitions

Prevents a Publish partition state inconsistency from blocking Submit fee
distribution. Previously, get_assignment() was called for ALL expired
partitions before the ledger type check — a missing Publish partition would
bail the entire function.

Fixes security review Finding 1 (Medium-High).
```

---

### Task 2: Add `debug_assert` guard at `calculate_expired_ledger_fees` entry (Finding 4 — Low)

**Files:**
- Modify: `crates/actors/src/block_producer/ledger_expiry.rs:87-100`

**Problem:** The old bail guard inside `collect_expired_partitions` was unreachable dead code. A future caller passing `DataLedger::Publish` would only discover the error at runtime. A `debug_assert` at the function entry point catches this during development.

**Step 1: Add debug_assert at the top of `calculate_expired_ledger_fees`**

In `crates/actors/src/block_producer/ledger_expiry.rs`, after the function signature (line 97) and before the `collect_expired_partitions` call (line 99), add:

```rust
    // Fee distribution is only implemented for Submit ledger. Publish expiry
    // simply resets partitions without fee redistribution.
    debug_assert_ne!(
        ledger_type,
        DataLedger::Publish,
        "fee distribution not supported for Publish ledger"
    );
```

**Step 2: Run compile check**

Run: `cargo check -p irys-actors`
Expected: PASS

**Step 3: Commit**

```
fix: add debug_assert preventing Publish fee distribution

Moves the unreachable bail guard from collect_expired_partitions to a
debug_assert at the calculate_expired_ledger_fees entry point. This
catches misuse during development without runtime overhead in release.

Fixes security review Finding 4 (Low).
```

---

### Task 3: Add `checked_mul` for expiry arithmetic (Finding 3 — Low)

**Files:**
- Modify: `crates/database/src/data_ledger.rs:82-103` (TermLedger)
- Modify: `crates/database/src/data_ledger.rs:293-300` (Publish expire_partitions)
- Modify: `crates/database/src/data_ledger.rs:343-350` (Publish get_expiring_partitions)

**Problem:** `epoch_length * num_blocks_in_epoch` is unchecked in 3 locations. Extreme config values could overflow and wrap in release builds.

**Step 1: Fix TermLedger::get_expired_slot_indexes (lines 82-103)**

Create a helper to compute `min_blocks` with `checked_mul`. Replace the raw multiplications in the function:

```rust
    pub fn get_expired_slot_indexes(&self, epoch_height: u64) -> Vec<usize> {
        let mut expired_slot_indexes = Vec::new();

        let min_blocks = self
            .epoch_length
            .checked_mul(self.num_blocks_in_epoch)
            .expect("epoch_length * num_blocks_in_epoch overflows u64");

        tracing::debug!(
            "expire_old_slots: epoch_height={}, epoch_length={}, num_blocks_in_epoch={}, min_height_needed={}",
            epoch_height,
            self.epoch_length,
            self.num_blocks_in_epoch,
            min_blocks
        );

        // Make sure enough blocks have transpired before calculating expiry height
        if epoch_height < min_blocks {
            tracing::warn!(
                "Not enough blocks yet: {} < {}, returning empty",
                epoch_height,
                min_blocks
            );
            return expired_slot_indexes;
        }

        let expiry_height = epoch_height - min_blocks;
        tracing::info!("Calculated expiry_height={}", expiry_height);
```

(rest of function unchanged)

**Step 2: Fix Ledgers::expire_partitions (line 298)**

Replace:
```rust
            let min_blocks = epoch_length * self.num_blocks_in_epoch;
```
With:
```rust
            let min_blocks = epoch_length
                .checked_mul(self.num_blocks_in_epoch)
                .expect("publish_ledger_epoch_length * num_blocks_in_epoch overflows u64");
```

**Step 3: Fix Ledgers::get_expiring_partitions (line 348)**

Same replacement:
```rust
            let min_blocks = epoch_length
                .checked_mul(self.num_blocks_in_epoch)
                .expect("publish_ledger_epoch_length * num_blocks_in_epoch overflows u64");
```

**Step 4: Run compile check**

Run: `cargo check -p irys-database`
Expected: PASS

**Step 5: Run existing expiry tests**

Run: `cargo nextest run -p irys-database --filter-expr 'test(expir)'`
Expected: PASS

**Step 6: Commit**

```
fix: use checked_mul for expiry height arithmetic

All 3 locations computing epoch_length * num_blocks_in_epoch now use
checked_mul with a descriptive panic. This prevents silent overflow
in release builds with extreme config values.

Applied to TermLedger::get_expired_slot_indexes, Ledgers::expire_partitions,
and Ledgers::get_expiring_partitions for consistency.

Fixes security review Finding 3 (Low).
```

---

### Task 4: Strengthen integration test assertions (Finding 2 — Medium)

**Files:**
- Modify: `crates/chain-tests/src/perm_ledger_expiry/mod.rs:6-77`

**Problem:** The test only asserts `final_height >= target_height`. It doesn't verify that Publish slots actually expired, that no fee distribution shadow txs were generated, or that expired partitions returned to the capacity pool.

**Context:** The test uses `IrysNodeTest` harness. Key patterns from the term ledger expiry test:
- `node.get_canonical_epoch_snapshot()` returns `Arc<EpochSnapshot>`
- `epoch_snapshot.ledgers.perm.slots` has slot state including `is_expired`
- Shadow transactions can be decoded from EVM blocks via `ShadowTransaction::decode`

**Step 1: Add imports needed for assertions**

At the top of the file, add:
```rust
use irys_types::block::DataLedger;
```

**Step 2: Replace the minimal assertion block with meaningful checks**

After `node.mine_block()` loop and the `final_height` check (keep that assertion), add:

```rust
    // --- Assertion 1: Publish ledger slots are marked expired ---
    let epoch_snapshot = node.get_canonical_epoch_snapshot();
    let perm_slots = &epoch_snapshot.ledgers.perm.slots;
    let num_slots = perm_slots.len();

    // At least one non-last slot should be expired (last slot is protected)
    let expired_count = perm_slots
        .iter()
        .enumerate()
        .filter(|(i, slot)| *i < num_slots - 1 && slot.is_expired)
        .count();
    assert!(
        expired_count > 0,
        "Expected at least one non-last perm slot to be expired after height {}",
        final_height
    );
    info!("{} of {} perm slots are expired", expired_count, num_slots);

    // --- Assertion 2: No TermFeeReward shadow txs for Publish expiry ---
    // Perm expiry should NOT generate fee distribution shadow transactions.
    // Check the epoch block (the block at the epoch boundary where expiry happens).
    let epoch_height = BLOCKS_PER_EPOCH * (PUBLISH_LEDGER_EPOCH_LENGTH + 1);
    if let Ok(epoch_block) = node.get_block_by_height(epoch_height).await {
        let evm_block = node
            .wait_for_evm_block(epoch_block.evm_block_hash, 30)
            .await?;
        for tx in &evm_block.body.transactions {
            let mut input = tx.input().as_ref();
            if let Ok(shadow) =
                irys_reth_node_bridge::irys_reth::shadow_tx::ShadowTransaction::decode(&mut input)
            {
                if let Some(packet) = shadow.as_v1() {
                    // TermFeeReward should never appear for Publish ledger expiry
                    assert!(
                        !matches!(
                            packet,
                            irys_reth_node_bridge::irys_reth::shadow_tx::TransactionPacket::TermFeeReward(_)
                        ),
                        "Unexpected TermFeeReward shadow tx in perm expiry epoch block"
                    );
                }
            }
        }
        info!("Verified no TermFeeReward shadow txs in epoch block at height {}", epoch_height);
    }

    // --- Assertion 3: Expired partitions returned to capacity pool ---
    let partition_assignments = &epoch_snapshot.partition_assignments;
    for (slot_index, slot) in perm_slots.iter().enumerate() {
        if slot_index < num_slots - 1 && slot.is_expired {
            for partition_hash in &slot.partitions {
                if let Some(assignment) = partition_assignments.get_assignment(*partition_hash) {
                    // Expired perm partitions should have no ledger_id (returned to capacity)
                    assert!(
                        assignment.ledger_id.is_none(),
                        "Expired perm partition {:?} at slot {} should be in capacity pool but has ledger_id={:?}",
                        partition_hash,
                        slot_index,
                        assignment.ledger_id
                    );
                }
            }
        }
    }
    info!("Verified expired perm partitions are in capacity pool");
```

**Step 3: Run the test to verify assertions pass**

Run: `cargo nextest run -p irys-chain-tests --filter-expr 'test(perm_ledger_expiry)' --no-capture`
Expected: PASS with new assertion log messages

NOTE: This is a `heavy_` integration test — it may take 30-60s. If any assertion fails, investigate the actual epoch state before adjusting — the test config uses `BLOCKS_PER_EPOCH=3` and `PUBLISH_LEDGER_EPOCH_LENGTH=2`, so expiry triggers at epoch height 9.

**Step 4: Commit**

```
test: add expiry state assertions to perm_ledger_expiry integration test

Verifies:
- Perm slots are marked is_expired after expiry height
- No TermFeeReward shadow txs in the expiry epoch block
- Expired partitions are returned to capacity pool

Fixes security review Finding 2 (Medium).
```

---

## Execution Order

| Order | Task | Finding | Severity | Independent? |
|-------|------|---------|----------|-------------|
| 1 | Task 1 + Task 2 | Findings 1 & 4 | Med-High + Low | Same file, do together |
| 2 | Task 3 | Finding 3 | Low | Independent |
| 3 | Task 4 | Finding 2 | Medium | Independent (benefits from Tasks 1-3 being merged first) |

Tasks 1 & 2 touch the same file (`ledger_expiry.rs`) and are logically connected — Task 1 removes the dead bail guard, Task 2 replaces it with a `debug_assert` at the right level. Do them as sequential commits.

Task 3 is fully independent (different file).

Task 4 (integration test) should run last to verify the whole system works with all fixes applied.
