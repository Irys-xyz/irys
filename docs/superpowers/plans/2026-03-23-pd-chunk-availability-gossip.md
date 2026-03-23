# PD Chunk Availability Gossip Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace pull-only PD chunk fetching with availability gossip + targeted pull, spreading load away from canonical partition assignees without gossiping chunk bytes.

**Architecture:** After a block is validated, an announcer task in `crates/actors` extracts the block's PD chunk specs, checks which chunks the local node can serve, coalesces them into compact range metadata, and sends pre-built fragments to `P2PService` via a dedicated channel. P2PService fans out fragments to a bounded peer subset. Receivers store provider hints in a TTL cache. When fetching PD chunks, nodes try hinted providers before canonical assignees. No chunk bytes are ever gossiped.

**Tech Stack:** Rust 1.93, moka (TTL cache), actix-web (HTTP route), tokio channels

**Spec:** `docs/superpowers/specs/2026-03-23-pd-chunk-availability-gossip-design.md`

---

## File Structure

### New Files

| File | Responsibility |
|------|---------------|
| `crates/p2p/src/pd_chunk_provider_cache.rs` | TTL-bounded cache: `(ledger, offset)` -> bounded set of provider `SocketAddr` |
| `crates/p2p/src/pd_availability_handler.rs` | Receive handler for `POST /gossip/v2/pd_chunk_availability` |
| `crates/p2p/src/pd_availability_rate_limiter.rs` | Per-peer sliding-window byte/chunk budget |
| `crates/actors/src/pd_availability_announcer.rs` | Background task: subscribe to Valid blocks, build availability fragments, send to P2PService for fanout |

### Modified Files

| File | Change |
|------|--------|
| `crates/types/src/gossip.rs` | Add `PdChunkRange`, `PdChunkAvailabilityBatch`, `GossipDataV2::PdChunkAvailability`, `GossipCacheKey::PdChunkAvailability` |
| `crates/types/src/config/node.rs` | Add `PdAvailabilityConfig` with fanout/rate-limit/fetch params, nest in `ConsensusConfig` |
| `crates/types/src/chunk_provider.rs` | Add `has_chunk_for_pd` method to `ChunkStorageProvider` trait |
| `crates/types/src/range_specifier.rs` | Add `specs_to_ledger_offsets()`, `coalesce_ledger_offsets_to_ranges()` |
| `crates/domain/src/models/chunk_provider.rs` | Implement `has_chunk_for_pd`; fix `get_chunk_for_pd` to check `CachedChunks` first |
| `crates/p2p/src/server.rs` | Register new route with per-route body limit |
| `crates/p2p/src/gossip_service.rs` | Spawn PD availability fanout task from new channel |
| `crates/p2p/src/pd_chunk_fetcher.rs` | Merge hinted providers with canonical assignees |
| `crates/p2p/src/cache.rs` | Add `pd_chunk_availability` field to `GossipCache` |
| `crates/p2p/src/types.rs` | Add `GossipRoutes::PdChunkAvailability` |
| `crates/p2p/src/lib.rs` | Declare new modules |
| `crates/actors/src/services.rs` | Add `pd_availability` channel pair to `ServiceSendersInner` / `ServiceReceivers` |
| `crates/actors/src/lib.rs` | Declare `pd_availability_announcer` module |
| `crates/chain/src/chain.rs` | Spawn announcer task, pass receiver to `P2PService` |

---

## Key Architectural Decisions

1. **Announce orchestration lives in `crates/actors`**, not `crates/p2p`. The announcer needs `ExecutionPayloadCache`, `BlockTreeReadGuard`, `extract_pd_chunk_specs` (from `irys-reth`), and `ChunkStorageProvider`. The actors crate already has access to all of these. P2PService only handles fanout (its natural responsibility).

2. **Communication via a dedicated `UnboundedSender/Receiver` channel** in `ServiceSenders`. The announcer sends pre-built `Vec<PdChunkAvailabilityBatch>` fragments. P2PService reads them and fans out to selected peers. This follows the existing `gossip_broadcast` channel pattern.

3. **No modification to `BlockStateUpdated`**. The announcer subscribes to the existing `block_state_events` broadcast, then independently looks up the block header (from `BlockTreeReadGuard`) and EVM block (from `ExecutionPayloadCache`) to extract PD chunk specs.

4. **`PdChunkProviderCache` uses `moka::sync::Cache`** with 5-minute TTL, matching the existing `GossipCache` pattern. Values are `SmallVec<[SocketAddr; 8]>` to stay cache-line friendly.

5. **Fanout peer selection**: half from best-scored peers, half random, shuffled. This avoids clique-only behavior while prioritizing reliable peers.

---

## Task 1: Foundation Wire Types

**Files:**
- Modify: `crates/types/src/gossip.rs`

- [ ] **Step 1: Add `PdChunkRange` struct**

Add after the existing `GossipCacheKey` block (around line 436):

```rust
/// A contiguous run of PD chunk coordinates that a peer can serve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PdChunkRange {
    pub ledger: u32,
    pub start_offset: u64,
    pub chunk_count: u16,
}

impl PdChunkRange {
    /// Total chunks represented by this range.
    pub fn total_chunks(&self) -> u64 {
        self.chunk_count as u64
    }
}
```

- [ ] **Step 2: Add `PdChunkAvailabilityBatch` struct**

```rust
/// One bounded availability announcement fragment.
/// `availability_id` is unique per fragment and used as the dedup key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PdChunkAvailabilityBatch {
    pub availability_id: H256,
    pub block_hash: BlockHash,
    pub ranges: Vec<PdChunkRange>,
}

impl PdChunkAvailabilityBatch {
    /// Sum of all chunks announced in this batch.
    pub fn total_announced_chunks(&self) -> u64 {
        self.ranges.iter().map(|r| r.total_chunks()).sum()
    }
}
```

- [ ] **Step 3: Add `GossipDataV2::PdChunkAvailability` variant**

In the `GossipDataV2` enum (around line 252), add after `PdChunk(ChunkFormat)`:

```rust
PdChunkAvailability(PdChunkAvailabilityBatch),
```

Update `to_v1()` (around line 279) to return `None` for this variant.
Update `data_type_and_id()` (around line 330) to return `("pd chunk availability", batch.availability_id.to_string())`.

- [ ] **Step 4: Add `GossipCacheKey::PdChunkAvailability` variant**

In the `GossipCacheKey` enum (around line 403), add:

```rust
PdChunkAvailability(H256),
```

Add a constructor method:

```rust
pub fn pd_chunk_availability(availability_id: H256) -> Self {
    Self::PdChunkAvailability(availability_id)
}
```

- [ ] **Step 5: Write serialization roundtrip tests**

```rust
#[cfg(test)]
mod pd_availability_tests {
    use super::*;

    #[test]
    fn pd_chunk_range_serde_roundtrip() {
        let range = PdChunkRange { ledger: 0, start_offset: 1000, chunk_count: 50 };
        let json = serde_json::to_string(&range).unwrap();
        let decoded: PdChunkRange = serde_json::from_str(&json).unwrap();
        assert_eq!(range, decoded);
    }

    #[test]
    fn pd_chunk_availability_batch_serde_roundtrip() {
        let batch = PdChunkAvailabilityBatch {
            availability_id: H256::random(),
            block_hash: H256::random().into(),
            ranges: vec![
                PdChunkRange { ledger: 0, start_offset: 100, chunk_count: 10 },
                PdChunkRange { ledger: 0, start_offset: 200, chunk_count: 5 },
            ],
        };
        let json = serde_json::to_string(&batch).unwrap();
        let decoded: PdChunkAvailabilityBatch = serde_json::from_str(&json).unwrap();
        assert_eq!(batch, decoded);
    }

    #[test]
    fn total_announced_chunks() {
        let batch = PdChunkAvailabilityBatch {
            availability_id: H256::random(),
            block_hash: H256::random().into(),
            ranges: vec![
                PdChunkRange { ledger: 0, start_offset: 0, chunk_count: 10 },
                PdChunkRange { ledger: 0, start_offset: 100, chunk_count: 20 },
            ],
        };
        assert_eq!(batch.total_announced_chunks(), 30);
    }
}
```

- [ ] **Step 6: Run tests**

Run: `cargo nextest run -p irys-types pd_availability_tests`
Expected: PASS

- [ ] **Step 7: Compile check**

Run: `cargo check -p irys-types -p irys-p2p -p irys-actors`
Expected: Compilation succeeds (update match arms in p2p/actors for new enum variants as needed — add `PdChunkAvailability` arms that mirror the existing `PdChunk` handling: return `None` in `pre_serialize_for_broadcast`, return `Rejected` in `send_data`, etc.)

---

## Task 2: Config Parameters

**Files:**
- Modify: `crates/types/src/config/node.rs`

- [ ] **Step 1: Add `PdAvailabilityConfig` struct**

Add near the existing `P2PGossipConfig` (around line 541):

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PdAvailabilityConfig {
    /// Number of peers to send availability announcements to.
    pub fanout: usize,
    /// Max chunks per availability message fragment.
    pub max_chunks_per_message: usize,
    /// Max ranges per availability message fragment.
    pub max_ranges_per_message: usize,
    /// Max body bytes for the receive route (pre-deserialization cap).
    pub max_body_bytes: usize,
    /// Max hinted providers to try before falling back to assignees.
    pub fetch_max_hinted_peers: usize,
    /// Max assignee peers to try during PD chunk fetch.
    pub fetch_max_assignee_peers: usize,
    /// Per-peer max announced chunks per minute (sliding window).
    pub rate_limit_chunks_per_minute: u64,
    /// TTL for provider hints in seconds.
    pub provider_cache_ttl_secs: u64,
    /// Max providers stored per chunk in the hint cache.
    pub max_providers_per_chunk: usize,
}

impl Default for PdAvailabilityConfig {
    fn default() -> Self {
        Self {
            fanout: 8,
            max_chunks_per_message: 256,
            max_ranges_per_message: 64,
            max_body_bytes: 256 * 1024,
            fetch_max_hinted_peers: 4,
            fetch_max_assignee_peers: 4,
            rate_limit_chunks_per_minute: 4096,
            provider_cache_ttl_secs: 300,
            max_providers_per_chunk: 8,
        }
    }
}
```

- [ ] **Step 2: Add field to `ConsensusConfig`**

In `ConsensusConfig` (around line 121), add:

```rust
pub pd_availability: PdAvailabilityConfig,
```

Ensure `#[serde(default)]` is applied so existing configs deserialize without this field.

- [ ] **Step 3: Compile check**

Run: `cargo check -p irys-types`
Expected: PASS

---

## Task 3: Shared Utility Functions

**Files:**
- Modify: `crates/types/src/range_specifier.rs`

- [ ] **Step 1: Write test for `specs_to_ledger_offsets`**

```rust
#[cfg(test)]
mod specs_to_ledger_offsets_tests {
    use super::*;

    #[test]
    fn single_spec_single_chunk() {
        let spec = ChunkRangeSpecifier {
            partition_index: U200::from(0u64),
            offset: 5,
            chunk_count: 1,
        };
        let offsets = specs_to_ledger_offsets(&[spec], 1000);
        // offset = 0 * 1000 + 5 + 0 = 5
        assert_eq!(offsets, vec![(0, 5)]);
    }

    #[test]
    fn single_spec_multiple_chunks() {
        let spec = ChunkRangeSpecifier {
            partition_index: U200::from(2u64),
            offset: 10,
            chunk_count: 3,
        };
        let offsets = specs_to_ledger_offsets(&[spec], 1000);
        // base = 2 * 1000 = 2000; offsets = 2010, 2011, 2012
        assert_eq!(offsets, vec![(0, 2010), (0, 2011), (0, 2012)]);
    }

    #[test]
    fn multiple_specs() {
        let specs = vec![
            ChunkRangeSpecifier {
                partition_index: U200::from(0u64),
                offset: 0,
                chunk_count: 2,
            },
            ChunkRangeSpecifier {
                partition_index: U200::from(1u64),
                offset: 5,
                chunk_count: 1,
            },
        ];
        let offsets = specs_to_ledger_offsets(&specs, 100);
        assert_eq!(offsets, vec![(0, 0), (0, 1), (0, 105)]);
    }

    #[test]
    fn overflow_skipped() {
        let spec = ChunkRangeSpecifier {
            partition_index: U200::from(u64::MAX),
            offset: 0,
            chunk_count: 1,
        };
        // partition_index exceeds u64, should be skipped
        let offsets = specs_to_ledger_offsets(&[spec], 1000);
        assert!(offsets.is_empty());
    }
}
```

- [ ] **Step 2: Run tests, verify fail**

Run: `cargo nextest run -p irys-types specs_to_ledger_offsets_tests`
Expected: FAIL — function not defined

- [ ] **Step 3: Implement `specs_to_ledger_offsets`**

Add to `crates/types/src/range_specifier.rs`:

```rust
use crate::block::DataLedger;

/// Convert PD chunk range specifiers to `(ledger, offset)` pairs.
///
/// Mirrors the logic in `PdService::specs_to_keys` but as a free function.
/// `num_chunks_in_partition` comes from `ChunkConfig`.
/// Ledger is always `DataLedger::Publish` (0) — PD is publish-only.
pub fn specs_to_ledger_offsets(
    chunk_specs: &[ChunkRangeSpecifier],
    num_chunks_in_partition: u64,
) -> Vec<(u32, u64)> {
    let ledger = DataLedger::Publish as u32;
    let mut offsets = Vec::new();

    for spec in chunk_specs {
        let partition_index: u64 = match spec.partition_index.try_into() {
            Ok(v) => v,
            Err(_) => continue, // partition_index exceeds u64
        };

        let base_offset = match num_chunks_in_partition.checked_mul(partition_index) {
            Some(v) => v,
            None => continue, // overflow
        };

        for i in 0..spec.chunk_count {
            if let Some(offset) = base_offset
                .checked_add(spec.offset as u64)
                .and_then(|v| v.checked_add(i as u64))
            {
                offsets.push((ledger, offset));
            }
        }
    }

    offsets
}
```

- [ ] **Step 4: Run tests, verify pass**

Run: `cargo nextest run -p irys-types specs_to_ledger_offsets_tests`
Expected: PASS

- [ ] **Step 5: Write test for `coalesce_ledger_offsets_to_ranges`**

```rust
#[cfg(test)]
mod coalesce_tests {
    use super::*;
    use crate::gossip::PdChunkRange;

    #[test]
    fn empty_input() {
        assert!(coalesce_ledger_offsets_to_ranges(&[]).is_empty());
    }

    #[test]
    fn single_offset() {
        let ranges = coalesce_ledger_offsets_to_ranges(&[(0, 42)]);
        assert_eq!(ranges, vec![PdChunkRange { ledger: 0, start_offset: 42, chunk_count: 1 }]);
    }

    #[test]
    fn contiguous_offsets_coalesced() {
        let offsets = vec![(0, 10), (0, 11), (0, 12), (0, 13)];
        let ranges = coalesce_ledger_offsets_to_ranges(&offsets);
        assert_eq!(ranges, vec![PdChunkRange { ledger: 0, start_offset: 10, chunk_count: 4 }]);
    }

    #[test]
    fn gap_splits_ranges() {
        let offsets = vec![(0, 10), (0, 11), (0, 20), (0, 21)];
        let ranges = coalesce_ledger_offsets_to_ranges(&offsets);
        assert_eq!(ranges, vec![
            PdChunkRange { ledger: 0, start_offset: 10, chunk_count: 2 },
            PdChunkRange { ledger: 0, start_offset: 20, chunk_count: 2 },
        ]);
    }

    #[test]
    fn chunk_count_capped_at_u16_max() {
        // More than u16::MAX contiguous offsets should split
        let offsets: Vec<(u32, u64)> = (0..=u16::MAX as u64 + 5)
            .map(|i| (0u32, i))
            .collect();
        let ranges = coalesce_ledger_offsets_to_ranges(&offsets);
        assert_eq!(ranges[0].chunk_count, u16::MAX);
        assert_eq!(ranges[1].start_offset, u16::MAX as u64);
    }

    #[test]
    fn unsorted_input_sorted_first() {
        let offsets = vec![(0, 20), (0, 10), (0, 11)];
        let ranges = coalesce_ledger_offsets_to_ranges(&offsets);
        assert_eq!(ranges, vec![
            PdChunkRange { ledger: 0, start_offset: 10, chunk_count: 2 },
            PdChunkRange { ledger: 0, start_offset: 20, chunk_count: 1 },
        ]);
    }
}
```

- [ ] **Step 6: Run tests, verify fail**

Run: `cargo nextest run -p irys-types coalesce_tests`
Expected: FAIL

- [ ] **Step 7: Implement `coalesce_ledger_offsets_to_ranges`**

```rust
use crate::gossip::PdChunkRange;

/// Coalesce sorted `(ledger, offset)` pairs into compact `PdChunkRange`s.
///
/// Input does not need to be sorted — it will be sorted internally.
/// Contiguous offsets within the same ledger are merged into a single range.
/// Ranges are capped at `u16::MAX` chunks.
pub fn coalesce_ledger_offsets_to_ranges(offsets: &[(u32, u64)]) -> Vec<PdChunkRange> {
    if offsets.is_empty() {
        return Vec::new();
    }

    let mut sorted: Vec<(u32, u64)> = offsets.to_vec();
    sorted.sort_unstable();
    sorted.dedup();

    let mut ranges = Vec::new();
    let (mut cur_ledger, mut cur_start) = sorted[0];
    let mut cur_count: u16 = 1;

    for &(ledger, offset) in &sorted[1..] {
        let expected_next = cur_start + cur_count as u64;
        if ledger == cur_ledger && offset == expected_next && cur_count < u16::MAX {
            cur_count += 1;
        } else {
            ranges.push(PdChunkRange {
                ledger: cur_ledger,
                start_offset: cur_start,
                chunk_count: cur_count,
            });
            cur_ledger = ledger;
            cur_start = offset;
            cur_count = 1;
        }
    }
    ranges.push(PdChunkRange {
        ledger: cur_ledger,
        start_offset: cur_start,
        chunk_count: cur_count,
    });

    ranges
}
```

- [ ] **Step 8: Run tests, verify pass**

Run: `cargo nextest run -p irys-types coalesce_tests`
Expected: PASS

- [ ] **Step 9: Export both functions from the module**

Ensure `specs_to_ledger_offsets` and `coalesce_ledger_offsets_to_ranges` are `pub` and re-exported from `crates/types/src/lib.rs` if needed.

- [ ] **Step 10: Compile check**

Run: `cargo check -p irys-types`
Expected: PASS

---

## Task 4: Storage Provider Enhancements

**Files:**
- Modify: `crates/types/src/chunk_provider.rs` (trait)
- Modify: `crates/domain/src/models/chunk_provider.rs` (impl)

- [ ] **Step 1: Add `has_chunk_for_pd` to `ChunkStorageProvider` trait**

In `crates/types/src/chunk_provider.rs` (around line 35), add to the trait:

```rust
/// Cheap existence check — does this node have the chunk available to serve?
/// Returns true if the chunk is in CachedChunks or local storage.
/// Must NOT materialize full chunk bytes.
fn has_chunk_for_pd(&self, ledger: u32, ledger_offset: u64) -> eyre::Result<bool>;
```

- [ ] **Step 2: Implement `has_chunk_for_pd` in domain**

In `crates/domain/src/models/chunk_provider.rs`, add the implementation. The approach:
1. Check CachedChunks MDBX table for the `(data_root, tx_chunk_offset)` — but we don't have those from `(ledger, offset)` alone. So check if the packed storage has it.
2. Alternatively: try the same path as `get_chunk_for_pd` but return `bool` without materializing bytes.

```rust
fn has_chunk_for_pd(&self, ledger: u32, ledger_offset: u64) -> eyre::Result<bool> {
    let ledger = DataLedger::try_from(ledger)?;
    // Check packed storage (does not require reading full bytes if the storage
    // module can do an index-only check).
    Ok(self.has_chunk_by_ledger_offset(ledger, LedgerChunkOffset::from(ledger_offset)))
}
```

If `has_chunk_by_ledger_offset` doesn't exist, implement it as a thin wrapper that checks the storage module's chunk index without reading the full chunk data. If the storage module API only supports full reads, fall back to `get_chunk_by_ledger_offset(...).map(|opt| opt.is_some())` and add a `// TODO: optimize to index-only check` comment.

- [ ] **Step 3: Fix `get_chunk_for_pd` to check CachedChunks first**

In `crates/domain/src/models/chunk_provider.rs:157-169`, the current implementation has a `TODO` to check CachedChunks. Fix it:

```rust
fn get_chunk_for_pd(&self, ledger: u32, ledger_offset: u64) -> eyre::Result<Option<ChunkFormat>> {
    // Check CachedChunks first (unpacked, already verified)
    if let Some(cached) = self.get_cached_chunk_for_pd(ledger, ledger_offset)? {
        return Ok(Some(ChunkFormat::Unpacked(cached)));
    }
    // Fall back to packed from storage module
    let ledger = DataLedger::try_from(ledger)?;
    match self.get_chunk_by_ledger_offset(ledger, LedgerChunkOffset::from(ledger_offset))? {
        Some(packed) => Ok(Some(ChunkFormat::Packed(packed))),
        None => Ok(None),
    }
}
```

The `get_cached_chunk_for_pd` helper needs to look up the chunk in the `CachedChunks` MDBX table. This requires resolving `(ledger, offset)` → `(data_root, tx_chunk_offset)` → `CachedChunksIndex` → `CachedChunks`. The resolution depends on the block index (to find which tx covers that offset and its data_root).

**Important:** If this resolution is complex, it is acceptable to leave the `CachedChunks` lookup as a follow-up. The `has_chunk_for_pd` check for the availability announce flow only needs to check packed storage (which is where assignees store data). Non-assignee validators will announce chunks they have in their in-memory `ChunkCache` (the LRU cache in `PdService`), not CachedChunks. The CachedChunks-first optimization for `get_chunk_for_pd` is a serving optimization that can be done separately.

For now, a simpler `has_chunk_for_pd`:

```rust
fn has_chunk_for_pd(&self, ledger: u32, ledger_offset: u64) -> eyre::Result<bool> {
    // For assignees: check packed storage
    self.get_chunk_for_pd(ledger, ledger_offset).map(|opt| opt.is_some())
}
```

- [ ] **Step 4: Update all implementations of `ChunkStorageProvider`**

Search for all types that implement `ChunkStorageProvider` (including mocks/test impls) and add `has_chunk_for_pd`. For test mocks, a default implementation delegating to `get_chunk_for_pd().map(|o| o.is_some())` suffices.

Run: `cargo check -p irys-types -p irys-domain -p irys-actors -p irys-p2p`

- [ ] **Step 5: Compile check**

Run: `cargo check --workspace`
Expected: PASS

---

## Task 5: PdChunkProviderCache

**Files:**
- Create: `crates/p2p/src/pd_chunk_provider_cache.rs`
- Modify: `crates/p2p/src/lib.rs`

- [ ] **Step 1: Write tests first**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    #[test]
    fn insert_and_lookup() {
        let cache = PdChunkProviderCache::new(300, 8);
        cache.insert(0, 100, addr(8000));
        let providers = cache.get_providers(0, 100);
        assert_eq!(providers.len(), 1);
        assert!(providers.contains(&addr(8000)));
    }

    #[test]
    fn multiple_providers_per_chunk() {
        let cache = PdChunkProviderCache::new(300, 8);
        cache.insert(0, 100, addr(8000));
        cache.insert(0, 100, addr(8001));
        cache.insert(0, 100, addr(8002));
        let providers = cache.get_providers(0, 100);
        assert_eq!(providers.len(), 3);
    }

    #[test]
    fn max_providers_bounded() {
        let cache = PdChunkProviderCache::new(300, 2);
        cache.insert(0, 100, addr(8000));
        cache.insert(0, 100, addr(8001));
        cache.insert(0, 100, addr(8002)); // should not be added
        let providers = cache.get_providers(0, 100);
        assert_eq!(providers.len(), 2);
    }

    #[test]
    fn duplicate_insert_ignored() {
        let cache = PdChunkProviderCache::new(300, 8);
        cache.insert(0, 100, addr(8000));
        cache.insert(0, 100, addr(8000)); // duplicate
        let providers = cache.get_providers(0, 100);
        assert_eq!(providers.len(), 1);
    }

    #[test]
    fn missing_key_returns_empty() {
        let cache = PdChunkProviderCache::new(300, 8);
        assert!(cache.get_providers(0, 999).is_empty());
    }
}
```

- [ ] **Step 2: Run tests, verify fail**

Run: `cargo nextest run -p irys-p2p pd_chunk_provider_cache::tests`
Expected: FAIL

- [ ] **Step 3: Implement `PdChunkProviderCache`**

```rust
use moka::sync::Cache;
use smallvec::SmallVec;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use parking_lot::Mutex;

type PdChunkKey = (u32, u64); // (ledger, offset)
type ProviderSet = Arc<Mutex<SmallVec<[SocketAddr; 8]>>>;

/// Short-lived cache mapping PD chunk coordinates to provider peer addresses.
/// Advisory only — entries may be stale. Pull code must tolerate misses.
#[derive(Clone)]
pub struct PdChunkProviderCache {
    inner: Cache<PdChunkKey, ProviderSet>,
    max_providers_per_chunk: usize,
}

impl PdChunkProviderCache {
    pub fn new(ttl_secs: u64, max_providers_per_chunk: usize) -> Self {
        let inner = Cache::builder()
            .time_to_live(Duration::from_secs(ttl_secs))
            .max_capacity(100_000) // max distinct chunks tracked
            .build();
        Self { inner, max_providers_per_chunk }
    }

    /// Record that `provider` can serve chunk `(ledger, offset)`.
    pub fn insert(&self, ledger: u32, offset: u64, provider: SocketAddr) {
        let key = (ledger, offset);
        let set = self.inner.get_with(key, || {
            Arc::new(Mutex::new(SmallVec::new()))
        });
        let mut guard = set.lock();
        if guard.len() < self.max_providers_per_chunk && !guard.contains(&provider) {
            guard.push(provider);
        }
    }

    /// Get known providers for chunk `(ledger, offset)`.
    pub fn get_providers(&self, ledger: u32, offset: u64) -> SmallVec<[SocketAddr; 8]> {
        match self.inner.get(&(ledger, offset)) {
            Some(set) => set.lock().clone(),
            None => SmallVec::new(),
        }
    }
}
```

- [ ] **Step 4: Add module declaration to `crates/p2p/src/lib.rs`**

```rust
pub mod pd_chunk_provider_cache;
```

- [ ] **Step 5: Run tests, verify pass**

Run: `cargo nextest run -p irys-p2p pd_chunk_provider_cache::tests`
Expected: PASS

- [ ] **Step 6: Verify `smallvec` dependency**

Check if `smallvec` is already in `crates/p2p/Cargo.toml`. If not, add it. Alternatively, use `Vec<SocketAddr>` instead of `SmallVec` to avoid a new dependency.

---

## Task 6: PD Availability Rate Limiter

**Files:**
- Create: `crates/p2p/src/pd_availability_rate_limiter.rs`
- Modify: `crates/p2p/src/lib.rs`

- [ ] **Step 1: Write tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_budget_allowed() {
        let limiter = PdAvailabilityRateLimiter::new(100);
        let peer = IrysPeerId::ZERO;
        assert!(limiter.check_and_record(&peer, 50));
        assert!(limiter.check_and_record(&peer, 49));
    }

    #[test]
    fn over_budget_rejected() {
        let limiter = PdAvailabilityRateLimiter::new(100);
        let peer = IrysPeerId::ZERO;
        assert!(limiter.check_and_record(&peer, 60));
        assert!(!limiter.check_and_record(&peer, 60)); // total 120 > 100
    }

    #[test]
    fn independent_peer_budgets() {
        let limiter = PdAvailabilityRateLimiter::new(100);
        let peer_a = IrysPeerId::ZERO;
        let peer_b = IrysPeerId::from(IrysAddress::random());
        assert!(limiter.check_and_record(&peer_a, 80));
        assert!(limiter.check_and_record(&peer_b, 80)); // different peer, own budget
    }
}
```

- [ ] **Step 2: Run tests, verify fail**

Run: `cargo nextest run -p irys-p2p pd_availability_rate_limiter::tests`
Expected: FAIL

- [ ] **Step 3: Implement `PdAvailabilityRateLimiter`**

```rust
use dashmap::DashMap;
use irys_types::IrysPeerId;
use std::time::{Duration, Instant};

const WINDOW_DURATION: Duration = Duration::from_secs(60);

struct PeerBudget {
    chunks_in_window: u64,
    window_start: Instant,
}

/// Sliding-window per-peer budget for PD availability announced chunks.
pub struct PdAvailabilityRateLimiter {
    max_chunks_per_minute: u64,
    peers: DashMap<IrysPeerId, PeerBudget>,
}

impl PdAvailabilityRateLimiter {
    pub fn new(max_chunks_per_minute: u64) -> Self {
        Self {
            max_chunks_per_minute,
            peers: DashMap::new(),
        }
    }

    /// Check if `peer` can announce `chunk_count` more chunks. If yes, records them.
    pub fn check_and_record(&self, peer: &IrysPeerId, chunk_count: u64) -> bool {
        let now = Instant::now();
        let mut entry = self.peers.entry(*peer).or_insert_with(|| PeerBudget {
            chunks_in_window: 0,
            window_start: now,
        });
        let budget = entry.value_mut();

        // Reset window if expired
        if now.duration_since(budget.window_start) >= WINDOW_DURATION {
            budget.chunks_in_window = 0;
            budget.window_start = now;
        }

        if budget.chunks_in_window + chunk_count > self.max_chunks_per_minute {
            return false;
        }

        budget.chunks_in_window += chunk_count;
        true
    }
}
```

- [ ] **Step 4: Add module declaration to `crates/p2p/src/lib.rs`**

```rust
pub mod pd_availability_rate_limiter;
```

- [ ] **Step 5: Run tests, verify pass**

Run: `cargo nextest run -p irys-p2p pd_availability_rate_limiter::tests`
Expected: PASS

---

## Task 7: GossipCache and GossipRoutes Updates

**Files:**
- Modify: `crates/p2p/src/cache.rs`
- Modify: `crates/p2p/src/types.rs`

- [ ] **Step 1: Add `pd_chunk_availability` field to `GossipCache`**

In `crates/p2p/src/cache.rs`, the `GossipCache` struct (around line 19) has five `moka::sync::Cache` fields. Add a sixth:

```rust
pd_chunk_availability: Cache<H256, Arc<RwLock<HashSet<IrysPeerId>>>>,
```

Initialize it in `new()` with the same `GOSSIP_CACHE_TTL` (5 minutes):

```rust
pd_chunk_availability: Cache::builder()
    .time_to_live(GOSSIP_CACHE_TTL)
    .build(),
```

- [ ] **Step 2: Update `record_seen` and `peers_that_have_seen` for the new key variant**

In the `record_seen` and `peers_that_have_seen` methods, add match arms for `GossipCacheKey::PdChunkAvailability(id)`:

```rust
GossipCacheKey::PdChunkAvailability(id) => {
    // same pattern as other variants, using self.pd_chunk_availability
}
```

- [ ] **Step 3: Add `GossipRoutes::PdChunkAvailability`**

In `crates/p2p/src/types.rs`, the `GossipRoutes` enum (around line 262), add:

```rust
PdChunkAvailability,
```

In `as_str()` (around line 283), add:

```rust
GossipRoutes::PdChunkAvailability => "/pd_chunk_availability",
```

- [ ] **Step 4: Compile check**

Run: `cargo check -p irys-p2p`
Expected: PASS

---

## Task 8: Receive Route and Handler

**Files:**
- Create: `crates/p2p/src/pd_availability_handler.rs`
- Modify: `crates/p2p/src/server.rs`

- [ ] **Step 1: Write handler validation tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::gossip::{PdChunkAvailabilityBatch, PdChunkRange};

    #[test]
    fn valid_batch_accepted() {
        let batch = PdChunkAvailabilityBatch {
            availability_id: H256::random(),
            block_hash: H256::random().into(),
            ranges: vec![
                PdChunkRange { ledger: 0, start_offset: 10, chunk_count: 5 },
                PdChunkRange { ledger: 0, start_offset: 20, chunk_count: 3 },
            ],
        };
        assert!(validate_batch(&batch, 256, 64).is_ok());
    }

    #[test]
    fn non_publish_ledger_rejected() {
        let batch = PdChunkAvailabilityBatch {
            availability_id: H256::random(),
            block_hash: H256::random().into(),
            ranges: vec![
                PdChunkRange { ledger: 1, start_offset: 10, chunk_count: 5 }, // Submit ledger
            ],
        };
        assert!(validate_batch(&batch, 256, 64).is_err());
    }

    #[test]
    fn unsorted_ranges_rejected() {
        let batch = PdChunkAvailabilityBatch {
            availability_id: H256::random(),
            block_hash: H256::random().into(),
            ranges: vec![
                PdChunkRange { ledger: 0, start_offset: 20, chunk_count: 5 },
                PdChunkRange { ledger: 0, start_offset: 10, chunk_count: 3 }, // out of order
            ],
        };
        assert!(validate_batch(&batch, 256, 64).is_err());
    }

    #[test]
    fn overlapping_ranges_rejected() {
        let batch = PdChunkAvailabilityBatch {
            availability_id: H256::random(),
            block_hash: H256::random().into(),
            ranges: vec![
                PdChunkRange { ledger: 0, start_offset: 10, chunk_count: 15 },
                PdChunkRange { ledger: 0, start_offset: 20, chunk_count: 5 }, // overlaps with first
            ],
        };
        assert!(validate_batch(&batch, 256, 64).is_err());
    }

    #[test]
    fn too_many_chunks_rejected() {
        let batch = PdChunkAvailabilityBatch {
            availability_id: H256::random(),
            block_hash: H256::random().into(),
            ranges: vec![
                PdChunkRange { ledger: 0, start_offset: 0, chunk_count: u16::MAX },
            ],
        };
        // total_announced_chunks = 65535 > max 256
        assert!(validate_batch(&batch, 256, 64).is_err());
    }

    #[test]
    fn too_many_ranges_rejected() {
        let ranges: Vec<PdChunkRange> = (0..100)
            .map(|i| PdChunkRange { ledger: 0, start_offset: i * 100, chunk_count: 1 })
            .collect();
        let batch = PdChunkAvailabilityBatch {
            availability_id: H256::random(),
            block_hash: H256::random().into(),
            ranges,
        };
        assert!(validate_batch(&batch, 256, 64).is_err()); // 100 > max 64
    }
}
```

- [ ] **Step 2: Run tests, verify fail**

Run: `cargo nextest run -p irys-p2p pd_availability_handler::tests`
Expected: FAIL

- [ ] **Step 3: Implement validation function**

```rust
use irys_types::gossip::PdChunkAvailabilityBatch;
use irys_types::block::DataLedger;
use eyre::{eyre, Result};

/// Validate structural invariants of a PdChunkAvailabilityBatch.
pub fn validate_batch(
    batch: &PdChunkAvailabilityBatch,
    max_chunks: usize,
    max_ranges: usize,
) -> Result<()> {
    // Range count bounded
    if batch.ranges.len() > max_ranges {
        return Err(eyre!("too many ranges: {} > {}", batch.ranges.len(), max_ranges));
    }

    // Total chunks bounded
    let total = batch.total_announced_chunks();
    if total > max_chunks as u64 {
        return Err(eyre!("too many announced chunks: {} > {}", total, max_chunks));
    }

    // All ranges must be publish-ledger
    let publish = DataLedger::Publish as u32;
    for range in &batch.ranges {
        if range.ledger != publish {
            return Err(eyre!("non-publish ledger: {}", range.ledger));
        }
    }

    // Ranges must be sorted and non-overlapping
    for window in batch.ranges.windows(2) {
        let (a, b) = (&window[0], &window[1]);
        let a_end = a.start_offset + a.chunk_count as u64;
        if b.ledger < a.ledger || (b.ledger == a.ledger && b.start_offset < a_end) {
            return Err(eyre!(
                "ranges not sorted or overlap: [{}, {}+{}) vs [{}, {})",
                a.start_offset, a.start_offset, a.chunk_count,
                b.start_offset, b.start_offset,
            ));
        }
    }

    Ok(())
}
```

- [ ] **Step 4: Run tests, verify pass**

Run: `cargo nextest run -p irys-p2p pd_availability_handler::tests`
Expected: PASS

- [ ] **Step 5: Implement the handler as a method on `GossipServer`**

All existing V2 handlers are methods on `GossipServer<M, B>` (e.g., `handle_chunk_v2`, `handle_block_header_v2`). Follow the same pattern. Add this method to the `impl GossipServer<M, B>` block in `crates/p2p/src/server.rs`:

```rust
/// Handler for POST /gossip/v2/pd_chunk_availability
async fn handle_pd_chunk_availability_v2(
    server: Data<Self>,
    body: web::Json<GossipRequestV2<PdChunkAvailabilityBatch>>,
    req: actix_web::HttpRequest,
) -> HttpResponse {
    // 1. Gossip-enabled gate
    if !server.sync_state.is_gossip_reception_enabled() {
        return HttpResponse::Ok().json(GossipResponse::<()>::rejected_gossip_disabled());
    }

    // 2. Peer verification (same pattern as other v2 handlers)
    let peer = match Self::check_peer_v2(
        &server.peer_list, &req, body.peer_id, body.miner_address,
    ) {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    let batch = &body.data;

    // 3. Rate limit check
    let total_chunks = batch.total_announced_chunks();
    if !server.pd_availability_rate_limiter.check_and_record(
        &body.peer_id,
        total_chunks,
    ) {
        return HttpResponse::Ok().json(GossipResponse::Rejected(RejectionReason::RateLimited));
    }

    // 4. Structural validation
    let pd_config = &server.config.consensus_config.pd_availability;
    if let Err(e) = validate_batch(
        batch,
        pd_config.max_chunks_per_message,
        pd_config.max_ranges_per_message,
    ) {
        tracing::debug!(?e, "rejecting invalid PD availability batch");
        return HttpResponse::Ok().json(GossipResponse::Rejected(RejectionReason::InvalidData));
    }

    // 5. Resolve source peer gossip address
    let peer_gossip_addr = peer.address.gossip;

    // 6. Expand ranges and insert into provider cache
    for range in &batch.ranges {
        for i in 0..range.chunk_count as u64 {
            let offset = range.start_offset + i;
            server.pd_chunk_provider_cache.insert(range.ledger, offset, peer_gossip_addr);
        }
    }

    // 7. Record in gossip dedup cache
    let cache_key = GossipCacheKey::pd_chunk_availability(batch.availability_id);
    let _ = server.cache.record_seen(body.peer_id, cache_key);

    HttpResponse::Ok().json(GossipResponse::Accepted(()))
}
```

The `validate_batch` function (from Step 3) can live in `pd_availability_handler.rs` as a standalone utility and be imported into `server.rs`, or it can be inlined. The handler method itself must live in `server.rs` alongside the other handlers.

- [ ] **Step 6: Register route in `server.rs`**

In `crates/p2p/src/server.rs`, in the `routes()` method (around line 1455), within the `/v2` scope, add:

```rust
.service(
    web::resource(GossipRoutes::PdChunkAvailability.as_str())
        .app_data(web::JsonConfig::default().limit(
            config.consensus_config.pd_availability.max_body_bytes
        ))
        .route(web::post().to(
            Self::handle_pd_chunk_availability_v2
        ))
)
```

The per-route `.app_data(web::JsonConfig)` overrides the global 100MB limit for this specific route. This follows the same registration pattern as the other V2 handlers (e.g., `Self::handle_chunk_v2`).

- [ ] **Step 7: Add `PdChunkProviderCache` and `PdAvailabilityRateLimiter` fields to `GossipServer`**

In `crates/p2p/src/server.rs`, add fields to `GossipServer`:

```rust
pub pd_chunk_provider_cache: PdChunkProviderCache,
pub pd_availability_rate_limiter: PdAvailabilityRateLimiter,
```

Initialize them in the constructor using `PdAvailabilityConfig` from `config.consensus_config.pd_availability`.

- [ ] **Step 8: Compile check**

Run: `cargo check -p irys-p2p`
Expected: PASS

---

## Task 9: Announce Orchestration

**Files:**
- Create: `crates/actors/src/pd_availability_announcer.rs`
- Modify: `crates/actors/src/lib.rs`
- Modify: `crates/actors/src/services.rs`

This task creates the background task that subscribes to `BlockStateUpdated::Valid`, extracts PD chunk specs from the validated block, checks local availability, builds `PdChunkAvailabilityBatch` fragments, and sends them to P2PService via a dedicated channel.

- [ ] **Step 1: Define the fanout message type**

In `crates/actors/src/pd_availability_announcer.rs`:

```rust
use irys_types::gossip::PdChunkAvailabilityBatch;

/// Message sent from the announcer to P2PService for fanout.
#[derive(Debug, Clone)]
pub struct PdAvailabilityFanoutMessage {
    pub fragments: Vec<PdChunkAvailabilityBatch>,
}
```

- [ ] **Step 2: Add channel to `ServiceSendersInner` and `ServiceReceivers`**

In `crates/actors/src/services.rs`, add to `ServiceSendersInner`:

```rust
pub pd_availability: UnboundedSender<PdAvailabilityFanoutMessage>,
```

Add to `ServiceReceivers`:

```rust
pub pd_availability: UnboundedReceiver<PdAvailabilityFanoutMessage>,
```

Create the channel in `init_with_sender` alongside the other channels.

- [ ] **Step 3: Write test for fragment building**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::range_specifier::{ChunkRangeSpecifier, specs_to_ledger_offsets, coalesce_ledger_offsets_to_ranges};
    use irys_types::U200;

    #[test]
    fn build_fragments_from_offsets() {
        let available_offsets: Vec<(u32, u64)> = vec![
            (0, 10), (0, 11), (0, 12),
            (0, 100), (0, 101),
        ];
        let block_hash = H256::random().into();
        let max_chunks = 256;
        let max_ranges = 64;

        let fragments = build_availability_fragments(
            &available_offsets,
            block_hash,
            max_chunks,
            max_ranges,
        );

        assert_eq!(fragments.len(), 1);
        assert_eq!(fragments[0].ranges.len(), 2); // two contiguous groups
        assert_eq!(fragments[0].total_announced_chunks(), 5);
        assert_eq!(fragments[0].block_hash, block_hash);
        // availability_id should be unique (non-zero)
        assert_ne!(fragments[0].availability_id, H256::zero());
    }

    #[test]
    fn fragments_split_when_exceeding_limits() {
        // Create offsets that exceed max_ranges_per_message
        let available_offsets: Vec<(u32, u64)> = (0..10)
            .map(|i| (0u32, i * 100)) // 10 non-contiguous offsets = 10 ranges
            .collect();
        let block_hash = H256::random().into();

        let fragments = build_availability_fragments(
            &available_offsets,
            block_hash,
            256,
            3, // only 3 ranges per fragment
        );

        assert!(fragments.len() >= 4); // ceil(10/3) = 4
        for f in &fragments {
            assert!(f.ranges.len() <= 3);
        }
    }
}
```

- [ ] **Step 4: Run tests, verify fail**

Run: `cargo nextest run -p irys-actors pd_availability_announcer::tests`
Expected: FAIL

- [ ] **Step 5: Implement `build_availability_fragments`**

```rust
use irys_types::gossip::{PdChunkAvailabilityBatch, PdChunkRange};
use irys_types::range_specifier::coalesce_ledger_offsets_to_ranges;
use irys_types::{BlockHash, H256};

/// Build bounded availability fragments from a set of available offsets.
pub fn build_availability_fragments(
    available_offsets: &[(u32, u64)],
    block_hash: BlockHash,
    max_chunks_per_message: usize,
    max_ranges_per_message: usize,
) -> Vec<PdChunkAvailabilityBatch> {
    if available_offsets.is_empty() {
        return Vec::new();
    }

    let ranges = coalesce_ledger_offsets_to_ranges(available_offsets);

    // Split ranges into fragments that fit within the limits
    let mut fragments = Vec::new();
    let mut current_ranges: Vec<PdChunkRange> = Vec::new();
    let mut current_chunks: u64 = 0;

    for range in ranges {
        let range_chunks = range.total_chunks();
        let would_exceed_ranges = current_ranges.len() + 1 > max_ranges_per_message;
        let would_exceed_chunks = current_chunks + range_chunks > max_chunks_per_message as u64;

        if !current_ranges.is_empty() && (would_exceed_ranges || would_exceed_chunks) {
            fragments.push(PdChunkAvailabilityBatch {
                availability_id: H256::random(),
                block_hash,
                ranges: std::mem::take(&mut current_ranges),
            });
            current_chunks = 0;
        }

        current_ranges.push(range);
        current_chunks += range_chunks;
    }

    if !current_ranges.is_empty() {
        fragments.push(PdChunkAvailabilityBatch {
            availability_id: H256::random(),
            block_hash,
            ranges: current_ranges,
        });
    }

    fragments
}
```

- [ ] **Step 6: Run tests, verify pass**

Run: `cargo nextest run -p irys-actors pd_availability_announcer::tests`
Expected: PASS

- [ ] **Step 7: Add `ValidationResult::is_valid` helper if it doesn't exist**

Check if `ValidationResult` (in `crates/actors/src/block_tree_service.rs:894`) has an `is_valid()` method. If not, add one:

```rust
impl ValidationResult {
    pub fn is_valid(&self) -> bool {
        matches!(self, Self::Valid)
    }
}
```

- [ ] **Step 8: Implement the announcer background task**

```rust
use crate::block_tree_service::BlockStateUpdated;
use irys_types::config::ConsensusConfig;
use irys_types::gossip::PdChunkAvailabilityBatch;
use irys_types::range_specifier::specs_to_ledger_offsets;
use irys_types::chunk_provider::ChunkStorageProvider;
use irys_types::BlockHash;
use irys_reth::pd_tx::extract_pd_chunk_specs;
use irys_domain::ExecutionPayloadCache;
use irys_domain::guards::BlockTreeReadGuard;
use tokio::sync::{broadcast, mpsc};
use std::sync::Arc;
use tracing::{debug, warn, instrument};

pub struct PdAvailabilityAnnouncer {
    block_state_rx: broadcast::Receiver<BlockStateUpdated>,
    fanout_tx: mpsc::UnboundedSender<PdAvailabilityFanoutMessage>,
    block_tree: BlockTreeReadGuard,
    execution_payload_cache: ExecutionPayloadCache,
    storage_provider: Arc<dyn ChunkStorageProvider>,
    consensus_config: ConsensusConfig,
}

impl PdAvailabilityAnnouncer {
    pub fn new(
        block_state_rx: broadcast::Receiver<BlockStateUpdated>,
        fanout_tx: mpsc::UnboundedSender<PdAvailabilityFanoutMessage>,
        block_tree: BlockTreeReadGuard,
        execution_payload_cache: ExecutionPayloadCache,
        storage_provider: Arc<dyn ChunkStorageProvider>,
        consensus_config: ConsensusConfig,
    ) -> Self {
        Self {
            block_state_rx,
            fanout_tx,
            block_tree,
            execution_payload_cache,
            storage_provider,
            consensus_config,
        }
    }

    pub async fn run(mut self) {
        loop {
            match self.block_state_rx.recv().await {
                Ok(event) => {
                    if event.validation_result.is_valid() && !event.discarded {
                        if let Err(e) = self.handle_valid_block(event.block_hash).await {
                            warn!(?e, "PD availability announce failed");
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(skipped = n, "PD availability announcer lagged");
                }
                Err(broadcast::error::RecvError::Closed) => {
                    debug!("block_state_events channel closed, stopping announcer");
                    break;
                }
            }
        }
    }

    async fn handle_valid_block(&self, block_hash: BlockHash) -> eyre::Result<()> {
        // 1. Look up block header from block tree
        let evm_block_hash = {
            let tree = self.block_tree.read();
            let header = tree.get_block(&block_hash)
                .ok_or_else(|| eyre::eyre!("block not in tree"))?;
            header.evm_block_hash
        };

        // 2. Get EVM block from execution payload cache
        let evm_block = self.execution_payload_cache
            .get_locally_stored_evm_block(evm_block_hash)
            .ok_or_else(|| eyre::eyre!("EVM block not in cache"))?;

        // 3. Extract PD chunk specs from EVM block
        let pd_specs: Vec<_> = evm_block.body.transactions.iter()
            .filter_map(|tx| tx.access_list())
            .flat_map(|al| extract_pd_chunk_specs(al))
            .collect();

        if pd_specs.is_empty() {
            return Ok(()); // No PD transactions in this block
        }

        // 4. Convert to ledger offsets
        let chunk_config = self.storage_provider.config();
        let all_offsets = specs_to_ledger_offsets(
            &pd_specs,
            chunk_config.num_chunks_in_partition,
        );

        // 5. Filter to offsets this node can serve
        let available_offsets: Vec<(u32, u64)> = all_offsets
            .into_iter()
            .filter(|(ledger, offset)| {
                self.storage_provider
                    .has_chunk_for_pd(*ledger, *offset)
                    .unwrap_or(false)
            })
            .collect();

        if available_offsets.is_empty() {
            return Ok(()); // We can't serve any of these chunks
        }

        // 6. Build fragments
        let pd_config = &self.consensus_config.pd_availability;
        let fragments = build_availability_fragments(
            &available_offsets,
            block_hash,
            pd_config.max_chunks_per_message,
            pd_config.max_ranges_per_message,
        );

        debug!(
            block = %block_hash,
            fragments = fragments.len(),
            chunks = available_offsets.len(),
            "announcing PD chunk availability"
        );

        // 7. Send to P2PService for fanout
        let _ = self.fanout_tx.send(PdAvailabilityFanoutMessage { fragments });

        Ok(())
    }
}
```

- [ ] **Step 9: Add module declaration to `crates/actors/src/lib.rs`**

```rust
pub mod pd_availability_announcer;
```

- [ ] **Step 10: Dependency check**

The announcer imports `extract_pd_chunk_specs` from `irys_reth::pd_tx`. Verify that `crates/actors/Cargo.toml` lists `irys-reth` as a dependency. If not, add it. The function only needs `alloy_eips::eip2930::AccessList` (available via `reth` re-exports) and `ChunkRangeSpecifier` (in `irys-types`).

- [ ] **Step 11: Compile check**

Run: `cargo check -p irys-actors`
Expected: PASS

---

## Task 10: Fanout Send Path

**Files:**
- Modify: `crates/p2p/src/gossip_service.rs`

This task adds the P2PService-side task that reads pre-built fragments from the announcer channel and sends them to a bounded fanout of peers.

- [ ] **Step 1: Implement `select_fanout_peers`**

Add to `gossip_service.rs` or a new helper file:

```rust
use irys_types::IrysPeerId;
use irys_types::peer_list::PeerListItem;
use rand::seq::SliceRandom;

/// Select a bounded fanout: half best-scored, half random, shuffled.
pub fn select_fanout_peers(
    all_peers: &[(IrysPeerId, PeerListItem)],
    fanout: usize,
    self_peer_id: &IrysPeerId,
) -> Vec<(IrysPeerId, PeerListItem)> {
    // Filter out self
    let candidates: Vec<_> = all_peers
        .iter()
        .filter(|(id, _)| id != self_peer_id)
        .cloned()
        .collect();

    if candidates.len() <= fanout {
        return candidates;
    }

    let half = fanout / 2;
    let mut rng = rand::rng();

    // First half: best-scored (candidates are already sorted by score descending)
    let best = &candidates[..half.min(candidates.len())];

    // Second half: random from the remainder
    let remainder = &candidates[half.min(candidates.len())..];
    let random_count = fanout - best.len();
    let mut random_picks: Vec<_> = remainder.to_vec();
    random_picks.shuffle(&mut rng);
    random_picks.truncate(random_count);

    let mut selected: Vec<_> = best.iter().chain(random_picks.iter()).cloned().collect();
    selected.shuffle(&mut rng); // shuffle final list
    selected
}
```

- [ ] **Step 2: Implement `spawn_pd_availability_fanout_task`**

**Important API note:** `GossipClient::send_preserialized` takes `(&self, gossip_address: &SocketAddr, route: GossipRoutes, body: bytes::Bytes)` — it constructs the URL internally. The body must be a serialized `GossipRequestV2<T>`, created via `client.create_request_v2(data)`. Do NOT use `send_preserialized_detached` here because it records `cache.record_seen` on ALL `Ok` results including `Rejected`, violating the design invariant that rejected responses must not be marked as seen.

```rust
fn spawn_pd_availability_fanout_task(
    mut rx: mpsc::UnboundedReceiver<PdAvailabilityFanoutMessage>,
    client: GossipClient,
    peer_list: PeerList,
    cache: Arc<GossipCache>,
    config: PdAvailabilityConfig,
    self_peer_id: IrysPeerId,
    sync_state: ChainSyncState,
    runtime_handle: tokio::runtime::Handle,
) {
    runtime_handle.spawn(async move {
        while let Some(msg) = rx.recv().await {
            if !sync_state.is_gossip_broadcast_enabled() {
                continue;
            }

            let all_peers = peer_list.all_peers_sorted_by_score();
            let fanout_peers = select_fanout_peers(
                &all_peers,
                config.fanout,
                &self_peer_id,
            );

            for fragment in &msg.fragments {
                let cache_key = GossipCacheKey::pd_chunk_availability(
                    fragment.availability_id,
                );

                // Pre-serialize using the client's create_request_v2 helper,
                // which wraps the data in GossipRequestV2 { peer_id, miner_address, data }
                let request = client.create_request_v2(fragment.clone());
                let serialized = match serde_json::to_vec(&request) {
                    Ok(bytes) => bytes::Bytes::from(bytes),
                    Err(e) => {
                        warn!(?e, "failed to serialize PD availability fragment");
                        continue;
                    }
                };

                for (peer_id, peer_item) in &fanout_peers {
                    // Skip peers that already have this fragment
                    if cache.peers_that_have_seen(&cache_key).contains(peer_id) {
                        continue;
                    }

                    // Use send_preserialized directly (not _detached) so we can
                    // distinguish Accepted from Rejected for cache recording
                    let client = client.clone();
                    let serialized = serialized.clone();
                    let cache = cache.clone();
                    let cache_key = cache_key.clone();
                    let peer_id = *peer_id;
                    let gossip_addr = peer_item.address.gossip;

                    runtime_handle.spawn(async move {
                        match client.send_preserialized(
                            &gossip_addr,
                            GossipRoutes::PdChunkAvailability,
                            serialized,
                        ).await {
                            Ok(GossipResponse::Accepted(_)) => {
                                cache.record_seen(peer_id, cache_key);
                            }
                            Ok(GossipResponse::Rejected(reason)) => {
                                debug!(?reason, %peer_id, "PD availability rejected");
                                // Do NOT record as seen — rejected responses
                                // must not suppress future attempts
                            }
                            Err(e) => {
                                debug!(?e, %peer_id, "PD availability send failed");
                            }
                        }
                    });
                }
            }
        }
    });
}
```

**Note:** `client.create_request_v2` is currently a private method on `GossipClient`. Either make it `pub(crate)` or extract the serialization logic. Alternatively, build the `GossipRequestV2` struct directly using `client.peer_id` and `client.mining_address` fields (also currently private — may need accessors or `pub(crate)` visibility).

- [ ] **Step 3: Wire the fanout task into `P2PService::run()`**

In `P2PService::run()` (around line 258), after spawning `spawn_broadcast_task`, spawn the fanout task:

```rust
// The receiver comes from ServiceReceivers or is passed as a parameter
spawn_pd_availability_fanout_task(
    pd_availability_rx,
    self.client.clone(),
    peer_list.clone(),
    self.cache.clone(),
    config.consensus_config.pd_availability.clone(),
    self_peer_id,
    self.sync_state.clone(),
    self.runtime_handle.clone(),
);
```

The `pd_availability_rx` receiver needs to be threaded through from `P2PService::run()`'s parameters. Add it as a new parameter or pass it via `ServiceReceivers`.

- [ ] **Step 4: Compile check**

Run: `cargo check -p irys-p2p`
Expected: PASS

---

## Task 11: Pull Integration — Merge Hinted Providers

**Files:**
- Modify: `crates/p2p/src/pd_chunk_fetcher.rs`

- [ ] **Step 1: Add `PdChunkProviderCache` to `GossipPdChunkFetcher`**

In `crates/p2p/src/pd_chunk_fetcher.rs` (around line 11), add a field:

```rust
pub struct GossipPdChunkFetcher {
    client: GossipClient,
    peer_list: PeerList,
    provider_cache: PdChunkProviderCache,
}
```

Update the constructor accordingly.

- [ ] **Step 2: Modify `fetch_chunk` to merge hinted providers**

The current `fetch_chunk` at line 27 delegates to `client.pull_pd_chunk_from_peers(peers, ledger, offset)` where `peers` comes from canonical assignees (resolved by `PdService::resolve_peers_for_chunk`).

Modify to prepend hinted providers:

```rust
async fn fetch_chunk(
    &self,
    assignee_peers: &[PeerAddress],
    ledger: u32,
    offset: u64,
) -> Result<(ChunkFormat, SocketAddr), PeerNetworkError> {
    let pd_config = /* get from config or store on struct */;

    // 1. Look up hinted providers
    let hinted_addrs = self.provider_cache.get_providers(ledger, offset);

    // 2. Convert hinted SocketAddrs to PeerAddress
    //    (look up full PeerAddress from PeerList by gossip address)
    let mut hinted_peers: Vec<PeerAddress> = hinted_addrs
        .iter()
        .filter_map(|addr| self.peer_list.peer_by_gossip_address(*addr))
        .map(|item| item.address.clone())
        .collect();

    // Shuffle and cap hinted peers
    hinted_peers.shuffle(&mut rand::rng());
    hinted_peers.truncate(pd_config.fetch_max_hinted_peers);

    // 3. Cap and shuffle assignee peers
    let mut assignees: Vec<PeerAddress> = assignee_peers.to_vec();
    assignees.shuffle(&mut rand::rng());
    assignees.truncate(pd_config.fetch_max_assignee_peers);

    // 4. Build candidate list: hinted first, then assignees, deduped
    let mut candidates = hinted_peers;
    for peer in assignees {
        if !candidates.iter().any(|p| p.gossip == peer.gossip) {
            candidates.push(peer);
        }
    }

    // 5. Remove self
    // (self-address filtering is done in PdService::resolve_peers_for_chunk,
    //  but hinted providers might include self)
    // candidates.retain(|p| p.gossip != self_address);

    self.client.pull_pd_chunk_from_peers(&candidates, ledger, offset, &self.peer_list).await
}
```

- [ ] **Step 3: Update `PdService` to pass `PdChunkProviderCache` when constructing `GossipPdChunkFetcher`**

In `crates/chain/src/chain.rs` where `GossipPdChunkFetcher` is constructed (around line 1848-1866), pass the `PdChunkProviderCache` instance. This cache must be shared between `GossipServer` (which writes to it in the handler) and `GossipPdChunkFetcher` (which reads from it). Since `PdChunkProviderCache` is `Clone` (wraps `moka::sync::Cache` which is `Clone`), clone it.

- [ ] **Step 4: Compile check**

Run: `cargo check -p irys-p2p -p irys-actors`
Expected: PASS

---

## Task 12: Chain Wiring

**Files:**
- Modify: `crates/chain/src/chain.rs`
- Modify: `crates/p2p/src/gossip_service.rs` (parameter threading)

- [ ] **Step 1: Create `PdChunkProviderCache` in chain startup**

In `chain.rs`, during service initialization, create the shared cache:

```rust
let pd_availability_config = &config.consensus_config.pd_availability;
let pd_chunk_provider_cache = PdChunkProviderCache::new(
    pd_availability_config.provider_cache_ttl_secs,
    pd_availability_config.max_providers_per_chunk,
);
```

- [ ] **Step 2: Create the announcer channel**

```rust
let (pd_availability_tx, pd_availability_rx) = tokio::sync::mpsc::unbounded_channel();
```

Or, if added to `ServiceSenders`, use the channel created there.

- [ ] **Step 3: Spawn the announcer task**

After service senders are initialized and the block tree / execution payload cache are available:

```rust
if let Some(ref storage_provider) = storage_provider {
    let announcer = PdAvailabilityAnnouncer::new(
        service_senders.subscribe_block_state_updates(),
        pd_availability_tx,
        block_tree.clone(),
        execution_payload_cache.clone(),
        storage_provider.clone(),
        config.consensus_config.clone(),
    );
    tokio::spawn(announcer.run());
}
```

- [ ] **Step 4: Pass `pd_availability_rx` and `pd_chunk_provider_cache` to `P2PService::run()`**

Thread the receiver and provider cache as new parameters to `P2PService::run()`. Inside `run()`, pass them to `spawn_pd_availability_fanout_task` and to `GossipServer` (for the receive handler) and `GossipPdChunkFetcher` (for pull integration).

- [ ] **Step 5: Full workspace compile check**

Run: `cargo check --workspace`
Expected: PASS

- [ ] **Step 6: Run existing tests to verify no regressions**

Run: `cargo nextest run -p irys-p2p -p irys-actors -p irys-types`
Expected: All existing tests PASS

- [ ] **Step 7: Run clippy**

Run: `cargo clippy --workspace --tests --all-targets`
Expected: No new warnings

---

## Dependency Graph

```
Task 1 (Wire Types)
  ├── Task 2 (Config)
  ├── Task 3 (Utilities) ──── depends on Task 1 for PdChunkRange
  ├── Task 4 (Storage) ────── independent of types but needed by Task 9
  ├── Task 5 (ProviderCache) ─ depends on Task 1
  ├── Task 6 (RateLimiter) ── independent
  └── Task 7 (GossipCache) ── depends on Task 1

Task 8 (Receive Handler) ──── depends on Tasks 1, 5, 6, 7
Task 9 (Announcer) ────────── depends on Tasks 1, 3, 4
Task 10 (Fanout Send) ─────── depends on Tasks 1, 7
Task 11 (Pull Integration) ── depends on Task 5
Task 12 (Chain Wiring) ─────── depends on all above
```

Tasks 1-7 can be parallelized (after Task 1 is done, Tasks 2-7 are independent).
Tasks 8-11 can be parallelized (each depends only on earlier foundation tasks).
Task 12 must come last.

---

## Testing Strategy

### Unit Tests (included in each task above)
- Wire type serialization roundtrips
- `specs_to_ledger_offsets` — arithmetic, overflow, multi-spec
- `coalesce_ledger_offsets_to_ranges` — contiguous, gaps, u16 cap, unsorted input
- `PdChunkProviderCache` — insert, lookup, max providers, dedup
- `PdAvailabilityRateLimiter` — budget enforcement, per-peer isolation
- `validate_batch` — publish-only, sorted, non-overlapping, bounded
- `build_availability_fragments` — splitting, single fragment, empty input
- `select_fanout_peers` — half-scored half-random, self-exclusion

### Integration Tests (follow-up)
After all tasks compile, add an integration test in `crates/chain/tests/`:

1. Start a 3-node test cluster
2. Node A produces a block with PD transactions
3. Verify Node A sends `PdChunkAvailabilityBatch` to Node B
4. Verify Node B stores provider hints for the announced offsets
5. When Node C fetches a PD chunk, verify it tries Node A/B (hinted) before assignees

This requires the `IrysNodeTest` harness from `crates/chain/tests/` and follows the existing multi-node test patterns.

---

## Guardrails Checklist

Per the design document, verify these invariants hold:

- [ ] **Never gossip PD chunk bytes.** Only `PdChunkAvailabilityBatch` metadata is gossiped. `GossipDataV2::PdChunk` remains excluded from `pre_serialize_for_broadcast`.
- [ ] **Never full-mesh PD availability.** `select_fanout_peers` caps at `pd_availability_fanout` peers, not all peers.
- [ ] **Never rebroadcast third-party announcements.** The announcer only announces chunks this node can serve (`has_chunk_for_pd` == true).
- [ ] **Never reuse `GossipCacheKey::Block`.** A dedicated `GossipCacheKey::PdChunkAvailability(H256)` variant is used.
- [ ] **Never rely on batch-count rate limiting alone.** The route has a `max_body_bytes` cap AND `PdAvailabilityRateLimiter` enforces per-peer chunk budget.
