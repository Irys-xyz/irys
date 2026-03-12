# PD Chunk Lock-Free Indexes Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate all `blocking_recv()` calls in the PD payload building and EVM execution paths by replacing channel-based request-reply with shared concurrent data structures.

**Architecture:** Two shared concurrent structures replace the blocking channels:
1. `ChunkDataIndex` (`Arc<DashMap>`) — PdService writes chunk bytes during provisioning; the PD precompile reads lock-free during EVM execution.
2. `ready_pd_txs` (`Arc<DashSet>`) — PdService writes directly on provision/release; the payload builder reads with `.contains()`.

PdService is the **single authority** for both structures. The Reth mempool is the single authority on transaction liveness — when it drops a tx, the monitor sends `TransactionRemoved`, and PdService cleans up. No independent TTL expiry.

**Key constraint:** Reth's `BestTransactionFilter` calls `mark_invalid` which blocks ALL subsequent txs from the same sender — too aggressive for PD gating. The current `CombinedTransactionIterator` intentionally uses `continue` (skip without mark_invalid) to preserve sender ordering. This plan keeps that behavior.

**Tech Stack:** `dashmap 6.0` (already a workspace dependency), existing `tokio::sync::mpsc` channel (retained for NewTransaction/TransactionRemoved/ProvisionBlockChunks/ReleaseBlockChunks).

---

## Architecture Diagram

```
                                   PdService (CL)
                                  ┌───────────────────────────┐
  mempool monitor ──NewTx───────> │ provisions chunks         │
  (event-driven)                  │ writes to:                │
       │                          │   ChunkDataIndex (DashMap) │──── Arc<DashMap<(u32,u64), Arc<Bytes>>>
       │                          │   ready_pd_txs (DashSet)   │         │
       │  ◄── TransactionRemoved  │ cleans up on release       │         │  PD precompile reads
       │                          └───────────────────────────┘         │  directly (no blocking)
       │                                   │                            │
       │                                   ▼                            │
       │                          ready_pd_txs: Arc<DashSet<B256>>      │
       │                          (PdService writes directly)           │
       │                                   │                            │
       ▼                                   ▼                            │
  pool lifecycle events ────>     CombinedTransactionIterator ──────────┘
  (Mined/Discarded/Invalid)         .contains(&tx_hash) → skip if not ready
                                    PD precompile → .get(&key) → Arc<Bytes>
```

## What's Removed (vs current code)

| Removed | Why |
|---------|-----|
| `PdChunkMessage::IsReady` variant | Replaced by `ready_pd_txs.contains()` (Phase 1) |
| `PdChunkMessage::GetChunk` variant | Replaced by `ChunkDataIndex.get()` (Phase 2 — kept during Phase 1) |
| `PdChunkMessage::Lock` / `Unlock` variants | Dead code (no senders exist outside enum/handler) (Phase 1) |
| `ProvisioningState::Locked` | No Lock/Unlock |
| TTL expiry (`expire_at_height`, `TTL_BLOCKS`, `expire_height`) | Reth pool owns tx liveness |
| `block_state_rx` subscription in PdService | Only used for TTL |
| `current_height` field in PdService | Only used for TTL |
| `lock_count` in `CachedChunkEntry` | No locking mechanism |
| `lock_chunks` / `unlock_chunks` on ChunkCache | No locking mechanism |
| `handle_lock` / `handle_unlock` on PdService | No Lock/Unlock messages |
| 100ms polling in mempool monitor | Replaced by event-driven listeners |
| `PdChunkSender` in `PdContext` / `IrysEvmFactory` | Replaced by `ChunkDataIndex` |
| `ConfigurePdChunkSender` trait | Replaced by `ConfigureChunkDataIndex` |

## File Structure

### Modified files

| File | Responsibility |
|------|---------------|
| `crates/types/src/chunk_provider.rs` | Add `ChunkDataIndex` type alias; remove `IsReady`, `Lock`, `Unlock` from `PdChunkMessage` (keep `GetChunk` until Phase 2) |
| `crates/actors/src/pd_service/provisioning.rs` | Remove `Locked` state, TTL fields, `expire_at_height`, `lock`/`unlock`, `is_ready` methods |
| `crates/actors/src/pd_service/cache.rs` | Add `ChunkDataIndex` field; mirror inserts/removes/evictions; remove `lock_count`, `lock_chunks`, `unlock_chunks` |
| `crates/actors/src/pd_service.rs` | Add `ready_pd_txs` + `ChunkDataIndex`; remove `block_state_rx`/`current_height`/lock handlers/TTL; write DashSet on provision/release |
| `crates/irys-reth/src/mempool.rs` | Switch to event-driven (pool listeners); add `ready_pd_txs` for passing to payload builder |
| `crates/irys-reth/src/payload.rs` | Replace blocking IsReady with `ready_pd_txs.contains()`; remove `PdChunkMessage` import |
| `crates/irys-reth/src/precompiles/pd/context.rs` | `ChunkSource::Manager` holds `ChunkDataIndex` instead of `PdChunkSender`; lock-free reads |
| `crates/irys-reth/src/evm.rs` | Replace `pd_chunk_sender` with `chunk_data_index` in `IrysEvmFactory`; rename trait |
| `crates/irys-reth/src/payload_builder_builder.rs` | Accept and forward `ready_pd_txs` and `chunk_data_index` |
| `crates/irys-reth/src/lib.rs` | Add new fields to `IrysEthereumNode` and `IrysExecutorBuilder`; thread through `components()` |
| `crates/reth-node-bridge/src/node.rs` | Accept and forward new types to `IrysEthereumNode` |
| `crates/chain/src/chain.rs` | Create `ChunkDataIndex` and `ready_pd_txs`; wire to PdService and reth |

---

## Chunk 1: Phase 1 — Simplify PdService + readiness via shared DashSet

### Task 1: Simplify types in irys-types

**Files:**
- Modify: `crates/types/src/chunk_provider.rs`
- Modify: `crates/types/Cargo.toml`

- [ ] **Step 1: Read the file**

Read `crates/types/src/chunk_provider.rs` to confirm current state.

- [ ] **Step 2: Add ChunkDataIndex type alias**

Add after the channel type aliases (after line 87):

```rust
use dashmap::DashMap;

/// Shared chunk data index for lock-free chunk reads during EVM execution.
/// PdService populates this during provisioning; the PD precompile reads it directly.
/// Keys are `(ledger: u32, offset: u64)` tuples matching `ChunkKey` semantics.
pub type ChunkDataIndex = Arc<DashMap<(u32, u64), Arc<Bytes>>>;
```

- [ ] **Step 3: Remove IsReady, Lock, Unlock from PdChunkMessage (keep GetChunk until Phase 2)**

Remove `IsReady`, `Lock`, and `Unlock` variants. **Keep `GetChunk`** — it's still used by `precompiles/pd/context.rs` until Task 9 replaces it with `ChunkDataIndex`. The enum should become:

```rust
#[derive(Debug)]
pub enum PdChunkMessage {
    /// New PD transaction detected — start provisioning chunks.
    NewTransaction {
        tx_hash: B256,
        chunk_specs: Vec<ChunkRangeSpecifier>,
    },
    /// Transaction removed from mempool (included in block or evicted).
    TransactionRemoved { tx_hash: B256 },
    /// Get cached chunk during EVM execution (removed in Phase 2).
    GetChunk {
        ledger: u32,
        offset: u64,
        response: oneshot::Sender<Option<Arc<Bytes>>>,
    },
    /// Provision chunks needed for validating a peer block.
    ProvisionBlockChunks {
        block_hash: B256,
        chunk_specs: Vec<ChunkRangeSpecifier>,
        response: oneshot::Sender<Result<(), Vec<(u32, u64)>>>,
    },
    /// Release chunks provisioned for a block after validation completes.
    ReleaseBlockChunks { block_hash: B256 },
}
```

- [ ] **Step 4: Add dashmap to crates/types/Cargo.toml**

Check and add: `dashmap = { workspace = true }`

- [ ] **Step 5: Verify compilation**

Run: `cargo check -p irys-types`

Will fail downstream — expected at this stage.

- [ ] **Step 6: Commit**

```
feat(types): add ChunkDataIndex type; simplify PdChunkMessage to 4 variants
```

---

### Task 2: Simplify ProvisioningTracker — remove TTL, Lock/Unlock, is_ready

**Files:**
- Modify: `crates/actors/src/pd_service/provisioning.rs`

- [ ] **Step 1: Read the file**

Read `crates/actors/src/pd_service/provisioning.rs` to confirm current state.

- [ ] **Step 2: Remove Locked from ProvisioningState**

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvisioningState {
    /// Fetching chunks from local storage (+ future gossip).
    Provisioning,
    /// All chunks available in cache.
    Ready,
    /// Some chunks unavailable (P2P not yet implemented).
    PartiallyReady { found: usize, total: usize },
}
```

- [ ] **Step 3: Remove TTL fields from TxProvisioningState**

```rust
pub struct TxProvisioningState {
    pub tx_hash: B256,
    pub required_chunks: HashSet<ChunkKey>,
    pub missing_chunks: HashSet<ChunkKey>,
    pub state: ProvisioningState,
    pub started_at: Instant,
}
```

Remove `expire_height` field and `TTL_BLOCKS` constant.

- [ ] **Step 4: Simplify register — no TTL parameter**

```rust
pub fn register(
    &mut self,
    tx_hash: B256,
    required_chunks: HashSet<ChunkKey>,
) -> &mut TxProvisioningState {
    self.txs
        .entry(tx_hash)
        .or_insert_with(|| TxProvisioningState {
            tx_hash,
            required_chunks,
            missing_chunks: HashSet::new(),
            state: ProvisioningState::Provisioning,
            started_at: Instant::now(),
        })
}
```

- [ ] **Step 5: Remove methods that are no longer needed**

Delete:
- `is_ready` — replaced by `ready_pd_txs` DashSet
- `lock` — no Lock message
- `unlock` — no Unlock message
- `expire_at_height` — pool owns tx liveness

Keep: `new`, `get`, `get_mut`, `remove`, `len`, `is_empty`, `register`.

- [ ] **Step 6: Update tests**

Remove tests for deleted methods:
- `test_is_ready_unknown_tx` — delete
- `test_lock_unlock_flow` — delete
- `test_expire_skips_locked` — delete

Update `test_register_and_query`:
```rust
#[test]
fn test_register_and_query() {
    let mut tracker = ProvisioningTracker::new();
    let tx = B256::ZERO;
    let chunks = HashSet::from([make_key(1), make_key(2)]);

    tracker.register(tx, chunks.clone());

    let state = tracker.get(&tx).unwrap();
    assert_eq!(state.state, ProvisioningState::Provisioning);
    assert_eq!(state.required_chunks, chunks);
}
```

Update `test_remove` — remove `current_height` param from `register`:
```rust
#[test]
fn test_remove() {
    let mut tracker = ProvisioningTracker::new();
    let tx = B256::ZERO;
    tracker.register(tx, HashSet::from([make_key(1)]));

    let removed = tracker.remove(&tx);
    assert!(removed.is_some());
    assert!(tracker.get(&tx).is_none());
}
```

- [ ] **Step 7: Verify compilation**

Run: `cargo check -p irys-actors`

- [ ] **Step 8: Commit**

```
refactor(provisioning): remove TTL, Lock/Unlock, is_ready — pool owns tx liveness
```

---

### Task 3: Simplify ChunkCache — remove lock_count, add shared index

**Files:**
- Modify: `crates/actors/src/pd_service/cache.rs`

- [ ] **Step 1: Read the file**

Read `crates/actors/src/pd_service/cache.rs` to confirm current state.

- [ ] **Step 2: Remove lock_count from CachedChunkEntry**

```rust
pub struct CachedChunkEntry {
    pub data: Arc<Bytes>,
    pub referencing_txs: HashSet<B256>,
    pub cached_at: Instant,
}
```

Update the `insert` method to not set `lock_count: 0`.

- [ ] **Step 3: Delete lock_chunks and unlock_chunks methods**

Remove both methods entirely.

- [ ] **Step 4: Update make_room_for_insert — no lock_count check**

Change the eviction filter from:
```rust
.find(|(_, entry)| entry.referencing_txs.is_empty() && entry.lock_count == 0)
```
to:
```rust
.find(|(_, entry)| entry.referencing_txs.is_empty())
```

- [ ] **Step 5: Update remove_reference — no lock_count check**

Change from:
```rust
entry.referencing_txs.is_empty() && entry.lock_count == 0
```
to:
```rust
entry.referencing_txs.is_empty()
```

- [ ] **Step 6: Add ChunkDataIndex field to ChunkCache**

```rust
pub struct ChunkCache {
    chunks: LruCache<ChunkKey, CachedChunkEntry>,
    /// Shared index for lock-free chunk reads. Mirrors data stored in the LRU.
    shared_index: irys_types::chunk_provider::ChunkDataIndex,
}
```

- [ ] **Step 7: Update constructors**

```rust
pub fn new(capacity: NonZeroUsize, shared_index: irys_types::chunk_provider::ChunkDataIndex) -> Self {
    Self {
        chunks: LruCache::new(capacity),
        shared_index,
    }
}

pub fn with_default_capacity(shared_index: irys_types::chunk_provider::ChunkDataIndex) -> Self {
    Self::new(
        NonZeroUsize::new(DEFAULT_CACHE_CAPACITY)
            .expect("DEFAULT_CACHE_CAPACITY must be non-zero"),
        shared_index,
    )
}
```

- [ ] **Step 8: Mirror inserts into shared index**

In the `insert` method, after `self.chunks.push(key, ...)`:

```rust
// Mirror into shared index for lock-free reads
self.shared_index.insert((key.ledger, key.offset), data);
```

Note: `data` is `Arc<Bytes>`, already cloned into the `CachedChunkEntry`. Pass a clone to the shared index. Ensure `data` is available at this point (before it's moved into the entry, or clone it).

- [ ] **Step 9: Mirror removals from shared index**

In `remove` method:

```rust
pub fn remove(&mut self, key: &ChunkKey) {
    self.chunks.pop(key);
    self.shared_index.remove(&(key.ledger, key.offset));
}
```

- [ ] **Step 10: Mirror LRU evictions in make_room_for_insert**

At the point where an evictable entry is popped, add:

```rust
self.shared_index.remove(&(key.ledger, key.offset));
```

- [ ] **Step 11: Update all cache.rs tests**

Every `ChunkCache` constructor call needs the new parameter. Add to test module:

```rust
use dashmap::DashMap;
use std::sync::Arc;
```

Replace all `ChunkCache::with_default_capacity()` with:
```rust
let shared_index: irys_types::chunk_provider::ChunkDataIndex = Arc::new(DashMap::new());
let mut cache = ChunkCache::with_default_capacity(shared_index);
```

Replace all `ChunkCache::new(cap)` with:
```rust
let shared_index: irys_types::chunk_provider::ChunkDataIndex = Arc::new(DashMap::new());
let mut cache = ChunkCache::new(cap, shared_index);
```

Remove `test_lock_prevents_eviction` test (lock_count no longer exists).

- [ ] **Step 12: Verify compilation**

Run: `cargo check -p irys-actors --tests`

- [ ] **Step 13: Commit**

```
refactor(cache): remove lock_count, add shared ChunkDataIndex mirroring
```

---

### Task 4: Update PdService — write ready_pd_txs directly, remove block_state_rx

**Files:**
- Modify: `crates/actors/src/pd_service.rs`

- [ ] **Step 1: Read the file**

Read `crates/actors/src/pd_service.rs` to confirm current state.

- [ ] **Step 2: Update PdService struct**

```rust
pub struct PdService {
    shutdown: Shutdown,
    msg_rx: PdChunkReceiver,
    cache: ChunkCache,
    tracker: ProvisioningTracker,
    storage_provider: Arc<dyn RethChunkProvider>,
    block_tracker: HashMap<B256, Vec<ChunkKey>>,
    /// Shared set of ready PD tx hashes. Written on provision/release.
    ready_pd_txs: Arc<dashmap::DashSet<B256>>,
}
```

Removed: `block_state_rx`, `current_height`.
Added: `ready_pd_txs`.

- [ ] **Step 3: Update spawn_service**

```rust
pub fn spawn_service(
    msg_rx: PdChunkReceiver,
    storage_provider: Arc<dyn RethChunkProvider>,
    runtime_handle: tokio::runtime::Handle,
    chunk_data_index: irys_types::chunk_provider::ChunkDataIndex,
    ready_pd_txs: Arc<dashmap::DashSet<B256>>,
) -> TokioServiceHandle {
    let (shutdown_signal, shutdown) = reth::tasks::shutdown::signal();

    let service = Self {
        shutdown,
        msg_rx,
        cache: ChunkCache::with_default_capacity(chunk_data_index),
        tracker: ProvisioningTracker::new(),
        storage_provider,
        block_tracker: HashMap::new(),
        ready_pd_txs,
    };

    let join_handle = runtime_handle.spawn(
        async move {
            service.start().await;
        }
        .in_current_span(),
    );

    TokioServiceHandle {
        name: "pd_service".to_string(),
        handle: join_handle,
        shutdown_signal,
    }
}
```

Removed: `block_state_rx` parameter.
Added: `chunk_data_index`, `ready_pd_txs` parameters.

- [ ] **Step 4: Simplify the start loop — remove block_state_rx arm**

```rust
async fn start(mut self) {
    info!("PdService started");

    loop {
        tokio::select! {
            biased;

            _ = &mut self.shutdown => {
                info!("PdService received shutdown signal");
                break;
            }

            msg = self.msg_rx.recv() => {
                match msg {
                    Some(message) => self.handle_message(message),
                    None => {
                        info!("PdService message channel closed");
                        break;
                    }
                }
            }
        }
    }

    info!("PdService stopped");
}
```

Remove the entire `block_state_rx` select arm and the `handle_block_state_update` method.

- [ ] **Step 5: Simplify handle_message — 5 variants (GetChunk stays until Phase 2)**

```rust
fn handle_message(&mut self, msg: PdChunkMessage) {
    match msg {
        PdChunkMessage::NewTransaction {
            tx_hash,
            chunk_specs,
        } => {
            self.handle_provision_chunks(tx_hash, chunk_specs);
        }
        PdChunkMessage::TransactionRemoved { tx_hash } => {
            self.handle_release_chunks(&tx_hash);
        }
        PdChunkMessage::GetChunk {
            ledger,
            offset,
            response,
        } => {
            let chunk = self.handle_get_chunk(ledger, offset);
            let _ = response.send(chunk);
        }
        PdChunkMessage::ProvisionBlockChunks {
            block_hash,
            chunk_specs,
            response,
        } => {
            self.handle_provision_block_chunks(block_hash, chunk_specs, response);
        }
        PdChunkMessage::ReleaseBlockChunks { block_hash } => {
            self.handle_release_block_chunks(&block_hash);
        }
    }
}
```

- [ ] **Step 6: Update handle_provision_chunks — write ready_pd_txs, no TTL**

At the `tracker.register` call, remove the `self.current_height` parameter:
```rust
let tx_state = self.tracker.register(tx_hash, required_chunks.clone());
```

After the state is set (after line ~297), add:
```rust
// Update shared readiness index
if tx_state.state == ProvisioningState::Ready {
    self.ready_pd_txs.insert(tx_hash);
}
```

- [ ] **Step 7: Update handle_release_chunks — remove from ready_pd_txs**

Add at the beginning of the method (before the `if let` block):
```rust
fn handle_release_chunks(&mut self, tx_hash: &B256) {
    // Remove from readiness set
    self.ready_pd_txs.remove(tx_hash);

    if let Some(tx_state) = self.tracker.remove(tx_hash) {
```

Also remove the `if tx_state.state == ProvisioningState::Locked` block (no Locked state).

The method becomes:
```rust
fn handle_release_chunks(&mut self, tx_hash: &B256) {
    self.ready_pd_txs.remove(tx_hash);

    if let Some(tx_state) = self.tracker.remove(tx_hash) {
        let mut evicted = 0;
        for key in &tx_state.required_chunks {
            let unreferenced = self.cache.remove_reference(key, tx_hash);
            if unreferenced {
                self.cache.remove(key);
                evicted += 1;
            }
        }

        trace!(
            tx_hash = %tx_hash,
            evicted_chunks = evicted,
            remaining_cached = self.cache.len(),
            "PD transaction removed, references decremented"
        );
    }
}
```

- [ ] **Step 8: Delete handle_is_ready, handle_lock, handle_unlock, handle_block_state_update**

Delete these four methods. **Keep `handle_get_chunk`** — still used by `GetChunk` message until Phase 2. Remove the `broadcast` import if no longer used.

- [ ] **Step 9: Update imports**

Add: `use dashmap::DashSet;`
Remove: `use crate::block_tree_service::BlockStateUpdated;` and `use tokio::sync::broadcast;`

- [ ] **Step 10: Update test_service helper**

```rust
fn test_service() -> PdService {
    let (_tx, rx) = mpsc::unbounded_channel();
    let provider = Arc::new(MockChunkProvider::new());
    let (_, shutdown) = reth::tasks::shutdown::signal();
    let chunk_data_index: irys_types::chunk_provider::ChunkDataIndex =
        Arc::new(dashmap::DashMap::new());
    let ready_pd_txs = Arc::new(dashmap::DashSet::new());
    PdService {
        shutdown,
        msg_rx: rx,
        cache: ChunkCache::with_default_capacity(chunk_data_index),
        tracker: ProvisioningTracker::new(),
        storage_provider: provider,
        block_tracker: HashMap::new(),
        ready_pd_txs,
    }
}
```

Add `use dashmap::DashMap;` in the test module.

- [ ] **Step 11: Update existing tests**

Change `let mut service = test_service();` in all tests (no destructuring needed — no second return value).

- [ ] **Step 12: Add test for ready_pd_txs and ChunkDataIndex population**

```rust
#[test]
fn test_provisioning_populates_ready_set_and_chunk_index() {
    let mut service = test_service();
    let ready_set = service.ready_pd_txs.clone();
    let tx_hash = B256::with_last_byte(0x01);

    let specs = vec![ChunkRangeSpecifier {
        partition_index: Default::default(),
        offset: 0,
        chunk_count: 3,
    }];

    service.handle_provision_chunks(tx_hash, specs);

    // Check tx is marked ready
    assert!(ready_set.contains(&tx_hash));

    // Release — should remove from ready set
    service.handle_release_chunks(&tx_hash);
    assert!(!ready_set.contains(&tx_hash));
}
```

Note: To verify chunk data index population, either add a `pub(crate)` getter on `ChunkCache` for the `shared_index`, or verify via the `cache.contains()` method (which tests the LRU) plus a separate index check. The simplest approach: make `shared_index` `pub(crate)` on `ChunkCache`.

- [ ] **Step 13: Verify compilation**

Run: `cargo check -p irys-actors --tests`

- [ ] **Step 14: Run tests**

Run: `cargo nextest run -p irys-actors`

- [ ] **Step 15: Commit**

```
feat(pd-service): write ready_pd_txs directly; remove TTL, lock, block_state subscription
```

---

### Task 5: Rewrite mempool monitor to be event-driven

**Files:**
- Modify: `crates/irys-reth/src/mempool.rs`

- [ ] **Step 1: Read the file**

Read `crates/irys-reth/src/mempool.rs` to confirm current state.

- [ ] **Step 2: Add ready_pd_txs to IrysPoolBuilder**

```rust
#[derive(Clone)]
#[non_exhaustive]
pub struct IrysPoolBuilder {
    hardfork_config: Arc<IrysHardforkConfig>,
    pd_chunk_sender: PdChunkSender,
    /// Shared set of PD tx hashes whose chunks are fully provisioned.
    /// Written by PdService, read by the payload builder.
    ready_pd_txs: Arc<dashmap::DashSet<B256>>,
}
```

Update Debug impl to include `ready_pd_txs: "<DashSet>"`.

- [ ] **Step 3: Update constructors**

```rust
pub fn new(
    hardfork_config: Arc<IrysHardforkConfig>,
    pd_chunk_sender: PdChunkSender,
    ready_pd_txs: Arc<dashmap::DashSet<B256>>,
) -> Self {
    Self {
        hardfork_config,
        pd_chunk_sender,
        ready_pd_txs,
    }
}
```

Remove `with_pd_chunk_sender` method if no longer used (check callers).

- [ ] **Step 4: Update the monitor spawn call in build_pool**

The monitor signature does NOT change (still 4 params). Only the `IrysPoolBuilder` constructor changes. Update the spawn call to match:

```rust
ctx.task_executor()
    .spawn_critical_with_graceful_shutdown_signal(
        "pd transaction monitoring task",
        |shutdown| pd_transaction_monitor(pool_clone, sender, hardfork_clone, shutdown),
    );
```

- [ ] **Step 5: Rewrite pd_transaction_monitor to be event-driven**

Replace the entire function body:

```rust
async fn pd_transaction_monitor<P>(
    pool: P,
    chunk_sender: PdChunkSender,
    hardfork_config: Arc<IrysHardforkConfig>,
    mut shutdown: GracefulShutdown,
) where
    P: TransactionPool,
    P::Transaction: EthPoolTransaction,
{
    use reth_transaction_pool::TransactionListenerKind;

    let mut known_pd_txs: HashSet<B256> = HashSet::new();
    let mut new_tx_listener =
        pool.new_transactions_listener_for(TransactionListenerKind::All);
    let mut all_events = pool.all_transactions_event_listener();

    debug!(target: "reth::pd", "PD transaction monitor started (event-driven)");

    loop {
        tokio::select! {
            biased;

            _ = &mut shutdown => {
                debug!(target: "reth::pd", "PD transaction monitor received shutdown signal");
                break;
            }

            // New transaction entered the pool
            Some(event) = new_tx_listener.recv() => {
                let now_secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let timestamp = irys_types::UnixTimestamp::from_secs(now_secs);
                if !hardfork_config.is_sprite_active(timestamp) {
                    continue;
                }

                let tx = &event.transaction;
                let tx_hash = *tx.hash();

                if let Ok(Some(_)) =
                    crate::pd_tx::detect_and_decode_pd_header(tx.transaction.input())
                {
                    if known_pd_txs.insert(tx_hash) {
                        if let Some(access_list) = tx.transaction.access_list() {
                            let chunk_specs =
                                crate::pd_tx::extract_pd_chunk_specs(access_list);
                            if !chunk_specs.is_empty() {
                                trace!(
                                    target: "reth::pd",
                                    tx_hash = %tx_hash,
                                    chunk_specs_count = chunk_specs.len(),
                                    "Detected new PD transaction, sending for provisioning"
                                );
                                let _ = chunk_sender.send(
                                    irys_types::chunk_provider::PdChunkMessage::NewTransaction {
                                        tx_hash,
                                        chunk_specs,
                                    },
                                );
                            }
                        }
                    }
                }
            }

            // Transaction lifecycle events (removals)
            Some(event) = all_events.next() => {
                use reth_transaction_pool::FullTransactionEvent;
                let removed_hash = match event {
                    FullTransactionEvent::Discarded(h)
                    | FullTransactionEvent::Invalid(h)
                    | FullTransactionEvent::Mined { tx_hash: h, .. } => Some(h),
                    FullTransactionEvent::Replaced { transaction, .. } => {
                        Some(*transaction.hash())
                    }
                    _ => None,
                };
                if let Some(tx_hash) = removed_hash {
                    if known_pd_txs.remove(&tx_hash) {
                        trace!(
                            target: "reth::pd",
                            tx_hash = %tx_hash,
                            "PD transaction removed from pool, notifying chunk manager"
                        );
                        let _ = chunk_sender.send(
                            irys_types::chunk_provider::PdChunkMessage::TransactionRemoved {
                                tx_hash,
                            },
                        );
                    }
                }
            }
        }
    }

    debug!(target: "reth::pd", "PD transaction monitor stopped");
}
```

Note: The monitor does NOT write `ready_pd_txs` — PdService does that directly. The `ready_pd_txs` field on `IrysPoolBuilder` exists only to thread it through to the payload builder via `components()`.

- [ ] **Step 6: Add dependencies to crates/irys-reth/Cargo.toml**

`futures` is currently only in `[dev-dependencies]`. Move it to `[dependencies]`:
```toml
[dependencies]
futures = { workspace = true }
dashmap = { workspace = true }
```

(`dashmap` is needed for `DashSet` in `IrysPoolBuilder` and other structs in `irys-reth`.)

- [ ] **Step 7: Add necessary imports in mempool.rs**

```rust
use futures::StreamExt as _;  // for all_events.next()
```

Remove unused imports: `Duration` (no more POLL_INTERVAL/CLEANUP_INTERVAL).

- [ ] **Step 8: Verify compilation**

Run: `cargo check -p irys-reth --tests`

- [ ] **Step 9: Commit**

```
feat(mempool): event-driven monitor with pool listeners instead of polling
```

---

### Task 6: Update payload builder to use ready_pd_txs

**Files:**
- Modify: `crates/irys-reth/src/payload.rs`

- [ ] **Step 1: Read the file**

Read `crates/irys-reth/src/payload.rs` to confirm current state.

- [ ] **Step 2: Add ready_pd_txs to IrysPayloadBuilder**

```rust
pub struct IrysPayloadBuilder<Pool, Client, EvmConfig = EthEvmConfig> {
    client: Client,
    pool: Pool,
    evm_config: EvmConfig,
    builder_config: EthereumBuilderConfig,
    max_pd_chunks_per_block: u64,
    hardforks: Arc<IrysHardforkConfig>,
    pd_chunk_sender: PdChunkSender,
    /// Shared set of PD tx hashes whose chunks are ready. Written by PdService.
    ready_pd_txs: Arc<dashmap::DashSet<revm_primitives::B256>>,
}
```

Update `new()` to accept and store it. Update `Debug` impl.

- [ ] **Step 3: Add ready_pd_txs to CombinedTransactionIterator**

```rust
pub struct CombinedTransactionIterator {
    shadow: ShadowTxQueue,
    pd_budget: PdChunkBudget,
    pool_iter: BestTransactionsIter,
    is_sprite_active: bool,
    pd_chunk_sender: PdChunkSender,
    /// Shared set of ready PD tx hashes. Lock-free reads replace blocking IsReady calls.
    ready_pd_txs: Arc<dashmap::DashSet<revm_primitives::B256>>,
}
```

Update `new()` to accept and store it.

- [ ] **Step 4: Replace blocking IsReady with ready_pd_txs.contains()**

Replace the IsReady block at `payload.rs:559-582`:

```rust
            // For PD transactions, check if chunks are ready
            if chunks > 0 {
                let tx_hash = revm_primitives::B256::from_slice(tx.hash().as_slice());

                // Lock-free readiness check via shared DashSet.
                // PdService populates this when provisioning completes.
                if !self.ready_pd_txs.contains(&tx_hash) {
                    tracing::debug!(
                        tx_hash = %tx_hash,
                        "Skipping PD transaction: chunks not ready"
                    );
                    continue; // Graceful skip — does NOT mark_invalid, preserves sender ordering
                }

                // Check PD budget
                if !self.pd_budget.try_consume(&tx, chunks) {
                    self.pd_budget.defer(tx);
                    continue;
                }

                return Some(tx);
            }
```

- [ ] **Step 5: Update best_transactions_with_attributes to pass ready_pd_txs**

```rust
Box::new(CombinedTransactionIterator::new(
    timestamp,
    shadow_txs,
    pool_txs,
    self.max_pd_chunks_per_block,
    is_sprite_active,
    self.pd_chunk_sender.clone(),
    self.ready_pd_txs.clone(),
))
```

- [ ] **Step 6: Update payload.rs test**

```rust
use dashmap::DashSet;

let ready_pd_txs: Arc<DashSet<revm_primitives::B256>> = Arc::new(DashSet::new());
let pd_small_b256 = revm_primitives::B256::from_slice(pd_small_hash.as_slice());
let pd_large_b256 = revm_primitives::B256::from_slice(pd_large_hash.as_slice());
ready_pd_txs.insert(pd_small_b256);
ready_pd_txs.insert(pd_large_b256);

let mut iterator = CombinedTransactionIterator::new(
    timestamp,
    vec![shadow_tx],
    pool_iter,
    7_500,
    true,
    pd_chunk_sender,
    ready_pd_txs,
);
```

- [ ] **Step 7: Remove unused imports**

Remove `PdChunkMessage` import from payload.rs. Remove `tokio::sync::oneshot` if no longer needed.

- [ ] **Step 8: Verify compilation**

Run: `cargo check -p irys-reth --tests`

- [ ] **Step 9: Commit**

```
feat(payload): replace blocking IsReady with lock-free ready_pd_txs check
```

---

### Task 7: Thread new types through wiring layers

**Files:**
- Modify: `crates/irys-reth/src/payload_builder_builder.rs`
- Modify: `crates/irys-reth/src/lib.rs`
- Modify: `crates/reth-node-bridge/src/node.rs`
- Modify: `crates/chain/src/chain.rs`
- Modify: `crates/chain/Cargo.toml`

- [ ] **Step 1: Add ready_pd_txs to IrysPayloadBuilderBuilder**

```rust
pub struct IrysPayloadBuilderBuilder {
    pub max_pd_chunks_per_block: u64,
    pub hardforks: Arc<IrysHardforkConfig>,
    pub pd_chunk_sender: PdChunkSender,
    pub ready_pd_txs: Arc<dashmap::DashSet<revm_primitives::B256>>,
}
```

Update Debug impl. Update `build_payload_builder` to pass `self.ready_pd_txs` to `IrysPayloadBuilder::new`.

- [ ] **Step 2: Add fields to IrysEthereumNode**

```rust
pub struct IrysEthereumNode {
    pub max_pd_chunks_per_block: u64,
    pub chunk_provider: Arc<dyn irys_types::chunk_provider::RethChunkProvider>,
    pub hardfork_config: Arc<irys_types::hardfork_config::IrysHardforkConfig>,
    pub pd_chunk_sender: irys_types::chunk_provider::PdChunkSender,
    pub ready_pd_txs: Arc<dashmap::DashSet<revm_primitives::B256>>,
    pub chunk_data_index: irys_types::chunk_provider::ChunkDataIndex,
}
```

- [ ] **Step 3: Update components()**

```rust
let pool_builder = IrysPoolBuilder::new(
    self.hardfork_config.clone(),
    self.pd_chunk_sender.clone(),
    self.ready_pd_txs.clone(),
);

// ... in executor:
.executor(IrysExecutorBuilder {
    chunk_provider: self.chunk_provider.clone(),
    hardfork_config: self.hardfork_config.clone(),
    pd_chunk_sender: self.pd_chunk_sender.clone(),
})

// ... in payload:
.payload(IyrsPayloadServiceBuilder::new(IrysPayloadBuilderBuilder {
    max_pd_chunks_per_block: self.max_pd_chunks_per_block,
    hardforks: self.hardfork_config.clone(),
    pd_chunk_sender: self.pd_chunk_sender.clone(),
    ready_pd_txs: self.ready_pd_txs.clone(),
}))
```

Note: `IrysExecutorBuilder` still has `pd_chunk_sender` at this stage. Phase 2 replaces it with `chunk_data_index`.

- [ ] **Step 4: Update IrysPoolBuilder::new call**

The `IrysPoolBuilder::new` now takes 3 params. Update the call in `components()`.

- [ ] **Step 5: Update run_node in reth-node-bridge**

```rust
pub async fn run_node(
    chainspec: Arc<ChainSpec>,
    task_executor: TaskExecutor,
    node_config: irys_types::NodeConfig,
    latest_block: u64,
    random_ports: bool,
    chunk_provider: Arc<dyn irys_types::chunk_provider::RethChunkProvider>,
    pd_chunk_sender: irys_types::chunk_provider::PdChunkSender,
    ready_pd_txs: Arc<dashmap::DashSet<revm_primitives::B256>>,
    chunk_data_index: irys_types::chunk_provider::ChunkDataIndex,
) -> eyre::Result<(RethNodeHandle, IrysRethNodeAdapter)> {
```

Update the `IrysEthereumNode` construction at ~line 232:
```rust
.node(IrysEthereumNode {
    max_pd_chunks_per_block,
    chunk_provider,
    hardfork_config: std::sync::Arc::new(hardfork_config.clone()),
    pd_chunk_sender,
    ready_pd_txs,
    chunk_data_index,
})
```

- [ ] **Step 6: Update start_reth_node in chain.rs**

Add `ready_pd_txs` and `chunk_data_index` parameters:
```rust
async fn start_reth_node(
    task_executor: TaskExecutor,
    chainspec: Arc<ChainSpec>,
    config: Config,
    latest_block: u64,
    chunk_provider: Arc<dyn irys_types::chunk_provider::RethChunkProvider>,
    pd_chunk_sender: irys_types::chunk_provider::PdChunkSender,
    ready_pd_txs: Arc<dashmap::DashSet<revm_primitives::B256>>,
    chunk_data_index: irys_types::chunk_provider::ChunkDataIndex,
) -> eyre::Result<(RethNodeHandle, RethNode)> {
```

Pass through to `run_node`.

- [ ] **Step 7: Create shared state in chain.rs init_services area**

Near line 1139-1141:

```rust
let (pd_chunk_tx, pd_chunk_rx) = tokio::sync::mpsc::unbounded_channel();
let chunk_data_index: irys_types::chunk_provider::ChunkDataIndex =
    std::sync::Arc::new(dashmap::DashMap::with_capacity(16_384));
let ready_pd_txs: Arc<dashmap::DashSet<revm_primitives::B256>> =
    std::sync::Arc::new(dashmap::DashSet::new());
```

Pass `chunk_data_index.clone()` and `ready_pd_txs.clone()` to `start_reth_node`.
Pass same to `init_services` for PdService spawn.

- [ ] **Step 8: Update init_services signature**

Remove `pd_chunk_rx` and `pd_chunk_sender` individual params if they're replaced, or add `chunk_data_index` and `ready_pd_txs` alongside. Follow the existing pattern.

- [ ] **Step 9: Update PdService::spawn_service call**

At chain.rs ~line 1816:
```rust
let pd_service_handle = irys_actors::pd_service::PdService::spawn_service(
    pd_chunk_rx,
    chunk_provider.clone(),
    runtime_handle.clone(),
    chunk_data_index.clone(),
    ready_pd_txs.clone(),
);
```

Note: no more `service_senders.subscribe_block_state_updates()` parameter.

- [ ] **Step 10: Add dashmap to crates/chain/Cargo.toml**

```toml
dashmap = { workspace = true }
```

- [ ] **Step 11: Update IrysEthereumNode test construction (lib.rs ~line 3353)**

```rust
IrysEthereumNode {
    max_pd_chunks_per_block: 7_500,
    chunk_provider: chunk_provider.clone(),
    hardfork_config: std::sync::Arc::new(
        irys_types::config::ConsensusConfig::testing()
            .hardforks
            .clone(),
    ),
    pd_chunk_sender,
    ready_pd_txs: std::sync::Arc::new(dashmap::DashSet::new()),
    chunk_data_index: std::sync::Arc::new(dashmap::DashMap::new()),
}
```

Search for all `IrysEthereumNode {` constructions: `grep -rn "IrysEthereumNode {" crates/ --include="*.rs"` and update all sites.

- [ ] **Step 12: Verify full workspace compilation**

Run: `cargo check --workspace --tests`

- [ ] **Step 13: Run tests**

Run: `cargo nextest run -p irys-types -p irys-actors -p irys-reth`

- [ ] **Step 14: Commit**

```
feat: wire ChunkDataIndex and ready_pd_txs through all layers
```

---

### Task 8: Phase 1 verification

- [ ] **Step 1: Run clippy**

Run: `cargo clippy --workspace --tests --all-targets`

- [ ] **Step 2: Run fmt**

Run: `cargo fmt --all`

- [ ] **Step 3: Commit fixes if any**

```
chore: clippy and fmt fixes for Phase 1
```

---

## Chunk 2: Phase 2 — ChunkDataIndex for lock-free precompile reads

### Task 9: Update PdContext to use ChunkDataIndex

**Files:**
- Modify: `crates/irys-reth/src/precompiles/pd/context.rs`

- [ ] **Step 1: Read the file**

Read `crates/irys-reth/src/precompiles/pd/context.rs` to confirm current state.

- [ ] **Step 2: Update ChunkSource::Manager to hold ChunkDataIndex**

```rust
enum ChunkSource {
    /// Fetch chunks via shared index (payload building with cached chunks).
    Manager {
        chunk_data_index: irys_types::chunk_provider::ChunkDataIndex,
    },
    /// Fetch chunks directly from storage (for block validation).
    Storage(Arc<dyn RethChunkProvider>),
}
```

Update the `Clone` impl — `ChunkDataIndex` is `Arc<DashMap>` so `.clone()` works.

- [ ] **Step 3: Update PdContext::new_with_manager**

```rust
pub fn new_with_manager(
    chunk_config: ChunkConfig,
    chunk_data_index: irys_types::chunk_provider::ChunkDataIndex,
) -> Self {
    Self {
        tx_hash: Arc::new(RwLock::new(None)),
        access_list: Arc::new(RwLock::new(Vec::new())),
        chunk_source: ChunkSource::Manager { chunk_data_index },
        chunk_config,
    }
}
```

- [ ] **Step 4: Replace get_chunk Manager path with DashMap read**

```rust
pub fn get_chunk(&self, ledger: u32, offset: u64) -> eyre::Result<Option<Bytes>> {
    match &self.chunk_source {
        ChunkSource::Manager { chunk_data_index } => {
            match chunk_data_index.get(&(ledger, offset)) {
                Some(data) => Ok(Some((**data).clone())),
                None => {
                    tracing::warn!(ledger, offset, "Chunk not found in shared index");
                    Ok(None)
                }
            }
        }
        ChunkSource::Storage(provider) => {
            provider.get_unpacked_chunk_by_ledger_offset(ledger, offset)
        }
    }
}
```

- [ ] **Step 5: Remove unused imports from context.rs**

Remove `PdChunkMessage` and `PdChunkSender` from context.rs imports.

- [ ] **Step 6: Remove GetChunk from PdChunkMessage (now safe)**

In `crates/types/src/chunk_provider.rs`, remove the `GetChunk` variant:
```rust
    GetChunk {
        ledger: u32,
        offset: u64,
        response: oneshot::Sender<Option<Arc<Bytes>>>,
    },
```

If `oneshot` is no longer used by any remaining variant, remove the `use tokio::sync::oneshot;` import. (Check: `ProvisionBlockChunks` still uses `oneshot::Sender` — so keep the import.)

- [ ] **Step 7: Remove handle_get_chunk from PdService and its match arm**

In `crates/actors/src/pd_service.rs`:
- Delete the `handle_get_chunk` method
- Remove the `GetChunk` match arm from `handle_message`

- [ ] **Step 8: Verify compilation**

Run: `cargo check -p irys-types -p irys-actors -p irys-reth`

- [ ] **Step 9: Commit**

```
feat(pd-context): replace blocking GetChunk channel with lock-free ChunkDataIndex reads
```

---

### Task 10: Thread ChunkDataIndex through EVM factory

**Files:**
- Modify: `crates/irys-reth/src/evm.rs`
- Modify: `crates/irys-reth/src/payload_builder_builder.rs`
- Modify: `crates/irys-reth/src/lib.rs`

- [ ] **Step 1: Replace pd_chunk_sender with chunk_data_index in IrysEvmFactory**

```rust
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct IrysEvmFactory {
    context: PdContext,
    hardfork_config: Arc<irys_types::hardfork_config::IrysHardforkConfig>,
    /// When set, creates Manager-mode PdContext for payload building.
    chunk_data_index: Option<irys_types::chunk_provider::ChunkDataIndex>,
}
```

- [ ] **Step 2: Update builder methods**

Replace `with_pd_chunk_sender` with `with_chunk_data_index`:

```rust
pub fn with_chunk_data_index(
    mut self,
    index: irys_types::chunk_provider::ChunkDataIndex,
) -> Self {
    self.chunk_data_index = Some(index);
    self
}
```

Update `new()` to set `chunk_data_index: None`.

- [ ] **Step 3: Update BOTH create_evm AND create_evm_with_inspector**

Both methods construct PdContext. Update both:

```rust
let pd_context = match &self.chunk_data_index {
    Some(index) => {
        PdContext::new_with_manager(self.context.chunk_config(), index.clone())
    }
    None => self.context.clone_for_new_evm(),
};
```

Search: `grep -n "new_with_manager\|pd_chunk_sender" crates/irys-reth/src/evm.rs` to find all call sites.

- [ ] **Step 4: Rename ConfigurePdChunkSender → ConfigureChunkDataIndex**

```rust
pub trait ConfigureChunkDataIndex: Sized {
    fn with_chunk_data_index(self, index: irys_types::chunk_provider::ChunkDataIndex) -> Self;
}

impl ConfigureChunkDataIndex for IrysEvmConfig {
    fn with_chunk_data_index(self, index: irys_types::chunk_provider::ChunkDataIndex) -> Self {
        Self::with_chunk_data_index(self, index)
    }
}
```

Update `IrysBlockExecutorFactory` — rename `with_pd_chunk_sender` to `with_chunk_data_index`.

Update `IrysEvmConfig` — rename the method that chains through to the factory.

- [ ] **Step 5: Update payload_builder_builder.rs**

Add `chunk_data_index` field to `IrysPayloadBuilderBuilder`:
```rust
pub struct IrysPayloadBuilderBuilder {
    pub max_pd_chunks_per_block: u64,
    pub hardforks: Arc<IrysHardforkConfig>,
    pub pd_chunk_sender: PdChunkSender,
    pub ready_pd_txs: Arc<dashmap::DashSet<revm_primitives::B256>>,
    pub chunk_data_index: irys_types::chunk_provider::ChunkDataIndex,
}
```

Update `build_payload_builder`:
```rust
let evm_config = evm_config.with_chunk_data_index(self.chunk_data_index.clone());
```

Update the trait bound: `+ ConfigureChunkDataIndex` instead of `+ ConfigurePdChunkSender`.

Update Debug impl.

- [ ] **Step 6: Update IrysExecutorBuilder in lib.rs**

Replace `pd_chunk_sender` with `chunk_data_index`:
```rust
pub struct IrysExecutorBuilder {
    chunk_provider: Arc<dyn irys_types::chunk_provider::RethChunkProvider>,
    hardfork_config: Arc<irys_types::hardfork_config::IrysHardforkConfig>,
    chunk_data_index: irys_types::chunk_provider::ChunkDataIndex,
}
```

Update `build_evm`:
```rust
let evm_factory = IrysEvmFactory::new(self.chunk_provider, self.hardfork_config)
    .with_chunk_data_index(self.chunk_data_index);
```

Update Debug impl.

- [ ] **Step 7: Update components() in lib.rs**

```rust
.executor(IrysExecutorBuilder {
    chunk_provider: self.chunk_provider.clone(),
    hardfork_config: self.hardfork_config.clone(),
    chunk_data_index: self.chunk_data_index.clone(),
})

.payload(IyrsPayloadServiceBuilder::new(IrysPayloadBuilderBuilder {
    max_pd_chunks_per_block: self.max_pd_chunks_per_block,
    hardforks: self.hardfork_config.clone(),
    pd_chunk_sender: self.pd_chunk_sender.clone(),
    ready_pd_txs: self.ready_pd_txs.clone(),
    chunk_data_index: self.chunk_data_index.clone(),
}))
```

- [ ] **Step 8: Verify compilation**

Run: `cargo check --workspace --tests`

- [ ] **Step 9: Commit**

```
feat(evm): replace PdChunkSender with ChunkDataIndex in EVM factory
```

---

### Task 11: Final verification and test updates

**Files:**
- Modify: `crates/irys-reth/tests/pd_context_isolation.rs`
- Any other test files that construct `PdContext`, `IrysEvmFactory`, or `IrysEthereumNode` directly

- [ ] **Step 1: Search for all direct constructions that need updating**

```
grep -rn "PdContext::new_with_manager\|IrysEvmFactory::new\|with_pd_chunk_sender\|ConfigurePdChunkSender" crates/ --include="*.rs"
```

Update each site to use the new signatures.

- [ ] **Step 2: Verify pd_context_isolation test**

Read `crates/irys-reth/tests/pd_context_isolation.rs`. This file uses `IrysEvmFactory::new()` (Storage mode), NOT `PdContext::new_with_manager`. It should compile without changes since `IrysEvmFactory::new()` signature is unchanged. Verify no changes needed.

- [ ] **Step 3: Run all unit tests**

Run: `cargo nextest run -p irys-types -p irys-actors -p irys-reth`

- [ ] **Step 4: Run integration tests**

Run: `cargo nextest run -p irys-chain`

- [ ] **Step 5: Run full CI checks**

Run: `cargo xtask local-checks`

- [ ] **Step 6: Commit fixes**

```
chore: fix tests and clippy for Phase 2
```

---

## Risk Notes

1. **DashMap memory**: The `ChunkDataIndex` mirrors the LRU cache — every insert/remove/eviction is reflected. Default LRU capacity is 16,384 entries. `Arc<Bytes>` chunk data is shared, not duplicated. Use `DashMap::with_capacity(16_384)` at creation to avoid rehashing.

2. **Pool as liveness authority**: The Reth pool decides when a tx is stale (eviction, replacement, mining). PdService never independently drops a tx. If the pool keeps a PD tx indefinitely, its chunks stay cached — bounded by the LRU size and the pool's own size limits.

3. **Event ordering**: `new_transactions_listener_for(All)` fires on pool insertion. The monitor sends `NewTransaction` to PdService, which provisions synchronously and writes `ready_pd_txs`. If the payload builder runs before provisioning completes, the PD tx is skipped (not ready yet) — correct behavior. It'll be included in the next block build.

4. **Mid-execution eviction**: If PdService processes a `TransactionRemoved` while EVM is mid-execution for the same tx, the DashMap entry is removed. However, any `Arc<Bytes>` already retrieved by the precompile via `.get()` remains valid (Arc keeps data alive). Only subsequent `.get()` calls for *other* chunks in the same tx could return `None`. This is safe — the tx was removed from the pool, so the EVM execution will be discarded anyway.

5. **BestTransactionFilter NOT used**: Reth's `BestTransactionFilter` calls `mark_invalid` which blocks ALL txs from the same sender. We intentionally use `continue` (graceful skip) in `CombinedTransactionIterator` to preserve sender ordering for mixed PD/non-PD senders.

6. **`FullTransactionEvent` import**: Verified in reth-irys fork at `crates/transaction-pool/src/pool/events.rs`. Variants: `Pending`, `Queued`, `Mined`, `Replaced`, `Discarded`, `Invalid`, `Propagated`. `AllTransactionsEvents<T>` implements `Stream<Item = FullTransactionEvent<T>>`.

7. **Lock/Unlock removal safety**: Confirmed no senders for `Lock`/`Unlock` exist outside the PdService handler code. They were wired but never called. The `Arc<Bytes>` pattern ensures data stays alive while any reference is held.
