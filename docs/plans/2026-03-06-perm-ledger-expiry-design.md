# Publish Ledger Expiry (Testnet-Only)

## Problem

The Publish (permanent) ledger never expires by design — data is stored for 200+ years. On testnet this is wasteful: testnet operators pay real infrastructure costs to store data that has no lasting value. We need the Publish ledger to expire on testnet while remaining permanent on mainnet.

## Decision

Add `publish_ledger_epoch_length: Option<u64>` to `EpochConfig`. When `None` (mainnet), the Publish ledger never expires. When `Some(n)` (testnet), Publish ledger slots expire after `n` epochs using the same mechanism as term ledger expiry. No fee distribution occurs on Publish expiry — partitions are simply reset.

## Design

### Config

**`EpochConfig`** (`crates/types/src/config/consensus.rs`):

```rust
pub struct EpochConfig {
    pub capacity_scalar: u64,
    pub num_blocks_in_epoch: u64,
    pub submit_ledger_epoch_length: u64,
    pub num_capacity_partitions: Option<u64>,
    pub publish_ledger_epoch_length: Option<u64>, // NEW — None = never expire
}
```

| Network | `publish_ledger_epoch_length` | Effective duration |
|---------|------------------------------|--------------------|
| Mainnet | `None` | Never expires |
| Testnet | `Some(168)` | ~7 days (168 epochs × 1 hour/epoch) |
| Testing | `None` | Never expires |

### `PermanentLedger` — optional expiry

**`PermanentLedger`** (`crates/database/src/data_ledger.rs`):

```rust
pub struct PermanentLedger {
    pub slots: Vec<LedgerSlot>,
    pub ledger_id: u32,
    pub num_partitions_per_slot: u64,
    pub epoch_length: Option<u64>,        // NEW
    pub num_blocks_in_epoch: Option<u64>, // NEW
}
```

Add two methods mirroring `TermLedger`:

- **`get_expired_slot_indexes(&self, epoch_height: u64) -> Vec<usize>`** — returns `vec![]` when `epoch_length` is `None`. Otherwise same logic as `TermLedger`: `expiry_height = epoch_height - (epoch_length * num_blocks_in_epoch)`, find slots with `last_height <= expiry_height && !is_expired`, skip last slot.
- **`expire_old_slots(&mut self, epoch_height: u64) -> Vec<usize>`** — calls `get_expired_slot_indexes`, marks matching slots `is_expired = true`.

Update **`get_slot_needs()`** — when expiry is configured, filter out `is_expired` slots (matching `TermLedger` behavior).

### `Ledgers::expire_term_partitions()` — include perm

Add perm processing at the top of the existing method:

```rust
pub fn expire_term_partitions(&mut self, epoch_height: u64) -> Vec<ExpiringPartitionInfo> {
    let mut expired_partitions = Vec::new();

    // Expire perm ledger slots if configured
    let perm_ledger_id = DataLedger::try_from(self.perm.ledger_id).unwrap();
    for expired_index in self.perm.expire_old_slots(epoch_height) {
        for partition_hash in self.perm.slots[expired_index].partitions.iter() {
            expired_partitions.push(ExpiringPartitionInfo {
                partition_hash: *partition_hash,
                ledger_id: perm_ledger_id,
                slot_index: expired_index,
            });
        }
    }

    // Existing term ledger loop (unchanged)
    for term_ledger in &mut self.term { ... }

    expired_partitions
}
```

### Construction

Where `PermanentLedger` is constructed, pass:
- `epoch_length: epoch_config.publish_ledger_epoch_length`
- `num_blocks_in_epoch: publish_ledger_epoch_length.map(|_| epoch_config.num_blocks_in_epoch)`

### What stays unchanged

| Component | Why no change needed |
|-----------|---------------------|
| Fee distribution | Block producer only calls `calculate_expired_ledger_fees()` for `DataLedger::Submit` — no fee shadow txs for perm expiry |
| `bail!("publish ledger cannot expire")` | Stays as safety net in `collect_expired_partitions()` — never reached |
| Block tree broadcast | Already generic — sends all expired partition hashes |
| Mining service reset | Already generic — resets any expired partition |
| Cache service | Already generic |
| Data sync | Already generic |

### Data flow on perm expiry (testnet)

```
Epoch boundary
  → Ledgers::expire_term_partitions() marks old Publish slots expired
  → Partitions returned to capacity pool
  → BroadcastPartitionsExpiration sent to miners
  → Storage modules reset, chunks deleted
  → No fee shadow transactions
  → API returns 404 for expired data
```

## Alternatives considered

1. **Construct Publish as a TermLedger when configured** — reuses all term logic, but requires invasive changes to Ledgers dispatch (Index/IndexMut). Higher risk.
2. **Extract expiry into a shared trait** — cleaner abstraction, but over-engineered for a testnet-only feature.
3. **Skip fees entirely (chosen)** vs distribute perm treasury to miners — simpler, and on testnet the economic model doesn't matter.
