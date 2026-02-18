# ChunkIngressService Extraction — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Extract all chunk validation, caching, storage writes, and ingress proof handling from the mempool into a new `ChunkIngressService` actor.

**Architecture:** A new actor with its own channel in `ServiceSenders`/`ServiceReceivers` receives all chunk and ingress proof messages. The mempool retains only a one-way fire-and-forget notification (`ProcessPendingChunks`) sent when a data TX header is ingested.

**Tech Stack:** Rust, tokio channels (`UnboundedSender`/`UnboundedReceiver`), `oneshot` for request-response, LRU caches, existing Irys actor pattern.

**Design doc:** `docs/plans/2026-02-18-chunk-ingress-extraction-design.md`

---

### Task 1: Add `chunk_ingress` channel to ServiceSenders/ServiceReceivers

This task adds the new channel plumbing so the new service can receive messages. No service implementation yet — just the channel infrastructure.

**Files:**
- Modify: `crates/actors/src/services.rs`
- Create: `crates/actors/src/chunk_ingress_service.rs` (just the message enum for now)

**Step 1: Create the message enum file**

Create `crates/actors/src/chunk_ingress_service.rs` with the message type:

```rust
use irys_types::{chunk::UnpackedChunk, DataRoot, IngressProof};
use tokio::sync::oneshot;

use crate::mempool_service::{ChunkIngressError, IngressProofError};

/// Messages handled by the ChunkIngressService
#[derive(Debug)]
pub enum ChunkIngressMessage {
    /// Ingest a chunk with a response channel for the result
    IngestChunk(
        UnpackedChunk,
        oneshot::Sender<Result<(), ChunkIngressError>>,
    ),
    /// Ingest a chunk without waiting for a response (fire-and-forget)
    IngestChunkFireAndForget(UnpackedChunk),
    /// Ingest an ingress proof received from a peer
    IngestIngressProof(
        IngressProof,
        oneshot::Sender<Result<(), IngressProofError>>,
    ),
    /// Process pending chunks for a data root after its TX header was ingested.
    /// Sent by the mempool when a data TX is successfully validated.
    ProcessPendingChunks(DataRoot),
}
```

**Step 2: Register the module in lib.rs**

In `crates/actors/src/lib.rs`, add `pub mod chunk_ingress_service;` after the `cache_service` module declaration (line 8).

**Step 3: Add the channel to ServiceSendersInner and ServiceReceivers**

In `crates/actors/src/services.rs`:

- Add import: `use crate::chunk_ingress_service::ChunkIngressMessage;`
- Add to `ServiceSendersInner` struct (after `chunk_migration` field, ~line 113):
  ```rust
  pub chunk_ingress: UnboundedSender<ChunkIngressMessage>,
  ```
- Add to `ServiceReceivers` struct (after `chunk_migration` field, ~line 91):
  ```rust
  pub chunk_ingress: UnboundedReceiver<ChunkIngressMessage>,
  ```
- Add channel creation in `ServiceSendersInner::init()` (after `chunk_migration` channel, ~line 136):
  ```rust
  let (chunk_ingress_sender, chunk_ingress_receiver) =
      unbounded_channel::<ChunkIngressMessage>();
  ```
- Add `chunk_ingress: chunk_ingress_sender` to the senders struct literal
- Add `chunk_ingress: chunk_ingress_receiver` to the receivers struct literal

**Step 4: Verify it compiles**

Run: `cargo xtask check`

**Step 5: Commit**

```
git add crates/actors/src/chunk_ingress_service.rs crates/actors/src/lib.rs crates/actors/src/services.rs
git commit -m "refactor: add ChunkIngressMessage enum and channel plumbing"
```

---

### Task 2: Create ChunkIngressService actor skeleton

Create the service struct with `spawn_service` and message loop, but with empty/stub handlers. The handlers will be populated by moving code in later tasks.

**Files:**
- Modify: `crates/actors/src/chunk_ingress_service.rs`

**Step 1: Implement the service skeleton**

Replace the contents of `crates/actors/src/chunk_ingress_service.rs` with the full service struct, spawn function, and message dispatch loop. The struct fields should mirror the dependencies chunk handling needs:

```rust
pub struct ChunkIngressServiceInner {
    pub block_tree_read_guard: BlockTreeReadGuard,
    pub config: Config,
    pub exec: TaskExecutor,
    pub irys_db: DatabaseProvider,
    pub service_senders: ServiceSenders,
    pub storage_modules_guard: StorageModulesReadGuard,
    pub recent_valid_chunks: tokio::sync::RwLock<LruCache<ChunkPathHash, ()>>,
    pub pending_chunks: tokio::sync::RwLock<PriorityPendingChunks>,
}
```

The `spawn_service` function should follow the exact pattern used by `MempoolService::spawn_service` in `crates/actors/src/mempool_service.rs:3061-3132`:
- Accept `UnboundedReceiver<ChunkIngressMessage>`, `DatabaseProvider`, `StorageModulesReadGuard`, `BlockTreeReadGuard`, `Config`, `ServiceSenders`, `tokio::runtime::Handle`
- Create a `Shutdown` signal pair
- Spawn a tokio task on the provided runtime handle
- Return `TokioServiceHandle`

The message loop should match against each `ChunkIngressMessage` variant and log `todo!()` or `tracing::warn!("not yet implemented")` for now. Do NOT use `todo!()` panics — use warn logs and send back errors on the oneshot channels so callers don't hang.

**Step 2: Verify it compiles**

Run: `cargo xtask check`

**Step 3: Commit**

```
git add crates/actors/src/chunk_ingress_service.rs
git commit -m "refactor: add ChunkIngressService actor skeleton with spawn_service"
```

---

### Task 3: Move chunk handling code from mempool to ChunkIngressService

This is the core extraction. Move `chunks.rs`, `ingress_proofs.rs`, and `pending_chunks.rs` from the mempool module into the chunk ingress service module.

**Files:**
- Move: `crates/actors/src/mempool_service/chunks.rs` → `crates/actors/src/chunk_ingress_service/chunks.rs`
- Move: `crates/actors/src/mempool_service/ingress_proofs.rs` → `crates/actors/src/chunk_ingress_service/ingress_proofs.rs`
- Move: `crates/actors/src/mempool_service/pending_chunks.rs` → `crates/actors/src/chunk_ingress_service/pending_chunks.rs`
- Modify: `crates/actors/src/chunk_ingress_service.rs` → convert to `crates/actors/src/chunk_ingress_service/mod.rs`
- Modify: `crates/actors/src/mempool_service.rs` (remove module declarations)

**Step 1: Convert chunk_ingress_service to a directory module**

- Create directory `crates/actors/src/chunk_ingress_service/`
- Move `crates/actors/src/chunk_ingress_service.rs` → `crates/actors/src/chunk_ingress_service/mod.rs`
- Add sub-module declarations to `mod.rs`:
  ```rust
  pub mod chunks;
  pub mod ingress_proofs;
  pub mod pending_chunks;
  ```

**Step 2: Copy the three files**

- Copy `crates/actors/src/mempool_service/chunks.rs` → `crates/actors/src/chunk_ingress_service/chunks.rs`
- Copy `crates/actors/src/mempool_service/ingress_proofs.rs` → `crates/actors/src/chunk_ingress_service/ingress_proofs.rs`
- Copy `crates/actors/src/mempool_service/pending_chunks.rs` → `crates/actors/src/chunk_ingress_service/pending_chunks.rs`

Do NOT delete from mempool yet — both copies will exist temporarily.

**Step 3: Update imports in the copied files**

In each copied file, change `impl Inner` to `impl ChunkIngressServiceInner` and update module paths:
- `use crate::mempool_service::Inner` → remove (it's now `super::ChunkIngressServiceInner`)
- `self.mempool_state` → `self.recent_valid_chunks` / `self.pending_chunks` (direct field access instead of going through `AtomicMempoolState`)
- `self.mempool_state.is_a_recent_valid_chunk(...)` → `self.recent_valid_chunks.read().await.contains(...)`
- `self.mempool_state.record_recent_valid_chunk(...)` → `self.recent_valid_chunks.write().await.put(..., ())`
- `self.mempool_state.put_chunk(chunk)` → `self.pending_chunks.write().await.put(chunk)`
- `self.mempool_state.pop_pending_chunks_cache(...)` → `self.pending_chunks.write().await.pop(...)`
- `self.mempool_state.pending_chunk_count_for_data_root(...)` → `self.pending_chunks.read().await.get(...).map(LruCache::len).unwrap_or(0)`

The key insight: chunk code only accessed `recent_valid_chunks` and `pending_chunks` on MempoolState. Those two fields now live directly on `ChunkIngressServiceInner` as `tokio::sync::RwLock`-wrapped fields, so the access pattern is nearly identical.

**Step 4: Wire up the message handlers in mod.rs**

Replace the stub handlers in the message loop with actual calls:
```rust
ChunkIngressMessage::IngestChunk(chunk, response) => {
    let result = inner.handle_chunk_ingress_message(chunk).await;
    let _ = response.send(result);
}
ChunkIngressMessage::IngestChunkFireAndForget(chunk) => {
    let _ = inner.handle_chunk_ingress_message(chunk).await;
}
ChunkIngressMessage::IngestIngressProof(proof, response) => {
    let result = inner.handle_ingest_ingress_proof(proof);
    let _ = response.send(result);
}
ChunkIngressMessage::ProcessPendingChunks(data_root) => {
    inner.process_pending_chunks_for_root(data_root).await;
}
```

Add a `process_pending_chunks_for_root` method to `ChunkIngressServiceInner` (moved from `data_txs.rs:444-470`):
```rust
async fn process_pending_chunks_for_root(&self, data_root: DataRoot) {
    let option_chunks_map = self.pending_chunks.write().await.pop(&data_root);
    if let Some(chunks_map) = option_chunks_map {
        let chunks: Vec<_> = chunks_map.into_iter().map(|(_, chunk)| chunk).collect();
        for chunk in chunks {
            if let Err(err) = self.handle_chunk_ingress_message(chunk).await {
                tracing::error!(
                    "Failed to handle pending chunk ingress for data_root {:?}: {:?}",
                    data_root, err
                );
            }
        }
    }
}
```

**Step 5: Verify it compiles**

Run: `cargo xtask check`

This step may require iterating on imports. The most common issues will be:
- Missing imports for types used in the chunk/proof code
- `self.mempool_state` references that need updating to direct field access
- Module path changes (`crate::mempool_service::X` → `crate::chunk_ingress_service::X` or `super::X`)

**Step 6: Commit**

```
git add crates/actors/src/chunk_ingress_service/
git commit -m "refactor: move chunk and ingress proof code to ChunkIngressService"
```

---

### Task 4: Remove chunk code from mempool

Now that the chunk handling code lives in ChunkIngressService, remove it from the mempool.

**Files:**
- Delete: `crates/actors/src/mempool_service/chunks.rs`
- Delete: `crates/actors/src/mempool_service/ingress_proofs.rs`
- Delete: `crates/actors/src/mempool_service/pending_chunks.rs`
- Modify: `crates/actors/src/mempool_service.rs`
- Modify: `crates/actors/src/mempool_service/data_txs.rs`

**Step 1: Remove module declarations from mempool_service.rs**

In `crates/actors/src/mempool_service.rs`, remove the `mod chunks;`, `mod ingress_proofs;`, and `mod pending_chunks;` declarations.

**Step 2: Remove chunk-related variants from MempoolServiceMessage**

In `crates/actors/src/mempool_service.rs`, remove these variants from the `MempoolServiceMessage` enum (~lines 207-212):
- `IngestChunk(...)`
- `IngestChunkFireAndForget(...)`
- `IngestIngressProof(...)`

Also remove their match arms in the message handler (~lines 342-361, 421-426) and their `Display`/name match arms (~lines 280-282).

**Step 3: Remove chunk-related fields from MempoolState**

In `crates/actors/src/mempool_service.rs`:
- Remove `recent_valid_chunks` field from `MempoolState` (~line 2702)
- Remove `pending_chunks` field from `MempoolState` (~line 2705)
- Remove their initialization in `create_state()` (~lines 2729-2733)
- Remove `storage_modules_guard` from `Inner` struct (~line 195)

**Step 4: Remove chunk-related methods from AtomicMempoolState**

Remove these methods from the `AtomicMempoolState` impl block:
- `is_a_recent_valid_chunk` (~lines 2079-2084)
- `record_recent_valid_chunk` (~lines 2120-2125)
- `put_chunk` (~lines 2106-2109)
- `pending_chunk_count_for_data_root` (~lines 2111-2118)
- `pop_pending_chunks_cache` (~lines 2275-2280)

**Step 5: Update data_txs.rs**

In `crates/actors/src/mempool_service/data_txs.rs`:
- In `postprocess_data_ingress()` (~line 90), replace:
  ```rust
  self.process_pending_chunks_for_root(tx.data_root).await?;
  ```
  with:
  ```rust
  let _ = self.service_senders.chunk_ingress.send(
      crate::chunk_ingress_service::ChunkIngressMessage::ProcessPendingChunks(tx.data_root),
  );
  ```
- Remove the `process_pending_chunks_for_root` method (~lines 442-470) entirely.
- Remove any now-unused imports (e.g., `record_chunk_error` if it was only used there).

**Step 6: Delete the old files**

```bash
rm crates/actors/src/mempool_service/chunks.rs
rm crates/actors/src/mempool_service/ingress_proofs.rs
rm crates/actors/src/mempool_service/pending_chunks.rs
```

**Step 7: Verify it compiles**

Run: `cargo xtask check`

Expect compile errors from external callers that still reference `MempoolServiceMessage::IngestChunk` etc. Those are fixed in the next task.

**Step 8: Commit (if it compiles — otherwise combine with Task 5)**

```
git add -A
git commit -m "refactor: remove chunk handling code from mempool service"
```

---

### Task 5: Update all callers to use ChunkIngressService

Route all chunk and ingress proof messages to the new service's channel instead of the mempool's channel.

**Files:**
- Modify: `crates/api-server/src/routes/post_chunk.rs`
- Modify: `crates/api-server/src/lib.rs`
- Modify: `crates/actors/src/mempool_service/facade.rs`
- Modify: `crates/p2p/src/gossip_data_handler.rs`
- Modify: `crates/actors/src/data_sync_service.rs`
- Modify: `crates/actors/src/block_validation.rs`
- Modify: `crates/chain/src/chain.rs`
- Modify: `crates/chain/tests/utils.rs`

**Step 1: Update ApiState and post_chunk**

In `crates/api-server/src/lib.rs`, add a field to `ApiState`:
```rust
pub chunk_ingress: UnboundedSender<ChunkIngressMessage>,
```

In `crates/api-server/src/routes/post_chunk.rs`, change the send from `MempoolServiceMessage::IngestChunk` to `ChunkIngressMessage::IngestChunk`:
```rust
use irys_actors::chunk_ingress_service::ChunkIngressMessage;
// ...
let tx_ingress_msg = ChunkIngressMessage::IngestChunk(chunk, oneshot_tx);
if let Err(err) = state.chunk_ingress.send(tx_ingress_msg) {
```

**Step 2: Split MempoolFacade — move chunk/proof methods**

In `crates/actors/src/mempool_service/facade.rs`:

Remove `handle_chunk_ingress` and `handle_ingest_ingress_proof` from the `MempoolFacade` trait and its `MempoolServiceFacadeImpl`.

Create a new `ChunkIngressFacade` trait and impl. This can live in `crates/actors/src/chunk_ingress_service/mod.rs` or a new `facade.rs` sub-module. The trait:

```rust
#[async_trait::async_trait]
pub trait ChunkIngressFacade: Clone + Send + Sync + 'static {
    async fn handle_chunk_ingress(&self, chunk: UnpackedChunk) -> Result<(), ChunkIngressError>;
    async fn handle_ingest_ingress_proof(
        &self,
        ingress_proof: IngressProof,
    ) -> Result<(), IngressProofError>;
}
```

With an impl that sends to the `chunk_ingress` channel (same pattern as `MempoolServiceFacadeImpl`).

**Step 3: Update GossipDataHandler**

In `crates/p2p/src/gossip_data_handler.rs`:

The `GossipDataHandler` struct is generic over `TMempoolFacade: MempoolFacade`. Since `handle_chunk_ingress` and `handle_ingest_ingress_proof` will no longer be on `MempoolFacade`, we need to add a `TChunkIngress: ChunkIngressFacade` type parameter, OR we can make the `GossipDataHandler` hold a separate field for chunk ingress.

Recommended approach: Add a `chunk_ingress` field of type `UnboundedSender<ChunkIngressMessage>` directly (simpler than adding another generic). Then:
- `handle_chunk` sends `ChunkIngressMessage::IngestChunk` via the new field
- `handle_ingress_proof` sends `ChunkIngressMessage::IngestIngressProof` via the new field

Update the constructor and all places that create `GossipDataHandler` to pass the new sender.

**Step 4: Update DataSyncService**

In `crates/actors/src/data_sync_service.rs` (~line 261), change:
```rust
.send(crate::MempoolServiceMessage::IngestChunkFireAndForget(chunk))
```
to:
```rust
.send(crate::chunk_ingress_service::ChunkIngressMessage::IngestChunkFireAndForget(chunk))
```

Using `service_senders.chunk_ingress` instead of `service_senders.mempool`.

**Step 5: Update BlockValidationService**

In `crates/actors/src/block_validation.rs` (~line 2464), change:
```rust
.send(crate::MempoolServiceMessage::IngestChunk(unpacked, ing_tx))
```
to:
```rust
.send(crate::chunk_ingress_service::ChunkIngressMessage::IngestChunk(unpacked, ing_tx))
```

Using `service_senders.chunk_ingress` instead of `service_senders.mempool`.

**Step 6: Update chain.rs wiring**

In `crates/chain/src/chain.rs`:
- Import `ChunkIngressService` and `ChunkIngressMessage`
- After the mempool service spawn (~line 1427), spawn the ChunkIngressService:
  ```rust
  let chunk_ingress_handle = ChunkIngressService::spawn_service(
      irys_db.clone(),
      storage_modules_guard.clone(),
      &block_tree_guard,
      receivers.chunk_ingress,
      &config,
      &service_senders,
      runtime_handle.clone(),
  )?;
  ```
- Remove `storage_modules_guard` from `MempoolService::spawn_service` arguments (it no longer needs it)
- Update `ApiState` construction to include `chunk_ingress: service_senders.chunk_ingress.clone()`
- Update `GossipDataHandler` construction to include the chunk_ingress sender
- Add `chunk_ingress_handle` to the service handles vec

**Step 7: Update test utils**

In `crates/chain/tests/utils.rs`:
- Change `MempoolServiceMessage::IngestChunk` (~line 2083) → `ChunkIngressMessage::IngestChunk` using `service_senders.chunk_ingress`
- Change `MempoolServiceMessage::IngestIngressProof` (~line 2592) → `ChunkIngressMessage::IngestIngressProof` using `service_senders.chunk_ingress`

**Step 8: Update re-exports in lib.rs**

In `crates/actors/src/lib.rs`, the `pub use mempool_service::*;` re-exports chunk types. Since `ChunkIngressError`, `CriticalChunkIngressError`, `AdvisoryChunkIngressError`, `IngressProofError` now live under `chunk_ingress_service`, add:
```rust
pub use chunk_ingress_service::*;
```

And remove any chunk-related re-exports that were coming from `mempool_service`.

**Step 9: Verify it compiles**

Run: `cargo xtask check`

This is the step most likely to require iteration. Watch for:
- Import paths that still reference `MempoolServiceMessage::IngestChunk`
- The `MempoolFacade` trait still being expected to have chunk methods somewhere
- Generic parameter changes in `GossipDataHandler` rippling through `GossipServer`, `BlockPool`, etc.

**Step 10: Commit**

```
git add -A
git commit -m "refactor: route all chunk and proof messages to ChunkIngressService"
```

---

### Task 6: Clean up and verify

Final verification pass: ensure no dead code, run full checks.

**Step 1: Remove unused imports**

Run: `cargo clippy --workspace --tests --all-targets`

Fix any unused import warnings in:
- `mempool_service.rs` (chunk-related types no longer needed)
- `facade.rs` (chunk error types no longer needed)
- `data_txs.rs` (chunk-related imports no longer needed)

**Step 2: Format**

Run: `cargo fmt --all`

**Step 3: Run tests**

Run: `cargo xtask test`

Focus on tests that exercise chunk ingestion:
- Any tests in `crates/actors/src/mempool_service.rs` that test chunk handling (these should now be in `chunk_ingress_service` or removed)
- Integration tests in `crates/chain/tests/` that ingest chunks
- Tests in `pending_chunks.rs` (should compile and pass in new location)

**Step 4: Run local-checks**

Run: `cargo xtask local-checks`

Fix any issues.

**Step 5: Final commit**

```
git add -A
git commit -m "refactor: clean up chunk ingress extraction"
```

---

## Dependency Graph

```
Task 1 (channel plumbing)
    └──► Task 2 (service skeleton)
              └──► Task 3 (move chunk code)
                        └──► Task 4 (remove from mempool)
                                  └──► Task 5 (update callers)
                                            └──► Task 6 (clean up + verify)
```

All tasks are strictly sequential — each builds on the previous.
