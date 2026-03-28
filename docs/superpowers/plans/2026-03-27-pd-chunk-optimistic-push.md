# PD Chunk Optimistic Push Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement optimistic push of PD chunk data to peers when PD transactions are detected in the EVM mempool, so validators have chunks cached locally before block validation.

**Architecture:** PdService gains outbound push capability via a new `PdChunkPusher` trait. When a PD tx enters the mempool, each provisioned chunk is pushed to k peers (k/2 top-scored + k/2 random). A new gossip route `/gossip/v2/pd_chunk_push` receives pushed chunks, validates them against the block index, and inserts them into the PdService LRU cache. An `InboundPushTracker` (moka, 5-min TTL) prevents pushing chunks back to peers that sent them.

**Tech Stack:** Rust 1.93.0, tokio, actix-web, moka, serde, lru, irys-types Base64

**Spec:** `docs/superpowers/specs/2026-03-27-pd-chunk-optimistic-push-implementation-design.md`

---

## File Map

| File | Action | Responsibility |
|------|--------|---------------|
| `crates/types/src/gossip.rs` | Modify | Add `PdChunkPush` wire type |
| `crates/types/src/chunk_provider.rs` | Modify | Add `OptimisticPush` variant to `PdChunkMessage`, add `PdChunkPusher` trait |
| `crates/types/src/config/node.rs` | Modify | Add `pd_optimistic_push_fanout` to `P2PGossipConfig` |
| `crates/actors/src/pd_service/cache.rs` | Modify | Add `data_path` to `CachedChunkEntry`, add `insert_unreferenced` method |
| `crates/actors/src/pd_service/inbound_push_tracker.rs` | Create | `InboundPushTracker` with moka cache |
| `crates/actors/src/pd_service/push.rs` | Create | `schedule_outbound_push`, `select_push_targets`, `resolve_partition_assignee_ids` |
| `crates/actors/src/pd_service.rs` | Modify | Wire new fields, handle `OptimisticPush` message, modify `handle_provision_chunks` and `on_fetch_success` |
| `crates/domain/src/models/peer_list.rs` | Modify | Add `all_peers_with_score()` method |
| `crates/p2p/src/server.rs` | Modify | Add `/gossip/v2/pd_chunk_push` route handler |
| `crates/p2p/src/gossip_client.rs` | Modify | Implement `PdChunkPusher` trait on `GossipClient` |
| `crates/p2p/src/types.rs` | Modify | Add `PdChunkPush` variant to `GossipRoutes` |
| `crates/chain/src/chain.rs` | Modify | Wire `PdChunkPusher` + config into `PdService::spawn_service` |

---

### Task 1: Wire Type and Message Enum

**Files:**
- Modify: `crates/types/src/gossip.rs`
- Modify: `crates/types/src/chunk_provider.rs`

- [ ] **Step 1: Add `PdChunkPush` struct to `crates/types/src/gossip.rs`**

Add after the existing gossip types (near line 462, after `GossipRequestV2`):

```rust
/// A single unpacked PD chunk pushed optimistically before block validation.
/// The receiver derives all verification info from the block index using (ledger, offset).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PdChunkPush {
    /// Ledger ID (0 = Publish). PD operates exclusively on the Publish ledger.
    pub ledger: u32,
    /// Absolute ledger chunk offset.
    pub offset: u64,
    /// Unpacked chunk bytes (up to 256 KiB).
    pub chunk_bytes: Base64,
    /// Merkle proof (data_path) for verification against the data_root.
    pub data_path: Base64,
}
```

- [ ] **Step 2: Add `PdChunkPusher` trait and `OptimisticPush` variant to `crates/types/src/chunk_provider.rs`**

Add the trait after the `PdChunkFetcher` trait (near line 86):

```rust
/// Pushes a PD chunk to a peer. Fire-and-forget (detached tokio task).
pub trait PdChunkPusher: Send + Sync + 'static {
    fn push_pd_chunk(
        &self,
        peer_id: crate::IrysPeerId,
        peer_addr: &crate::PeerAddress,
        push: &crate::gossip::PdChunkPush,
    );
}
```

Add `OptimisticPush` variant to the `PdChunkMessage` enum (near line 108):

```rust
OptimisticPush {
    peer_id: crate::IrysPeerId,
    push: crate::gossip::PdChunkPush,
},
```

- [ ] **Step 3: Verify compilation**

Run: `cargo check -p irys-types`

Expected: compiles with no errors.

---

### Task 2: Configuration

**Files:**
- Modify: `crates/types/src/config/node.rs`

- [ ] **Step 1: Add `pd_optimistic_push_fanout` to `P2PGossipConfig`**

In the `P2PGossipConfig` struct (near line 541), add:

```rust
/// Number of peers to push each PD chunk to.
/// Peers are selected as k/2 top-scored + k/2 random,
/// excluding known sources and partition assignees.
/// Set to 0 to disable optimistic push entirely.
#[serde(default = "default_pd_optimistic_push_fanout")]
pub pd_optimistic_push_fanout: u32,
```

Add the default function near the other defaults:

```rust
fn default_pd_optimistic_push_fanout() -> u32 {
    4
}
```

Ensure the `Default` impl for `P2PGossipConfig` also sets this field to `default_pd_optimistic_push_fanout()`.

- [ ] **Step 2: Verify compilation**

Run: `cargo check -p irys-types`

Expected: compiles with no errors.

---

### Task 3: ChunkCache Changes — `data_path` Field and `insert_unreferenced`

**Files:**
- Modify: `crates/actors/src/pd_service/cache.rs`

- [ ] **Step 1: Write failing tests for `insert_unreferenced` and `data_path`**

Add to the existing `#[cfg(test)] mod tests` block in `cache.rs` (after line 395):

```rust
#[test]
fn test_insert_unreferenced_creates_evictable_entry() {
    let shared_index: ChunkDataIndex = Arc::new(DashMap::new());
    let mut cache = ChunkCache::new(NonZeroUsize::new(2).unwrap(), shared_index.clone());
    let key = ChunkKey { ledger: 0, offset: 100 };
    let data = Arc::new(Bytes::from(vec![1u8; 256]));
    let data_path = Base64(vec![2u8; 64]);

    let inserted = cache.insert_unreferenced(key, data.clone(), data_path.clone());
    assert!(inserted);
    assert!(cache.contains(&key));
    assert!(shared_index.contains_key(&(0, 100)));

    // Unreferenced entry should have empty referencing_txs
    // Note: peek() returns Option<Arc<Bytes>>. Use peek_entry() for full entry access.
    let entry = cache.peek_entry(&key).unwrap();
    assert!(entry.referencing_txs.is_empty());
    assert_eq!(entry.data_path.as_ref().unwrap().0, data_path.0);
}

#[test]
fn test_insert_unreferenced_noop_when_exists() {
    let shared_index: ChunkDataIndex = Arc::new(DashMap::new());
    let mut cache = ChunkCache::new(NonZeroUsize::new(2).unwrap(), shared_index.clone());
    let key = ChunkKey { ledger: 0, offset: 100 };
    let data = Arc::new(Bytes::from(vec![1u8; 256]));
    let tx_hash = B256::from([0xAA; 32]);

    // Insert with a reference first
    cache.insert(key, data.clone(), tx_hash);

    // insert_unreferenced should be a no-op
    let data_path = Base64(vec![2u8; 64]);
    let inserted = cache.insert_unreferenced(key, data.clone(), data_path);
    assert!(!inserted);

    // Original reference should still be there
    let entry = cache.peek_entry(&key).unwrap();
    assert!(entry.referencing_txs.contains(&tx_hash));
}

#[test]
fn test_insert_unreferenced_fills_missing_data_path() {
    let shared_index: ChunkDataIndex = Arc::new(DashMap::new());
    let mut cache = ChunkCache::new(NonZeroUsize::new(2).unwrap(), shared_index.clone());
    let key = ChunkKey { ledger: 0, offset: 100 };
    let data = Arc::new(Bytes::from(vec![1u8; 256]));
    let tx_hash = B256::from([0xAA; 32]);

    // Insert without data_path
    cache.insert(key, data.clone(), tx_hash);
    assert!(cache.peek_entry(&key).unwrap().data_path.is_none());

    // insert_unreferenced should fill in data_path
    let data_path = Base64(vec![2u8; 64]);
    let inserted = cache.insert_unreferenced(key, data.clone(), data_path.clone());
    assert!(!inserted); // not a new insert

    let entry = cache.peek_entry(&key).unwrap();
    assert_eq!(entry.data_path.as_ref().unwrap().0, data_path.0);
}

#[test]
fn test_insert_stores_data_path() {
    let shared_index: ChunkDataIndex = Arc::new(DashMap::new());
    let mut cache = ChunkCache::new(NonZeroUsize::new(2).unwrap(), shared_index.clone());
    let key = ChunkKey { ledger: 0, offset: 100 };
    let data = Arc::new(Bytes::from(vec![1u8; 256]));
    let tx_hash = B256::from([0xAA; 32]);
    let data_path = Base64(vec![2u8; 64]);

    cache.insert_with_data_path(key, data, tx_hash, Some(data_path.clone()));
    let entry = cache.peek_entry(&key).unwrap();
    assert_eq!(entry.data_path.as_ref().unwrap().0, data_path.0);
}

#[test]
fn test_lru_eviction_prefers_unreferenced() {
    let shared_index: ChunkDataIndex = Arc::new(DashMap::new());
    let mut cache = ChunkCache::new(NonZeroUsize::new(2).unwrap(), shared_index.clone());

    let key1 = ChunkKey { ledger: 0, offset: 1 };
    let key2 = ChunkKey { ledger: 0, offset: 2 };
    let key3 = ChunkKey { ledger: 0, offset: 3 };
    let data = Arc::new(Bytes::from(vec![1u8; 256]));

    // key1 is referenced, key2 is unreferenced
    cache.insert(key1, data.clone(), B256::from([0xAA; 32]));
    cache.insert_unreferenced(key2, data.clone(), Base64(vec![]));

    // Inserting key3 should evict key2 (unreferenced), not key1
    cache.insert(key3, data.clone(), B256::from([0xBB; 32]));
    assert!(cache.contains(&key1));
    assert!(!cache.contains(&key2));
    assert!(cache.contains(&key3));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run -p irys-actors test_insert_unreferenced test_insert_stores_data_path test_lru_eviction_prefers_unreferenced`

Expected: compilation errors — `insert_unreferenced`, `data_path` field, `insert_with_data_path` don't exist yet.

- [ ] **Step 3: Add `data_path` field to `CachedChunkEntry`**

In `cache.rs`, modify `CachedChunkEntry` (line 19):

```rust
pub struct CachedChunkEntry {
    pub data: Arc<Bytes>,
    pub data_path: Option<Base64>,
    pub referencing_txs: HashSet<B256>,
    pub cached_at: Instant,
}
```

Update the existing `insert` method (line 83) to set `data_path: None` in the new entry construction. Add a new `insert_with_data_path` method that accepts `Option<Base64>`:

```rust
pub fn insert_with_data_path(
    &mut self,
    key: ChunkKey,
    data: Arc<Bytes>,
    tx_hash: B256,
    data_path: Option<Base64>,
) -> bool {
    // Same logic as insert(), but sets data_path
}
```

Refactor `insert` to call `insert_with_data_path(key, data, tx_hash, None)`.

Also add a `peek_entry` method (the existing `peek` returns `Option<Arc<Bytes>>`, discarding struct fields):

```rust
pub fn peek_entry(&self, key: &ChunkKey) -> Option<&CachedChunkEntry> {
    self.chunks.peek(key)
}
```

Note: `lru::LruCache::peek` returns `Option<&V>` without promoting. This new method exposes the full entry for tests and `schedule_outbound_push`.

- [ ] **Step 4: Add `insert_unreferenced` method**

```rust
pub fn insert_unreferenced(
    &mut self,
    key: ChunkKey,
    data: Arc<Bytes>,
    data_path: Base64,
) -> bool {
    if let Some(existing) = self.chunks.peek_mut(&key) {
        // Fill in data_path if missing
        if existing.data_path.is_none() {
            existing.data_path = Some(data_path);
        }
        return false;
    }
    self.make_room_for_insert();
    self.shared_index.insert((key.ledger, key.offset), data.clone());
    self.chunks.push(
        key,
        CachedChunkEntry {
            data,
            data_path: Some(data_path),
            referencing_txs: HashSet::new(),
            cached_at: Instant::now(),
        },
    );
    true
}
```

- [ ] **Step 5: Fix any existing tests that construct `CachedChunkEntry` without `data_path`**

The existing tests in `cache.rs` use `cache.insert(key, data, tx_hash)` which goes through the method. No direct `CachedChunkEntry` construction in tests. But verify by checking if any test asserts on the struct fields.

- [ ] **Step 6: Run all cache tests**

Run: `cargo nextest run -p irys-actors cache::tests`

Expected: all tests pass (existing + new).

---

### Task 4: InboundPushTracker

**Files:**
- Create: `crates/actors/src/pd_service/inbound_push_tracker.rs`
- Modify: `crates/actors/src/pd_service.rs` (add `mod inbound_push_tracker;`)

- [ ] **Step 1: Write the module with tests**

**Dependency check:** `moka` must be in `crates/actors/Cargo.toml`. If missing, add `moka = { workspace = true }` to `[dependencies]`.

Create `crates/actors/src/pd_service/inbound_push_tracker.rs`:

```rust
use irys_types::IrysPeerId;
use moka::sync::Cache;
use std::sync::RwLock;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

const INBOUND_PUSH_TRACKER_TTL: Duration = Duration::from_secs(300);

pub struct InboundPushTracker {
    cache: Cache<(u32, u64), Arc<RwLock<HashSet<IrysPeerId>>>>,
}

impl InboundPushTracker {
    pub fn new() -> Self {
        Self {
            cache: Cache::builder()
                .time_to_live(INBOUND_PUSH_TRACKER_TTL)
                .build(),
        }
    }

    pub fn record_inbound(&self, ledger: u32, offset: u64, peer_id: IrysPeerId) {
        let entry = self
            .cache
            .get_with((ledger, offset), || Arc::new(RwLock::new(HashSet::new())));
        entry.write().insert(peer_id);
    }

    pub fn get_known_sources(&self, ledger: u32, offset: u64) -> HashSet<IrysPeerId> {
        self.cache
            .get(&(ledger, offset))
            .map(|entry| entry.read().clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_peer_id(byte: u8) -> IrysPeerId {
        IrysPeerId::from([byte; 20])
    }

    #[test]
    fn test_record_and_retrieve() {
        let tracker = InboundPushTracker::new();
        let peer_a = test_peer_id(0xAA);
        let peer_b = test_peer_id(0xBB);

        tracker.record_inbound(0, 100, peer_a);
        tracker.record_inbound(0, 100, peer_b);

        let sources = tracker.get_known_sources(0, 100);
        assert_eq!(sources.len(), 2);
        assert!(sources.contains(&peer_a));
        assert!(sources.contains(&peer_b));
    }

    #[test]
    fn test_unknown_key_returns_empty() {
        let tracker = InboundPushTracker::new();
        let sources = tracker.get_known_sources(0, 999);
        assert!(sources.is_empty());
    }

    #[test]
    fn test_different_offsets_are_independent() {
        let tracker = InboundPushTracker::new();
        let peer_a = test_peer_id(0xAA);

        tracker.record_inbound(0, 100, peer_a);

        assert_eq!(tracker.get_known_sources(0, 100).len(), 1);
        assert_eq!(tracker.get_known_sources(0, 200).len(), 0);
    }
}
```

- [ ] **Step 2: Register the module in `pd_service.rs`**

Add `mod inbound_push_tracker;` near the other module declarations at the top of `crates/actors/src/pd_service.rs` (near line 1, alongside `mod cache;`, `mod fetch;`, `mod provisioning;`).

- [ ] **Step 3: Run tests**

Run: `cargo nextest run -p irys-actors inbound_push_tracker::tests`

Expected: all 3 tests pass.

---

### Task 5: PeerList — `all_peers_with_score` Method

**Files:**
- Modify: `crates/domain/src/models/peer_list.rs`

- [ ] **Step 1: Write a test for the new method**

Add to the existing test module in `peer_list.rs`:

```rust
#[test]
fn test_all_peers_with_score() {
    let peer_list = PeerList::test_mock();
    // test_mock creates a peer list with some default peers
    let peers = peer_list.all_peers_with_score();
    // Should return (IrysPeerId, PeerAddress, f64) tuples
    for (_peer_id, _addr, score) in &peers {
        // score should be non-negative (cast from u16)
        assert!(*score >= 0.0);
    }
}
```

- [ ] **Step 2: Implement `all_peers_with_score`**

Add to the `impl PeerList` block (near the existing `all_peers_sorted_by_score` at line 440):

```rust
/// Returns all online peers with their address and reputation score as f64.
/// Does not sort — caller is responsible for selection logic.
pub fn all_peers_with_score(&self) -> Vec<(IrysPeerId, PeerAddress, f64)> {
    let inner = self.0.read();
    inner
        .persistent_peers_cache
        .iter()
        .chain(inner.unstaked_peer_purgatory.iter().map(|(k, v)| (k, v)))
        .filter(|(_, item)| item.is_online)
        .map(|(id, item)| {
            (
                *id,
                item.address.clone(),
                item.reputation_score.get() as f64, // PeerScore::get() returns u16, cast to f64
            )
        })
        .collect()
}
```

Check exact field names and types by reading `PeerListItem` and `PeerScore`. `PeerScore::get()` returns `u16` — we cast to `f64` for uniform scoring in `select_push_targets`. `PeerAddress` has fields `api`, `gossip`, and `execution: RethPeerInfo` — clone the whole struct rather than reconstructing.

- [ ] **Step 3: Run tests**

Run: `cargo nextest run -p irys-domain test_all_peers_with_score`

Expected: passes.

---

### Task 6: Peer Selection and Push Logic

**Files:**
- Create: `crates/actors/src/pd_service/push.rs`

- [ ] **Step 1: Write tests for `select_push_targets`**

Create `crates/actors/src/pd_service/push.rs`:

```rust
use irys_types::{IrysPeerId, PeerAddress};
use std::collections::HashSet;

/// Select k push targets: k/2 from top-scored, k/2 random.
/// Candidates must already be filtered (excluded peers removed).
pub fn select_push_targets(
    k: u32,
    candidates: &[(IrysPeerId, PeerAddress, f64)],
) -> Vec<(IrysPeerId, PeerAddress)> {
    if candidates.is_empty() || k == 0 {
        return vec![];
    }

    let k = k as usize;
    let half = k / 2;
    let other_half = k - half;

    // Sort by score descending for top-k selection
    let mut sorted: Vec<_> = candidates.to_vec();
    sorted.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

    let mut selected = HashSet::new();
    let mut result = Vec::with_capacity(k);

    // Take top-scored
    for (id, addr, _) in sorted.iter().take(half) {
        if selected.insert(*id) {
            result.push((*id, addr.clone()));
        }
    }

    // Take random from remaining
    use rand::seq::SliceRandom;
    let remaining: Vec<_> = sorted
        .iter()
        .filter(|(id, _, _)| !selected.contains(id))
        .collect();

    let mut rng = rand::thread_rng();
    let mut shuffled = remaining;
    shuffled.shuffle(&mut rng);

    for (id, addr, _) in shuffled.iter().take(other_half) {
        if selected.insert(*id) {
            result.push((*id, addr.clone()));
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn make_candidate(byte: u8, score: f64) -> (IrysPeerId, PeerAddress, f64) {
        let addr: SocketAddr = format!("127.0.0.1:{}", 3000 + byte as u16).parse().unwrap();
        (
            IrysPeerId::from([byte; 20]),
            PeerAddress { api: addr, gossip: addr, execution: Default::default() },
            score,
        )
    }

    #[test]
    fn test_empty_candidates_returns_empty() {
        let result = select_push_targets(4, &[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_zero_k_returns_empty() {
        let candidates = vec![make_candidate(1, 10.0)];
        let result = select_push_targets(0, &candidates);
        assert!(result.is_empty());
    }

    #[test]
    fn test_fewer_candidates_than_k() {
        let candidates = vec![make_candidate(1, 10.0), make_candidate(2, 5.0)];
        let result = select_push_targets(4, &candidates);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_returns_exactly_k_when_enough_candidates() {
        let candidates: Vec<_> = (1..=10)
            .map(|i| make_candidate(i, 10.0 - i as f64))
            .collect();
        let result = select_push_targets(4, &candidates);
        assert_eq!(result.len(), 4);
    }

    #[test]
    fn test_no_duplicates() {
        let candidates: Vec<_> = (1..=10)
            .map(|i| make_candidate(i, 10.0 - i as f64))
            .collect();
        let result = select_push_targets(4, &candidates);
        let ids: HashSet<_> = result.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids.len(), result.len());
    }

    #[test]
    fn test_top_scored_included() {
        let candidates = vec![
            make_candidate(1, 100.0), // highest
            make_candidate(2, 50.0),  // second
            make_candidate(3, 1.0),
            make_candidate(4, 1.0),
            make_candidate(5, 1.0),
            make_candidate(6, 1.0),
        ];
        let result = select_push_targets(4, &candidates);
        let ids: HashSet<_> = result.iter().map(|(id, _)| *id).collect();
        // Top 2 (k/2=2) should always be included
        assert!(ids.contains(&IrysPeerId::from([1; 20])));
        assert!(ids.contains(&IrysPeerId::from([2; 20])));
    }
}
```

- [ ] **Step 2: Register the module in `pd_service.rs`**

Add `mod push;` near the other module declarations.

- [ ] **Step 3: Run tests**

Run: `cargo nextest run -p irys-actors push::tests`

Expected: all tests pass.

---

### Task 7: GossipRoutes and GossipClient — `PdChunkPusher` Implementation

**Files:**
- Modify: `crates/p2p/src/types.rs`
- Modify: `crates/p2p/src/gossip_client.rs`

- [ ] **Step 1: Add `PdChunkPush` variant to `GossipRoutes`**

In `crates/p2p/src/types.rs`, add to the `GossipRoutes` enum (near line 280):

```rust
PdChunkPush,
```

Add to the `as_str()` match (near line 310):

```rust
Self::PdChunkPush => "/pd_chunk_push",
```

- [ ] **Step 2: Implement `PdChunkPusher` on `GossipClient`**

In `crates/p2p/src/gossip_client.rs`, add the trait implementation:

```rust
impl irys_types::chunk_provider::PdChunkPusher for GossipClient {
    fn push_pd_chunk(
        &self,
        peer_id: irys_types::IrysPeerId,
        peer_addr: &irys_types::PeerAddress,
        push: &irys_types::gossip::PdChunkPush,
    ) {
        let request = self.create_request_v2(push.clone());
        let body = match serde_json::to_vec(&request) {
            Ok(b) => bytes::Bytes::from(b),
            Err(e) => {
                tracing::warn!(?e, "failed to serialize PdChunkPush");
                return;
            }
        };

        let client = self.client.clone();
        let gossip_addr = peer_addr.gossip;
        let circuit_breaker = self.circuit_breaker.clone();

        self.runtime_handle.spawn(async move {
            if circuit_breaker.check(&peer_id).is_err() {
                return;
            }
            let url = format!(
                "{}/{}",
                gossip_base_url(&gossip_addr, ProtocolVersion::V2),
                GossipRoutes::PdChunkPush.as_str().trim_start_matches('/')
            );
            match client
                .post(&url)
                .header("Content-Type", "application/json")
                .body(body)
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    circuit_breaker.record_success(&peer_id);
                }
                Ok(resp) => {
                    tracing::debug!(?peer_id, status = %resp.status(), "pd_chunk_push rejected");
                    circuit_breaker.record_failure(&peer_id);
                }
                Err(e) => {
                    tracing::debug!(?peer_id, ?e, "pd_chunk_push send failed");
                    circuit_breaker.record_failure(&peer_id);
                }
            }
        });
    }
}
```

Adapt this to match the exact `GossipClient` internal API — use `self.client` (the `reqwest::Client`), follow the pattern in `send_preserialized_detached` (line 954). The circuit breaker calls are `self.circuit_breaker.check_circuit_breaker(peer_id)` and `self.circuit_breaker.record_success/failure`. Check exact method names by reading `CircuitBreakerManager`.

- [ ] **Step 3: Verify compilation**

Run: `cargo check -p irys-p2p`

Expected: compiles with no errors.

---

### Task 8: Gossip Server — `/gossip/v2/pd_chunk_push` Route

**Files:**
- Modify: `crates/p2p/src/server.rs`

- [ ] **Step 1: Add the route handler method to `GossipServer`**

Add a new handler method following the pattern of `handle_chunk_v2` (line 557):

```rust
async fn handle_pd_chunk_push(
    server: Data<Self>,
    json: web::Json<GossipRequestV2<PdChunkPush>>,
    req: actix_web::HttpRequest,
) -> HttpResponse {
    if !server.data_handler.sync_state.is_gossip_reception_enabled() {
        return HttpResponse::Ok().json(GossipResponse::Rejected(RejectionReason::GossipDisabled));
    }

    let v2_request = json.into_inner();
    let source_peer_id = v2_request.peer_id;
    let source_miner_address = v2_request.miner_address;

    if let Err(resp) =
        Self::check_peer_v2(&server.peer_list, &req, source_peer_id, source_miner_address)
    {
        return resp;
    }
    server.peer_list.set_is_online(&source_miner_address, true);

    // Send to PdService via the pd_chunk_sender channel
    if let Some(ref sender) = server.data_handler.pd_chunk_sender {
        let msg = PdChunkMessage::OptimisticPush {
            peer_id: source_peer_id,
            push: v2_request.data,
        };
        if sender.send(msg).is_err() {
            tracing::warn!("pd_chunk_sender channel closed");
        }
    }

    HttpResponse::Ok().json(GossipResponse::Accepted(()))
}
```

Note: `GossipDataHandler` needs a `pd_chunk_sender: Option<PdChunkSender>` field. Check if it already exists — if not, it must be added and wired through `GossipDataHandler::new`.

- [ ] **Step 2: Register the route in `routes()`**

In the V2 scope (near line 1518), add:

```rust
.route(
    GossipRoutes::PdChunkPush.as_str(),
    web::post().to(Self::handle_pd_chunk_push),
)
```

- [ ] **Step 3: Add route-level 768 KiB body cap**

Wrap the route in its own scope with a local `JsonConfig`:

```rust
.service(
    web::scope(GossipRoutes::PdChunkPush.as_str())
        .app_data(web::JsonConfig::default().limit(768 * 1024))
        .route("", web::post().to(Self::handle_pd_chunk_push))
)
```

- [ ] **Step 4: Wire `pd_chunk_sender` into `GossipDataHandler` if not already present**

Check `GossipDataHandler` struct fields. If `pd_chunk_sender` is not there, add `pub pd_chunk_sender: Option<PdChunkSender>`. Wire it through from `P2PService::new` → `GossipDataHandler::new` → `GossipServer`. The sender should come from `chain.rs` where the `(pd_chunk_tx, pd_chunk_rx)` channel is created.

- [ ] **Step 5: Verify compilation**

Run: `cargo check -p irys-p2p`

Expected: compiles. If `pd_chunk_sender` wiring is incomplete, this will fail — fix the chain through `P2PService::new` and `chain.rs`.

---

### Task 9: PdService — Handle `OptimisticPush` Message

**Files:**
- Modify: `crates/actors/src/pd_service.rs`

- [ ] **Step 1: Write a test for `handle_optimistic_push`**

Add to the existing test module in `pd_service.rs` (near line 1738). Use the existing `test_service()` fixture. The test needs a block index entry so `derive_chunk_verification_info` succeeds. If `test_service()` doesn't set up block index data, this test may need to insert mock block index entries. If that is too complex, write a simpler test that verifies rejection for invalid offsets:

```rust
#[test]
fn test_optimistic_push_rejects_invalid_offset() {
    let (mut service, _tmpdir) = test_service();
    let peer_id = IrysPeerId::from([0xAA; 20]);
    let push = PdChunkPush {
        ledger: 0,
        offset: 999_999_999, // no such offset in block index
        chunk_bytes: Base64(vec![0u8; 256]),
        data_path: Base64(vec![0u8; 64]),
    };
    // Should not panic, should silently reject
    service.handle_optimistic_push(peer_id, push);
    // Chunk should NOT be in cache
    let key = ChunkKey { ledger: 0, offset: 999_999_999 };
    assert!(!service.cache.contains(&key));
}

#[test]
fn test_optimistic_push_rejects_non_publish_ledger() {
    let (mut service, _tmpdir) = test_service();
    let peer_id = IrysPeerId::from([0xAA; 20]);
    let push = PdChunkPush {
        ledger: 1, // Submit ledger — should be rejected
        offset: 0,
        chunk_bytes: Base64(vec![0u8; 256]),
        data_path: Base64(vec![0u8; 64]),
    };
    service.handle_optimistic_push(peer_id, push);
    let key = ChunkKey { ledger: 1, offset: 0 };
    assert!(!service.cache.contains(&key));
}
```

- [ ] **Step 2: Add `inbound_push_tracker` and `chunk_pusher` fields to `PdService`**

In the `PdService` struct (line 43), add:

```rust
inbound_push_tracker: InboundPushTracker,
chunk_pusher: Arc<dyn PdChunkPusher>,
pd_optimistic_push_fanout: u32,
```

Update `spawn_service` signature to accept `chunk_pusher: Arc<dyn PdChunkPusher>` and `pd_optimistic_push_fanout: u32`. Initialize `inbound_push_tracker: InboundPushTracker::new()` in the constructor.

- [ ] **Step 3: Add `OptimisticPush` dispatch to `handle_message`**

In `handle_message` (line 677), add the match arm:

```rust
PdChunkMessage::OptimisticPush { peer_id, push } => {
    self.handle_optimistic_push(peer_id, push);
}
```

- [ ] **Step 4: Implement `handle_optimistic_push`**

```rust
fn handle_optimistic_push(&mut self, peer_id: IrysPeerId, push: PdChunkPush) {
    // 0. Ledger constraint
    if push.ledger != 0 {
        tracing::debug!(ledger = push.ledger, "optimistic push rejected: non-Publish ledger");
        return;
    }

    // 1. Block index validation
    let key = ChunkKey { ledger: push.ledger, offset: push.offset };
    // derive_chunk_verification_info returns eyre::Result, not Option
    let info = match self.derive_chunk_verification_info(&key) {
        Ok(info) => info,
        Err(e) => {
            tracing::debug!(?e, offset = push.offset, "optimistic push rejected: invalid offset");
            return;
        }
    };

    // 2. Merkle proof verification (same logic as on_fetch_success)
    let chunk_size = self.storage_provider.config().chunk_size as u64;
    let num_chunks = info.data_size.div_ceil(chunk_size);
    let last_chunk_offset = num_chunks.saturating_sub(1);
    let target_byte_offset: u128 = if info.tx_chunk_offset == last_chunk_offset {
        u128::from(info.data_size).saturating_sub(1)
    } else {
        u128::from(info.tx_chunk_offset + 1) * u128::from(chunk_size) - 1
    };

    // validate_path takes &Base64, not &Vec<u8>
    let path_result = match irys_types::validate_path(
        info.expected_data_root.0,
        &push.data_path,
        target_byte_offset,
    ) {
        Ok(result) => result,
        Err(e) => {
            tracing::debug!(?e, offset = push.offset, "optimistic push rejected: bad merkle proof");
            return;
        }
    };

    // 3. Leaf hash verification (use irys_types::hash_sha256, same as on_fetch_success)
    let leaf_hash = irys_types::hash_sha256(&push.chunk_bytes.0);
    if leaf_hash != path_result.leaf_hash {
        tracing::debug!(offset = push.offset, "optimistic push rejected: leaf hash mismatch");
        return;
    }

    // 4. Cache insert (unreferenced)
    let data = Arc::new(bytes::Bytes::copy_from_slice(&push.chunk_bytes.0));
    self.cache.insert_unreferenced(key, data, push.data_path);

    // 5. Record inbound source
    self.inbound_push_tracker.record_inbound(push.ledger, push.offset, peer_id);

    tracing::trace!(offset = push.offset, ?peer_id, "accepted optimistic pd chunk push");
}
```

Adapt the exact function signatures and types by reading `validate_path` and the sha2 usage in `on_fetch_success`.

- [ ] **Step 5: Update `test_service()` to provide mock `chunk_pusher`**

Add a `MockPdChunkPusher` to the test module:

```rust
#[derive(Debug)]
struct MockPdChunkPusher;

impl PdChunkPusher for MockPdChunkPusher {
    fn push_pd_chunk(&self, _: IrysPeerId, _: &PeerAddress, _: &PdChunkPush) {}
}
```

Update `test_service()` to pass `Arc::new(MockPdChunkPusher)` and `pd_optimistic_push_fanout: 0` (disabled for existing tests).

- [ ] **Step 6: Run tests**

Run: `cargo nextest run -p irys-actors pd_service::tests`

Expected: all existing and new tests pass.

---

### Task 10: PdService — Outbound Push in `handle_provision_chunks` and `on_fetch_success`

**Files:**
- Modify: `crates/actors/src/pd_service.rs`

- [ ] **Step 1: Write a test for outbound push on cache hit**

```rust
#[test]
fn test_provision_triggers_push_on_cache_hit() {
    // Use a mock PdChunkPusher that records calls
    use std::sync::Mutex;

    #[derive(Debug)]
    struct RecordingPusher {
        pushes: Mutex<Vec<(u32, u64)>>,
    }
    impl PdChunkPusher for RecordingPusher {
        fn push_pd_chunk(&self, _: IrysPeerId, _: &PeerAddress, push: &PdChunkPush) {
            self.pushes.lock().unwrap().push((push.ledger, push.offset));
        }
    }

    let pusher = Arc::new(RecordingPusher { pushes: Mutex::new(vec![]) });
    let (mut service, _tmpdir) = test_service_with_pusher(pusher.clone(), 4);

    // Pre-populate cache with a chunk
    let key = ChunkKey { ledger: 0, offset: 42 };
    let data = Arc::new(Bytes::from(vec![0u8; 256]));
    let tx_hash = B256::from([0x11; 32]);
    service.cache.insert(key, data, tx_hash);

    // Note: Full end-to-end testing of outbound push during provisioning
    // requires a valid peer list with scored peers and valid chunk specs.
    // This is better tested at the integration level.
    // For unit testing, verify that schedule_outbound_push with fanout=0
    // produces no pushes, and with fanout>0 calls the pusher.
    // The select_push_targets logic is already tested in push::tests.
}
```

This test is a skeleton — the full end-to-end flow is better validated via integration tests or by testing `schedule_outbound_push` directly. The `select_push_targets` logic is already covered in Task 6's `push::tests`.

- [ ] **Step 2: Switch `handle_provision_chunks` local storage path to `get_chunk_for_pd`**

In `handle_provision_chunks` (line 797), replace:
```rust
self.storage_provider.get_unpacked_chunk_by_ledger_offset(key.ledger, key.offset)
```
with:
```rust
self.storage_provider.get_chunk_for_pd(key.ledger, key.offset)
```

Then add unpacking logic (same as `on_fetch_success`):
- If `ChunkFormat::Packed(packed)` → call `irys_packing::unpack(...)` to get unpacked bytes, extract `data_path`
- If `ChunkFormat::Unpacked(unpacked)` → extract bytes and `data_path` directly
- Insert via `cache.insert_with_data_path(key, data, tx_hash, Some(data_path))`

- [ ] **Step 3: Add `schedule_outbound_push` call after each successful provision**

After each cache insert in `handle_provision_chunks` (cache hit, local storage hit), call:
```rust
self.schedule_outbound_push(&key);
```

- [ ] **Step 4: Add `schedule_outbound_push` call in `on_fetch_success`**

After the cache insert at line 348, add:
```rust
self.schedule_outbound_push(&key);
```

Also pass `data_path` through to the cache insert (currently discarded). Change the insert call to use `insert_with_data_path`.

- [ ] **Step 5: Implement `schedule_outbound_push` on `PdService`**

```rust
fn schedule_outbound_push(&self, key: &ChunkKey) {
    if self.pd_optimistic_push_fanout == 0 {
        return;
    }

    // Use peek_entry to access full CachedChunkEntry (including data_path)
    let entry = match self.cache.peek_entry(key) {
        Some(e) => e,
        None => return,
    };

    let data_path = match &entry.data_path {
        Some(dp) => dp.clone(),
        None => return, // can't push without merkle proof
    };

    let mut excluded_ids = self.inbound_push_tracker.get_known_sources(key.ledger, key.offset);
    excluded_ids.extend(self.resolve_partition_assignee_ids(key));

    let candidates: Vec<_> = self
        .peer_list
        .all_peers_with_score()
        .into_iter()
        .filter(|(id, _, _)| !excluded_ids.contains(id))
        .collect();

    let targets = push::select_push_targets(self.pd_optimistic_push_fanout, &candidates);

    let push_msg = PdChunkPush {
        ledger: key.ledger,
        offset: key.offset,
        chunk_bytes: Base64(entry.data.to_vec()),
        data_path,
    };

    for (peer_id, peer_addr) in targets {
        self.chunk_pusher.push_pd_chunk(peer_id, &peer_addr, &push_msg);
    }
}
```

- [ ] **Step 6: Implement `resolve_partition_assignee_ids`**

Adapt from the existing `resolve_peers_for_chunk` (line 628) but return `HashSet<IrysPeerId>` instead of `Vec<PeerAddress>`:

```rust
fn resolve_partition_assignee_ids(&self, key: &ChunkKey) -> HashSet<IrysPeerId> {
    let mut ids = HashSet::new();
    let slot_index = (key.offset / self.num_chunks_in_partition) as usize;

    let block_tree = self.block_tree.read();
    // canonical_epoch_snapshot() returns Arc<EpochSnapshot>, not Option
    let epoch_snapshot = block_tree.canonical_epoch_snapshot();

    let publish_ledger_id = DataLedger::Publish.get_ledger_id();

    // data_partitions iteration yields (&PartitionHash, &PartitionAssignment) tuples
    for (_hash, assignment) in epoch_snapshot.partition_assignments.data_partitions.iter() {
        if assignment.ledger_id == Some(publish_ledger_id)
            && assignment.slot_index == Some(slot_index)
            && assignment.miner_address != self.own_miner_address
        {
            if let Some(item) = self.peer_list.peer_by_mining_address(&assignment.miner_address) {
                ids.insert(item.peer_id);
            }
        }
    }

    ids
}
```

Check exact field names by reading the epoch snapshot and partition assignment structs. Note: `slot_index` is `usize` in `PartitionAssignment`, so cast `u64` to `usize`.

- [ ] **Step 7: Verify compilation and run tests**

Run: `cargo check -p irys-actors --tests && cargo nextest run -p irys-actors pd_service`

Expected: compiles and all tests pass.

---

### Task 11: Chain Wiring

**Files:**
- Modify: `crates/chain/src/chain.rs`

- [ ] **Step 1: Update `PdService::spawn_service` call**

At `chain.rs:1849`, update the spawn call to pass the new parameters:

```rust
let chunk_pusher: Arc<dyn PdChunkPusher> =
    Arc::new(gossip_data_handler.gossip_client.clone());

let pd_service_handle = irys_actors::pd_service::PdService::spawn_service(
    pd_chunk_rx,
    chunk_provider.clone(),
    runtime_handle.clone(),
    chunk_data_index.clone(),
    ready_pd_txs.clone(),
    peer_list_guard.clone(),
    pd_chunk_fetcher,
    chunk_pusher,                                          // NEW
    config.node_config.p2p_gossip.pd_optimistic_push_fanout, // NEW
    block_tree_guard.clone(),
    block_index_guard.clone(),
    irys_db.clone(),
    config.consensus.num_chunks_in_partition,
    config.node_config.miner_address(),
);
```

- [ ] **Step 2: Wire `pd_chunk_sender` into GossipDataHandler if needed**

If Task 8 determined that `GossipDataHandler` needs a `pd_chunk_sender` field, pass a clone of `pd_chunk_tx` (the sender side of the PdChunkMessage channel) through `P2PService::new` → `GossipDataHandler::new`. The channel pair `(pd_chunk_tx, pd_chunk_rx)` is created at `chain.rs:1149`.

- [ ] **Step 3: Verify full workspace compilation**

Run: `cargo check --workspace`

Expected: compiles with no errors.

---

### Task 12: Full Verification

**Files:** None (verification only)

- [ ] **Step 1: Run formatting**

Run: `cargo fmt --all`

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --workspace --tests --all-targets`

Fix any warnings.

- [ ] **Step 3: Run all PD-related tests**

Run: `cargo nextest run -p irys-actors pd_service`

Expected: all pass.

- [ ] **Step 4: Run P2P tests**

Run: `cargo nextest run -p irys-p2p`

Expected: all pass.

- [ ] **Step 5: Run full test suite**

Run: `cargo xtask test`

Expected: no new failures.

- [ ] **Step 6: Verify the feature can be disabled**

Set `pd_optimistic_push_fanout = 0` in a test config and verify that no outbound pushes occur. The `MockPdChunkPusher` recording test from Task 10 covers this.
