# PD Chunk Lock-Free Indexes Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate all `blocking_recv()` calls in the PD payload building and EVM execution paths by replacing channel-based request-reply with shared concurrent data structures.

**Architecture:** A single `ChunkDataIndex` (`DashMap`) holds provisioned chunk bytes — PdService writes during provisioning, the PD precompile reads lock-free during EVM execution. Readiness tracking is pushed into the mempool monitor: PdService sends a callback notification when provisioning completes, the monitor maintains a `ready_pd_txs` (`DashSet`), and the payload builder's `CombinedTransactionIterator` reads it with a non-blocking `.contains()` check.

**Key constraint:** Reth's `BestTransactionFilter` calls `mark_invalid` which blocks ALL subsequent txs from the same sender — too aggressive for PD gating. The current `CombinedTransactionIterator` intentionally uses `continue` (skip without mark_invalid) to preserve sender ordering. This plan keeps that behavior.

**Tech Stack:** `dashmap 6.0` (already a workspace dependency), existing `tokio::sync::mpsc` channel (retained for NewTransaction/TransactionRemoved/Lock/Unlock).

---

## Architecture Diagram

```
                                   PdService (CL)
                                  ┌───────────────────────┐
  mempool monitor ──NewTx───────> │ provisions chunks     │
  (event-driven)                  │ writes to:            │
       ▲                          │   ChunkDataIndex      │──── DashMap<(u32,u64), Arc<Bytes>>
       │                          │ sends back:           │         │
       │ ◄──Ready(tx_hash)──────  │   callback channel    │         │  PD precompile reads
       │                          └───────────────────────┘         │  directly (no blocking)
       │                                                            │
       ▼                                                            │
  ready_pd_txs: DashSet<B256>                                      │
  (monitor maintains)                                               │
       │                                                            │
       ▼                                                            │
  CombinedTransactionIterator ──────────────────────────────────────┘
    .contains(&tx_hash) → skip if not ready (no mark_invalid!)
    PD precompile → .get(&(ledger, offset)) → Arc<Bytes>
```

## File Structure

### Modified files

| File | Responsibility |
|------|---------------|
| `crates/types/src/chunk_provider.rs` | Add `ChunkDataIndex` type, `PdReadyNotification` types; remove `IsReady` and `GetChunk` from `PdChunkMessage` |
| `crates/actors/src/pd_service/cache.rs` | Add `ChunkDataIndex` field to `ChunkCache`; mirror inserts/removes to shared index |
| `crates/actors/src/pd_service.rs` | Add `ChunkDataIndex` + ready notification sender; fire callback on provisioning complete; remove `handle_is_ready`/`handle_get_chunk` |
| `crates/irys-reth/src/mempool.rs` | Switch to event-driven; receive ready callbacks; maintain `ready_pd_txs: Arc<DashSet>` |
| `crates/irys-reth/src/payload.rs` | Replace blocking IsReady with `ready_pd_txs.contains()`; add `ready_pd_txs` field to `CombinedTransactionIterator` and `IrysPayloadBuilder` |
| `crates/irys-reth/src/precompiles/pd/context.rs` | Add `ChunkDataIndex` to `ChunkSource::Manager`; read directly instead of `GetChunk` channel |
| `crates/irys-reth/src/evm.rs` | Thread `ChunkDataIndex` through `IrysEvmFactory`; pass to `PdContext` |
| `crates/irys-reth/src/payload_builder_builder.rs` | Accept and forward `ready_pd_txs` and `ChunkDataIndex` |
| `crates/irys-reth/src/lib.rs` | Add new fields to `IrysEthereumNode`; thread through component builders |
| `crates/reth-node-bridge/src/node.rs` | Accept and forward new types to `IrysEthereumNode` |
| `crates/chain/src/chain.rs` | Create `ChunkDataIndex`, ready callback channel, `ready_pd_txs`; wire everything |

---

## Chunk 1: Phase 1 — Readiness via mempool monitor + ready callback

### Task 1: Add new types to irys-types

**Files:**
- Modify: `crates/types/src/chunk_provider.rs`

- [ ] **Step 1: Read the file**

Read `crates/types/src/chunk_provider.rs` to confirm current state.

- [ ] **Step 2: Add ChunkDataIndex type**

Add after the channel type aliases (after line 87):

```rust
use dashmap::DashMap;

/// Shared chunk data index for lock-free chunk reads during EVM execution.
/// PdService populates this during provisioning; the PD precompile reads it directly.
/// Keys use `(ledger: u32, offset: u64)` tuples matching `ChunkKey` semantics.
pub type ChunkDataIndex = Arc<DashMap<(u32, u64), Arc<Bytes>>>;

/// Notification sent from PdService when a transaction's chunk provisioning completes.
#[derive(Debug, Clone)]
pub struct PdReadyNotification {
    pub tx_hash: B256,
    pub is_ready: bool,
}

/// Channel for PdService to notify the mempool monitor about provisioning completion.
pub type PdReadySender = mpsc::UnboundedSender<PdReadyNotification>;
pub type PdReadyReceiver = mpsc::UnboundedReceiver<PdReadyNotification>;
```

- [ ] **Step 3: Remove IsReady and GetChunk variants from PdChunkMessage**

Remove from the enum:

```rust
    IsReady {
        tx_hash: B256,
        response: oneshot::Sender<bool>,
    },
```

and:

```rust
    GetChunk {
        ledger: u32,
        offset: u64,
        response: oneshot::Sender<Option<Arc<Bytes>>>,
    },
```

- [ ] **Step 4: Add dashmap to crates/types/Cargo.toml if needed**

Check and add: `dashmap = { workspace = true }`

- [ ] **Step 5: Verify compilation**

Run: `cargo check -p irys-types`

Will fail downstream — expected at this stage.

- [ ] **Step 6: Commit**

```
feat(types): add ChunkDataIndex, PdReadyNotification types; remove IsReady/GetChunk variants
```

---

### Task 2: Update PdService to send ready notifications and maintain ChunkDataIndex

**Files:**
- Modify: `crates/actors/src/pd_service/cache.rs:34-36,40-47,75-99,142,159-181`
- Modify: `crates/actors/src/pd_service.rs:25-34,38-69,116-157,211-306,309-333,335-368,371-390`

- [ ] **Step 1: Add ChunkDataIndex to ChunkCache**

At `cache.rs:34-36`:

```rust
pub struct ChunkCache {
    chunks: LruCache<ChunkKey, CachedChunkEntry>,
    /// Shared index for lock-free chunk reads. Mirrors data stored in the LRU.
    shared_index: irys_types::chunk_provider::ChunkDataIndex,
}
```

- [ ] **Step 2: Update ChunkCache constructors**

```rust
pub fn new(capacity: NonZeroUsize, shared_index: irys_types::chunk_provider::ChunkDataIndex) -> Self {
    Self {
        chunks: LruCache::new(capacity),
        shared_index,
    }
}

pub fn with_default_capacity(shared_index: irys_types::chunk_provider::ChunkDataIndex) -> Self {
    Self::new(
        NonZeroUsize::new(DEFAULT_CACHE_CAPACITY).unwrap(),
        shared_index,
    )
}
```

- [ ] **Step 3: Mirror inserts into shared index**

In `insert` method, after `self.chunks.put(key, entry)`:

```rust
// Mirror into shared index for lock-free reads
self.shared_index.insert((key.ledger, key.offset), data);
```

- [ ] **Step 4: Mirror removals from shared index**

In `remove` method:

```rust
pub fn remove(&mut self, key: &ChunkKey) {
    self.chunks.pop(key);
    self.shared_index.remove(&(key.ledger, key.offset));
}
```

- [ ] **Step 5: Mirror LRU evictions in make_room_for_insert**

Read `make_room_for_insert` first. At the point where an evictable entry is popped, add:

```rust
self.shared_index.remove(&(key.ledger, key.offset));
```

- [ ] **Step 6: Update all cache.rs tests**

All `ChunkCache` constructors in cache.rs tests (search: `grep -n "with_default_capacity\|ChunkCache::new(" crates/actors/src/pd_service/cache.rs`) need the new parameter:

```rust
let shared_index: irys_types::chunk_provider::ChunkDataIndex = Arc::new(DashMap::new());
let mut cache = ChunkCache::with_default_capacity(shared_index);
```

Add `use dashmap::DashMap; use std::sync::Arc;` in test module.

- [ ] **Step 7: Add fields to PdService struct**

```rust
pub struct PdService {
    shutdown: Shutdown,
    msg_rx: PdChunkReceiver,
    cache: ChunkCache,
    tracker: ProvisioningTracker,
    storage_provider: Arc<dyn RethChunkProvider>,
    block_state_rx: broadcast::Receiver<BlockStateUpdated>,
    current_height: Option<u64>,
    block_tracker: HashMap<B256, Vec<ChunkKey>>,
    /// Sends notifications when PD transaction provisioning completes.
    ready_notify_tx: irys_types::chunk_provider::PdReadySender,
}
```

- [ ] **Step 8: Update spawn_service**

```rust
pub fn spawn_service(
    msg_rx: PdChunkReceiver,
    storage_provider: Arc<dyn RethChunkProvider>,
    block_state_rx: broadcast::Receiver<BlockStateUpdated>,
    runtime_handle: tokio::runtime::Handle,
    chunk_data_index: irys_types::chunk_provider::ChunkDataIndex,
    ready_notify_tx: irys_types::chunk_provider::PdReadySender,
) -> TokioServiceHandle {
    // ...
    let service = Self {
        shutdown,
        msg_rx,
        cache: ChunkCache::with_default_capacity(chunk_data_index),
        tracker: ProvisioningTracker::new(),
        storage_provider,
        block_state_rx,
        current_height: None,
        block_tracker: HashMap::new(),
        ready_notify_tx,
    };
    // ...
```

- [ ] **Step 9: Send ready notification at end of handle_provision_chunks**

After the state is set (line ~297), add:

```rust
// Notify mempool monitor about provisioning result
let is_ready = matches!(tx_state.state, ProvisioningState::Ready);
let _ = self.ready_notify_tx.send(irys_types::chunk_provider::PdReadyNotification {
    tx_hash,
    is_ready,
});
```

- [ ] **Step 10: Remove handle_is_ready, handle_get_chunk, and their match arms**

Delete `handle_is_ready` (lines 335-338) and `handle_get_chunk` (lines 365-368). Remove the corresponding match arms from `handle_message` (lines 127-130 for IsReady, lines 138-145 for GetChunk).

- [ ] **Step 11: Update pd_service.rs tests**

Update `test_service()`:

```rust
fn test_service() -> (PdService, irys_types::chunk_provider::PdReadyReceiver) {
    let (_tx, rx) = mpsc::unbounded_channel();
    let (_, block_state_rx) = tokio::sync::broadcast::channel(16);
    let provider = Arc::new(MockChunkProvider::new());
    let (_, shutdown) = reth::tasks::shutdown::signal();
    let chunk_data_index: irys_types::chunk_provider::ChunkDataIndex =
        Arc::new(DashMap::new());
    let (ready_notify_tx, ready_notify_rx) = mpsc::unbounded_channel();
    let service = PdService {
        shutdown,
        msg_rx: rx,
        cache: ChunkCache::with_default_capacity(chunk_data_index),
        tracker: ProvisioningTracker::new(),
        storage_provider: provider,
        block_state_rx,
        current_height: None,
        block_tracker: HashMap::new(),
        ready_notify_tx,
    };
    (service, ready_notify_rx)
}
```

Update all existing tests to destructure `let (mut service, _ready_rx) = test_service();`.

- [ ] **Step 12: Add test for ready notification and ChunkDataIndex population**

```rust
#[test]
fn test_provisioning_sends_ready_notification_and_populates_chunk_index() {
    let (mut service, mut ready_rx) = test_service();
    let chunk_data = service.cache.shared_index().clone(); // need a getter or access field
    let tx_hash = B256::with_last_byte(0x01);

    let specs = vec![ChunkRangeSpecifier {
        partition_index: Default::default(),
        offset: 0,
        chunk_count: 3,
    }];

    service.handle_provision_chunks(tx_hash, specs);

    // Check ready notification was sent
    let notification = ready_rx.try_recv().expect("should receive notification");
    assert_eq!(notification.tx_hash, tx_hash);
    assert!(notification.is_ready);

    // Check chunk data index was populated
    assert!(chunk_data.contains_key(&(0u32, 0u64)));
    assert!(chunk_data.contains_key(&(0u32, 1u64)));
    assert!(chunk_data.contains_key(&(0u32, 2u64)));

    // Release — chunks should be cleaned up
    service.handle_release_chunks(&tx_hash);
    assert!(chunk_data.is_empty());
}
```

Note: You may need to add a `pub fn shared_index(&self) -> &ChunkDataIndex` getter on `ChunkCache`, or make the field `pub(crate)`.

- [ ] **Step 13: Verify compilation**

Run: `cargo check -p irys-actors --tests`

- [ ] **Step 14: Commit**

```
feat(pd-service): send ready notifications and maintain ChunkDataIndex on provisioning
```

---

### Task 3: Rewrite mempool monitor to be event-driven and maintain ready_pd_txs

**Files:**
- Modify: `crates/irys-reth/src/mempool.rs:41-48,59-73,83-220,457-590`

- [ ] **Step 1: Add ready_pd_txs and ready callback to IrysPoolBuilder**

```rust
#[derive(Clone)]
#[non_exhaustive]
pub struct IrysPoolBuilder {
    hardfork_config: Arc<IrysHardforkConfig>,
    pd_chunk_sender: PdChunkSender,
    /// Shared set of PD tx hashes whose chunks are fully provisioned.
    /// Written by the monitor, read by the payload builder.
    ready_pd_txs: Arc<DashSet<B256>>,
    /// Receives ready notifications from PdService.
    ready_notify_rx: Arc<tokio::sync::Mutex<irys_types::chunk_provider::PdReadyReceiver>>,
}
```

Note: `PdReadyReceiver` is `mpsc::UnboundedReceiver` which is not `Clone`. Wrap in `Arc<Mutex<>>` so `IrysPoolBuilder` (which is `Clone`) can hold it. The mutex is only locked once (when spawning the monitor task), so no contention.

Update the `new` constructor to accept the new fields.

- [ ] **Step 2: Pass ready_pd_txs and ready_notify_rx to the monitor task**

In `build_pool` (where `pd_transaction_monitor` is spawned), pass the new parameters:

```rust
let ready_pd_txs = self.ready_pd_txs.clone();
let ready_notify_rx = self.ready_notify_rx.clone();
// ... spawn pd_transaction_monitor with these
```

- [ ] **Step 3: Rewrite pd_transaction_monitor to be event-driven with ready callback**

Replace the function body. The new implementation uses:
- `pool.new_transactions_listener_for(TransactionListenerKind::All)` — instant notification on pool insertion
- `pool.all_transactions_event_listener()` — lifecycle events for removal detection
- `ready_notify_rx` — ready callbacks from PdService

```rust
async fn pd_transaction_monitor<P>(
    pool: P,
    chunk_sender: PdChunkSender,
    hardfork_config: Arc<IrysHardforkConfig>,
    mut shutdown: GracefulShutdown,
    ready_pd_txs: Arc<DashSet<B256>>,
    mut ready_notify_rx: irys_types::chunk_provider::PdReadyReceiver,
) where
    P: TransactionPool,
    P::Transaction: EthPoolTransaction,
{
    use reth_transaction_pool::TransactionListenerKind;

    let mut known_pd_txs: HashSet<B256> = HashSet::new();
    let mut new_tx_listener =
        pool.new_transactions_listener_for(TransactionListenerKind::All);
    let mut all_events = pool.all_transactions_event_listener();

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                tracing::info!("PD transaction monitor shutting down");
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
                let b256_hash = B256::from_slice(tx_hash.as_slice());

                if let Ok(Some(_)) =
                    crate::pd_tx::detect_and_decode_pd_header(tx.transaction.input())
                {
                    if known_pd_txs.insert(b256_hash) {
                        if let Some(access_list) = tx.transaction.access_list() {
                            let chunk_specs =
                                crate::pd_tx::extract_pd_chunk_specs(access_list);
                            if !chunk_specs.is_empty() {
                                tracing::debug!(
                                    tx_hash = %b256_hash,
                                    chunk_specs_count = chunk_specs.len(),
                                    "Detected new PD transaction, sending for provisioning"
                                );
                                let _ = chunk_sender.send(
                                    irys_types::chunk_provider::PdChunkMessage::NewTransaction {
                                        tx_hash: b256_hash,
                                        chunk_specs,
                                    },
                                );
                            }
                        }
                    }
                }
            }

            // PdService says provisioning is complete for a tx
            Some(notification) = ready_notify_rx.recv() => {
                if notification.is_ready {
                    ready_pd_txs.insert(notification.tx_hash);
                    tracing::debug!(
                        tx_hash = %notification.tx_hash,
                        "PD transaction chunks provisioned, marked ready"
                    );
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
                    let b256_hash = B256::from_slice(tx_hash.as_slice());
                    if known_pd_txs.remove(&b256_hash) {
                        ready_pd_txs.remove(&b256_hash);
                        let _ = chunk_sender.send(
                            irys_types::chunk_provider::PdChunkMessage::TransactionRemoved {
                                tx_hash: b256_hash,
                            },
                        );
                    }
                }
            }
        }
    }
}
```

- [ ] **Step 4: Add necessary imports**

```rust
use dashmap::DashSet;
use futures::StreamExt as _;  // for all_events.next()
```

Remove unused `tokio::time` imports (POLL_INTERVAL, CLEANUP_INTERVAL are gone).

- [ ] **Step 5: Verify compilation**

Run: `cargo check -p irys-reth --tests`

- [ ] **Step 6: Commit**

```
feat(mempool): event-driven monitor with ready callback from PdService
```

---

### Task 4: Update payload builder to use ready_pd_txs

**Files:**
- Modify: `crates/irys-reth/src/payload.rs:331-349,371-380,507-522,554-598,626-647,659-679`

- [ ] **Step 1: Add ready_pd_txs to IrysPayloadBuilder**

```rust
pub struct IrysPayloadBuilder<Pool, Client, EvmConfig = EthEvmConfig> {
    client: Client,
    pool: Pool,
    evm_config: EvmConfig,
    builder_config: EthereumBuilderConfig,
    max_pd_chunks_per_block: u64,
    hardforks: Arc<IrysHardforkConfig>,
    pd_chunk_sender: PdChunkSender,
    /// Shared set of PD tx hashes whose chunks are ready. Maintained by mempool monitor.
    ready_pd_txs: Arc<dashmap::DashSet<revm_primitives::B256>>,
}
```

Update `new()` to accept and store it. Update `Debug` impl.

- [ ] **Step 2: Add ready_pd_txs to CombinedTransactionIterator**

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

- [ ] **Step 3: Replace blocking IsReady with ready_pd_txs.contains()**

Replace the IsReady block at `payload.rs:559-582`:

```rust
            // For PD transactions, check if chunks are ready
            if chunks > 0 {
                let tx_hash = revm_primitives::B256::from_slice(tx.hash().as_slice());

                // Lock-free readiness check via shared DashSet.
                // The mempool monitor populates this when PdService confirms provisioning.
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

- [ ] **Step 4: Update best_transactions_with_attributes to pass ready_pd_txs**

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

- [ ] **Step 5: Update payload.rs test**

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

- [ ] **Step 6: Remove unused imports**

Remove `PdChunkMessage` import from payload.rs if no longer used. Remove `tokio::sync::oneshot` if no longer needed.

- [ ] **Step 7: Verify compilation**

Run: `cargo check -p irys-reth --tests`

- [ ] **Step 8: Commit**

```
feat(payload): replace blocking IsReady with lock-free ready_pd_txs check
```

---

### Task 5: Thread new types through wiring layers

**Files:**
- Modify: `crates/irys-reth/src/payload_builder_builder.rs`
- Modify: `crates/irys-reth/src/lib.rs`
- Modify: `crates/reth-node-bridge/src/node.rs`
- Modify: `crates/chain/src/chain.rs`

- [ ] **Step 1: Add ready_pd_txs to IrysPayloadBuilderBuilder**

```rust
pub struct IrysPayloadBuilderBuilder {
    pub max_pd_chunks_per_block: u64,
    pub hardforks: Arc<IrysHardforkConfig>,
    pub pd_chunk_sender: PdChunkSender,
    pub ready_pd_txs: Arc<dashmap::DashSet<revm_primitives::B256>>,
}
```

Update `build_payload_builder` to pass `self.ready_pd_txs` to `IrysPayloadBuilder::new`.

- [ ] **Step 2: Add ready_pd_txs to IrysPoolBuilder fields and IrysEthereumNode**

Add to `IrysEthereumNode`:
```rust
pub struct IrysEthereumNode {
    pub max_pd_chunks_per_block: u64,
    pub chunk_provider: Arc<dyn irys_types::chunk_provider::RethChunkProvider>,
    pub hardfork_config: Arc<irys_types::hardfork_config::IrysHardforkConfig>,
    pub pd_chunk_sender: irys_types::chunk_provider::PdChunkSender,
    pub ready_pd_txs: Arc<dashmap::DashSet<revm_primitives::B256>>,
    pub ready_notify_rx: Arc<tokio::sync::Mutex<irys_types::chunk_provider::PdReadyReceiver>>,
    pub chunk_data_index: irys_types::chunk_provider::ChunkDataIndex,
}
```

Update `components()` to pass these to `IrysPoolBuilder` and `IrysPayloadBuilderBuilder`.

- [ ] **Step 3: Update IrysPoolBuilder::new**

```rust
pub fn new(
    hardfork_config: Arc<IrysHardforkConfig>,
    pd_chunk_sender: PdChunkSender,
    ready_pd_txs: Arc<DashSet<B256>>,
    ready_notify_rx: Arc<tokio::sync::Mutex<irys_types::chunk_provider::PdReadyReceiver>>,
) -> Self
```

- [ ] **Step 4: Update run_node in reth-node-bridge**

Add `ready_pd_txs`, `ready_notify_rx`, `chunk_data_index` parameters. Pass through to `IrysEthereumNode`.

- [ ] **Step 5: Update start_reth_node and init_services in chain.rs**

Add parameters. Update call sites.

- [ ] **Step 6: Create shared state in chain.rs**

Near line 1139-1141 where the mpsc channel is created:

```rust
let (pd_chunk_tx, pd_chunk_rx) = tokio::sync::mpsc::unbounded_channel();
let chunk_data_index: irys_types::chunk_provider::ChunkDataIndex =
    std::sync::Arc::new(dashmap::DashMap::with_capacity(16_384));
let (ready_notify_tx, ready_notify_rx) = tokio::sync::mpsc::unbounded_channel();
let ready_pd_txs: Arc<dashmap::DashSet<revm_primitives::B256>> =
    std::sync::Arc::new(dashmap::DashSet::new());
```

Pass `chunk_data_index.clone()` and `ready_notify_tx` to `PdService::spawn_service`.
Pass `ready_pd_txs.clone()`, `ready_notify_rx`, and `chunk_data_index.clone()` to `start_reth_node`.

- [ ] **Step 7: Add dashmap to crates/chain/Cargo.toml**

```toml
dashmap = { workspace = true }
```

- [ ] **Step 8: Update IrysEthereumNode test construction (lib.rs ~line 3353)**

Add new fields with defaults:
```rust
ready_pd_txs: std::sync::Arc::new(dashmap::DashSet::new()),
ready_notify_rx: std::sync::Arc::new(tokio::sync::Mutex::new(
    tokio::sync::mpsc::unbounded_channel().1,
)),
chunk_data_index: std::sync::Arc::new(dashmap::DashMap::new()),
```

Search for all `IrysEthereumNode {` constructions: `grep -rn "IrysEthereumNode {" crates/ --include="*.rs"` and update all sites.

- [ ] **Step 9: Pass PdService::spawn_service the new parameters**

At chain.rs ~line 1816:
```rust
let pd_service_handle = irys_actors::pd_service::PdService::spawn_service(
    pd_chunk_rx,
    chunk_provider.clone(),
    service_senders.subscribe_block_state_updates(),
    runtime_handle.clone(),
    chunk_data_index.clone(),
    ready_notify_tx,
);
```

- [ ] **Step 10: Verify full workspace compilation**

Run: `cargo check --workspace --tests`

- [ ] **Step 11: Run tests**

Run: `cargo nextest run -p irys-types -p irys-actors -p irys-reth`

- [ ] **Step 12: Commit**

```
feat: wire ChunkDataIndex, ready callback, and ready_pd_txs through all layers
```

---

### Task 6: Phase 1 verification

- [ ] **Step 1: Run clippy**

Run: `cargo clippy --workspace --tests --all-targets`

- [ ] **Step 2: Run fmt**

Run: `cargo fmt --all`

- [ ] **Step 3: Commit fixes**

```
chore: clippy and fmt fixes for Phase 1
```

---

## Chunk 2: Phase 2 — ChunkDataIndex for lock-free precompile reads

### Task 7: Update PdContext to use ChunkDataIndex

**Files:**
- Modify: `crates/irys-reth/src/precompiles/pd/context.rs:11-16,27-38,56-62,122-146`

- [ ] **Step 1: Update ChunkSource::Manager to hold ChunkDataIndex**

```rust
enum ChunkSource {
    /// Fetch chunks via shared index (payload building with cached chunks).
    Manager {
        chunk_data_index: irys_types::chunk_provider::ChunkDataIndex,
    },
    /// Fetch chunks directly from storage (for block validation)
    Storage(Arc<dyn RethChunkProvider>),
}
```

Note: The `PdChunkSender` is no longer needed in the Manager variant since `GetChunk` is removed. If `Lock`/`Unlock` are needed later, they can be re-added.

- [ ] **Step 2: Update PdContext::new_with_manager**

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

- [ ] **Step 3: Replace get_chunk Manager path with DashMap read**

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

- [ ] **Step 4: Update clone_for_new_evm**

Ensure the Manager variant with `ChunkDataIndex` (which is `Arc<DashMap>`) clones correctly.

- [ ] **Step 5: Remove unused imports**

Remove `PdChunkMessage` and `PdChunkSender` from context.rs imports if no longer used.

- [ ] **Step 6: Verify compilation**

Run: `cargo check -p irys-reth`

- [ ] **Step 7: Commit**

```
feat(pd-context): replace blocking GetChunk channel with lock-free ChunkDataIndex reads
```

---

### Task 8: Thread ChunkDataIndex through EVM factory

**Files:**
- Modify: `crates/irys-reth/src/evm.rs:369-410,443-512`
- Modify: `crates/irys-reth/src/payload_builder_builder.rs`

- [ ] **Step 1: Replace pd_chunk_sender with chunk_data_index in IrysEvmFactory**

Since `PdContext::new_with_manager` no longer needs a `PdChunkSender`, replace the field:

```rust
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

- [ ] **Step 3: Update BOTH create_evm AND create_evm_with_inspector**

Both methods have the same PdContext construction. Update both:

```rust
let pd_context = match &self.chunk_data_index {
    Some(index) => {
        PdContext::new_with_manager(self.context.chunk_config(), index.clone())
    }
    None => self.context.clone_for_new_evm(),
};
```

Search: `grep -n "new_with_manager" crates/irys-reth/src/evm.rs` to find all call sites.

- [ ] **Step 4: Update ConfigurePdChunkSender trait → ConfigureChunkDataIndex**

Rename or replace the trait:

```rust
pub trait ConfigureChunkDataIndex: Sized {
    fn with_chunk_data_index(self, index: irys_types::chunk_provider::ChunkDataIndex) -> Self;
}
```

Update `IrysEvmConfig` impl and `IrysBlockExecutorFactory::with_chunk_data_index`.

- [ ] **Step 5: Update payload_builder_builder.rs**

Add `chunk_data_index` field. Update `build_payload_builder` to call `evm_config.with_chunk_data_index(self.chunk_data_index.clone())`. Update the trait bound to use `ConfigureChunkDataIndex` instead of `ConfigurePdChunkSender`.

- [ ] **Step 6: Update IrysExecutorBuilder in lib.rs**

Replace `pd_chunk_sender` with `chunk_data_index` in the executor builder. Update `build_evm` to chain `.with_chunk_data_index(self.chunk_data_index)`.

- [ ] **Step 7: Update components() in lib.rs**

Pass `chunk_data_index` to both `IrysExecutorBuilder` and `IrysPayloadBuilderBuilder`.

- [ ] **Step 8: Verify compilation**

Run: `cargo check --workspace --tests`

- [ ] **Step 9: Commit**

```
feat(evm): replace PdChunkSender with ChunkDataIndex in EVM factory
```

---

### Task 9: Final verification and test updates

**Files:**
- Modify: `crates/irys-reth/tests/pd_context_isolation.rs`
- Any other test files that construct `PdContext`, `IrysEvmFactory`, or `IrysEthereumNode` directly

- [ ] **Step 1: Search for all direct constructions that need updating**

```
grep -rn "PdContext::new_with_manager\|IrysEvmFactory::new\|with_pd_chunk_sender\|ConfigurePdChunkSender" crates/ --include="*.rs"
```

Update each site to use the new signatures.

- [ ] **Step 2: Update pd_context_isolation test**

```rust
let chunk_data_index: irys_types::chunk_provider::ChunkDataIndex =
    std::sync::Arc::new(dashmap::DashMap::new());
PdContext::new_with_manager(chunk_config, chunk_data_index)
```

- [ ] **Step 3: Run all tests**

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

1. **DashMap memory**: The `ChunkDataIndex` is bounded by the LRU cache size — every insert/remove/eviction is mirrored. Default LRU capacity is 16,384 entries. Overhead per entry is ~100 bytes (the `Arc<Bytes>` chunk data is shared, not duplicated). Use `DashMap::with_capacity(16_384)` to avoid rehashing.

2. **Stale ready_pd_txs**: A tx could be marked ready just as it's being removed from the pool. The monitor handles this correctly: the `Discarded`/`Invalid`/`Mined` events remove the tx from `ready_pd_txs`. If the payload builder sees a stale entry, the tx will fail during EVM execution and be handled by the existing `mark_invalid` flow.

3. **Event ordering**: `new_transactions_listener_for(All)` fires on pool insertion. The monitor sends `NewTransaction` to PdService, which provisions and sends back `PdReadyNotification`. If the payload builder runs before the notification arrives, the PD tx is skipped (not ready yet) — correct behavior. It'll be included in the next block build.

4. **BestTransactionFilter NOT used**: Reth's `BestTransactionFilter` calls `mark_invalid` which blocks ALL txs from the same sender. We intentionally use `continue` (graceful skip) in `CombinedTransactionIterator` to preserve sender ordering for mixed PD/non-PD senders.

5. **Lock/Unlock messages**: These are removed from `PdContext` since the `PdChunkSender` is no longer stored there. If Lock/Unlock are needed for cache pinning during EVM execution, they would need to be re-added via a separate mechanism. Currently the `ChunkDataIndex` uses `Arc<Bytes>` which keeps data alive regardless of LRU eviction, so explicit locking may not be necessary.

6. **`FullTransactionEvent` import**: Verify the exact import path in the reth-irys fork. Check with `grep -rn "FullTransactionEvent" ~/.cargo/` or the reth-irys source.
