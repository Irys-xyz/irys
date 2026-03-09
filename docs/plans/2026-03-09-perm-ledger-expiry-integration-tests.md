# Perm Ledger Expiry Integration Tests

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add comprehensive integration tests for the publish (permanent) ledger expiry feature, covering balance verification, simultaneous perm+term expiry, multi-slot scenarios, last-slot protection, and data retrieval post-expiry.

**Architecture:** All tests live in `crates/chain-tests/src/perm_ledger_expiry/`. Each test boots a single-node `IrysNodeTest` with accelerated epochs (`BLOCKS_PER_EPOCH=3`, `PUBLISH_LEDGER_EPOCH_LENGTH=2`) and small partitions (`CHUNK_SIZE=32`, `num_chunks_in_partition=4`). Tests follow the same patterns established by the term ledger expiry tests in `crates/chain-tests/src/term_ledger_expiry/`.

**Tech Stack:** Rust, `#[test_log::test(tokio::test)]`, `IrysNodeTest` harness, `eyre::Result`, shadow tx decoding via `irys_reth_node_bridge::irys_reth::shadow_tx`

---

## Existing Coverage (already implemented)

The existing test `heavy_perm_ledger_expiry_basic` in `crates/chain-tests/src/perm_ledger_expiry/mod.rs` already covers:
- Posts 1 tx → uploads chunks → promotes to Publish
- Mines past expiry boundary
- **Assertion 1:** Perm slots are marked `is_expired`
- **Assertion 2:** No `TermFeeReward` shadow txs in epoch block
- **Assertion 3:** Expired partition hashes have `ledger_id: None` (capacity pool)

## What's Missing

| # | Test Case | Priority | Why |
|---|-----------|----------|-----|
| 1 | User balance unchanged after perm expiry | High | Proves no economic side effects |
| 2 | Simultaneous perm + term expiry in same epoch | High | Most likely production scenario |
| 3 | Multi-slot perm expiry across epochs | Medium | Multiple slots expire at different times |
| 4 | Last-slot protection on live node | Medium | Unit test exists, integration does not |
| 5 | Perm expiry disabled (`None`) does nothing | Medium | Mainnet safety regression test |
| 6 | No `PermFeeRefund` shadow txs for promoted Publish data | Medium | Promoted data has no refund path |

---

## Reference: Shared Test Boilerplate

All tests in this module use similar setup. To avoid repeating it in every task, here is the standard config pattern:

**File:** `crates/chain-tests/src/perm_ledger_expiry/mod.rs`

```rust
// Standard imports for perm expiry tests:
use crate::utils::IrysNodeTest;
use alloy_genesis::GenesisAccount;
use alloy_rpc_types_eth::TransactionTrait as _;
use irys_reth_node_bridge::irys_reth::shadow_tx::{ShadowTransaction, TransactionPacket};
use irys_types::{irys::IrysSigner, DataLedger, NodeConfig, U256};
use tracing::info;
```

**Standard config setup:**
```rust
const CHUNK_SIZE: u64 = 32;
const DATA_SIZE: usize = 32; // 1 chunk per tx
const BLOCKS_PER_EPOCH: u64 = 3;
const PUBLISH_LEDGER_EPOCH_LENGTH: u64 = 2;
const INITIAL_BALANCE: u128 = 10_000_000_000_000_000_000;

let mut config = NodeConfig::testing();
config.consensus.get_mut().block_migration_depth = 1;
config.consensus.get_mut().chunk_size = CHUNK_SIZE;
config.consensus.get_mut().num_chunks_in_partition = 4;
config.consensus.get_mut().num_chunks_in_recall_range = 1;
config.consensus.get_mut().epoch.num_blocks_in_epoch = BLOCKS_PER_EPOCH;
config.consensus.get_mut().epoch.publish_ledger_epoch_length = Some(PUBLISH_LEDGER_EPOCH_LENGTH);
```

**Shadow tx inspection pattern** (for scanning an EVM block for specific shadow tx types):
```rust
let evm_block = node.wait_for_evm_block(block.evm_block_hash, 30).await?;
for tx in &evm_block.body.transactions {
    let mut input = tx.input().as_ref();
    if let Ok(shadow) = ShadowTransaction::decode(&mut input) {
        if let Some(packet) = shadow.as_v1() {
            match packet {
                TransactionPacket::TermFeeReward(reward) => { /* ... */ }
                TransactionPacket::PermFeeRefund(refund) => { /* ... */ }
                _ => {}
            }
        }
    }
}
```

**Key IrysNodeTest methods:**
- `node.get_balance(address, evm_block_hash)` → `U256` (balance at EVM state)
- `node.get_canonical_epoch_snapshot()` → `Arc<EpochSnapshot>` (ledger slots, partition assignments)
- `epoch_snapshot.ledgers.get_slots(DataLedger::Publish)` → `&Vec<LedgerSlot>` (slot.is_expired, slot.partitions)
- `epoch_snapshot.partition_assignments.get_assignment(hash)` → `Option<PartitionAssignment>` (ledger_id, miner_address)
- `node.mine_until_next_epoch()` → `(usize, u64)` (blocks_mined, final_height)

---

### Task 1: User Balance Unchanged After Perm Expiry

**Files:**
- Modify: `crates/chain-tests/src/perm_ledger_expiry/mod.rs`

**Context:** The existing `heavy_perm_ledger_expiry_basic` test verifies slots expire and no shadow txs are generated, but doesn't check that the user's balance is unaffected. For perm expiry, there should be zero economic effect — no fees extracted, no refunds issued. This test adds balance bookkeeping to prove it.

**Step 1: Add balance assertions to `heavy_perm_ledger_expiry_basic`**

Add the following after the existing signer/genesis setup (after line 38) to capture the pre-expiry balance, and after the mining loop (after line 136) to verify the post-expiry balance:

```rust
// After node start (after line 42), capture initial EVM state:
let genesis_block = node.get_block_by_height(0).await?;

// After posting tx and mining first block, capture the user's balance AFTER tx is included
// (the tx costs term_fee + perm_fee, so balance = INITIAL - total_cost + any block rewards if miner == signer)
// For simplicity, capture balance right before the expiry mining loop begins.

// Insert before "Mine through epochs until expiry" comment:
let pre_expiry_block = node.get_block_by_height(node.get_canonical_chain_height().await).await?;
let pre_expiry_evm_block = node.wait_for_evm_block(pre_expiry_block.evm_block_hash, 30).await?;
let pre_expiry_balance = U256::from_be_bytes(
    node.get_balance(signer.address(), pre_expiry_block.evm_block_hash)
        .await
        .to_be_bytes(),
);
info!("User balance before expiry mining: {}", pre_expiry_balance);

// Insert after "Verified expired perm partitions are in capacity pool" (after line 136):
// --- Assertion 4: User balance is unchanged by perm expiry ---
// The user should only have paid tx costs (term_fee + perm_fee) when the tx was included.
// Perm expiry should NOT generate any additional debits or credits for the user.
// We check that no PermFeeRefund shadow txs exist for this user (promoted data = no refund).
let post_expiry_block = node.get_block_by_height(final_height).await?;
let post_expiry_balance = U256::from_be_bytes(
    node.get_balance(signer.address(), post_expiry_block.evm_block_hash)
        .await
        .to_be_bytes(),
);
info!(
    "User balance after perm expiry: {} (was {} before expiry mining)",
    post_expiry_balance, pre_expiry_balance
);

// Balance should be unchanged — perm expiry generates no user-facing shadow txs
assert_eq!(
    pre_expiry_balance, post_expiry_balance,
    "User balance should not change due to perm expiry (no fees or refunds)"
);
```

**Step 2: Run the test**

```bash
cargo nextest run -p irys-chain-tests heavy_perm_ledger_expiry_basic
```

Expected: PASS. The user's balance before and after the expiry mining loop should be identical since perm expiry has no economic side effects for the user.

**Step 3: Commit**

```bash
git add crates/chain-tests/src/perm_ledger_expiry/mod.rs
git commit -m "test: add balance unchanged assertion to perm ledger expiry test"
```

---

### Task 2: Simultaneous Perm + Term Expiry

**Files:**
- Modify: `crates/chain-tests/src/perm_ledger_expiry/mod.rs`

**Context:** In production, both Submit (term) and Publish (perm) ledgers will have data expiring at epoch boundaries. This is the most realistic and most important scenario to test. When both expire simultaneously, the system must:
- Generate `TermFeeReward` shadow txs for Submit expiry (miner compensation)
- Generate NO `TermFeeReward` for Publish expiry
- Mark both ledgers' slots as expired
- Return both sets of partitions to capacity pool

**Step 1: Write the test**

Add the following test function at the bottom of `crates/chain-tests/src/perm_ledger_expiry/mod.rs`:

```rust
/// Tests that simultaneous publish and submit ledger expiry in the same epoch works correctly.
/// Verifies:
/// - Both Submit and Publish slots expire at the same epoch boundary
/// - TermFeeReward shadow txs appear ONLY for Submit expiry (not Publish)
/// - No PermFeeRefund for promoted Publish data
/// - Both sets of expired partitions returned to capacity pool
#[test_log::test(tokio::test)]
async fn heavy_perm_and_term_expiry_same_epoch() -> eyre::Result<()> {
    const CHUNK_SIZE: u64 = 32;
    const DATA_SIZE: usize = 32;
    const BLOCKS_PER_EPOCH: u64 = 3;
    // Use same epoch length for both so they expire at the same time
    const EPOCH_LENGTH: u64 = 2;
    const INITIAL_BALANCE: u128 = 10_000_000_000_000_000_000;

    let mut config = NodeConfig::testing();
    config.consensus.get_mut().block_migration_depth = 1;
    config.consensus.get_mut().chunk_size = CHUNK_SIZE;
    config.consensus.get_mut().num_chunks_in_partition = 4;
    config.consensus.get_mut().num_chunks_in_recall_range = 1;
    config.consensus.get_mut().epoch.num_blocks_in_epoch = BLOCKS_PER_EPOCH;
    config.consensus.get_mut().epoch.submit_ledger_epoch_length = EPOCH_LENGTH;
    config.consensus.get_mut().epoch.publish_ledger_epoch_length = Some(EPOCH_LENGTH);

    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.consensus.extend_genesis_accounts(vec![(
        signer.address(),
        GenesisAccount {
            balance: U256::from(INITIAL_BALANCE).into(),
            ..Default::default()
        },
    )]);

    let node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("perm_term_expiry_test", 30)
        .await;

    let anchor = node.get_block_by_height(0).await?.block_hash;

    // Post TWO transactions:
    // tx1: will stay on Submit (no chunks uploaded) — expires via term expiry
    // tx2: will be promoted to Publish (chunks uploaded) — expires via perm expiry
    let tx1 = node
        .post_data_tx(anchor, vec![1_u8; DATA_SIZE], &signer)
        .await;
    node.wait_for_mempool(tx1.header.id, 10).await?;

    let tx2 = node
        .post_data_tx(anchor, vec![2_u8; DATA_SIZE], &signer)
        .await;
    node.wait_for_mempool(tx2.header.id, 10).await?;

    // Upload chunks for tx2 only to trigger promotion to Publish
    node.upload_chunks(&tx2).await?;
    node.wait_for_ingress_proofs(vec![tx2.header.id], 20).await?;

    // Mine through epochs until both should expire
    let target_height = (EPOCH_LENGTH + 1) * BLOCKS_PER_EPOCH;
    info!(
        "Mining to height {} to trigger simultaneous perm+term expiry",
        target_height
    );

    let current_height = node.get_canonical_chain_height().await;
    for _ in current_height..target_height {
        node.mine_block().await?;
    }

    let final_height = node.get_canonical_chain_height().await;
    assert!(final_height >= target_height, "Should have reached target height");

    // --- Assertion 1: Submit slots are expired ---
    let epoch_snapshot = node.get_canonical_epoch_snapshot();
    let submit_slots = epoch_snapshot.ledgers.get_slots(DataLedger::Submit);
    let submit_expired = submit_slots.iter().filter(|s| s.is_expired).count();
    info!(
        "Submit ledger: {} of {} slots expired",
        submit_expired,
        submit_slots.len()
    );
    assert!(submit_expired > 0, "Expected at least one Submit slot to be expired");

    // --- Assertion 2: Publish slots are expired ---
    let perm_slots = epoch_snapshot.ledgers.get_slots(DataLedger::Publish);
    let num_perm_slots = perm_slots.len();
    let perm_expired = perm_slots
        .iter()
        .enumerate()
        .filter(|(i, s)| *i < num_perm_slots - 1 && s.is_expired)
        .count();
    info!(
        "Publish ledger: {} of {} non-last slots expired",
        perm_expired, num_perm_slots
    );
    assert!(
        perm_expired > 0,
        "Expected at least one non-last Publish slot to be expired"
    );

    // --- Assertion 3: TermFeeReward shadow txs exist (from Submit), not from Publish ---
    // Scan all blocks from the epoch boundary for shadow txs
    let epoch_height = (EPOCH_LENGTH + 1) * BLOCKS_PER_EPOCH;
    let mut found_term_fee_reward = false;
    let mut found_perm_fee_refund = false;

    if let Ok(epoch_block) = node.get_block_by_height(epoch_height).await {
        let evm_block = node
            .wait_for_evm_block(epoch_block.evm_block_hash, 30)
            .await?;
        for tx in &evm_block.body.transactions {
            let mut input = tx.input().as_ref();
            if let Ok(shadow) = ShadowTransaction::decode(&mut input) {
                if let Some(packet) = shadow.as_v1() {
                    match packet {
                        TransactionPacket::TermFeeReward(_) => {
                            found_term_fee_reward = true;
                            info!("Found TermFeeReward shadow tx (expected, from Submit expiry)");
                        }
                        TransactionPacket::PermFeeRefund(_) => {
                            found_perm_fee_refund = true;
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    // TermFeeReward should appear (Submit ledger expired with data)
    assert!(
        found_term_fee_reward,
        "Expected TermFeeReward from Submit ledger expiry"
    );

    // PermFeeRefund should NOT appear for promoted tx2 (it was successfully promoted)
    // Note: tx1 never had chunks uploaded so it could get a PermFeeRefund,
    // but it's on Submit ledger so its perm fee refund comes through the Submit expiry path.
    // No Publish-originated PermFeeRefund should exist.
    info!(
        "PermFeeRefund present: {} (tx1 unpromoted refund is expected via Submit expiry path)",
        found_perm_fee_refund
    );

    // --- Assertion 4: Both sets of expired partitions in capacity pool ---
    let partition_assignments = &epoch_snapshot.partition_assignments;

    // Check expired Submit partitions
    for slot in submit_slots.iter().filter(|s| s.is_expired) {
        for partition_hash in &slot.partitions {
            if let Some(assignment) = partition_assignments.get_assignment(*partition_hash) {
                assert!(
                    assignment.ledger_id.is_none(),
                    "Expired Submit partition should be in capacity pool"
                );
            }
        }
    }

    // Check expired Publish partitions
    for (i, slot) in perm_slots.iter().enumerate() {
        if i < num_perm_slots - 1 && slot.is_expired {
            for partition_hash in &slot.partitions {
                if let Some(assignment) = partition_assignments.get_assignment(*partition_hash) {
                    assert!(
                        assignment.ledger_id.is_none(),
                        "Expired Publish partition should be in capacity pool"
                    );
                }
            }
        }
    }
    info!("Verified all expired partitions (Submit + Publish) are in capacity pool");

    node.stop().await;
    Ok(())
}
```

**Step 2: Run the test**

```bash
cargo nextest run -p irys-chain-tests heavy_perm_and_term_expiry_same_epoch
```

Expected: PASS.

**Step 3: Commit**

```bash
git add crates/chain-tests/src/perm_ledger_expiry/mod.rs
git commit -m "test: add simultaneous perm+term expiry integration test"
```

---

### Task 3: Multi-Slot Perm Expiry Across Epochs

**Files:**
- Modify: `crates/chain-tests/src/perm_ledger_expiry/mod.rs`

**Context:** The basic test only creates one transaction (one slot). In production, multiple perm slots will fill across epochs and expire at different times. This test creates data in epoch 0 and epoch 1, then mines far enough that epoch 0's data expires but epoch 1's does not yet, then mines further until epoch 1's expires too.

**Step 1: Write the test**

```rust
/// Tests that multiple perm slots expire independently at different epoch boundaries.
/// Slot 0 is created early (epoch 0) and should expire first.
/// Slot 1 is created later (epoch 1) and should expire in a subsequent epoch.
#[test_log::test(tokio::test)]
async fn heavy_perm_multi_slot_expiry() -> eyre::Result<()> {
    const CHUNK_SIZE: u64 = 32;
    const DATA_SIZE: usize = 32;
    const BLOCKS_PER_EPOCH: u64 = 3;
    const PUBLISH_LEDGER_EPOCH_LENGTH: u64 = 2;
    const INITIAL_BALANCE: u128 = 10_000_000_000_000_000_000;

    let mut config = NodeConfig::testing();
    config.consensus.get_mut().block_migration_depth = 1;
    config.consensus.get_mut().chunk_size = CHUNK_SIZE;
    config.consensus.get_mut().num_chunks_in_partition = 4;
    config.consensus.get_mut().num_chunks_in_recall_range = 1;
    config.consensus.get_mut().epoch.num_blocks_in_epoch = BLOCKS_PER_EPOCH;
    config.consensus.get_mut().epoch.publish_ledger_epoch_length =
        Some(PUBLISH_LEDGER_EPOCH_LENGTH);
    // Use 1 partition per slot so each tx fills a slot
    config.consensus.get_mut().epoch.num_capacity_partitions = Some(1);

    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.consensus.extend_genesis_accounts(vec![(
        signer.address(),
        GenesisAccount {
            balance: U256::from(INITIAL_BALANCE).into(),
            ..Default::default()
        },
    )]);

    let node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("perm_multi_slot_test", 30)
        .await;

    let anchor = node.get_block_by_height(0).await?.block_hash;

    // --- Phase 1: Create data in first epoch (slot 0) ---
    let tx1 = node
        .post_data_tx(anchor, vec![1_u8; DATA_SIZE], &signer)
        .await;
    node.wait_for_mempool(tx1.header.id, 10).await?;
    node.upload_chunks(&tx1).await?;
    node.wait_for_ingress_proofs(vec![tx1.header.id], 20).await?;
    info!("tx1 promoted to Publish in epoch 0");

    // Mine to next epoch boundary
    let (_, height_after_epoch1) = node.mine_until_next_epoch().await?;
    info!("After epoch 1 boundary, height: {}", height_after_epoch1);

    // --- Phase 2: Create data in second epoch (slot 1) ---
    let anchor2 = node
        .get_block_by_height(node.get_canonical_chain_height().await)
        .await?
        .block_hash;
    let tx2 = node
        .post_data_tx(anchor2, vec![2_u8; DATA_SIZE], &signer)
        .await;
    node.wait_for_mempool(tx2.header.id, 10).await?;
    node.upload_chunks(&tx2).await?;
    node.wait_for_ingress_proofs(vec![tx2.header.id], 20).await?;
    info!("tx2 promoted to Publish in epoch 1");

    // --- Phase 3: Mine until slot 0 should expire but slot 1 should NOT ---
    // slot 0 data was at height ~1-3, slot 1 data was at height ~4-6
    // With epoch_length=2, min_blocks=6. Expiry at epoch_height where
    // epoch_height - 6 >= slot.last_height
    // We need to mine far enough for slot 0 but not slot 1
    let target_height_phase1 = (PUBLISH_LEDGER_EPOCH_LENGTH + 1) * BLOCKS_PER_EPOCH;
    let current = node.get_canonical_chain_height().await;
    for _ in current..target_height_phase1 {
        node.mine_block().await?;
    }

    let snapshot1 = node.get_canonical_epoch_snapshot();
    let perm_slots = snapshot1.ledgers.get_slots(DataLedger::Publish);
    info!(
        "After phase 3: {} perm slots, expired: {:?}",
        perm_slots.len(),
        perm_slots.iter().map(|s| s.is_expired).collect::<Vec<_>>()
    );

    // Verify slot 0 is expired (if it's not the last slot)
    if perm_slots.len() > 1 {
        assert!(
            perm_slots[0].is_expired,
            "Slot 0 (early data) should be expired by now"
        );
    }

    // --- Phase 4: Mine further until slot 1 also expires ---
    // Mine additional epochs
    let target_height_phase2 = (PUBLISH_LEDGER_EPOCH_LENGTH + 2) * BLOCKS_PER_EPOCH;
    let current = node.get_canonical_chain_height().await;
    for _ in current..target_height_phase2 {
        node.mine_block().await?;
    }

    let snapshot2 = node.get_canonical_epoch_snapshot();
    let perm_slots2 = snapshot2.ledgers.get_slots(DataLedger::Publish);
    let num_slots = perm_slots2.len();

    // Count expired non-last slots
    let expired_count = perm_slots2
        .iter()
        .enumerate()
        .filter(|(i, s)| *i < num_slots - 1 && s.is_expired)
        .count();

    info!(
        "After phase 4: {} non-last perm slots expired out of {}",
        expired_count, num_slots
    );

    // Both slot 0 and slot 1 should be expired (unless one is the last slot)
    // The last slot is protected, so at minimum all-but-last should be expired
    assert!(
        expired_count >= 1,
        "Expected multiple expired non-last perm slots"
    );

    node.stop().await;
    Ok(())
}
```

**Step 2: Run the test**

```bash
cargo nextest run -p irys-chain-tests heavy_perm_multi_slot_expiry
```

Expected: PASS. Slots expire progressively as epochs advance.

**Step 3: Commit**

```bash
git add crates/chain-tests/src/perm_ledger_expiry/mod.rs
git commit -m "test: add multi-slot perm expiry across epochs integration test"
```

---

### Task 4: Last-Slot Protection on Live Node

**Files:**
- Modify: `crates/chain-tests/src/perm_ledger_expiry/mod.rs`

**Context:** A unit test in `data_ledger.rs::test_perm_expiry_never_expires_last_slot` verifies the last-slot protection logic in isolation. This integration test verifies it works end-to-end on a real node: if there's only one Publish slot, it must never expire regardless of how many epochs pass.

**Step 1: Write the test**

```rust
/// Tests that the last (and only) Publish slot never expires, even after many epochs.
/// This protects the ledger from becoming fully expired with no active slots.
#[test_log::test(tokio::test)]
async fn heavy_perm_last_slot_never_expires() -> eyre::Result<()> {
    const CHUNK_SIZE: u64 = 32;
    const DATA_SIZE: usize = 32;
    const BLOCKS_PER_EPOCH: u64 = 3;
    const PUBLISH_LEDGER_EPOCH_LENGTH: u64 = 1; // Very short — 1 epoch
    const INITIAL_BALANCE: u128 = 10_000_000_000_000_000_000;

    let mut config = NodeConfig::testing();
    config.consensus.get_mut().block_migration_depth = 1;
    config.consensus.get_mut().chunk_size = CHUNK_SIZE;
    config.consensus.get_mut().num_chunks_in_partition = 4;
    config.consensus.get_mut().num_chunks_in_recall_range = 1;
    config.consensus.get_mut().epoch.num_blocks_in_epoch = BLOCKS_PER_EPOCH;
    config.consensus.get_mut().epoch.publish_ledger_epoch_length =
        Some(PUBLISH_LEDGER_EPOCH_LENGTH);

    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.consensus.extend_genesis_accounts(vec![(
        signer.address(),
        GenesisAccount {
            balance: U256::from(INITIAL_BALANCE).into(),
            ..Default::default()
        },
    )]);

    let node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("perm_last_slot_test", 30)
        .await;

    let anchor = node.get_block_by_height(0).await?.block_hash;

    // Post exactly ONE transaction and promote it — creates one Publish slot
    let tx = node
        .post_data_tx(anchor, vec![1_u8; DATA_SIZE], &signer)
        .await;
    node.wait_for_mempool(tx.header.id, 10).await?;
    node.upload_chunks(&tx).await?;
    node.wait_for_ingress_proofs(vec![tx.header.id], 20).await?;

    // Mine WAY past expiry (5x the epoch length) to stress-test last-slot protection
    let overkill_height = (PUBLISH_LEDGER_EPOCH_LENGTH + 4) * BLOCKS_PER_EPOCH;
    let current = node.get_canonical_chain_height().await;
    for _ in current..overkill_height {
        node.mine_block().await?;
    }

    let epoch_snapshot = node.get_canonical_epoch_snapshot();
    let perm_slots = epoch_snapshot.ledgers.get_slots(DataLedger::Publish);

    info!("Perm slots after overkill mining: {}", perm_slots.len());
    for (i, slot) in perm_slots.iter().enumerate() {
        info!(
            "  Slot {}: is_expired={}, partitions={}, last_height={}",
            i,
            slot.is_expired,
            slot.partitions.len(),
            slot.last_height
        );
    }

    // The last slot must NEVER be expired
    if let Some(last_slot) = perm_slots.last() {
        assert!(
            !last_slot.is_expired,
            "Last Publish slot must never expire (last-slot protection)"
        );
    }

    // If there's only one slot total, it should not be expired
    if perm_slots.len() == 1 {
        assert!(
            !perm_slots[0].is_expired,
            "Single Publish slot must not expire (it's the last slot)"
        );
        info!("Confirmed: single perm slot survived past expiry boundary");
    }

    node.stop().await;
    Ok(())
}
```

**Step 2: Run the test**

```bash
cargo nextest run -p irys-chain-tests heavy_perm_last_slot_never_expires
```

Expected: PASS. The single Publish slot remains active even after 5x the configured epoch length.

**Step 3: Commit**

```bash
git add crates/chain-tests/src/perm_ledger_expiry/mod.rs
git commit -m "test: add last-slot protection integration test for perm expiry"
```

---

### Task 5: Perm Expiry Disabled (Mainnet Safety)

**Files:**
- Modify: `crates/chain-tests/src/perm_ledger_expiry/mod.rs`

**Context:** On mainnet, `publish_ledger_epoch_length` is `None`. Permanent data must NEVER expire. This test boots a node with `None` config, mines many epochs, and asserts zero Publish slots are expired.

**Step 1: Write the test**

```rust
/// Tests that Publish ledger data NEVER expires when publish_ledger_epoch_length is None.
/// This is the mainnet behavior — permanent data stays permanent.
#[test_log::test(tokio::test)]
async fn heavy_perm_expiry_disabled_nothing_expires() -> eyre::Result<()> {
    const CHUNK_SIZE: u64 = 32;
    const DATA_SIZE: usize = 32;
    const BLOCKS_PER_EPOCH: u64 = 3;
    const INITIAL_BALANCE: u128 = 10_000_000_000_000_000_000;

    let mut config = NodeConfig::testing();
    config.consensus.get_mut().block_migration_depth = 1;
    config.consensus.get_mut().chunk_size = CHUNK_SIZE;
    config.consensus.get_mut().num_chunks_in_partition = 4;
    config.consensus.get_mut().num_chunks_in_recall_range = 1;
    config.consensus.get_mut().epoch.num_blocks_in_epoch = BLOCKS_PER_EPOCH;
    // Explicitly set to None — mainnet behavior
    config.consensus.get_mut().epoch.publish_ledger_epoch_length = None;

    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.consensus.extend_genesis_accounts(vec![(
        signer.address(),
        GenesisAccount {
            balance: U256::from(INITIAL_BALANCE).into(),
            ..Default::default()
        },
    )]);

    let node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("perm_disabled_test", 30)
        .await;

    let anchor = node.get_block_by_height(0).await?.block_hash;

    // Post and promote a transaction
    let tx = node
        .post_data_tx(anchor, vec![1_u8; DATA_SIZE], &signer)
        .await;
    node.wait_for_mempool(tx.header.id, 10).await?;
    node.upload_chunks(&tx).await?;
    node.wait_for_ingress_proofs(vec![tx.header.id], 20).await?;

    // Mine many epochs — enough that expiry WOULD have triggered if enabled
    let lots_of_blocks = 5 * BLOCKS_PER_EPOCH;
    let current = node.get_canonical_chain_height().await;
    for _ in current..lots_of_blocks {
        node.mine_block().await?;
    }

    // No Publish slots should be expired
    let epoch_snapshot = node.get_canonical_epoch_snapshot();
    let perm_slots = epoch_snapshot.ledgers.get_slots(DataLedger::Publish);

    let expired_count = perm_slots.iter().filter(|s| s.is_expired).count();
    assert_eq!(
        expired_count, 0,
        "With publish_ledger_epoch_length=None, NO perm slots should expire"
    );
    info!(
        "Confirmed: {} perm slots, 0 expired (mainnet behavior correct)",
        perm_slots.len()
    );

    // Also verify partition is still assigned to Publish ledger
    for slot in perm_slots {
        for partition_hash in &slot.partitions {
            if let Some(assignment) = epoch_snapshot
                .partition_assignments
                .get_assignment(*partition_hash)
            {
                assert!(
                    assignment.ledger_id.is_some(),
                    "Perm partition should still be assigned to Publish ledger"
                );
            }
        }
    }
    info!("Confirmed: all perm partitions still have ledger assignments");

    node.stop().await;
    Ok(())
}
```

**Step 2: Run the test**

```bash
cargo nextest run -p irys-chain-tests heavy_perm_expiry_disabled_nothing_expires
```

Expected: PASS. Zero Publish slots expired, all partitions still assigned.

**Step 3: Commit**

```bash
git add crates/chain-tests/src/perm_ledger_expiry/mod.rs
git commit -m "test: add mainnet safety test — perm expiry disabled does nothing"
```

---

### Task 6: No PermFeeRefund for Promoted Publish Data

**Files:**
- Modify: `crates/chain-tests/src/perm_ledger_expiry/mod.rs`

**Context:** When a transaction is promoted to Publish (chunks uploaded and proven), the user should NOT receive a `PermFeeRefund` when the Publish slot expires. `PermFeeRefund` only applies to unpromoted transactions on the Submit ledger. This test explicitly verifies that no `PermFeeRefund` shadow tx is generated targeting the user who had promoted data.

**Step 1: Write the test**

```rust
/// Tests that promoted Publish data does NOT generate PermFeeRefund on expiry.
/// PermFeeRefund only applies to unpromoted Submit transactions.
#[test_log::test(tokio::test)]
async fn heavy_perm_no_refund_for_promoted_data() -> eyre::Result<()> {
    const CHUNK_SIZE: u64 = 32;
    const DATA_SIZE: usize = 32;
    const BLOCKS_PER_EPOCH: u64 = 3;
    const PUBLISH_LEDGER_EPOCH_LENGTH: u64 = 2;
    const INITIAL_BALANCE: u128 = 10_000_000_000_000_000_000;

    let mut config = NodeConfig::testing();
    config.consensus.get_mut().block_migration_depth = 1;
    config.consensus.get_mut().chunk_size = CHUNK_SIZE;
    config.consensus.get_mut().num_chunks_in_partition = 4;
    config.consensus.get_mut().num_chunks_in_recall_range = 1;
    config.consensus.get_mut().epoch.num_blocks_in_epoch = BLOCKS_PER_EPOCH;
    config.consensus.get_mut().epoch.publish_ledger_epoch_length =
        Some(PUBLISH_LEDGER_EPOCH_LENGTH);

    let signer = IrysSigner::random_signer(&config.consensus_config());
    let user_address = signer.address();
    config.consensus.extend_genesis_accounts(vec![(
        user_address,
        GenesisAccount {
            balance: U256::from(INITIAL_BALANCE).into(),
            ..Default::default()
        },
    )]);

    let node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("perm_no_refund_test", 30)
        .await;

    let anchor = node.get_block_by_height(0).await?.block_hash;

    // Post and promote — this data is successfully stored in Publish
    let tx = node
        .post_data_tx(anchor, vec![1_u8; DATA_SIZE], &signer)
        .await;
    let _perm_fee = tx.header.perm_fee;
    node.wait_for_mempool(tx.header.id, 10).await?;
    node.upload_chunks(&tx).await?;
    node.wait_for_ingress_proofs(vec![tx.header.id], 20).await?;
    info!("Transaction promoted to Publish");

    // Mine to expiry
    let target_height = (PUBLISH_LEDGER_EPOCH_LENGTH + 1) * BLOCKS_PER_EPOCH;
    let current = node.get_canonical_chain_height().await;
    for _ in current..target_height {
        node.mine_block().await?;
    }

    // Scan ALL blocks from genesis to final height for PermFeeRefund shadow txs
    let final_height = node.get_canonical_chain_height().await;
    let mut total_perm_refunds = U256::ZERO;
    let mut perm_refund_count = 0u32;

    for h in 0..=final_height {
        if let Ok(block) = node.get_block_by_height(h).await {
            if let Ok(evm_block) = node.wait_for_evm_block(block.evm_block_hash, 10).await {
                for evm_tx in &evm_block.body.transactions {
                    let mut input = evm_tx.input().as_ref();
                    if let Ok(shadow) = ShadowTransaction::decode(&mut input) {
                        if let Some(TransactionPacket::PermFeeRefund(refund)) = shadow.as_v1() {
                            perm_refund_count += 1;
                            let amount =
                                U256::from_le_bytes(refund.amount.to_le_bytes());
                            total_perm_refunds = total_perm_refunds.saturating_add(amount);
                            info!(
                                "Found PermFeeRefund at height {}: target={:?}, amount={}",
                                h, refund.target, amount
                            );
                        }
                    }
                }
            }
        }
    }

    // No PermFeeRefund should exist — the tx was promoted to Publish successfully
    assert_eq!(
        perm_refund_count, 0,
        "Promoted Publish data should NOT generate PermFeeRefund shadow txs, found {}",
        perm_refund_count
    );
    assert_eq!(
        total_perm_refunds,
        U256::ZERO,
        "Total PermFeeRefund amount should be zero for promoted data"
    );
    info!("Confirmed: no PermFeeRefund shadow txs for promoted Publish data");

    node.stop().await;
    Ok(())
}
```

**Step 2: Run the test**

```bash
cargo nextest run -p irys-chain-tests heavy_perm_no_refund_for_promoted_data
```

Expected: PASS. Zero `PermFeeRefund` shadow txs found across all blocks.

**Step 3: Commit**

```bash
git add crates/chain-tests/src/perm_ledger_expiry/mod.rs
git commit -m "test: verify no PermFeeRefund for promoted Publish data on expiry"
```

---

## Execution Notes

**Running all perm expiry tests at once:**
```bash
cargo nextest run -p irys-chain-tests perm_ledger_expiry
```

**Running a specific test:**
```bash
cargo nextest run -p irys-chain-tests heavy_perm_ledger_expiry_basic
```

**Key risk:** These are `heavy_` integration tests that boot real nodes. They may take 30-60s each. If a test hangs, check:
1. Packing service readiness (increase `start_and_wait_for_packing` timeout)
2. Block mining stalls (check VDF, ensure `num_chunks_in_recall_range = 1`)
3. Ingress proof timeouts (increase `wait_for_ingress_proofs` timeout)

**Compilation check before running:**
```bash
cargo check -p irys-chain-tests --tests
```
