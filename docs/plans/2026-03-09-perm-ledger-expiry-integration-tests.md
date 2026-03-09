# Perm Ledger Expiry Integration Tests

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add integration tests for publish (permanent) ledger expiry covering balance verification, simultaneous perm+term expiry, exact-boundary expiry, last-slot protection, and mainnet safety.

**File:** All tests go in `crates/chain-tests/src/perm_ledger_expiry/mod.rs`

---

## Harness Rules

These apply to every test in this plan:

1. **`wait_for_ingress_proofs()` mines blocks internally.** Use `wait_for_ingress_proofs_no_mining()` for all expiry tests, then mine explicitly. Otherwise heights drift past epoch boundaries silently.

2. **Slots are allocated at epoch boundaries, not when txs are posted.** `last_height` is set to the epoch block height at allocation time. Genesis init allocates 1 slot per ledger at height 0.

3. **Derive expected expiry from observed state.** Read `slot.last_height` and compute the exact epoch boundary. Never hardcode a target height and hope it lines up.

4. **`mine_until_next_epoch()` always advances at least 1 block.** Use explicit `mine_block()` loops when you need precise height control.

5. **`num_capacity_partitions` controls capacity pool size, NOT partitions per slot.** To get 1 partition per slot, set `num_partitions_per_slot = 1`.

6. **Use a separate `IrysSigner::random_signer()` funded via genesis**, not `config.signer()`. Mining rewards corrupt balance assertions if the signer is also the miner.

---

## Standard Config

```rust
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
config.consensus.get_mut().epoch.publish_ledger_epoch_length = Some(PUBLISH_LEDGER_EPOCH_LENGTH);
```

---

### Task 1: User Balance Unchanged After Perm Expiry ✅ DONE

**What:** Add balance assertions to the existing `heavy_perm_ledger_expiry_basic` test.

**Approach:** Capture the user's EVM balance after tx inclusion/promotion but before the expiry mining loop. After expiry completes, assert the balance is identical. Perm expiry has zero economic side effects.

**Implementation:**

Insert before the `let target_height` line:

```rust
let pre_expiry_height = node.get_canonical_chain_height().await;
let pre_expiry_block = node.get_block_by_height(pre_expiry_height).await?;
let pre_expiry_balance = U256::from_be_bytes(
    node.get_balance(signer.address(), pre_expiry_block.evm_block_hash)
        .await
        .to_be_bytes(),
);
info!("User balance before expiry mining: {}", pre_expiry_balance);
```

Insert after the existing Assertion 3 (capacity pool check):

```rust
let post_expiry_block = node.get_block_by_height(final_height).await?;
let post_expiry_balance = U256::from_be_bytes(
    node.get_balance(signer.address(), post_expiry_block.evm_block_hash)
        .await
        .to_be_bytes(),
);
assert_eq!(
    pre_expiry_balance, post_expiry_balance,
    "User balance should not change due to perm expiry (no fees or refunds)"
);
```

**Validation:**
- `pre_expiry_balance == post_expiry_balance` (exact equality, not approximate)

**Run:** `cargo nextest run -p irys-chain-tests heavy_perm_ledger_expiry_basic`

**Commit:** `test: add balance unchanged assertion to perm ledger expiry test`

---

### Task 2: Simultaneous Perm + Term Expiry

**What:** New test `heavy_perm_and_term_expiry_same_epoch`. Both ledger types expire in the same epoch block. Proves Publish expiry does not block Submit fee distribution.

**Approach:**
- Set `submit_ledger_epoch_length = publish_ledger_epoch_length = 2`
- Post tx1 (no chunks → stays on Submit) and tx2 (chunks uploaded → promoted to Publish)
- Use `wait_for_ingress_proofs_no_mining` to avoid hidden height drift
- Assert `get_is_promoted(tx1) == false` and `get_is_promoted(tx2) == true` before mining to expiry
- Mine to `EPOCH_LENGTH * BLOCKS_PER_EPOCH` (genesis slot at height 0 expires when `epoch_height >= min_blocks`)

**Validation criteria (all must pass):**
1. `submit_slots.iter().filter(|s| s.is_expired).count() > 0`
2. `perm_slots` has at least one expired non-last slot
3. `TermFeeReward` shadow tx found in the expiry epoch block (proves Submit fee distribution ran)
4. All expired Submit partitions have `assignment.ledger_id == None`
5. All expired non-last Publish partitions have `assignment.ledger_id == None`

**Key assertion:** #3 is the non-interference proof. If Publish expiry blocks Submit fee distribution, `TermFeeReward` will be absent and the test fails.

**Run:** `cargo nextest run -p irys-chain-tests heavy_perm_and_term_expiry_same_epoch`

**Commit:** `test: add simultaneous perm+term expiry non-interference integration test`

---

### Task 3: Exact-Boundary Expiry Transition

**What:** New test `heavy_perm_exact_boundary_expiry`. Proves slot is NOT expired at epoch E-1, IS expired at epoch E. Best defense against off-by-one bugs.

**Approach:**
- Post and promote a tx, mine 1 block to include it
- Read `perm_slots[0].last_height` from the epoch snapshot
- Compute the exact epoch boundary where slot 0 must expire:
  ```
  min_blocks = PUBLISH_LEDGER_EPOCH_LENGTH * BLOCKS_PER_EPOCH
  earliest_expiry = min_blocks + slot0_last_height
  expiry_epoch = ceil_to_epoch_boundary(earliest_expiry)
  pre_expiry_epoch = expiry_epoch - BLOCKS_PER_EPOCH
  ```
- Mine to `pre_expiry_epoch`, assert slot 0 is NOT expired
- Mine to `expiry_epoch`, assert slot 0 IS expired (unless last-slot protected)

**Validation criteria (all must pass):**
1. At `pre_expiry_epoch`: `perm_slots[0].is_expired == false`
2. At `expiry_epoch` with `num_slots > 1`: `perm_slots[0].is_expired == true`
3. At `expiry_epoch` with `num_slots == 1`: `perm_slots[0].is_expired == false` (last-slot protection)

**Run:** `cargo nextest run -p irys-chain-tests heavy_perm_exact_boundary_expiry`

**Commit:** `test: add exact-boundary perm expiry transition test`

---

### Task 4: Last-Slot Protection with Side Effects

**What:** New test `heavy_perm_last_slot_never_expires`. Single Publish slot, mine 5x past expiry boundary.

**Approach:**
- `PUBLISH_LEDGER_EPOCH_LENGTH = 1` (very short)
- Post 1 tx, promote it (uses genesis slot)
- Use `wait_for_ingress_proofs_no_mining`
- Mine to `(PUBLISH_LEDGER_EPOCH_LENGTH + 4) * BLOCKS_PER_EPOCH`

**Validation criteria (all must pass):**
1. `perm_slots.last().is_expired == false`
2. If `perm_slots.len() == 1`: `perm_slots[0].is_expired == false`
3. All last-slot partitions have `assignment.ledger_id.is_some()` (still on Publish, not capacity pool)
4. No `TermFeeReward` shadow txs in any epoch block past `min_blocks` (scan epoch blocks at heights `min_blocks, min_blocks + B, min_blocks + 2B, ...`)

**What this proves beyond the unit test:** Partition assignments survive, no shadow txs generated, slot stays active under real epoch processing.

**Run:** `cargo nextest run -p irys-chain-tests heavy_perm_last_slot_never_expires`

**Commit:** `test: add last-slot protection integration test with side-effect checks`

---

### Task 5: Perm Expiry Disabled — Mainnet Safety

**What:** New test `heavy_perm_expiry_disabled_nothing_expires`. Config with `publish_ledger_epoch_length = None`, multi-slot setup.

**Approach:**
- Set `publish_ledger_epoch_length = None` and `num_partitions_per_slot = 1`
- Post + promote tx1 in epoch 0
- Mine to first epoch boundary (triggers slot allocation → now 2+ Publish slots)
- Post + promote tx2 in epoch 1
- Use `wait_for_ingress_proofs_no_mining` for both
- Mine to `8 * BLOCKS_PER_EPOCH` (far past where expiry would trigger if `Some(2)`)

**Validation criteria (all must pass):**
1. `perm_slots.len() >= 2` (multi-slot — **this is a precondition**, fail the test if not met)
2. `perm_slots.iter().filter(|s| s.is_expired).count() == 0`
3. All Publish partition assignments have `ledger_id.is_some()`

**Why multi-slot matters:** With a single slot, last-slot protection prevents expiry regardless of config. Two slots proves the `None` config gate is what blocks expiry.

**Run:** `cargo nextest run -p irys-chain-tests heavy_perm_expiry_disabled_nothing_expires`

**Commit:** `test: add mainnet safety test — perm expiry disabled with multi-slot verification`

---

## Execution

```bash
# All perm expiry tests
cargo nextest run -p irys-chain-tests perm_ledger_expiry

# Single test
cargo nextest run -p irys-chain-tests heavy_perm_ledger_expiry_basic

# Compile check first
cargo check -p irys-chain-tests --tests
```

If a test hangs: increase `start_and_wait_for_packing` timeout, verify `num_chunks_in_recall_range = 1`, confirm you're using `_no_mining` variant of ingress proof waiter.
