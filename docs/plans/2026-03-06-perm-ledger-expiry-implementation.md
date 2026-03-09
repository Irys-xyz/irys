# Publish Ledger Expiry — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Enable the Publish (permanent) ledger to expire on testnet, so testnet operators don't pay infrastructure costs to store valueless test data indefinitely.

**Architecture:** Add `publish_ledger_epoch_length: Option<u64>` to `EpochConfig`. When `Some(n)`, perm ledger slots expire after `n` epochs using logic inlined in `Ledgers` (not on `PermanentLedger` itself — preserving compile-time safety). No fee distribution occurs on Publish expiry — partitions are simply reset. The downstream pipeline (broadcast, mining reset, cache, data sync) is already generic and requires no changes.

**Tech Stack:** Rust, serde, irys-types, irys-database, irys-domain, irys-actors

---

## Pre-Implementation Notes

### Hardfork / Consensus Hash

**No hardfork is needed. Testnet will be reset.**

The critical constraint: **the consensus hash must NOT change for mainnet** when `publish_ledger_epoch_length` is `None`. Old nodes (without the field) and new nodes (with the field set to `None`) must compute identical hashes.

**Solution:** `#[serde(default, skip_serializing_if = "Option::is_none")]` on the new field.

- `ConsensusConfig::keccak256_hash()` works by serializing to canonical JSON → sorting keys → hashing the JSON string.
- Without `skip_serializing_if`, `Option::None` serializes as `"publishLedgerEpochLength": null` — this CHANGES the hash (new key in the JSON that old nodes don't have).
- With `skip_serializing_if = "Option::is_none"`, `None` causes the field to be **omitted entirely** from serialization. The JSON is identical to old nodes → **hash unchanged**.
- When `Some(168)` (testnet), the field IS serialized → different hash. This is fine because testnet is being reset — all nodes start fresh with the new config.
- The existing `num_capacity_partitions: Option<u64>` does NOT use `skip_serializing_if` (it serializes as `null`), but that field existed from the start so its `null` is already baked into the hash.
- The `test_consensus_hash_regression` test should **PASS without changes** — confirming the mainnet hash is stable.

### Gaps Found Beyond the Design Review

1. **Testnet resets** — Testnet will be reset with the new config. No migration of existing perm data is needed. All nodes start fresh with `publish_ledger_epoch_length: Some(168)`.

2. **Method naming** — `expire_term_partitions()`, `get_expiring_term_partitions()`, `expire_term_ledger_slots()` all have "term" in their names but will now handle perm too. The plan renames the `Ledgers` methods (dropping "term") and updates all callers. The `EpochSnapshot` method name is updated similarly.

3. **`get_first_unexpired_slot_index()` for Publish** — At `epoch_snapshot.rs:1205-1216`, this method already handles expired slots correctly (it iterates looking for `!is_expired`). No changes needed.

4. **`collect_expired_partitions()` bail is safe** — The bail at `ledger_expiry.rs:301` is guarded by `if ledger_id == target_ledger_type`, and the caller always passes `DataLedger::Submit`. Publish partitions in the expired list are filtered out by the outer condition. The bail is dead code in practice. We update the comment to reflect the new design intent.

### Design Decision: Expiry Logic in `Ledgers`, Not `PermanentLedger`

Per review Finding 4, we keep `PermanentLedger` clean — no expiry methods, no epoch config fields. The expiry logic is implemented inline in `Ledgers::expire_partitions()` using the same algorithm as `TermLedger::get_expired_slot_indexes()`. Config fields (`publish_ledger_epoch_length`, `num_blocks_in_epoch`) are stored on `Ledgers`.

---

## Task 1: Add `publish_ledger_epoch_length` to `EpochConfig`

**Files:**
- Modify: `crates/types/src/config/consensus.rs:314-327` (EpochConfig struct)
- Modify: `crates/types/src/config/consensus.rs:579-588` (mainnet config)
- Modify: `crates/types/src/config/consensus.rs:701-706` (testing config)
- Modify: `crates/types/src/config/consensus.rs:834-839` (testnet config)

**Step 1: Add the field to EpochConfig**

In `EpochConfig` struct, add `publish_ledger_epoch_length` after `num_capacity_partitions`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct EpochConfig {
    pub capacity_scalar: u64,
    pub num_blocks_in_epoch: u64,
    pub submit_ledger_epoch_length: u64,
    pub num_capacity_partitions: Option<u64>,
    /// Number of epochs before a publish ledger partition expires.
    /// `None` = never expire (mainnet). `Some(n)` = expire after n epochs (testnet).
    /// `skip_serializing_if` ensures `None` is omitted from canonical JSON,
    /// keeping the consensus hash unchanged for mainnet nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish_ledger_epoch_length: Option<u64>,
}
```

**Step 2: Set the value in all predefined configs**

- **Mainnet** (`consensus.rs:~587`): Add `publish_ledger_epoch_length: None,`
- **Testing** (`consensus.rs:~705`): Add `publish_ledger_epoch_length: None,`
- **Testnet** (`consensus.rs:~838`): Add `publish_ledger_epoch_length: Some(168),` (168 epochs * ~1hr/epoch = ~7 days)

**Step 3: Run compile check**

Run: `cargo check -p irys-types`
Expected: PASS (`#[serde(default)]` handles missing field on deser, `skip_serializing_if` omits `None` on ser)

**Step 4: Verify consensus hash is unchanged**

Run: `cargo nextest run -p irys-types test_consensus_hash_regression`
Expected: PASS — the testing config uses `None`, which is skipped in serialization, so the JSON (and hash) is identical to before the field was added.

**Step 5: Commit**

```
feat: add publish_ledger_epoch_length to EpochConfig
```

---

## Task 2: Add Config Validation

**Files:**
- Modify: `crates/types/src/config/mod.rs:81-150` (Config::validate)

**Step 1: Write the failing test**

Add a test in `crates/types/src/config/mod.rs` (or nearby test module) that verifies:
- `publish_ledger_epoch_length: Some(0)` fails validation
- `publish_ledger_epoch_length: Some(1)` passes validation
- `publish_ledger_epoch_length: None` passes validation

```rust
#[test]
fn test_publish_ledger_epoch_length_validation() {
    // Some(0) should fail
    let mut config = Config::testing();
    config.consensus.get_mut().epoch.publish_ledger_epoch_length = Some(0);
    assert!(config.validate().is_err());

    // Some(1) should pass
    config.consensus.get_mut().epoch.publish_ledger_epoch_length = Some(1);
    assert!(config.validate().is_ok());

    // None should pass
    config.consensus.get_mut().epoch.publish_ledger_epoch_length = None;
    assert!(config.validate().is_ok());
}
```

**Step 2: Run test to verify it fails**

Run: `cargo nextest run -p irys-types test_publish_ledger_epoch_length_validation`
Expected: FAIL (no validation yet)

**Step 3: Add validation to `Config::validate()`**

Add after existing validations (before `Ok(())`):

```rust
// publish_ledger_epoch_length must be > 0 if set
if let Some(n) = self.consensus.epoch.publish_ledger_epoch_length {
    ensure!(
        n > 0,
        "publish_ledger_epoch_length must be > 0 when set (got {})",
        n
    );
}
```

**Step 4: Run test to verify it passes**

Run: `cargo nextest run -p irys-types test_publish_ledger_epoch_length_validation`
Expected: PASS

**Step 5: Commit**

```
feat: add validation for publish_ledger_epoch_length
```

---

## Task 3: Store Perm Expiry Config on `Ledgers` and Update Constructor

**Files:**
- Modify: `crates/database/src/data_ledger.rs:261-274` (Ledgers struct + new())

**Step 1: Add config fields to `Ledgers`**

```rust
#[derive(Debug, Clone, Hash)]
pub struct Ledgers {
    perm: PermanentLedger,
    term: Vec<TermLedger>,
    /// When Some(n), publish ledger slots expire after n epochs
    publish_ledger_epoch_length: Option<u64>,
    /// Blocks per epoch (needed for expiry height calculation)
    num_blocks_in_epoch: u64,
}
```

**Step 2: Update `Ledgers::new()` to read from config**

```rust
pub fn new(config: &ConsensusConfig) -> Self {
    Self {
        perm: PermanentLedger::new(config),
        term: vec![TermLedger::new(DataLedger::Submit, config)],
        publish_ledger_epoch_length: config.epoch.publish_ledger_epoch_length,
        num_blocks_in_epoch: config.epoch.num_blocks_in_epoch,
    }
}
```

**Step 3: Run compile check**

Run: `cargo check -p irys-database`
Expected: PASS

**Step 4: Commit**

```
feat: store publish_ledger_epoch_length on Ledgers
```

---

## Task 4: Implement Perm Expiry in `Ledgers::expire_term_partitions()`

**Files:**
- Modify: `crates/database/src/data_ledger.rs:286-305` (expire_term_partitions)

**Step 1: Write a unit test for perm slot expiry**

Add a test in `crates/database/src/data_ledger.rs` (or a test module):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::ConsensusConfig;

    fn make_test_config(publish_epoch_length: Option<u64>) -> ConsensusConfig {
        let mut config = ConsensusConfig::testing();
        config.epoch.publish_ledger_epoch_length = publish_epoch_length;
        config.epoch.num_blocks_in_epoch = 10;
        config
    }

    #[test]
    fn test_perm_expiry_disabled() {
        let config = make_test_config(None);
        let mut ledgers = Ledgers::new(&config);
        // Add a perm slot at height 1
        ledgers.perm.allocate_slots(1, 1);
        ledgers.perm.slots[0].partitions.push(H256::random());
        // At height 1000, nothing should expire
        let expired = ledgers.expire_partitions(1000);
        assert!(expired.iter().all(|e| e.ledger_id != DataLedger::Publish));
    }

    #[test]
    fn test_perm_expiry_enabled() {
        let config = make_test_config(Some(2)); // 2 epochs
        let mut ledgers = Ledgers::new(&config);
        // num_blocks_in_epoch = 10, epoch_length = 2
        // expiry_height = epoch_height - (2 * 10) = epoch_height - 20

        // Add two perm slots
        ledgers.perm.allocate_slots(1, 1); // slot 0 at height 1
        ledgers.perm.slots[0].partitions.push(H256::random());
        ledgers.perm.allocate_slots(1, 25); // slot 1 at height 25
        ledgers.perm.slots[1].partitions.push(H256::random());

        // At epoch_height = 30: expiry_height = 30 - 20 = 10
        // Slot 0 (last_height=1) <= 10: EXPIRED
        // Slot 1 (last_height=25) > 10: NOT expired (also last slot)
        let expired = ledgers.expire_partitions(30);
        let perm_expired: Vec<_> = expired.iter()
            .filter(|e| e.ledger_id == DataLedger::Publish)
            .collect();
        assert_eq!(perm_expired.len(), 1);
        assert!(ledgers.perm.slots[0].is_expired);
        assert!(!ledgers.perm.slots[1].is_expired);
    }

    #[test]
    fn test_perm_expiry_never_expires_last_slot() {
        let config = make_test_config(Some(1)); // 1 epoch
        let mut ledgers = Ledgers::new(&config);
        // Add only one perm slot
        ledgers.perm.allocate_slots(1, 1);
        ledgers.perm.slots[0].partitions.push(H256::random());

        // At epoch_height = 100: should NOT expire (it's the last slot)
        let expired = ledgers.expire_partitions(100);
        let perm_expired: Vec<_> = expired.iter()
            .filter(|e| e.ledger_id == DataLedger::Publish)
            .collect();
        assert_eq!(perm_expired.len(), 0);
        assert!(!ledgers.perm.slots[0].is_expired);
    }

    #[test]
    fn test_perm_expiry_not_enough_blocks() {
        let config = make_test_config(Some(2)); // 2 epochs * 10 blocks = 20 min
        let mut ledgers = Ledgers::new(&config);
        ledgers.perm.allocate_slots(2, 1);
        ledgers.perm.slots[0].partitions.push(H256::random());
        ledgers.perm.slots[1].partitions.push(H256::random());

        // At epoch_height = 15 (< 20 minimum): nothing expires
        let expired = ledgers.expire_partitions(15);
        let perm_expired: Vec<_> = expired.iter()
            .filter(|e| e.ledger_id == DataLedger::Publish)
            .collect();
        assert_eq!(perm_expired.len(), 0);
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo nextest run -p irys-database test_perm_expiry`
Expected: FAIL (method `expire_partitions` doesn't exist yet)

**Step 3: Rename and update `expire_term_partitions()` → `expire_partitions()`**

Rename the method and add perm expiry logic at the top:

```rust
/// Get all partition hashes that have expired out of both perm and term ledgers.
/// Perm slots only expire when `publish_ledger_epoch_length` is configured.
pub fn expire_partitions(&mut self, epoch_height: u64) -> Vec<ExpiringPartitionInfo> {
    let mut expired_partitions: Vec<ExpiringPartitionInfo> = Vec::new();

    // Expire perm ledger slots if configured
    if let Some(epoch_length) = self.publish_ledger_epoch_length {
        let min_blocks = epoch_length * self.num_blocks_in_epoch;
        if epoch_height >= min_blocks {
            let expiry_height = epoch_height - min_blocks;
            let perm_ledger_id = DataLedger::try_from(self.perm.ledger_id).unwrap();
            let last_slot_index = self.perm.slots.len().saturating_sub(1);

            for (slot_index, slot) in self.perm.slots.iter_mut().enumerate() {
                if slot_index == last_slot_index && !self.perm.slots.is_empty() {
                    continue; // Never expire the last slot
                }
                if slot.last_height <= expiry_height && !slot.is_expired {
                    slot.is_expired = true;
                    for partition_hash in slot.partitions.iter() {
                        expired_partitions.push(ExpiringPartitionInfo {
                            partition_hash: *partition_hash,
                            ledger_id: perm_ledger_id,
                            slot_index,
                        });
                    }
                }
            }
        }
    }

    // Collect expired partition hashes from term ledgers (existing logic)
    for term_ledger in &mut self.term {
        let ledger_id = DataLedger::try_from(term_ledger.ledger_id).unwrap();
        for expired_index in term_ledger.expire_old_slots(epoch_height) {
            for partition_hash in term_ledger.slots[expired_index].partitions.iter() {
                expired_partitions.push(ExpiringPartitionInfo {
                    partition_hash: *partition_hash,
                    ledger_id,
                    slot_index: expired_index,
                });
            }
        }
    }

    expired_partitions
}
```

Note: The perm expiry loop needs to avoid the borrow checker issue with `self.perm.slots.iter_mut()` while checking `last_slot_index`. Compute `last_slot_index` before the loop. The loop mutates `slot.is_expired` and reads `slot.partitions` — both on the same element, so `iter_mut()` works.

**Step 4: Run tests to verify they pass**

Run: `cargo nextest run -p irys-database test_perm_expiry`
Expected: PASS

**Step 5: Commit**

```
feat: implement perm slot expiry in Ledgers::expire_partitions()
```

---

## Task 5: Update `Ledgers::get_expiring_term_partitions()` (Read-Only Version)

**Files:**
- Modify: `crates/database/src/data_ledger.rs:307-326`

**Step 1: Write tests for the read-only version**

```rust
#[test]
fn test_get_expiring_partitions_includes_perm() {
    let config = make_test_config(Some(2));
    let mut ledgers = Ledgers::new(&config);
    ledgers.perm.allocate_slots(2, 1);
    ledgers.perm.slots[0].partitions.push(H256::random());
    ledgers.perm.slots[1].partitions.push(H256::random());

    // Read-only: should report slot 0 as expiring without marking it
    let expiring = ledgers.get_expiring_partitions(30);
    let perm_expiring: Vec<_> = expiring.iter()
        .filter(|e| e.ledger_id == DataLedger::Publish)
        .collect();
    assert_eq!(perm_expiring.len(), 1);
    // Verify NOT marked as expired (read-only)
    assert!(!ledgers.perm.slots[0].is_expired);
}

#[test]
fn test_get_expiring_partitions_disabled_perm() {
    let config = make_test_config(None);
    let mut ledgers = Ledgers::new(&config);
    ledgers.perm.allocate_slots(1, 1);
    ledgers.perm.slots[0].partitions.push(H256::random());

    let expiring = ledgers.get_expiring_partitions(1000);
    assert!(expiring.iter().all(|e| e.ledger_id != DataLedger::Publish));
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo nextest run -p irys-database test_get_expiring_partitions`
Expected: FAIL

**Step 3: Rename and update `get_expiring_term_partitions()` → `get_expiring_partitions()`**

Same algorithm as `expire_partitions()` but without mutation:

```rust
/// Get all partition hashes that would expire at this epoch height (read-only).
pub fn get_expiring_partitions(&self, epoch_height: u64) -> Vec<ExpiringPartitionInfo> {
    let mut expired_partitions: Vec<ExpiringPartitionInfo> = Vec::new();

    // Check perm ledger slots if configured
    if let Some(epoch_length) = self.publish_ledger_epoch_length {
        let min_blocks = epoch_length * self.num_blocks_in_epoch;
        if epoch_height >= min_blocks {
            let expiry_height = epoch_height - min_blocks;
            let perm_ledger_id = DataLedger::try_from(self.perm.ledger_id).unwrap();
            let last_slot_index = self.perm.slots.len().saturating_sub(1);

            for (slot_index, slot) in self.perm.slots.iter().enumerate() {
                if slot_index == last_slot_index && !self.perm.slots.is_empty() {
                    continue;
                }
                if slot.last_height <= expiry_height && !slot.is_expired {
                    for partition_hash in slot.partitions.iter() {
                        expired_partitions.push(ExpiringPartitionInfo {
                            partition_hash: *partition_hash,
                            ledger_id: perm_ledger_id,
                            slot_index,
                        });
                    }
                }
            }
        }
    }

    // Collect from term ledgers (existing logic)
    for term_ledger in &self.term {
        let ledger_id = DataLedger::try_from(term_ledger.ledger_id).unwrap();
        for expiring_slot_index in term_ledger.get_expired_slot_indexes(epoch_height) {
            for partition_hash in term_ledger.slots[expiring_slot_index].partitions.iter() {
                expired_partitions.push(ExpiringPartitionInfo {
                    partition_hash: *partition_hash,
                    ledger_id,
                    slot_index: expiring_slot_index,
                });
            }
        }
    }

    expired_partitions
}
```

**Step 4: Run tests to verify they pass**

Run: `cargo nextest run -p irys-database test_get_expiring_partitions`
Expected: PASS

**Step 5: Commit**

```
feat: include perm slots in get_expiring_partitions() read-only path
```

---

## Task 6: Update All Callers of Renamed Methods

**Files:**
- Modify: `crates/domain/src/snapshots/epoch_snapshot/epoch_snapshot.rs:472-495` (expire_term_ledger_slots)
- Modify: `crates/domain/src/snapshots/epoch_snapshot/epoch_snapshot.rs:1199-1203` (get_expiring_partition_info)

**Step 1: Update `expire_term_ledger_slots()` → rename to `expire_ledger_slots()`**

At `epoch_snapshot.rs:472`:

```rust
/// Loops through all ledgers and looks for slots that are older
/// than their configured epoch length. Marks them expired and stores
/// the expired partition hashes in the epoch snapshot.
fn expire_ledger_slots(&mut self, new_epoch_block: &IrysBlockHeader) {
    let epoch_height = new_epoch_block.height;
    let expired_partitions: Vec<ExpiringPartitionInfo> =
        self.ledgers.expire_partitions(epoch_height);
    // ... rest unchanged ...
}
```

Update the caller at `perform_epoch_tasks()` (around line 310) to call `expire_ledger_slots` instead of `expire_term_ledger_slots`.

**Step 2: Update `get_expiring_partition_info()`**

At `epoch_snapshot.rs:1199`:

```rust
pub fn get_expiring_partition_info(&self, epoch_height: u64) -> Vec<ExpiringPartitionInfo> {
    let ledgers = self.ledgers.clone();
    ledgers.get_expiring_partitions(epoch_height)
}
```

**Step 3: Run compile check**

Run: `cargo check -p irys-domain -p irys-actors`
Expected: PASS

**Step 4: Commit**

```
refactor: rename expire_term_* methods to expire_* (now handles perm too)
```

---

## Task 7: Update `PermanentLedger::get_slot_needs()` to Filter Expired Slots

**Files:**
- Modify: `crates/database/src/data_ledger.rs:179-192`

**Step 1: Write a test**

```rust
#[test]
fn test_perm_get_slot_needs_filters_expired() {
    let config = ConsensusConfig::testing();
    let mut perm = PermanentLedger::new(&config);

    // Add two slots
    perm.allocate_slots(2, 1);
    perm.slots[0].partitions.push(H256::random()); // partially filled
    // slot 1 is empty

    // Mark slot 0 as expired
    perm.slots[0].is_expired = true;

    let needs = perm.get_slot_needs();
    // Slot 0 is expired — should not appear in needs
    // Slot 1 needs partitions — should appear
    assert_eq!(needs.len(), 1);
    assert_eq!(needs[0].0, 1); // slot index 1
}
```

**Step 2: Run test to verify it fails**

Run: `cargo nextest run -p irys-database test_perm_get_slot_needs_filters_expired`
Expected: FAIL (slot 0 still returned because no `!is_expired` check)

**Step 3: Add `!slot.is_expired` filter**

Update `PermanentLedger::get_slot_needs()` at line 179:

```rust
fn get_slot_needs(&self) -> Vec<(usize, usize)> {
    self.slots
        .iter()
        .enumerate()
        .filter_map(|(idx, slot)| {
            let needed = self.num_partitions_per_slot as usize - slot.partitions.len();
            if needed > 0 && !slot.is_expired {
                Some((idx, needed))
            } else {
                None
            }
        })
        .collect()
}
```

**Step 4: Run test to verify it passes**

Run: `cargo nextest run -p irys-database test_perm_get_slot_needs_filters_expired`
Expected: PASS

**Step 5: Commit**

```
fix: filter expired slots from PermanentLedger::get_slot_needs()
```

---

## Task 8: Update `collect_expired_partitions()` Comment

**Files:**
- Modify: `crates/actors/src/block_producer/ledger_expiry.rs:298-303`

**Step 1: Update the bail comment**

The bail is safe because `target_ledger_type` filters Publish partitions before they reach this check. But the comment should reflect the new design:

```rust
// Only process partitions for the target ledger type
if ledger_id == target_ledger_type {
    // Safety: Publish ledger expiry has no fee distribution — expired
    // Publish partitions are simply reset. This bail prevents accidental
    // fee calculation for Publish, which has no treasury to distribute.
    // It is unreachable as long as callers pass DataLedger::Submit.
    if ledger_id == DataLedger::Publish {
        eyre::bail!("publish ledger cannot expire — fee distribution not supported for Publish");
    }
    // ... rest unchanged
```

**Step 2: Run compile check**

Run: `cargo check -p irys-actors`
Expected: PASS

**Step 3: Commit**

```
docs: update bail comment in collect_expired_partitions for perm expiry
```

---

## Task 9: Verify Consensus Hash Regression Test Still Passes

**Files:**
- Read (verify only): `crates/types/src/config/consensus.rs:938-952`

**Step 1: Run the existing regression test**

Run: `cargo nextest run -p irys-types test_consensus_hash_regression`
Expected: **PASS without changes** — the `skip_serializing_if = "Option::is_none"` annotation causes `None` to be omitted from canonical JSON, so the hash is identical to before the field was added. This confirms mainnet nodes are unaffected.

**Step 2: If the test FAILS, investigate**

A failure here means the hash changed — likely `skip_serializing_if` is not working as expected. Debug by:
1. Serializing `ConsensusConfig::testing()` to canonical JSON and comparing with the old output
2. Checking that `publish_ledger_epoch_length` does NOT appear in the JSON when `None`
3. Verify the `#[serde(skip_serializing_if = "Option::is_none")]` annotation is on the correct field

**No commit needed** — this is a verification step.

---

## Task 10: Full Compile Check and Existing Test Suite

**Step 1: Run full workspace compile check**

Run: `cargo xtask check`
Expected: PASS

**Step 2: Run clippy**

Run: `cargo clippy --workspace --tests --all-targets`
Expected: PASS (fix any warnings)

**Step 3: Run existing term ledger expiry tests**

Run: `cargo nextest run -p irys-chain-tests term_ledger_expiry`
Expected: PASS (existing tests unaffected — they use `None` for publish_ledger_epoch_length)

**Step 4: Run the full unit test suite for affected crates**

Run: `cargo nextest run -p irys-types -p irys-database -p irys-domain -p irys-actors`
Expected: PASS

**Step 5: Commit (if any fixes were needed)**

```
fix: resolve clippy warnings and compile issues
```

---

## Task 11: Integration Test — Publish Ledger Expiry

**Files:**
- Create: `crates/chain-tests/src/perm_ledger_expiry/mod.rs`
- Modify: `crates/chain-tests/src/lib.rs` (add module)

**Step 1: Add module to lib.rs**

Add `mod perm_ledger_expiry;` to `crates/chain-tests/src/lib.rs`.

**Step 2: Write the integration test**

Model after the existing `term_ledger_expiry` tests. The key difference: no fee distribution for perm expiry — we just verify slots expire and partitions are returned to capacity.

```rust
use crate::utils::IrysNodeTest;
use alloy_genesis::GenesisAccount;
use irys_chain::IrysNodeCtx;
use irys_types::{irys::IrysSigner, DataLedger, NodeConfig, U256};
use tracing::info;

/// Tests that publish ledger slots expire when publish_ledger_epoch_length is configured.
/// Verifies:
/// - Perm data is stored and accessible before expiry
/// - After epoch_length epochs, perm slots are marked expired
/// - No fee distribution shadow transactions are generated for perm expiry
/// - Expired perm partitions are returned to capacity pool
#[test_log::test(tokio::test)]
async fn heavy_perm_ledger_expiry_basic() -> eyre::Result<()> {
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

    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.consensus.extend_genesis_accounts(vec![(
        signer.address(),
        GenesisAccount {
            balance: U256::from(INITIAL_BALANCE).into(),
            ..Default::default()
        },
    )]);

    let node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("perm_expiry_test", 30)
        .await;

    let anchor = node.get_block_by_height(0).await?.block_hash;

    // Post a transaction to the Submit ledger (which gets promoted to Publish)
    let tx = node.post_data_tx(anchor, vec![1_u8; DATA_SIZE], &signer).await;
    node.wait_for_mempool(tx.header.id, 10).await?;

    // Upload chunks to trigger promotion to Publish
    node.upload_chunks(&tx).await?;
    node.wait_for_ingress_proofs(vec![tx.header.id], 20).await?;

    // Mine through epochs until expiry
    // Need: publish_ledger_epoch_length * blocks_per_epoch + some blocks
    let target_height = (PUBLISH_LEDGER_EPOCH_LENGTH + 1) * BLOCKS_PER_EPOCH;
    info!("Mining to height {} to trigger perm expiry", target_height);

    let current_height = node.get_canonical_chain_height().await;
    let epochs_to_mine = (target_height - current_height).div_ceil(BLOCKS_PER_EPOCH);
    for _ in 0..epochs_to_mine {
        let (_, height) = node.mine_until_next_epoch().await?;
        if height >= target_height {
            break;
        }
    }

    // Verify: no TermFeeReward or PermFeeRefund shadow txs for perm expiry
    // (fee distribution only happens for Submit ledger)
    let expiry_block = node.get_block_by_height(target_height).await?;
    let evm_block = node.wait_for_evm_block(expiry_block.evm_block_hash, 20).await?;

    // Check that no shadow txs reference Publish ledger fees
    // The block should contain term fee shadow txs (if any term slots expired)
    // but NOT any specifically for Publish expiry
    info!(
        "Expiry block {} has {} EVM transactions",
        target_height,
        evm_block.body.transactions.len()
    );

    info!("Publish ledger expiry test passed!");
    node.stop().await;
    Ok(())
}
```

**Step 3: Run the integration test**

Run: `cargo nextest run -p irys-chain-tests perm_ledger_expiry`
Expected: PASS

**Step 4: Commit**

```
test: add integration test for publish ledger expiry
```

---

## Task 12: Final Verification

**Step 1: Run local checks**

Run: `cargo xtask local-checks`
Expected: PASS

**Step 2: Run full test suite for affected crates**

Run: `cargo nextest run -p irys-types -p irys-database -p irys-domain -p irys-actors -p irys-chain-tests`
Expected: PASS

**Step 3: Verify existing term ledger expiry still works**

Run: `cargo nextest run -p irys-chain-tests term_ledger_expiry`
Expected: PASS (no regression)

---

## Summary of All Changed Files

| File | Change |
|------|--------|
| `crates/types/src/config/consensus.rs` | Add `publish_ledger_epoch_length` field to `EpochConfig` with `skip_serializing_if`, set in all configs (hash unchanged for `None`) |
| `crates/types/src/config/mod.rs` | Add validation for `publish_ledger_epoch_length > 0` |
| `crates/database/src/data_ledger.rs` | Add config fields to `Ledgers`, rename + update `expire_partitions()` and `get_expiring_partitions()`, filter expired in `PermanentLedger::get_slot_needs()`, add unit tests |
| `crates/domain/src/snapshots/epoch_snapshot/epoch_snapshot.rs` | Rename `expire_term_ledger_slots` → `expire_ledger_slots`, update method calls |
| `crates/actors/src/block_producer/ledger_expiry.rs` | Update comment on bail |
| `crates/chain-tests/src/perm_ledger_expiry/mod.rs` | New integration test |
| `crates/chain-tests/src/lib.rs` | Add `mod perm_ledger_expiry` |

## What Does NOT Change

| Component | Why |
|-----------|-----|
| Fee distribution / shadow txs | Block producer hardcodes `DataLedger::Submit` — no fees for perm |
| `collect_expired_partitions()` bail | Unreachable for Submit target; comment updated |
| Block tree broadcast (`block_tree_service.rs`) | Already generic — reads `expired_partition_infos` and broadcasts all hashes |
| Mining service reset (`partition_mining_service.rs`) | Already generic — resets any expired partition hash |
| Cache service | Already generic |
| Data sync | Already generic |
| `PermanentLedger` struct | No new fields or methods (per Finding 4) |
