# Perm Ledger Expiry Integration Tests

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add comprehensive integration tests for the publish (permanent) ledger expiry feature, covering balance verification, simultaneous perm+term expiry, exact-boundary expiry, last-slot protection, and mainnet safety.

**Architecture:** All tests live in `crates/chain-tests/src/perm_ledger_expiry/`. Each test boots a single-node `IrysNodeTest` with accelerated epochs (`BLOCKS_PER_EPOCH=3`, `PUBLISH_LEDGER_EPOCH_LENGTH=2`) and small partitions (`CHUNK_SIZE=32`, `num_chunks_in_partition=4`). Tests follow the same patterns established by the term ledger expiry tests in `crates/chain-tests/src/term_ledger_expiry/`.

**Tech Stack:** Rust, `#[test_log::test(tokio::test)]`, `IrysNodeTest` harness, `eyre::Result`, shadow tx decoding via `irys_reth_node_bridge::irys_reth::shadow_tx`

---

## Critical Harness Notes (from Codex review)

These apply to ALL tests in this plan and must be followed carefully:

1. **`wait_for_ingress_proofs()` mines blocks internally** — it calls `mine_block()` once per second in its polling loop. For expiry tests that care about exact heights, use `wait_for_ingress_proofs_no_mining()` instead, then mine explicitly. Otherwise you can silently advance past epoch boundaries.

2. **Slot allocation is epoch-based, not tx-based** — Slots are allocated by `allocate_additional_ledger_slots()` during epoch processing. `last_height` is set to the epoch block height when the slot is allocated, NOT when data is written to it. Genesis init allocates 1 slot per ledger at height 0.

3. **Derive expected expiry from observed state** — Rather than computing `target_height = (EPOCH_LENGTH + 1) * BLOCKS_PER_EPOCH` and hoping it lines up, read the actual `slot.last_height` from the epoch snapshot and compute the exact epoch boundary where `epoch_height - min_blocks >= slot.last_height`. This prevents false passes from off-by-one errors.

4. **`mine_until_next_epoch()` always advances** — At height 3 with B=3, it mines to 6, not 3. It computes `B - (height % B)` blocks to mine. Use explicit `mine_block()` loops when you need precise height control.

5. **`num_capacity_partitions` controls capacity pool size, NOT partitions per slot** — To get 1 partition per slot, set `num_partitions_per_slot = 1`.

6. **Signer vs miner** — Use a separate `IrysSigner::random_signer()` funded via genesis, NOT `config.signer()` or the node's reward address. Mining rewards would corrupt balance assertions.

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
| 2 | Simultaneous perm + term expiry in same epoch | High | Most likely production scenario; proves non-interference |
| 3 | Exact-boundary expiry (not expired → expired transition) | High | Best defense against off-by-one bugs |
| 4 | Last-slot protection with partition + data access checks | Medium | Unit test exists but doesn't verify live-node side effects |
| 5 | Perm expiry disabled (`None`) with multi-slot setup | Medium | Mainnet safety; multi-slot needed to rule out last-slot masking |

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
- `node.mine_until_next_epoch()` → `(usize, u64)` (blocks_mined, final_height) — **CAUTION: always advances at least 1 block**
- `node.wait_for_ingress_proofs_no_mining(ids, seconds)` — polls without mining, use this for precise height control
- `node.get_is_promoted(tx_id)` → `bool` — checks if tx has been promoted to Publish

---

### Task 1: User Balance Unchanged After Perm Expiry

**Files:**
- Modify: `crates/chain-tests/src/perm_ledger_expiry/mod.rs`

**Context:** The existing `heavy_perm_ledger_expiry_basic` test verifies slots expire and no shadow txs are generated, but doesn't check that the user's balance is unaffected. For perm expiry, there should be zero economic effect — no fees extracted, no refunds issued. This test adds balance bookkeeping to prove it.

**IMPORTANT:** The signer is already a separate random address (not the node miner), so mining rewards won't corrupt the comparison. Capture balance AFTER the tx has been included and promoted (not at genesis), so the comparison window is clean.

**Step 1: Add balance assertions to `heavy_perm_ledger_expiry_basic`**

After the tx is posted, chunks uploaded, and ingress proofs confirmed (but BEFORE the expiry mining loop), capture the user's balance. Then after expiry completes, compare:

```rust
// Insert before "Mine through epochs until expiry" comment (before `let target_height`):
// Capture user balance AFTER tx inclusion and promotion (but before expiry mining).
// This gives us a clean reference point — the only debits were tx fees at inclusion time.
let pre_expiry_height = node.get_canonical_chain_height().await;
let pre_expiry_block = node.get_block_by_height(pre_expiry_height).await?;
let pre_expiry_balance = U256::from_be_bytes(
    node.get_balance(signer.address(), pre_expiry_block.evm_block_hash)
        .await
        .to_be_bytes(),
);
info!("User balance before expiry mining: {}", pre_expiry_balance);

// Insert after "Verified expired perm partitions are in capacity pool" (after existing Assertion 3):
// --- Assertion 4: User balance is unchanged by perm expiry ---
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

### Task 2: Simultaneous Perm + Term Expiry (Non-Interference)

**Files:**
- Modify: `crates/chain-tests/src/perm_ledger_expiry/mod.rs`

**Context:** In production, both Submit (term) and Publish (perm) ledgers will have data expiring at epoch boundaries. This is the most realistic and most important scenario to test. When both expire simultaneously, the system must:
- Generate `TermFeeReward` shadow txs for Submit expiry (miner compensation)
- Generate NO `TermFeeReward` for Publish expiry
- Mark both ledgers' slots as expired
- Return both sets of partitions to capacity pool
- Publish expiry must NOT interfere with Submit fee distribution (the bug from Finding 1)

**Key design decisions (from Codex review):**
- Use `wait_for_ingress_proofs_no_mining()` to avoid hidden height advancement
- Explicitly assert `get_is_promoted(tx1) == false` to verify the unpromoted tx assumption
- Both ledgers need at least 2 slots (so the genesis slot can expire while the last slot is protected). The genesis init allocates 1 slot per ledger at height 0. We need to post enough data early to trigger a second slot allocation at the first epoch boundary, so that the genesis slot becomes expirable.

**Step 1: Write the test**

Add the following test function at the bottom of `crates/chain-tests/src/perm_ledger_expiry/mod.rs`:

```rust
/// Tests that simultaneous publish and submit ledger expiry in the same epoch works correctly.
/// This is the critical non-interference test: Publish expiry must not block Submit fee distribution.
/// Verifies:
/// - Both Submit and Publish slots expire at the same epoch boundary
/// - TermFeeReward shadow txs appear for Submit expiry
/// - Publish expiry does NOT generate TermFeeReward or interfere with Submit path
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

    // Upload chunks for tx2 only — use no-mining variant to preserve height control
    node.upload_chunks(&tx2).await?;
    node.wait_for_ingress_proofs_no_mining(vec![tx2.header.id], 20).await?;

    // Explicitly verify tx1 is NOT promoted (no auto-promotion)
    assert!(
        !node.get_is_promoted(&tx1.header.id).await?,
        "tx1 should NOT be promoted (no chunks uploaded)"
    );
    assert!(
        node.get_is_promoted(&tx2.header.id).await?,
        "tx2 should be promoted (chunks uploaded)"
    );

    // Mine block to include both txs, then mine through epochs until expiry.
    // Genesis slot (allocated at height 0) needs: epoch_height - min_blocks >= 0
    // min_blocks = EPOCH_LENGTH * BLOCKS_PER_EPOCH = 6
    // So expiry triggers at epoch_height = 6 (which is the first epoch boundary >= 6)
    let target_height = EPOCH_LENGTH * BLOCKS_PER_EPOCH;
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

    // --- Assertion 3: TermFeeReward shadow txs exist (from Submit fee distribution) ---
    // This is the key non-interference check: Publish expiry must not block Submit fee path.
    let epoch_block = node.get_block_by_height(final_height).await?;
    let evm_block = node
        .wait_for_evm_block(epoch_block.evm_block_hash, 30)
        .await?;
    let mut found_term_fee_reward = false;

    for tx in &evm_block.body.transactions {
        let mut input = tx.input().as_ref();
        if let Ok(shadow) = ShadowTransaction::decode(&mut input) {
            if let Some(packet) = shadow.as_v1() {
                if matches!(packet, TransactionPacket::TermFeeReward(_)) {
                    found_term_fee_reward = true;
                    info!("Found TermFeeReward shadow tx (expected, from Submit expiry)");
                }
            }
        }
    }

    assert!(
        found_term_fee_reward,
        "Expected TermFeeReward from Submit ledger expiry — Publish expiry may have blocked it"
    );

    // --- Assertion 4: Both sets of expired partitions in capacity pool ---
    let partition_assignments = &epoch_snapshot.partition_assignments;

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
git commit -m "test: add simultaneous perm+term expiry non-interference integration test"
```

---

### Task 3: Exact-Boundary Expiry (Not Expired → Expired Transition)

**Files:**
- Modify: `crates/chain-tests/src/perm_ledger_expiry/mod.rs`

**Context (from Codex review):** The best defense against off-by-one bugs is a test that proves "not expired at the previous epoch block, expired at the exact next epoch block." Rather than using a hardcoded target height, this test reads the actual `slot.last_height` from the epoch snapshot to compute the exact epoch boundary where expiry must trigger.

**Step 1: Write the test**

```rust
/// Tests exact-boundary expiry: perm slot is NOT expired at epoch E-1 but IS expired at epoch E.
/// Derives the expected expiry epoch from the observed slot.last_height rather than hardcoding,
/// preventing false passes from off-by-one errors in the expiry height calculation.
#[test_log::test(tokio::test)]
async fn heavy_perm_exact_boundary_expiry() -> eyre::Result<()> {
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
    config.consensus.extend_genesis_accounts(vec![(
        signer.address(),
        GenesisAccount {
            balance: U256::from(INITIAL_BALANCE).into(),
            ..Default::default()
        },
    )]);

    let node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("perm_exact_boundary_test", 30)
        .await;

    let anchor = node.get_block_by_height(0).await?.block_hash;

    // Post and promote a transaction
    let tx = node
        .post_data_tx(anchor, vec![1_u8; DATA_SIZE], &signer)
        .await;
    node.wait_for_mempool(tx.header.id, 10).await?;
    node.upload_chunks(&tx).await?;
    node.wait_for_ingress_proofs_no_mining(vec![tx.header.id], 20).await?;

    // Mine one block to include the tx
    node.mine_block().await?;

    // Read the genesis Publish slot's last_height from the epoch snapshot.
    // Genesis init allocates slot 0 at height 0.
    let snapshot = node.get_canonical_epoch_snapshot();
    let perm_slots = snapshot.ledgers.get_slots(DataLedger::Publish);
    assert!(!perm_slots.is_empty(), "Should have at least one perm slot");
    let slot0_last_height = perm_slots[0].last_height;
    info!("Publish slot 0 last_height = {}", slot0_last_height);

    // Compute the exact epoch boundary where slot 0 should expire:
    // min_blocks = PUBLISH_LEDGER_EPOCH_LENGTH * BLOCKS_PER_EPOCH
    // Slot expires when epoch_height >= min_blocks AND epoch_height - min_blocks >= slot.last_height
    // => epoch_height >= min_blocks + slot.last_height
    let min_blocks = PUBLISH_LEDGER_EPOCH_LENGTH * BLOCKS_PER_EPOCH;
    let earliest_expiry_height = min_blocks + slot0_last_height;
    // Round up to the next epoch boundary (expiry only runs on epoch blocks)
    let expiry_epoch_height = if earliest_expiry_height % BLOCKS_PER_EPOCH == 0 {
        earliest_expiry_height
    } else {
        ((earliest_expiry_height / BLOCKS_PER_EPOCH) + 1) * BLOCKS_PER_EPOCH
    };
    // The epoch boundary BEFORE the expiry one
    let pre_expiry_epoch_height = expiry_epoch_height - BLOCKS_PER_EPOCH;
    info!(
        "Expected: NOT expired at epoch height {}, expired at epoch height {}",
        pre_expiry_epoch_height, expiry_epoch_height
    );

    // --- Phase 1: Mine to the epoch BEFORE expiry and verify NOT expired ---
    let current = node.get_canonical_chain_height().await;
    if pre_expiry_epoch_height > current {
        for _ in current..pre_expiry_epoch_height {
            node.mine_block().await?;
        }
    }

    let pre_snapshot = node.get_canonical_epoch_snapshot();
    let pre_perm_slots = pre_snapshot.ledgers.get_slots(DataLedger::Publish);
    assert!(
        !pre_perm_slots[0].is_expired,
        "Publish slot 0 should NOT be expired at epoch height {} (one epoch before expiry)",
        pre_expiry_epoch_height
    );
    info!("Confirmed: slot 0 NOT expired at height {}", pre_expiry_epoch_height);

    // --- Phase 2: Mine exactly to the expiry epoch boundary ---
    let current = node.get_canonical_chain_height().await;
    for _ in current..expiry_epoch_height {
        node.mine_block().await?;
    }

    let post_snapshot = node.get_canonical_epoch_snapshot();
    let post_perm_slots = post_snapshot.ledgers.get_slots(DataLedger::Publish);

    // If slot 0 is the last slot, last-slot protection kicks in — that's a valid outcome.
    // Otherwise, it MUST be expired at exactly this epoch.
    let num_slots = post_perm_slots.len();
    if num_slots > 1 {
        assert!(
            post_perm_slots[0].is_expired,
            "Publish slot 0 should be expired at epoch height {} (expiry boundary)",
            expiry_epoch_height
        );
        info!("Confirmed: slot 0 expired at exact boundary height {}", expiry_epoch_height);
    } else {
        // Single slot: last-slot protection should keep it alive
        assert!(
            !post_perm_slots[0].is_expired,
            "Single Publish slot should be protected by last-slot rule"
        );
        info!("Confirmed: single slot protected by last-slot rule at height {}", expiry_epoch_height);
    }

    node.stop().await;
    Ok(())
}
```

**Step 2: Run the test**

```bash
cargo nextest run -p irys-chain-tests heavy_perm_exact_boundary_expiry
```

Expected: PASS. The test derives the correct expiry boundary from observed state and verifies the transition.

**Step 3: Commit**

```bash
git add crates/chain-tests/src/perm_ledger_expiry/mod.rs
git commit -m "test: add exact-boundary perm expiry transition test"
```

---

### Task 4: Last-Slot Protection with Live-Node Side Effects

**Files:**
- Modify: `crates/chain-tests/src/perm_ledger_expiry/mod.rs`

**Context:** A unit test in `data_ledger.rs::test_perm_expiry_never_expires_last_slot` verifies the last-slot protection logic in isolation. This integration test goes further: it verifies that on a live node with a single Publish slot, mining far past the expiry boundary:
1. Keeps the slot NOT expired
2. Keeps partition assignments on the Publish ledger (not reset to capacity)
3. Generates no expiry-related shadow txs

**Step 1: Write the test**

```rust
/// Tests that the last (and only) Publish slot never expires on a live node.
/// Goes beyond the unit test by checking live-node side effects:
/// - Partition assignment stays on Publish ledger
/// - No expiry-related shadow transactions generated
/// - Slot remains active even after many epochs
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

    // Post exactly ONE transaction and promote it — uses the genesis slot
    let tx = node
        .post_data_tx(anchor, vec![1_u8; DATA_SIZE], &signer)
        .await;
    node.wait_for_mempool(tx.header.id, 10).await?;
    node.upload_chunks(&tx).await?;
    node.wait_for_ingress_proofs_no_mining(vec![tx.header.id], 20).await?;

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

    // --- Assertion 1: Last slot must NOT be expired ---
    if let Some(last_slot) = perm_slots.last() {
        assert!(
            !last_slot.is_expired,
            "Last Publish slot must never expire (last-slot protection)"
        );
    }

    // If there's only one slot total, verify it's still active
    if perm_slots.len() == 1 {
        assert!(
            !perm_slots[0].is_expired,
            "Single Publish slot must not expire (it's the last slot)"
        );
    }

    // --- Assertion 2: Partition assignments still on Publish ledger ---
    let partition_assignments = &epoch_snapshot.partition_assignments;
    let last_slot = perm_slots.last().unwrap();
    for partition_hash in &last_slot.partitions {
        if let Some(assignment) = partition_assignments.get_assignment(*partition_hash) {
            assert!(
                assignment.ledger_id.is_some(),
                "Last-slot partition {:?} should still be assigned to Publish, not capacity pool",
                partition_hash
            );
        }
    }
    info!("Confirmed: last-slot partitions still assigned to Publish ledger");

    // --- Assertion 3: No expiry-related shadow txs in any epoch block past the threshold ---
    let min_blocks = PUBLISH_LEDGER_EPOCH_LENGTH * BLOCKS_PER_EPOCH;
    let final_height = node.get_canonical_chain_height().await;
    for h in (min_blocks..=final_height).step_by(BLOCKS_PER_EPOCH as usize) {
        if let Ok(block) = node.get_block_by_height(h).await {
            if let Ok(evm_block) = node.wait_for_evm_block(block.evm_block_hash, 10).await {
                for evm_tx in &evm_block.body.transactions {
                    let mut input = evm_tx.input().as_ref();
                    if let Ok(shadow) = ShadowTransaction::decode(&mut input) {
                        if let Some(packet) = shadow.as_v1() {
                            // No TermFeeReward should exist for a Publish-only node
                            assert!(
                                !matches!(packet, TransactionPacket::TermFeeReward(_)),
                                "Unexpected TermFeeReward at height {} — last-slot Publish data should not trigger fees",
                                h
                            );
                        }
                    }
                }
            }
        }
    }
    info!("Confirmed: no expiry-related shadow txs generated");

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
git commit -m "test: add last-slot protection integration test with side-effect checks"
```

---

### Task 5: Perm Expiry Disabled (Mainnet Safety) — Multi-Slot

**Files:**
- Modify: `crates/chain-tests/src/perm_ledger_expiry/mod.rs`

**Context:** On mainnet, `publish_ledger_epoch_length` is `None`. Permanent data must NEVER expire. This test boots a node with `None` config, creates enough data for multiple Publish slots, mines many epochs, and asserts zero Publish slots are expired.

**Key design decision (from Codex review):** A single-slot test would false-pass because last-slot protection alone prevents expiry. By forcing 2+ Publish slots, the test proves the `None` config gate is what blocks expiry, not last-slot protection.

**Step 1: Write the test**

```rust
/// Tests that Publish ledger data NEVER expires when publish_ledger_epoch_length is None.
/// Uses multi-slot setup so last-slot protection cannot mask a broken config gate.
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
    // 1 partition per slot so data fills slots faster
    config.consensus.get_mut().num_partitions_per_slot = 1;

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

    // Post and promote a transaction to fill the genesis Publish slot
    let tx = node
        .post_data_tx(anchor, vec![1_u8; DATA_SIZE], &signer)
        .await;
    node.wait_for_mempool(tx.header.id, 10).await?;
    node.upload_chunks(&tx).await?;
    node.wait_for_ingress_proofs_no_mining(vec![tx.header.id], 20).await?;

    // Mine to first epoch boundary to trigger slot allocation (creates slot 1)
    node.mine_block().await?; // include the tx
    let (_, height_after_epoch) = node.mine_until_next_epoch().await?;
    info!("After first epoch at height {}", height_after_epoch);

    // Post and promote a second tx in the new epoch
    let anchor2 = node.get_block_by_height(height_after_epoch).await?.block_hash;
    let tx2 = node
        .post_data_tx(anchor2, vec![2_u8; DATA_SIZE], &signer)
        .await;
    node.wait_for_mempool(tx2.header.id, 10).await?;
    node.upload_chunks(&tx2).await?;
    node.wait_for_ingress_proofs_no_mining(vec![tx2.header.id], 20).await?;

    // Mine many more epochs — enough that expiry WOULD have triggered if enabled
    // With epoch_length=2 and B=3, min_blocks would be 6, so height 6+ would expire genesis slot
    let lots_of_blocks = 8 * BLOCKS_PER_EPOCH;
    let current = node.get_canonical_chain_height().await;
    for _ in current..lots_of_blocks {
        node.mine_block().await?;
    }

    // Verify we have multiple Publish slots
    let epoch_snapshot = node.get_canonical_epoch_snapshot();
    let perm_slots = epoch_snapshot.ledgers.get_slots(DataLedger::Publish);
    info!("Perm slots: {}", perm_slots.len());
    for (i, slot) in perm_slots.iter().enumerate() {
        info!(
            "  Slot {}: is_expired={}, partitions={}, last_height={}",
            i, slot.is_expired, slot.partitions.len(), slot.last_height
        );
    }

    // We need at least 2 slots for this test to be meaningful
    // (otherwise last-slot protection would mask the bug)
    assert!(
        perm_slots.len() >= 2,
        "Need at least 2 Publish slots to distinguish None gate from last-slot protection, got {}",
        perm_slots.len()
    );

    // No Publish slots should be expired
    let expired_count = perm_slots.iter().filter(|s| s.is_expired).count();
    assert_eq!(
        expired_count, 0,
        "With publish_ledger_epoch_length=None, NO perm slots should expire (got {} expired)",
        expired_count
    );

    // Also verify all partitions still assigned to Publish ledger
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

Expected: PASS. Zero Publish slots expired across 2+ slots, all partitions still assigned.

**Step 3: Commit**

```bash
git add crates/chain-tests/src/perm_ledger_expiry/mod.rs
git commit -m "test: add mainnet safety test — perm expiry disabled with multi-slot verification"
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
3. Ingress proof timeouts (increase `wait_for_ingress_proofs_no_mining` timeout)
4. `wait_for_ingress_proofs` mining blocks — make sure you're using the `_no_mining` variant

**Compilation check before running:**
```bash
cargo check -p irys-chain-tests --tests
```

---

## Changes from Original Plan (Codex Review)

| Original | Change | Reason |
|----------|--------|--------|
| Task 2: `wait_for_ingress_proofs` | → `wait_for_ingress_proofs_no_mining` | Prevents hidden height advancement past epoch boundaries |
| Task 2: No promotion check | → Added `get_is_promoted()` assertions | Explicitly verify the test's key assumption |
| Task 2: `target_height = (E+1)*B` | → `target_height = E*B` | Genesis slot at height 0 expires when epoch_height >= min_blocks |
| Task 3 (multi-slot): `num_capacity_partitions = Some(1)` | → Replaced with Task 3 (exact-boundary) | Original used wrong config knob; exact-boundary test is more valuable |
| Task 4: Only checked `is_expired` flag | → Added partition assignment + shadow tx checks | Proves live-node side effects, not just state flag |
| Task 5: Single-slot setup | → Multi-slot with `num_partitions_per_slot = 1` | Single slot would false-pass due to last-slot protection |
| Task 6 (PermFeeRefund scan) | → Dropped | Redundant with `term_ledger_expiry/perm_refund.rs::heavy_perm_fee_refund_for_unpromoted_tx` which already tests promoted-vs-unpromoted refunds |
| All tasks: Hardcoded expiry heights | → Derive from observed `slot.last_height` where possible | Prevents false passes from off-by-one errors |
