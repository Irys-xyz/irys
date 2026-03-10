# PD Chunk Lock-Free Indexes Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate all `blocking_recv()` calls in the PD payload building and EVM execution paths by replacing channel-based request-reply with shared concurrent indexes.

**Architecture:** Two shared `DashMap` indexes — `ReadinessIndex` (tx readiness) and `ChunkDataIndex` (chunk bytes) — are populated by `PdService` during provisioning and read lock-free by the payload builder iterator and PD precompile respectively. The mempool monitor switches from 100ms polling to reth's event-driven `TransactionPool` listeners.

**Tech Stack:** `dashmap 6.0` (already a workspace dependency), existing `tokio::sync::mpsc` channel (retained for mutations: Lock/Unlock/NewTransaction/TransactionRemoved).

---

## File Structure

### New files
- None. All changes are modifications to existing files.

### Modified files

| File | Responsibility |
|------|---------------|
| `crates/types/src/chunk_provider.rs` | Add `ReadinessIndex`, `ChunkDataIndex` type aliases; remove `IsReady` and `GetChunk` variants from `PdChunkMessage` |
| `crates/actors/src/pd_service/cache.rs` | Add `ChunkDataIndex` field to `ChunkCache`; mirror inserts/removes to shared index |
| `crates/actors/src/pd_service.rs` | Add `ReadinessIndex` field; update it on every provisioning state transition; pass `ChunkDataIndex` through to cache |
| `crates/irys-reth/src/payload.rs` | Replace `pd_chunk_sender` IsReady call with `ReadinessIndex.get()`; add index fields to `CombinedTransactionIterator` and `IrysPayloadBuilder` |
| `crates/irys-reth/src/precompiles/pd/context.rs` | Add `ChunkDataIndex` to `ChunkSource::Manager`; read directly instead of `GetChunk` channel |
| `crates/irys-reth/src/evm.rs` | Thread `ChunkDataIndex` through `IrysEvmFactory`; pass to `PdContext` |
| `crates/irys-reth/src/payload_builder_builder.rs` | Accept and forward `ReadinessIndex` |
| `crates/irys-reth/src/lib.rs` | Add `ReadinessIndex` and `ChunkDataIndex` to `IrysEthereumNode`; thread through component builders |
| `crates/reth-node-bridge/src/node.rs` | Accept and forward both indexes to `IrysEthereumNode` |
| `crates/chain/src/chain.rs` | Create both indexes; pass to `start_reth_node` and `PdService::spawn_service` |
| `crates/irys-reth/src/mempool.rs` | Replace 100ms polling with event-driven `new_transactions_listener_for(All)` + `all_transactions_event_listener()` |

---

## Chunk 1: Phase 1 — ReadinessIndex (eliminate IsReady blocking)

### Task 1: Add ReadinessIndex type and remove IsReady message variant

**Files:**
- Modify: `crates/types/src/chunk_provider.rs:42-87`

- [ ] **Step 1: Read the file**

Read `crates/types/src/chunk_provider.rs` to confirm current state.

- [ ] **Step 2: Add the ReadinessIndex type alias**

Add after the channel type aliases (after line 87):

```rust
/// Shared readiness index for PD transactions.
/// PdService writes `true` when all chunks for a tx are provisioned.
/// The payload builder reads this lock-free instead of sending IsReady messages.
pub type ReadinessIndex = Arc<DashMap<B256, bool>>;
```

Add the necessary imports at the top of the file:

```rust
use dashmap::DashMap;
```

Note: `Arc` and `B256` should already be in scope (check imports — `Arc` is used by `Arc<Bytes>` in `GetChunk`, `B256` is from `revm_primitives`).

- [ ] **Step 3: Remove the IsReady variant from PdChunkMessage**

Remove these lines from the `PdChunkMessage` enum:

```rust
    IsReady {
        tx_hash: B256,
        response: oneshot::Sender<bool>,
    },
```

- [ ] **Step 4: Add dashmap to crates/types Cargo.toml**

Check if `dashmap` is already a dependency of `irys-types`. If not, add:

```toml
dashmap = { workspace = true }
```

- [ ] **Step 5: Verify compilation**

Run: `cargo check -p irys-types`

This will fail because `PdChunkMessage::IsReady` is referenced in other crates. That's expected — we fix consumers in subsequent tasks.

- [ ] **Step 6: Commit**

```
feat(types): add ReadinessIndex type and remove IsReady message variant
```

---

### Task 2: Update PdService to maintain ReadinessIndex

**Files:**
- Modify: `crates/actors/src/pd_service.rs:25-34` (struct), `38-69` (spawn_service), `116-157` (handle_message), `211-306` (handle_provision_chunks), `309-333` (handle_release_chunks), `335-338` (handle_is_ready), `371-390` (handle_block_state_update)

- [ ] **Step 1: Add ReadinessIndex field to PdService struct**

At `pd_service.rs:25-34`, add a new field:

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
    /// Shared readiness index — written here, read lock-free by payload builder.
    readiness_index: irys_types::chunk_provider::ReadinessIndex,
}
```

- [ ] **Step 2: Update spawn_service to accept ReadinessIndex**

At `pd_service.rs:38-69`, add parameter and pass it through:

```rust
pub fn spawn_service(
    msg_rx: PdChunkReceiver,
    storage_provider: Arc<dyn RethChunkProvider>,
    block_state_rx: broadcast::Receiver<BlockStateUpdated>,
    runtime_handle: tokio::runtime::Handle,
    readiness_index: irys_types::chunk_provider::ReadinessIndex,
) -> TokioServiceHandle {
    let (shutdown_signal, shutdown) = reth::tasks::shutdown::signal();

    let service = Self {
        shutdown,
        msg_rx,
        cache: ChunkCache::with_default_capacity(),
        tracker: ProvisioningTracker::new(),
        storage_provider,
        block_state_rx,
        current_height: None,
        block_tracker: HashMap::new(),
        readiness_index,
    };
    // ... rest unchanged
```

- [ ] **Step 3: Update handle_provision_chunks to write readiness**

At the end of `handle_provision_chunks` (after line 297 where state is set), add:

```rust
// Update shared readiness index
match &tx_state.state {
    ProvisioningState::Ready => {
        self.readiness_index.insert(tx_hash, true);
    }
    _ => {
        self.readiness_index.insert(tx_hash, false);
    }
}
```

- [ ] **Step 4: Update handle_release_chunks to remove from index**

In `handle_release_chunks` (line 310), after `self.tracker.remove(tx_hash)` succeeds, add:

```rust
self.readiness_index.remove(tx_hash);
```

- [ ] **Step 5: Update handle_block_state_update to clean expired entries**

In `handle_block_state_update` (line 374), after `self.tracker.expire_at_height(height)`, add cleanup for each expired tx:

```rust
for (tx_hash, required_chunks) in &expired {
    self.readiness_index.remove(tx_hash);
    // ... existing cache cleanup
}
```

- [ ] **Step 6: Remove handle_is_ready method and its match arm**

Delete the `handle_is_ready` method (lines 335-338). Remove the `PdChunkMessage::IsReady` match arm from `handle_message` (lines 127-130).

- [ ] **Step 7: Update tests to pass ReadinessIndex**

In the `test_service()` function (line 500-515), add the new field:

```rust
fn test_service() -> PdService {
    let (_tx, rx) = mpsc::unbounded_channel();
    let (_, block_state_rx) = tokio::sync::broadcast::channel(16);
    let provider = Arc::new(MockChunkProvider::new());
    let (_, shutdown) = reth::tasks::shutdown::signal();
    PdService {
        shutdown,
        msg_rx: rx,
        cache: ChunkCache::with_default_capacity(),
        tracker: ProvisioningTracker::new(),
        storage_provider: provider,
        block_state_rx,
        current_height: None,
        block_tracker: HashMap::new(),
        readiness_index: Arc::new(DashMap::new()),
    }
}
```

Add to imports in test module:

```rust
use dashmap::DashMap;
use std::sync::Arc;
```

- [ ] **Step 8: Add a test for readiness index updates**

Add a new test that verifies the readiness index is maintained:

```rust
#[test]
fn test_readiness_index_updated_on_provision_and_release() {
    let mut service = test_service();
    let readiness = service.readiness_index.clone();
    let tx_hash = B256::with_last_byte(0x01);

    let specs = vec![ChunkRangeSpecifier {
        partition_index: Default::default(),
        offset: 0,
        chunk_count: 2,
    }];

    // Before provisioning — not in index
    assert!(!readiness.contains_key(&tx_hash));

    // Provision — should be ready (MockChunkProvider always succeeds)
    service.handle_provision_chunks(tx_hash, specs);
    assert_eq!(readiness.get(&tx_hash).map(|v| *v), Some(true));

    // Release — should be removed from index
    service.handle_release_chunks(&tx_hash);
    assert!(!readiness.contains_key(&tx_hash));
}
```

- [ ] **Step 9: Add dashmap to crates/actors Cargo.toml if not present**

`dashmap` is already in `crates/actors/Cargo.toml:29`. Verify this is still the case. If the import `use dashmap::DashMap;` is needed in tests, it's already available.

- [ ] **Step 10: Verify compilation**

Run: `cargo check -p irys-actors --tests`

This may still fail due to downstream consumers of the removed `IsReady` variant. That's expected.

- [ ] **Step 11: Commit**

```
feat(pd-service): maintain shared ReadinessIndex on provisioning state transitions
```

---

### Task 3: Update payload builder to use ReadinessIndex

**Files:**
- Modify: `crates/irys-reth/src/payload.rs:331-349` (IrysPayloadBuilder struct), `371-380` (CombinedTransactionIterator struct), `507-522` (CombinedTransactionIterator::new), `554-598` (Iterator impl), `626-647` (IrysPayloadBuilder::new), `659-679` (best_transactions_with_attributes)

- [ ] **Step 1: Add ReadinessIndex to IrysPayloadBuilder struct**

At `payload.rs:331-349`:

```rust
pub struct IrysPayloadBuilder<Pool, Client, EvmConfig = EthEvmConfig> {
    client: Client,
    pool: Pool,
    evm_config: EvmConfig,
    builder_config: EthereumBuilderConfig,
    max_pd_chunks_per_block: u64,
    hardforks: Arc<IrysHardforkConfig>,
    pd_chunk_sender: PdChunkSender,
    /// Shared readiness index — lock-free reads during payload building.
    readiness_index: irys_types::chunk_provider::ReadinessIndex,
}
```

- [ ] **Step 2: Update IrysPayloadBuilder::new to accept ReadinessIndex**

At `payload.rs:626-647`:

```rust
pub fn new(
    client: Client,
    pool: Pool,
    evm_config: EvmConfig,
    builder_config: EthereumBuilderConfig,
    max_pd_chunks_per_block: u64,
    hardforks: Arc<IrysHardforkConfig>,
    pd_chunk_sender: PdChunkSender,
    readiness_index: irys_types::chunk_provider::ReadinessIndex,
) -> Self {
    Self {
        client,
        pool,
        evm_config,
        builder_config,
        max_pd_chunks_per_block,
        hardforks,
        pd_chunk_sender,
        readiness_index,
    }
}
```

- [ ] **Step 3: Update Debug impl for IrysPayloadBuilder**

At `payload.rs:351-368`, add the new field:

```rust
.field("readiness_index", &format!("<{} entries>", self.readiness_index.len()))
```

- [ ] **Step 4: Add ReadinessIndex to CombinedTransactionIterator**

At `payload.rs:371-380`:

```rust
pub struct CombinedTransactionIterator {
    shadow: ShadowTxQueue,
    pd_budget: PdChunkBudget,
    pool_iter: BestTransactionsIter,
    is_sprite_active: bool,
    pd_chunk_sender: PdChunkSender,
    /// Shared readiness index — lock-free reads replace IsReady channel calls.
    readiness_index: irys_types::chunk_provider::ReadinessIndex,
}
```

- [ ] **Step 5: Update CombinedTransactionIterator::new**

At `payload.rs:507-522`, add the parameter:

```rust
pub fn new(
    timestamp: Instant,
    shadow_txs: Vec<EthPooledTransaction>,
    pool_iter: BestTransactionsIter,
    max_pd_chunks_per_block: u64,
    is_sprite_active: bool,
    pd_chunk_sender: PdChunkSender,
    readiness_index: irys_types::chunk_provider::ReadinessIndex,
) -> Self {
    Self {
        shadow: ShadowTxQueue::from_shadow_transactions(timestamp, shadow_txs),
        pd_budget: PdChunkBudget::new(max_pd_chunks_per_block),
        pool_iter,
        is_sprite_active,
        pd_chunk_sender,
        readiness_index,
    }
}
```

- [ ] **Step 6: Replace blocking_recv with ReadinessIndex lookup in Iterator::next**

Replace the entire IsReady block at `payload.rs:559-582` with a lock-free DashMap read:

**Remove:**
```rust
            // For PD transactions, check if chunks are ready
            if chunks > 0 {
                // Check chunk readiness
                let tx_hash = revm_primitives::B256::from_slice(tx.hash().as_slice());

                // Query readiness via channel (blocking)
                let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                if self
                    .pd_chunk_sender
                    .send(PdChunkMessage::IsReady {
                        tx_hash,
                        response: resp_tx,
                    })
                    .is_ok()
                {
                    // Use blocking recv since we're in a sync iterator context.
                    // block_in_place is required when called from within a tokio runtime.
                    if let Ok(false) = tokio::task::block_in_place(|| resp_rx.blocking_recv()) {
                        tracing::debug!(
                            tx_hash = %tx_hash,
                            "Skipping PD transaction: chunks not ready"
                        );
                        continue; // Skip this transaction, try next
                    }
                }

                // Check PD budget
                if !self.pd_budget.try_consume(&tx, chunks) {
                    self.pd_budget.defer(tx);
                    continue;
                }

                return Some(tx);
            }
```

**Replace with:**
```rust
            // For PD transactions, check if chunks are ready
            if chunks > 0 {
                let tx_hash = revm_primitives::B256::from_slice(tx.hash().as_slice());

                // Lock-free readiness check via shared DashMap index.
                // PdService populates this during provisioning.
                // Unknown tx_hash (not in index) → treat as not ready (skip).
                let is_ready = self
                    .readiness_index
                    .get(&tx_hash)
                    .map(|v| *v)
                    .unwrap_or(false);

                if !is_ready {
                    tracing::debug!(
                        tx_hash = %tx_hash,
                        "Skipping PD transaction: chunks not ready"
                    );
                    continue;
                }

                // Check PD budget
                if !self.pd_budget.try_consume(&tx, chunks) {
                    self.pd_budget.defer(tx);
                    continue;
                }

                return Some(tx);
            }
```

- [ ] **Step 7: Update best_transactions_with_attributes to pass ReadinessIndex**

At `payload.rs:659-679`, pass the index through to `CombinedTransactionIterator::new`:

```rust
pub fn best_transactions_with_attributes(
    &self,
    attributes: BestTransactionsAttributes,
    shadow_txs: Vec<EthPooledTransaction>,
    is_sprite_active: bool,
) -> BestTransactionsIter {
    let timestamp = Instant::now();
    let pool_txs = self.pool.best_transactions_with_attributes(attributes);

    Box::new(CombinedTransactionIterator::new(
        timestamp,
        shadow_txs,
        pool_txs,
        self.max_pd_chunks_per_block,
        is_sprite_active,
        self.pd_chunk_sender.clone(),
        self.readiness_index.clone(),
    ))
}
```

- [ ] **Step 8: Update payload.rs test to pass ReadinessIndex**

In the test `combined_iterator_prioritizes_shadow_and_respects_pd_budget` (line 913-920), update the `CombinedTransactionIterator::new` call:

```rust
use dashmap::DashMap;

// Create readiness index — mark pd_small as ready, pd_large as ready
let readiness_index: irys_types::chunk_provider::ReadinessIndex = Arc::new(DashMap::new());
let pd_small_b256 = revm_primitives::B256::from_slice(pd_small_hash.as_slice());
let pd_large_b256 = revm_primitives::B256::from_slice(pd_large_hash.as_slice());
readiness_index.insert(pd_small_b256, true);
readiness_index.insert(pd_large_b256, true);

let mut iterator = CombinedTransactionIterator::new(
    timestamp,
    vec![shadow_tx],
    pool_iter,
    7_500,
    true,
    pd_chunk_sender,
    readiness_index,
);
```

- [ ] **Step 9: Remove unused imports**

Remove `tokio::sync::oneshot` import if no longer needed in payload.rs. Remove `PdChunkMessage` import if only used for `IsReady`. Check carefully — `PdChunkSender` is still used (for Lock/Unlock in future), so the `pd_chunk_sender` field stays.

- [ ] **Step 10: Verify compilation**

Run: `cargo check -p irys-reth --tests`

- [ ] **Step 11: Commit**

```
feat(payload): replace blocking IsReady channel with lock-free ReadinessIndex lookup
```

---

### Task 4: Thread ReadinessIndex through wiring layers

**Files:**
- Modify: `crates/irys-reth/src/payload_builder_builder.rs:22-27,57-81`
- Modify: `crates/irys-reth/src/lib.rs:96-104,126-163`
- Modify: `crates/reth-node-bridge/src/node.rs:100-108,230-237`
- Modify: `crates/chain/src/chain.rs:343-350,1139-1154,1364-1385,1816-1821`

- [ ] **Step 1: Add ReadinessIndex to IrysPayloadBuilderBuilder**

At `payload_builder_builder.rs:22-27`:

```rust
#[derive(Clone)]
pub struct IrysPayloadBuilderBuilder {
    pub max_pd_chunks_per_block: u64,
    pub hardforks: Arc<IrysHardforkConfig>,
    pub pd_chunk_sender: PdChunkSender,
    pub readiness_index: irys_types::chunk_provider::ReadinessIndex,
}
```

Update `build_payload_builder` (line 57-81) to pass `readiness_index` to `IrysPayloadBuilder::new`:

```rust
Ok(crate::payload::IrysPayloadBuilder::new(
    ctx.provider().clone(),
    pool,
    evm_config,
    EthereumBuilderConfig::new().with_gas_limit(gas_limit),
    self.max_pd_chunks_per_block,
    self.hardforks,
    self.pd_chunk_sender,
    self.readiness_index,
))
```

- [ ] **Step 2: Add ReadinessIndex to IrysEthereumNode**

At `lib.rs:96-104`:

```rust
#[derive(Clone)]
pub struct IrysEthereumNode {
    pub max_pd_chunks_per_block: u64,
    pub chunk_provider: Arc<dyn irys_types::chunk_provider::RethChunkProvider>,
    pub hardfork_config: Arc<irys_types::hardfork_config::IrysHardforkConfig>,
    pub pd_chunk_sender: irys_types::chunk_provider::PdChunkSender,
    pub readiness_index: irys_types::chunk_provider::ReadinessIndex,
}
```

- [ ] **Step 3: Update components() in lib.rs to pass ReadinessIndex**

In the `components()` method, find where `IrysPayloadBuilderBuilder` is constructed and add the field. Search for `IrysPayloadBuilderBuilder` in `lib.rs` and update:

```rust
IrysPayloadBuilderBuilder {
    max_pd_chunks_per_block: self.max_pd_chunks_per_block,
    hardforks: self.hardfork_config.clone(),
    pd_chunk_sender: self.pd_chunk_sender.clone(),
    readiness_index: self.readiness_index.clone(),
}
```

- [ ] **Step 4: Update run_node in reth-node-bridge**

At `reth-node-bridge/src/node.rs:100-108`, add parameter:

```rust
pub async fn run_node(
    chainspec: Arc<ChainSpec>,
    task_executor: TaskExecutor,
    node_config: irys_types::NodeConfig,
    latest_block: u64,
    random_ports: bool,
    chunk_provider: Arc<dyn irys_types::chunk_provider::RethChunkProvider>,
    pd_chunk_sender: irys_types::chunk_provider::PdChunkSender,
    readiness_index: irys_types::chunk_provider::ReadinessIndex,
) -> eyre::Result<(RethNodeHandle, IrysRethNodeAdapter)> {
```

Update the `IrysEthereumNode` construction (line 232-237):

```rust
.node(IrysEthereumNode {
    max_pd_chunks_per_block,
    chunk_provider,
    hardfork_config: std::sync::Arc::new(hardfork_config.clone()),
    pd_chunk_sender,
    readiness_index,
})
```

- [ ] **Step 5: Update start_reth_node in chain.rs**

At `chain.rs:343-350`, add parameter:

```rust
async fn start_reth_node(
    task_executor: TaskExecutor,
    chainspec: Arc<ChainSpec>,
    config: Config,
    latest_block: u64,
    chunk_provider: Arc<dyn irys_types::chunk_provider::RethChunkProvider>,
    pd_chunk_sender: irys_types::chunk_provider::PdChunkSender,
    readiness_index: irys_types::chunk_provider::ReadinessIndex,
) -> eyre::Result<(RethNodeHandle, RethNode)> {
```

Pass through to `run_node` call (line 352-360):

```rust
let (node_handle, _reth_node_adapter) = irys_reth_node_bridge::node::run_node(
    // ... existing args ...
    pd_chunk_sender,
    readiness_index,
)
```

- [ ] **Step 6: Add dashmap dependency to crates/chain/Cargo.toml**

`dashmap` is a workspace dependency but not yet in `crates/chain/Cargo.toml`. Add it:

```toml
dashmap = { workspace = true }
```

Alternatively, add a constructor to `irys-types` (e.g., `pub fn new_readiness_index() -> ReadinessIndex`) to avoid the dependency. The direct approach is simpler.

- [ ] **Step 7: Create ReadinessIndex in chain.rs and wire it**

Near line 1139-1141 where the channel is created, also create the index:

```rust
let (pd_chunk_tx, pd_chunk_rx) = tokio::sync::mpsc::unbounded_channel();
let readiness_index: irys_types::chunk_provider::ReadinessIndex =
    std::sync::Arc::new(dashmap::DashMap::new());
```

Pass `readiness_index.clone()` to `start_reth_node` (line 1147-1154).

- [ ] **Step 8: Update init_services signature and call site in chain.rs**

At `chain.rs:1364-1385`, add `readiness_index` parameter. Also update the **call site** of `init_services` (near line 1172-1187) to pass `readiness_index.clone()`.

```rust
async fn init_services(
    // ... existing params ...
    pd_chunk_rx: irys_types::chunk_provider::PdChunkReceiver,
    pd_chunk_sender: irys_types::chunk_provider::PdChunkSender,
    readiness_index: irys_types::chunk_provider::ReadinessIndex,
) -> eyre::Result<(...)>
```

- [ ] **Step 9: Pass ReadinessIndex to PdService::spawn_service**

At `chain.rs:1816-1821`:

```rust
let pd_service_handle = irys_actors::pd_service::PdService::spawn_service(
    pd_chunk_rx,
    chunk_provider.clone(),
    service_senders.subscribe_block_state_updates(),
    runtime_handle.clone(),
    readiness_index,
);
```

- [ ] **Step 10: Update IrysEthereumNode test construction in lib.rs**

There is a test helper at `crates/irys-reth/src/lib.rs` (around line 3353) that constructs `IrysEthereumNode` directly. Add the new field:

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
    readiness_index: std::sync::Arc::new(dashmap::DashMap::new()),
}
```

Search for any other direct constructions: `grep -rn "IrysEthereumNode {" crates/ --include="*.rs"` and update all sites.

- [ ] **Step 11: Verify full workspace compilation**

Run: `cargo check --workspace --tests`

Fix any remaining compilation errors from the removed `IsReady` variant or new parameters.

- [ ] **Step 12: Run existing tests**

Run: `cargo nextest run -p irys-reth -p irys-actors`

All existing tests should pass (with the test updates from Task 2 and Task 3).

- [ ] **Step 13: Commit**

```
feat: wire ReadinessIndex through chain → reth-node-bridge → irys-reth → payload builder
```

---

### Task 5: Switch mempool monitor to event-driven

**Files:**
- Modify: `crates/irys-reth/src/mempool.rs:457-590`

This task replaces the 100ms polling `pd_transaction_monitor` with reth's `TransactionPool` event listeners. The monitor will react instantly to new transactions instead of polling.

- [ ] **Step 1: Read the current pd_transaction_monitor implementation**

Read `crates/irys-reth/src/mempool.rs` lines 457-590 to confirm current state.

- [ ] **Step 2: Rewrite pd_transaction_monitor to be event-driven**

Replace the function body. The new implementation uses two listeners:
- `pool.new_transactions_listener_for(TransactionListenerKind::All)` — instant notification when a tx enters the pool
- `pool.all_transactions_event_listener()` — lifecycle events including `Discarded`, `Replaced`, `Invalid` for removal detection

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

    // Track known PD transactions for removal detection
    let mut known_pd_txs: HashSet<B256> = HashSet::new();

    // Subscribe to new transactions (fires instantly on pool insertion)
    let mut new_tx_listener =
        pool.new_transactions_listener_for(TransactionListenerKind::All);

    // Subscribe to all transaction lifecycle events (for removals)
    let mut all_events = pool.all_transactions_event_listener();

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                tracing::info!("PD transaction monitor shutting down");
                break;
            }

            Some(event) = new_tx_listener.recv() => {
                // Check if Sprite hardfork is active
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

                // Check if this is a PD transaction
                if let Ok(Some(_header)) =
                    crate::pd_tx::detect_and_decode_pd_header(tx.transaction.input())
                {
                    if known_pd_txs.insert(b256_hash) {
                        // Extract chunk specs from access list
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

            Some(event) = all_events.next() => {
                use reth_transaction_pool::FullTransactionEvent;
                match event {
                    FullTransactionEvent::Discarded(tx_hash)
                    | FullTransactionEvent::Invalid(tx_hash)
                    | FullTransactionEvent::Mined { tx_hash, .. } => {
                        let b256_hash = B256::from_slice(tx_hash.as_slice());
                        if known_pd_txs.remove(&b256_hash) {
                            tracing::debug!(
                                tx_hash = %b256_hash,
                                "PD transaction removed from pool"
                            );
                            let _ = chunk_sender.send(
                                irys_types::chunk_provider::PdChunkMessage::TransactionRemoved {
                                    tx_hash: b256_hash,
                                },
                            );
                        }
                    }
                    FullTransactionEvent::Replaced { transaction, replaced_by } => {
                        let old_hash = B256::from_slice(transaction.hash().as_slice());
                        if known_pd_txs.remove(&old_hash) {
                            let _ = chunk_sender.send(
                                irys_types::chunk_provider::PdChunkMessage::TransactionRemoved {
                                    tx_hash: old_hash,
                                },
                            );
                        }
                    }
                    _ => {} // Pending, Queued, Propagated — no action needed
                }
            }
        }
    }
}
```

- [ ] **Step 3: Add necessary imports**

Add to the imports section of mempool.rs:

```rust
use futures::StreamExt as _;  // for all_events.next()
use reth_transaction_pool::FullTransactionEvent;
```

- [ ] **Step 4: Remove unused imports**

Remove `tokio::time::{self, Duration}` if no longer used (the polling interval constants `POLL_INTERVAL` and `CLEANUP_INTERVAL` are gone).

- [ ] **Step 5: Verify compilation**

Run: `cargo check -p irys-reth --tests`

- [ ] **Step 6: Commit**

```
feat(mempool): replace 100ms polling with event-driven PD transaction monitor
```

---

### Task 6: Phase 1 verification

- [ ] **Step 1: Run all unit tests**

Run: `cargo nextest run -p irys-types -p irys-actors -p irys-reth`

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --workspace --tests --all-targets`

Fix any warnings.

- [ ] **Step 3: Run fmt**

Run: `cargo fmt --all`

- [ ] **Step 4: Commit any fixes**

```
chore: clippy and fmt fixes for Phase 1
```

---

## Chunk 2: Phase 2 — ChunkDataIndex (eliminate GetChunk blocking)

### Task 7: Add ChunkDataIndex type and remove GetChunk variant

**Files:**
- Modify: `crates/types/src/chunk_provider.rs`

- [ ] **Step 1: Add ChunkDataIndex type alias**

Add after the `ReadinessIndex` definition:

```rust
/// Shared chunk data index for lock-free chunk reads during EVM execution.
/// PdService populates this during provisioning; the PD precompile reads it directly.
/// Keys use `(ledger: u32, offset: u64)` tuples matching `ChunkKey` semantics.
pub type ChunkDataIndex = Arc<DashMap<(u32, u64), Arc<Bytes>>>;
```

Note: We use `(u32, u64)` tuple rather than importing `ChunkKey` from `irys-actors` because `irys-types` is a foundation crate that cannot depend on `irys-actors`. The `ChunkKey` struct fields are just `ledger: u32, offset: u64`, so the tuple is semantically equivalent.

- [ ] **Step 2: Remove GetChunk variant from PdChunkMessage**

Remove from the enum:

```rust
    GetChunk {
        ledger: u32,
        offset: u64,
        response: oneshot::Sender<Option<Arc<Bytes>>>,
    },
```

- [ ] **Step 3: Verify compilation of irys-types**

Run: `cargo check -p irys-types`

- [ ] **Step 4: Commit**

```
feat(types): add ChunkDataIndex type and remove GetChunk message variant
```

---

### Task 8: Update ChunkCache and PdService to maintain ChunkDataIndex

**Files:**
- Modify: `crates/actors/src/pd_service/cache.rs:34-36` (ChunkCache struct), `40-47` (new/with_default_capacity), `75-99` (insert), `142` (remove)
- Modify: `crates/actors/src/pd_service.rs:25-34,38-69,116-157,211-306,309-333,365-368,394-472,475-489`

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

Update `new` and `with_default_capacity` to accept ChunkDataIndex:

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

In `insert` method (line 75-99), after successfully inserting into the LRU, also insert into the shared index:

```rust
pub fn insert(&mut self, key: ChunkKey, data: Arc<Bytes>, tx_hash: B256) -> bool {
    if let Some(entry) = self.chunks.get_mut(&key) {
        entry.referencing_txs.insert(tx_hash);
        return false; // Already existed
    }

    self.make_room_for_insert();

    let entry = CachedChunkEntry {
        data: data.clone(),
        referencing_txs: HashSet::from([tx_hash]),
        lock_count: 0,
        cached_at: Instant::now(),
    };
    self.chunks.put(key, entry);

    // Mirror into shared index for lock-free reads
    self.shared_index.insert((key.ledger, key.offset), data);

    true
}
```

- [ ] **Step 4: Mirror removals from shared index**

In `remove` method (line 142), also remove from the shared index:

```rust
pub fn remove(&mut self, key: &ChunkKey) {
    self.chunks.pop(key);
    self.shared_index.remove(&(key.ledger, key.offset));
}
```

- [ ] **Step 5: Mirror LRU evictions in make_room_for_insert**

In `make_room_for_insert` (line 159-181), when an evictable entry is found and removed, also remove from shared index. Find the line where the evicted key is popped and add:

```rust
fn make_room_for_insert(&mut self) {
    if self.chunks.len() < self.chunks.cap().get() {
        return;
    }

    // Find an evictable entry (no references, not locked)
    let evictable_key = {
        let mut key = None;
        for (k, entry) in self.chunks.iter() {
            if entry.referencing_txs.is_empty() && entry.lock_count == 0 {
                key = Some(*k);
                break;
            }
        }
        key
    };

    if let Some(key) = evictable_key {
        self.chunks.pop(&key);
        self.shared_index.remove(&(key.ledger, key.offset));
    } else {
        // All entries are pinned — grow capacity
        let new_cap = self.chunks.cap().get() * 2;
        self.chunks.resize(NonZeroUsize::new(new_cap).unwrap());
    }
}
```

Note: Read the actual `make_room_for_insert` implementation first. The above is a reference — adapt to match the existing logic, just adding the `shared_index.remove()` call at the eviction point.

- [ ] **Step 6: Update PdService to create and pass ChunkDataIndex**

At `pd_service.rs`, add field to struct (line 25-34):

```rust
pub struct PdService {
    // ... existing fields ...
    readiness_index: irys_types::chunk_provider::ReadinessIndex,
    /// Shared chunk data index — written here, read lock-free by PD precompile.
    chunk_data_index: irys_types::chunk_provider::ChunkDataIndex,
}
```

- [ ] **Step 7: Update spawn_service to accept ChunkDataIndex**

```rust
pub fn spawn_service(
    msg_rx: PdChunkReceiver,
    storage_provider: Arc<dyn RethChunkProvider>,
    block_state_rx: broadcast::Receiver<BlockStateUpdated>,
    runtime_handle: tokio::runtime::Handle,
    readiness_index: irys_types::chunk_provider::ReadinessIndex,
    chunk_data_index: irys_types::chunk_provider::ChunkDataIndex,
) -> TokioServiceHandle {
```

Pass `chunk_data_index.clone()` to `ChunkCache::with_default_capacity(chunk_data_index)`:

```rust
let service = Self {
    shutdown,
    msg_rx,
    cache: ChunkCache::with_default_capacity(chunk_data_index.clone()),
    tracker: ProvisioningTracker::new(),
    storage_provider,
    block_state_rx,
    current_height: None,
    block_tracker: HashMap::new(),
    readiness_index,
    chunk_data_index,
};
```

- [ ] **Step 8: Remove handle_get_chunk and its match arm**

Delete the `handle_get_chunk` method (lines 365-368). Remove the `PdChunkMessage::GetChunk` match arm from `handle_message` (lines 138-145).

- [ ] **Step 9: Update cache.rs tests**

All `ChunkCache` test constructors in `crates/actors/src/pd_service/cache.rs` (around lines 198, 210, 224, 241, 261, 282) need updating because the constructor now requires a `ChunkDataIndex`. Search with: `grep -n "with_default_capacity\|ChunkCache::new(" crates/actors/src/pd_service/cache.rs`

Update each call site. For `with_default_capacity()`:
```rust
let shared_index: irys_types::chunk_provider::ChunkDataIndex = Arc::new(DashMap::new());
let mut cache = ChunkCache::with_default_capacity(shared_index);
```

For `ChunkCache::new(cap)`:
```rust
let shared_index: irys_types::chunk_provider::ChunkDataIndex = Arc::new(DashMap::new());
let mut cache = ChunkCache::new(cap, shared_index);
```

Add imports in the cache.rs test module:
```rust
use dashmap::DashMap;
use std::sync::Arc;
```

- [ ] **Step 10: Update pd_service.rs tests**

Update `test_service()` to pass ChunkDataIndex:

```rust
fn test_service() -> PdService {
    let (_tx, rx) = mpsc::unbounded_channel();
    let (_, block_state_rx) = tokio::sync::broadcast::channel(16);
    let provider = Arc::new(MockChunkProvider::new());
    let (_, shutdown) = reth::tasks::shutdown::signal();
    let chunk_data_index: irys_types::chunk_provider::ChunkDataIndex =
        Arc::new(DashMap::new());
    PdService {
        shutdown,
        msg_rx: rx,
        cache: ChunkCache::with_default_capacity(chunk_data_index.clone()),
        tracker: ProvisioningTracker::new(),
        storage_provider: provider,
        block_state_rx,
        current_height: None,
        block_tracker: HashMap::new(),
        readiness_index: Arc::new(DashMap::new()),
        chunk_data_index,
    }
}
```

- [ ] **Step 11: Add test for ChunkDataIndex population**

```rust
#[test]
fn test_chunk_data_index_populated_on_provision() {
    let mut service = test_service();
    let chunk_data = service.chunk_data_index.clone();
    let tx_hash = B256::with_last_byte(0x01);

    let specs = vec![ChunkRangeSpecifier {
        partition_index: Default::default(),
        offset: 0,
        chunk_count: 3,
    }];

    // Before provisioning — index is empty
    assert!(chunk_data.is_empty());

    // Provision
    service.handle_provision_chunks(tx_hash, specs);

    // Chunks should be in the shared index
    assert!(chunk_data.contains_key(&(0u32, 0u64)));
    assert!(chunk_data.contains_key(&(0u32, 1u64)));
    assert!(chunk_data.contains_key(&(0u32, 2u64)));
    assert_eq!(chunk_data.len(), 3);

    // Release — chunks should be removed (no other references)
    service.handle_release_chunks(&tx_hash);
    assert!(chunk_data.is_empty());
}
```

- [ ] **Step 12: Verify compilation**

Run: `cargo check -p irys-actors --tests`

- [ ] **Step 13: Commit**

```
feat(pd-service): maintain shared ChunkDataIndex alongside LRU cache
```

---

### Task 9: Update PdContext to use ChunkDataIndex

**Files:**
- Modify: `crates/irys-reth/src/precompiles/pd/context.rs:11-16,27-38,56-62,122-146`

- [ ] **Step 1: Update ChunkSource::Manager to include ChunkDataIndex**

At `context.rs:11-16`:

```rust
enum ChunkSource {
    /// Fetch chunks via shared index (payload building with cached chunks).
    /// Falls back to direct storage if chunk not found (should not happen for ready txs).
    Manager {
        chunk_data_index: irys_types::chunk_provider::ChunkDataIndex,
        /// Retained for Lock/Unlock operations (if needed in future).
        sender: PdChunkSender,
    },
    /// Fetch chunks directly from storage (for block validation)
    Storage(Arc<dyn RethChunkProvider>),
}
```

- [ ] **Step 2: Update PdContext::new_with_manager**

At `context.rs:56-62`:

```rust
pub fn new_with_manager(
    chunk_sender: PdChunkSender,
    chunk_config: ChunkConfig,
    chunk_data_index: irys_types::chunk_provider::ChunkDataIndex,
) -> Self {
    Self {
        tx_hash: Arc::new(RwLock::new(None)),
        access_list: Arc::new(RwLock::new(Vec::new())),
        chunk_source: ChunkSource::Manager {
            chunk_data_index,
            sender: chunk_sender,
        },
        chunk_config,
    }
}
```

- [ ] **Step 3: Replace get_chunk Manager path with DashMap read**

At `context.rs:122-146`, replace the Manager arm:

```rust
pub fn get_chunk(&self, ledger: u32, offset: u64) -> eyre::Result<Option<Bytes>> {
    match &self.chunk_source {
        ChunkSource::Manager { chunk_data_index, .. } => {
            // Lock-free read from shared chunk data index.
            // PdService populates this during provisioning.
            match chunk_data_index.get(&(ledger, offset)) {
                Some(data) => Ok(Some((**data).clone())),
                None => {
                    tracing::warn!(
                        ledger,
                        offset,
                        "Chunk not found in shared index during payload building"
                    );
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

- [ ] **Step 4: Update clone_for_new_evm if it exists**

Check if `clone_for_new_evm` method clones the ChunkSource. If so, ensure the new `Manager` variant with its fields is properly cloned. Since `ChunkDataIndex` is `Arc<DashMap<...>>`, clone is cheap.

- [ ] **Step 5: Remove unused PdChunkMessage import from context.rs**

After removing the `GetChunk` send, the `PdChunkMessage` import is no longer used in `context.rs`. Remove it from the import line:

```rust
// Before:
use irys_types::chunk_provider::{ChunkConfig, PdChunkMessage, PdChunkSender, RethChunkProvider};
// After:
use irys_types::chunk_provider::{ChunkConfig, PdChunkSender, RethChunkProvider};
```

If `PdChunkSender` is also unused (because `sender` in the Manager variant is never accessed), add `#[allow(dead_code)]` to the `sender` field or remove it entirely. Check with: `grep -n "sender" crates/irys-reth/src/precompiles/pd/context.rs`

- [ ] **Step 6: Verify compilation**

Run: `cargo check -p irys-reth`

- [ ] **Step 7: Commit**

```
feat(pd-context): replace blocking GetChunk channel with lock-free ChunkDataIndex reads
```

---

### Task 10: Thread ChunkDataIndex through EVM factory and wiring

**Files:**
- Modify: `crates/irys-reth/src/evm.rs:382-410,443-486` (IrysEvmFactory)
- Modify: `crates/irys-reth/src/lib.rs:96-104,232-237,256-272`
- Modify: `crates/reth-node-bridge/src/node.rs:100-108,230-237`
- Modify: `crates/chain/src/chain.rs:343-350,1139-1154,1816-1821`

- [ ] **Step 1: Add ChunkDataIndex to IrysEvmFactory**

At `evm.rs:382-388`:

```rust
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct IrysEvmFactory {
    context: PdContext,
    hardfork_config: Arc<irys_types::hardfork_config::IrysHardforkConfig>,
    pd_chunk_sender: Option<irys_types::chunk_provider::PdChunkSender>,
    /// Shared chunk data index for Manager-mode PdContext.
    chunk_data_index: Option<irys_types::chunk_provider::ChunkDataIndex>,
}
```

- [ ] **Step 2: Update IrysEvmFactory::new and with_pd_chunk_sender**

Add a `with_chunk_data_index` method and update the existing builder chain:

```rust
pub fn new(
    chunk_provider: Arc<dyn irys_types::chunk_provider::RethChunkProvider>,
    hardfork_config: Arc<irys_types::hardfork_config::IrysHardforkConfig>,
) -> Self {
    let context = PdContext::new(chunk_provider);
    Self {
        context,
        hardfork_config,
        pd_chunk_sender: None,
        chunk_data_index: None,
    }
}

pub fn with_pd_chunk_sender(
    mut self,
    sender: irys_types::chunk_provider::PdChunkSender,
) -> Self {
    self.pd_chunk_sender = Some(sender);
    self
}

pub fn with_chunk_data_index(
    mut self,
    index: irys_types::chunk_provider::ChunkDataIndex,
) -> Self {
    self.chunk_data_index = Some(index);
    self
}
```

- [ ] **Step 3: Update BOTH create_evm AND create_evm_with_inspector**

There are TWO methods in `IrysEvmFactory` that create `PdContext` — `create_evm` (line ~457) and `create_evm_with_inspector` (line ~509). **Both must be updated** with the same change:

```rust
let pd_context = match (&self.pd_chunk_sender, &self.chunk_data_index) {
    (Some(sender), Some(index)) => {
        PdContext::new_with_manager(
            sender.clone(),
            self.context.chunk_config(),
            index.clone(),
        )
    }
    (Some(_), None) | (None, Some(_)) => {
        tracing::warn!("PD chunk sender and data index should both be set or both be None");
        self.context.clone_for_new_evm()
    }
    _ => self.context.clone_for_new_evm(),
};
```

Search for all `PdContext::new_with_manager` calls in `evm.rs` to confirm: `grep -n "new_with_manager" crates/irys-reth/src/evm.rs`

- [ ] **Step 4: Update IrysBlockExecutorFactory::with_pd_chunk_sender to also chain ChunkDataIndex**

In `evm.rs`, find `IrysBlockExecutorFactory::with_pd_chunk_sender` (line 231-243). It needs to also forward the chunk_data_index. Add a similar method or extend the existing one:

```rust
pub fn with_pd_chunk_sender(self, sender: irys_types::chunk_provider::PdChunkSender) -> Self {
    let receipt_builder = *self.inner.receipt_builder();
    let spec = Arc::clone(self.inner.spec());
    let evm_factory = self
        .inner
        .evm_factory()
        .clone()
        .with_pd_chunk_sender(sender);
    Self::new(receipt_builder, spec, evm_factory)
}

pub fn with_chunk_data_index(self, index: irys_types::chunk_provider::ChunkDataIndex) -> Self {
    let receipt_builder = *self.inner.receipt_builder();
    let spec = Arc::clone(self.inner.spec());
    let evm_factory = self
        .inner
        .evm_factory()
        .clone()
        .with_chunk_data_index(index);
    Self::new(receipt_builder, spec, evm_factory)
}
```

- [ ] **Step 5: Update ConfigurePdChunkSender trait or add a new trait**

The `ConfigurePdChunkSender` trait at `evm.rs:369-378` configures just the sender. Extend it to also accept ChunkDataIndex, or add a second trait. The simplest approach: add a `ConfigureChunkDataIndex` trait:

```rust
pub trait ConfigureChunkDataIndex: Sized {
    fn with_chunk_data_index(self, index: irys_types::chunk_provider::ChunkDataIndex) -> Self;
}

impl ConfigureChunkDataIndex for IrysEvmConfig {
    fn with_chunk_data_index(self, index: irys_types::chunk_provider::ChunkDataIndex) -> Self {
        Self {
            executor_factory: self.executor_factory.with_chunk_data_index(index),
            ..self
        }
    }
}
```

- [ ] **Step 6: Add ChunkDataIndex to IrysEthereumNode and thread through**

At `lib.rs:96-104`:

```rust
pub struct IrysEthereumNode {
    // ... existing fields ...
    pub readiness_index: irys_types::chunk_provider::ReadinessIndex,
    pub chunk_data_index: irys_types::chunk_provider::ChunkDataIndex,
}
```

Update `IrysExecutorBuilder` to carry and forward `chunk_data_index`:

```rust
pub struct IrysExecutorBuilder {
    chunk_provider: Arc<dyn irys_types::chunk_provider::RethChunkProvider>,
    hardfork_config: Arc<irys_types::hardfork_config::IrysHardforkConfig>,
    pd_chunk_sender: irys_types::chunk_provider::PdChunkSender,
    chunk_data_index: irys_types::chunk_provider::ChunkDataIndex,
}
```

In `build_evm`, chain `.with_chunk_data_index(self.chunk_data_index)`:

```rust
let evm_factory = IrysEvmFactory::new(self.chunk_provider, self.hardfork_config)
    .with_pd_chunk_sender(self.pd_chunk_sender)
    .with_chunk_data_index(self.chunk_data_index);
```

- [ ] **Step 7: Update payload_builder_builder.rs**

Add the field to `IrysPayloadBuilderBuilder`:

```rust
pub struct IrysPayloadBuilderBuilder {
    pub max_pd_chunks_per_block: u64,
    pub hardforks: Arc<IrysHardforkConfig>,
    pub pd_chunk_sender: PdChunkSender,
    pub readiness_index: irys_types::chunk_provider::ReadinessIndex,
    pub chunk_data_index: irys_types::chunk_provider::ChunkDataIndex,
}
```

**IMPORTANT**: Update the trait bound on the `PayloadBuilderBuilder` impl block (at `payload_builder_builder.rs:39-54`). Add `+ ConfigureChunkDataIndex` alongside the existing `+ ConfigurePdChunkSender`:

```rust
impl<Types, Node, Pool, Evm> PayloadBuilderBuilder<Node, Pool, Evm> for IrysPayloadBuilderBuilder
where
    // ... existing bounds ...
    Evm: ConfigureEvm<...>
        + ConfigurePdChunkSender
        + ConfigureChunkDataIndex  // ADD THIS
        + 'static,
```

In `build_payload_builder`, chain the chunk_data_index when configuring evm_config:

```rust
let evm_config = evm_config
    .with_pd_chunk_sender(self.pd_chunk_sender.clone())
    .with_chunk_data_index(self.chunk_data_index.clone());
```

- [ ] **Step 8: Update run_node to accept ChunkDataIndex**

At `reth-node-bridge/src/node.rs`, add parameter:

```rust
pub async fn run_node(
    // ... existing params ...
    readiness_index: irys_types::chunk_provider::ReadinessIndex,
    chunk_data_index: irys_types::chunk_provider::ChunkDataIndex,
) -> eyre::Result<(RethNodeHandle, IrysRethNodeAdapter)> {
```

Pass through to `IrysEthereumNode` construction.

- [ ] **Step 9: Update start_reth_node and chain.rs wiring**

At `chain.rs`, create the index alongside the ReadinessIndex:

```rust
let chunk_data_index: irys_types::chunk_provider::ChunkDataIndex =
    std::sync::Arc::new(dashmap::DashMap::new());
```

Pass to `start_reth_node`, `init_services` (both signature and call site), and `PdService::spawn_service`.

Use `DashMap::with_capacity(16_384)` for the ChunkDataIndex to avoid rehashing during initial population (matching the LRU cache default capacity).

- [ ] **Step 10: Update IrysEthereumNode test construction**

Same as Phase 1 — update the test helper at `crates/irys-reth/src/lib.rs` (around line 3353) to include `chunk_data_index: std::sync::Arc::new(dashmap::DashMap::new())`.

- [ ] **Step 11: Verify compilation**

Run: `cargo check --workspace --tests`

- [ ] **Step 12: Commit**

```
feat: wire ChunkDataIndex through chain → evm factory → PdContext for lock-free chunk reads
```

---

### Task 11: Update integration tests and final verification

**Files:**
- Modify: `crates/irys-reth/tests/pd_context_isolation.rs` (if it creates PdContext directly)
- All test files that construct `IrysEvmFactory`, `PdContext`, `ChunkCache`, or `PdService` directly

- [ ] **Step 1: Search for all direct PdContext constructions in tests**

Run: `grep -rn "PdContext::new_with_manager\|ChunkCache::with_default_capacity\|ChunkCache::new(" crates/ --include="*.rs"`

Update each call site to pass the new parameters.

- [ ] **Step 2: Update pd_context_isolation test**

In `crates/irys-reth/tests/pd_context_isolation.rs`, update any `PdContext::new_with_manager` calls to pass a `ChunkDataIndex`:

```rust
let chunk_data_index: irys_types::chunk_provider::ChunkDataIndex =
    std::sync::Arc::new(dashmap::DashMap::new());
PdContext::new_with_manager(sender, chunk_config, chunk_data_index)
```

- [ ] **Step 3: Run all tests**

Run: `cargo nextest run -p irys-types -p irys-actors -p irys-reth`

- [ ] **Step 4: Run integration tests**

Run: `cargo nextest run -p irys-chain`

Note: Integration tests may take longer. If specific tests fail, check whether they construct `PdService` or `IrysEthereumNode` directly and need the new index parameters.

- [ ] **Step 5: Run full CI checks**

Run: `cargo xtask local-checks`

- [ ] **Step 6: Commit any fixes**

```
chore: fix tests and clippy for Phase 2
```

---

## Risk Notes

1. **DashMap memory**: The `ChunkDataIndex` DashMap is bounded by the LRU cache size because every insert/remove/eviction is mirrored. Default LRU capacity is 16,384 entries. The DashMap overhead per entry is ~100 bytes (the `Arc<Bytes>` chunk data is shared, not duplicated). Total overhead: ~1.6 MB. Use `DashMap::with_capacity(16_384)` to avoid rehashing.

2. **Stale reads**: A DashMap read could see `true` for readiness just as PdService transitions to a different state. This is safe because the Lock mechanism (still channel-based) is the true synchronization point for EVM execution. A stale `true` means the tx gets included in the candidate set but may fail during EVM execution (handled by existing `mark_invalid` flow).

3. **Event listener ordering**: The `new_transactions_listener_for(All)` fires on pool insertion. If a transaction is inserted and immediately replaced before the listener fires, we might miss the initial `NewTransaction` notification. The `all_transactions_event_listener` catches the replacement event. This edge case is handled correctly because we only send `TransactionRemoved` for hashes in `known_pd_txs`.

4. **Lock/Unlock messages**: These are NOT removed in this plan. They remain channel-based because they mutate PdService state (cache lock counts). They are called infrequently (once per tx before/after EVM execution) and are orthogonal to the hot path optimized here. If Lock/Unlock are not currently wired into the payload builder (they appear unused in the iterator), this is a no-op concern.

5. **`FullTransactionEvent` import**: Verify the exact import path in the reth-irys fork. The type may be at `reth_transaction_pool::FullTransactionEvent` or a different path. Check with `grep -rn "FullTransactionEvent" ~/.cargo/registry/src/` or the reth-irys source.
