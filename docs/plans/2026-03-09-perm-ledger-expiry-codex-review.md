# Publish Ledger Expiry — Codex + Claude Security Review

**Date:** 2026-03-09
**Branch:** `rob/perm-ledger-expiry`
**Reviewers:** OpenAI Codex (gpt-5.3-codex), Claude (claude-opus-4-6)
**Scope:** Blockchain security, fundamental correctness, edge cases
**Method:** Static analysis of the 11 commits on the branch

---

## Branch Summary

This branch adds optional expiry for the Publish (permanent) ledger, intended for testnet only. On mainnet, `publish_ledger_epoch_length: None` means the Publish ledger never expires. On testnet, `Some(168)` causes Publish slots to expire after ~7 days.

Key changes:
- `EpochConfig` gains `publish_ledger_epoch_length: Option<u64>` with `#[serde(default, skip_serializing_if = "Option::is_none")]`
- `Ledgers::expire_partitions()` and `Ledgers::get_expiring_partitions()` (renamed from `expire_term_*`) now include perm slots when configured
- `PermanentLedger::get_slot_needs()` filters expired slots
- No fee distribution for Publish expiry — partitions are simply reset
- The bail guard in `collect_expired_partitions()` is unchanged (comment updated)

---

## Finding 1 (Medium-High): Cross-ledger coupling in fee distribution path

**Severity:** Medium-High
**Status:** Open
**Files:** `crates/actors/src/block_producer/ledger_expiry.rs:277-299`

### Problem

`collect_expired_partitions()` calls `get_expiring_partition_info()` at line 277, which now returns **both** Publish and Submit expired partitions. Before the ledger type filter at line 299, it performs a `partition_assignments.get_assignment()` lookup for **every** expired partition at line 291-293:

```rust
for expired_partition in expired_partition_info {
    let partition = partition_assignments
        .get_assignment(expired_partition.partition_hash)
        .ok_or_eyre("could not get expired partition")?;  // <-- bails on ANY missing partition

    let ledger_id = expired_partition.ledger_id;

    if ledger_id == target_ledger_type {  // <-- filter happens AFTER the lookup
        // ...
    }
}
```

If a Publish partition hash is expired but missing from `partition_assignments` (e.g. due to a reorg or state inconsistency), the `.ok_or_eyre()` would bail and **prevent all Submit fee distribution** for that epoch block. This coupling didn't exist before — previously only Submit partitions appeared in the expired list.

### Impact

A Publish-related state inconsistency could halt Submit fee processing, which generates shadow transactions for miner rewards and user refunds. This would cause the block producer to fail to build the epoch block.

### Recommendation

Filter by ledger type **before** the assignment lookup, or handle missing Publish partition assignments gracefully (e.g. `continue` instead of `bail`).

---

## Finding 2 (Medium): Integration test does not verify expiry behavior

**Severity:** Medium
**Status:** Open
**Files:** `crates/chain-tests/src/perm_ledger_expiry/mod.rs:6-77`

### Problem

The test's doc comment claims to verify:
- Perm data is stored and accessible before expiry
- After `epoch_length` epochs, perm slots are marked expired
- No fee distribution shadow transactions are generated for perm expiry
- Expired perm partitions are returned to capacity pool

But the only assertions are:

```rust
assert!(final_height >= target_height, "Should have reached target height");
```

This proves the node doesn't crash when Publish expiry occurs, which is valuable. But it doesn't verify any of the claimed behaviors. The test would pass even if expiry was completely broken, as long as the node mines to the target height.

### Recommendation

Add assertions that:
1. Check the epoch snapshot's Publish ledger slots are marked `is_expired == true` after the expiry height
2. Verify no fee-related shadow transactions exist in the expiry epoch block
3. Confirm expired partition hashes are returned to the capacity pool (present in `capacity_partitions`, absent from `data_partitions`)

---

## Finding 3 (Low): Unchecked multiplication overflow in expiry arithmetic

**Severity:** Low (pre-existing pattern, extremely low practical risk)
**Status:** Open
**Files:** `crates/database/src/data_ledger.rs:298, :348`

### Problem

```rust
let min_blocks = epoch_length * self.num_blocks_in_epoch;
```

This multiplication is unchecked. With extreme config values (e.g. `publish_ledger_epoch_length: Some(u64::MAX)`), it could wrap to a small value in release builds and cause premature Publish expiry.

### Mitigating factors

- Both values are consensus config parameters verified via P2P hash handshake
- Validation ensures `publish_ledger_epoch_length > 0` but has no upper bound
- The **exact same unchecked multiplication** exists in `TermLedger::get_expired_slot_indexes()` — this is a pre-existing pattern, not a new risk
- Any value large enough to overflow would represent epochs lasting billions of years

### Recommendation

Add `checked_mul` with a descriptive panic/bail, or add a config validation upper bound (e.g. `<= 1_000_000`). Apply to both Publish and Term paths for consistency.

---

## Finding 4 (Low): Bail guard is dead code and a future-proofing trap

**Severity:** Low
**Status:** Open
**Files:** `crates/actors/src/block_producer/ledger_expiry.rs:299-308`

### Problem

```rust
if ledger_id == target_ledger_type {
    if ledger_id == DataLedger::Publish {
        eyre::bail!("publish ledger cannot expire — fee distribution not supported for Publish");
    }
    // ... collect miners for fee distribution
}
```

This bail is unreachable today because both call sites hardcode `DataLedger::Submit`:
- `block_producer.rs:1588` — `DataLedger::Submit`
- `block_validation.rs:1623` — `DataLedger::Submit`

The comment correctly documents this. However, a future developer adding a third call site with `DataLedger::Publish` would compile successfully and only discover the failure at runtime when the first Publish expiry occurs.

### Recommendation

Move the guard to the function entry point of `calculate_expired_ledger_fees()`:

```rust
debug_assert_ne!(ledger_type, DataLedger::Publish, "fee distribution not supported for Publish");
```

Or use a compile-time constraint (e.g. a newtype wrapper that can only be `Submit`).

---

## Confirmed Safe: Consensus hash stability

**Status:** Safe
**Files:** `crates/types/src/config/consensus.rs:332`

The `#[serde(default, skip_serializing_if = "Option::is_none")]` annotation on `publish_ledger_epoch_length` correctly omits `None` from the canonical JSON serialization. This means:

- **Mainnet** (`None`): field is absent from JSON, hash is identical to before the field was added
- **Testnet** (`Some(168)`): field is present in JSON, hash changes — acceptable since testnet is being reset
- The `test_consensus_hash_regression` test passes without changes

This is the correct approach. The existing `num_capacity_partitions: Option<u64>` does NOT use `skip_serializing_if` (serializes as `null`), but that field existed from the start so its `null` is baked into the hash.

---

## Confirmed Safe: Expiry algorithm correctness

**Status:** Safe
**Files:** `crates/database/src/data_ledger.rs:290-337, 340-382`

The Publish expiry logic in `expire_partitions()` and `get_expiring_partitions()` exactly mirrors `TermLedger::get_expired_slot_indexes()`:

| Aspect | TermLedger | Publish (new) |
|--------|-----------|---------------|
| Min blocks check | `epoch_height < min_blocks → return` | `epoch_height >= min_blocks` guard |
| Expiry height calc | `epoch_height - min_blocks` | Same |
| Last slot protection | `*idx != last_slot` | `num_slots > 0 && slot_index == last_slot_index` |
| Already-expired skip | `!slot.is_expired` | Same |
| Mutating vs read-only | `expire_old_slots` vs `get_expired_slot_indexes` | `expire_partitions` vs `get_expiring_partitions` |

The mutating and read-only versions are logically consistent. They will agree on what expires at any given `epoch_height`.

---

## Confirmed Safe: Edge cases

| Scenario | Behavior | Correct? |
|----------|----------|----------|
| 0 slots | No iteration | Yes |
| 1 slot | Never expires (last-slot protection) | Yes |
| Empty slots (no partitions) | Slot marked expired but no `ExpiringPartitionInfo` emitted | Yes |
| `epoch_height == 0` | `0 >= min_blocks` is false (unless overflow), no expiry | Yes |
| `epoch_height == min_blocks` | `expiry_height = 0`, expires slots with `last_height <= 0` except last | Yes (matches Term) |
| `last_height == 0` | Expires at first eligible boundary | Yes (matches Term) |
| Underflow in subtraction | Guarded by `epoch_height >= min_blocks` | Yes |
| `publish_ledger_epoch_length: None` | `if let Some(epoch_length)` skips entire block | Yes |
| Data race between mutating/read-only | Read-only clones `Ledgers` before iterating | Yes |

---

## Confirmed Safe: Mainnet data loss risk

**Risk level:** Very low

Multiple layers protect against accidental Publish expiry on mainnet:

1. **Config default**: Mainnet config hardcodes `publish_ledger_epoch_length: None` (`consensus.rs:596`)
2. **`Option` guard**: The expiry code is gated on `if let Some(epoch_length) = self.publish_ledger_epoch_length` — `None` skips entirely
3. **Consensus hash**: If an operator accidentally sets `Some(n)` on mainnet, the consensus hash changes, and the node cannot join the canonical mainnet P2P network
4. **Config validation**: `publish_ledger_epoch_length > 0` is enforced at startup

The design review (Finding 5) suggested also validating `publish_ledger_epoch_length.is_none()` when `chain_id == 3282` (mainnet). This was not implemented but is a reasonable additional safeguard.

---

## Summary of Actionable Items

| # | Severity | Finding | Action needed |
|---|----------|---------|---------------|
| 1 | Medium-High | Cross-ledger coupling in fee path | Filter by ledger type before partition lookup |
| 2 | Medium | Integration test lacks assertions | Add expiry state verification |
| 3 | Low | Unchecked multiplication overflow | Add `checked_mul` or config upper bound |
| 4 | Low | Bail guard is unreachable dead code | Move guard to function entry or use type constraint |
