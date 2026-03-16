# Codex Second Opinion: PD Chunk P2P Pull Design Spec

> **Note (2026-03-16):** Findings from this review have been integrated into the design spec. The design doc is the authoritative source for implementation.

**Date**: 2026-03-13
**Reviewer**: OpenAI Codex (gpt-5.4) via `codex exec`
**Spec reviewed**: `docs/superpowers/specs/2026-03-13-pd-chunk-p2p-pull-design.md`

## Overall Assessment

Overall direction is reasonable: keeping fetch ownership in `PdService` is the right shape. The weak points are the response authenticity model, cancellation boundaries, and serving-side cost control.

## Findings

### 1. Response authenticity gap (Critical)

The design does not authenticate that the response is the chunk for the requested `(ledger, offset)`. The proposed `PdChunk` response reuses `GossipDataV2::Chunk(Arc<UnpackedChunk>)`, and the spec's validation checks only that the returned chunk is internally consistent with its own `data_root`/`data_path`. But `UnpackedChunk` does not carry the requested ledger position, so a malicious peer can return a different valid chunk and still pass that validation, after which PdService would cache it under the wrong key.

**References**: `crates/types/src/chunk.rs:52`, `crates/types/src/gossip.rs:243`

#### Resolution: tx_path proof (Approach B)

**Chosen fix**: The serving peer returns the `tx_path` (merkle proof from `tx_root` → `data_root`) alongside the `UnpackedChunk`. The requesting node verifies the full proof chain:

```
ledger_offset → BlockIndex::get_block_bounds() → tx_root (known, trusted)
  → validate_path(tx_root, tx_path, chunk_byte_offset_in_block) → proves data_root is at correct position
    → validate_path(data_root, data_path, chunk_byte_offset_in_tx) → proves chunk membership
      → SHA256(bytes) == leaf_hash → binds raw data to proof
```

This is the same proof chain that PoA validation already uses (`block_validation.rs:1194`), reapplied to the PD pull context.

**Wire format change**: Add a new `GossipDataV2::PdChunk(Arc<UnpackedChunk>, TxPath)` response variant. The `GossipDataRequestV2::PdChunk(u32, u64)` request variant stays the same. V1 peers return `None` for PdChunk requests (existing behavior).

**Serving side cost**: Cheap. The storage module already reads `tx_path` when constructing a chunk by offset (to extract `data_root` in `generate_full_chunk`, `storage_module.rs:1262`). The submodule DB has a direct `offset → tx_path_hash → tx_path` index (two MDBX gets, `submodule/db.rs:54-62`). This is returning already-indexed bytes, not computing a proof on demand.

**Requester side cost**: One `BlockIndex::get_block_bounds(ledger, offset)` call (binary search over MDBX, already used in PoA validation) plus two `validate_path` calls per chunk. The `get_block_bounds` result can be cached when fetching multiple chunks from the same block.

**BlockIndex availability guarantee**: The BlockIndex always has the committing block when a PD tx references that offset. `persist_block()` atomically writes the block header, tx headers, and BlockIndex entry (`block_migration_service.rs:242-296`). Only afterward does `ChunkMigrationService::BlockMigrated` fire to write chunk data into storage modules (`chunk_migration_service.rs:224`, `storage_module.rs:984`). Since PD txs can only reference chunks that exist in storage, and chunks only exist in storage after migration, the committing block is guaranteed to be in the BlockIndex. (`PD-readable ⟹ BlockIndex present` is always true.)

**Batch optimization**: `tx_path` is identical for all chunks belonging to the same DataTransaction. When fetching a range of chunks from the same tx, the proof only needs to be transmitted and verified once per tx boundary.

**New method on `ChunkStorageProvider`**: The existing `get_full_unpacked_chunk_by_ledger_offset` (proposed in the spec) should be extended to also return the `tx_path`:

```rust
fn get_full_unpacked_chunk_with_tx_path(
    &self,
    ledger: u32,
    ledger_offset: u64,
) -> eyre::Result<Option<(UnpackedChunk, TxPath)>>;
```

The implementation calls `generate_full_chunk_ledger_offset` (which already reads `tx_path` internally) but preserves and returns the `tx_path` bytes instead of discarding them.

**Requester-side verification in PdService `on_fetch_done`**:

```rust
// 1. Get the trusted tx_root from BlockIndex
let bounds = block_index.get_block_bounds(DataLedger::Publish, LedgerChunkOffset::from(key.offset))?;

// 2. Compute the chunk's byte offset within the block's tx tree
//    (offset relative to block start, converted to bytes)
let offset_in_block = key.offset - bounds.start_chunk_offset;
let target_byte_offset = (offset_in_block + 1) * chunk_size; // end-byte convention

// 3. Verify tx_path: tx_root → data_root
let tx_path_result = validate_path(bounds.tx_root.0, &tx_path, target_byte_offset as u128)?;
// tx_path_result.leaf_hash should equal SHA256(data_root || data_size_note)
// which confirms chunk.data_root is at the correct position in the block

// 4. Verify data_path: data_root → chunk bytes (already in the spec)
let data_path_result = validate_path(chunk.data_root.0, &chunk.data_path, chunk_end_byte)?;
ensure!(SHA256(chunk.bytes) == data_path_result.leaf_hash);
```

#### Alternatives considered and rejected

**Approach A — Derive expected `data_root` from block state locally**: The requester would call `BlockIndex::get_block_bounds` to find the committing block, load the block header to get `tx_ids`, then load each `DataTransactionHeader` from MDBX to find the `data_root` at the target offset by accumulating chunk counts. This is cryptographically sound (the `data_root` values are from the trusted local DB), but turns every cache miss into multiple DB reads (1 block bounds lookup + N tx header reads, where N is the number of txs in that block's ledger). Approach B is strictly better: it shifts the proof burden to the server (which already has the data in hand) and requires only the single `get_block_bounds` call on the requester side. Both Codex and Claude independently recommended B over A.

**Approach C — Trust authenticated peers (gossip handshake staking verification)**: The spec already requires serving peers to pass staking verification via the gossip handshake. However, authenticated peers provide admission control, not consensus-critical correctness. A Byzantine or compromised staker can return a valid chunk from a different position — it would pass merkle validation against its own `data_root` but deliver wrong data for the requested offset. PD execution feeds directly into EVM state transitions; it cannot inherit a "trust the peer" model. Both reviewers agreed this is insufficient as the primary correctness mechanism.

### 2. Retry/backoff sleep blocks the actor (Important)

The spec says `on_fetch_done` will "sleep briefly" and then respawn on `AllPeersFailed`, but `PdService` is explicitly a synchronous actor loop polling `msg_rx` and `join_set.join_next()`. Sleeping there either blocks the entire actor or requires turning the handler async in a way the current design does not show. Backoff needs to live in a spawned task or a separate timer-driven state machine, not inside the actor's completion handler.

**References**: `crates/actors/src/pd_service.rs:69`

**Claude's take**: Valid. The fix is simple: instead of sleeping in `on_fetch_done`, spawn a `tokio::time::sleep` task into the JoinSet that returns a "retry" sentinel, or add a `tokio::time::interval` arm to the select loop for pending retries.

#### Resolution: Actor-owned `DelayQueue` retry arm

**Chosen fix**: Separate retry scheduling from fetch execution. Active fetches stay in the `JoinSet`; retry timers live in a `tokio_util::time::DelayQueue` owned by the actor. This gives PdService full ownership of retry state — retries are data in a queue, not detached tasks or sleeping sentinels mixed with fetch results.

**How it works**:

1. `on_fetch_done` receives `AllPeersFailed` from the `JoinSet`.
2. It checks whether waiters still exist for the chunk key. If none remain, it cleans up and skips retry.
3. If waiters remain, it computes the backoff delay (`min(1s * 2^attempt, 30s)`), inserts a `RetryEntry { key, attempt, generation, excluded_peers }` into the `DelayQueue`, and stores the returned `delay_queue::Key` in `PdChunkFetchState` for eager cancellation.
4. It transitions the fetch state to `FetchPhase::Backoff`.
5. When the `DelayQueue` fires (via a dedicated `select!` arm), the handler validates the `generation` matches, re-resolves peers, and spawns a new fetch task into the `JoinSet`, transitioning back to `FetchPhase::Fetching`.

**Select loop with retry arm**:

```rust
loop {
    tokio::select! {
        biased;

        _ = &mut self.shutdown => { break; }

        msg = self.msg_rx.recv() => {
            match msg {
                Some(message) => self.handle_message(message),
                None => { break; }
            }
        }

        Some(result) = self.fetch_join_set.join_next() => {
            self.on_fetch_done(result);
        }

        Some(expired) = self.retry_queue.next(), if !self.retry_queue.is_empty() => {
            self.on_retry_ready(expired.into_inner());
        }
    }
}
```

**`on_fetch_done` retry path (pseudocode)**:

```rust
fn on_fetch_done(&mut self, result: JoinResult<PdChunkFetchResult>) {
    let PdChunkFetchResult { key, result } = /* unwrap join result */;
    match result {
        Ok(chunk) => { /* cache chunk, notify waiters */ }
        Err(PdChunkFetchError::AllPeersFailed { excluded_peers }) => {
            let Some(state) = self.pending_fetches.get_mut(&key) else { return };

            // No waiters left — skip retry, clean up
            if state.waiting_txs.is_empty() && state.waiting_blocks.is_empty() {
                self.pending_fetches.remove(&key);
                return;
            }

            // Exceeded max retries — fail permanently
            if state.attempt >= MAX_CHUNK_FETCH_RETRIES {
                self.fail_chunk_permanently(key);
                return;
            }

            // Schedule retry via DelayQueue
            let delay = backoff_duration(state.attempt); // min(1s * 2^attempt, 30s)
            let retry_entry = RetryEntry {
                key,
                attempt: state.attempt + 1,
                generation: state.generation,
                excluded_peers,
            };
            let queue_key = self.retry_queue.insert(retry_entry, delay);

            state.attempt += 1;
            state.status = FetchPhase::Backoff;
            state.retry_queue_key = Some(queue_key);
            state.excluded_peers = excluded_peers;
        }
    }
}
```

**`on_retry_ready` handler (pseudocode)**:

```rust
fn on_retry_ready(&mut self, entry: RetryEntry) {
    let Some(state) = self.pending_fetches.get_mut(&entry.key) else { return };

    // Stale generation — this retry belongs to an old provisioning lifecycle
    if state.generation != entry.generation { return; }

    // Waiters vanished during backoff
    if state.waiting_txs.is_empty() && state.waiting_blocks.is_empty() {
        self.pending_fetches.remove(&entry.key);
        return;
    }

    // Re-resolve peers (picks up epoch changes, new peers coming online)
    let peers = self.resolve_peers_for_chunk(&entry.key, &entry.excluded_peers);
    let abort_handle = self.fetch_join_set.spawn(
        fetch_chunk_from_peers(entry.key, peers, self.storage_provider.clone())
    );

    state.status = FetchPhase::Fetching;
    state.abort_handle = Some(abort_handle);
    state.retry_queue_key = None;
}
```

**Changes to `PdChunkFetchState`**:

```rust
/// Phase of a single chunk key's fetch lifecycle.
enum FetchPhase {
    Fetching,
    Backoff,
}

struct PdChunkFetchState {
    waiting_txs: HashSet<B256>,
    waiting_blocks: HashSet<B256>,
    attempt: u32,
    /// Distinguishes provisioning lifecycles. Incremented when a key
    /// re-enters Fetching after being cleaned up and reprovisioned.
    /// Prevents stale retry timers from acting on a new lifecycle.
    generation: u64,
    excluded_peers: HashSet<IrysAddress>,
    status: FetchPhase,
    /// Handle to cancel the in-flight fetch task (when Fetching).
    abort_handle: Option<AbortHandle>,
    /// Handle to cancel the pending retry timer (when Backoff).
    retry_queue_key: Option<delay_queue::Key>,
}
```

**Cancellation path**: When all waiters for a key are removed (tx removed + block released), PdService checks `status`:
- `Fetching` → call `abort_handle.abort()` to cancel the in-flight fetch task
- `Backoff` → call `self.retry_queue.remove(&queue_key)` to cancel the pending timer

Both are eager cancellation — no dangling tasks or timers survive waiter cleanup.

**Backpressure**: Pending retries are entries in a queue, not spawned tasks. Queue length is bounded by the number of tracked chunk keys, which is itself bounded by mempool + active validation limits. Under adversarial churn, the queue grows linearly with tracked keys, not with retry attempts.

**Observability**: All retry state lives in actor-owned structs. Operators can log: current phase, attempt count, generation, next retry deadline, excluded peer count, and queue depth — all from the actor's synchronous state.

#### Alternatives considered and rejected

**Approach A — Sleep-in-JoinSet (spawn sleep task returning RetryReady sentinel)**: Spawn `async { sleep(delay).await; RetryReady { key, attempt, ... } }` into the same `JoinSet` used for fetches. The `join_next()` arm dispatches on the enum variant. This keeps everything under one supervised surface and provides cancellation via `AbortHandle`. However, it mixes two different semantics (IO completion vs. timer expiry) in one `JoinSet`, which hurts observability and creates a fairness concern: with `biased` select, expired retry sentinels and actual fetch completions compete within the same arm. More critically, sleeping tasks are still runtime tasks — under adversarial churn with many chunk keys in backoff, each consumes a tokio task slot, whereas `DelayQueue` entries are pure data. Viable if hardened with a `generation` field and explicit `FetchPhase`, but strictly dominated by the `DelayQueue` approach on backpressure and observability.

**Approach B — Fire-and-forget spawn + send-message-back (matches `peer_network_service.rs:618`)**: Spawn a detached `tokio::spawn(async { sleep(delay).await; sender.send(RetryChunkFetch { ... }) })`. This matches an existing codebase pattern used for handshake retries. However, it breaks actor ownership of the retry lifecycle: detached sleepers are not cancellable (waiter checks are lazy, on message arrival only), and the unbounded channel becomes an amplification path under churn — sleeping tasks accumulate and later flood the message queue. Fine for fire-and-forget control-path nudges (handshakes), but chunk provisioning is a resource-tracked pipeline where retry state must be actor-owned. Both Codex (gpt-5.4) and Claude independently rejected this approach for L1 use.

### 3. `tokio::join!` doesn't cancel sibling tasks (Important)

The cancellation model ignores the more common case where another validation branch already made the block invalid. `validate_block()` uses `tokio::join!` across six tasks, so a failing POA/recall/fees task does not cancel `shadow_transactions_are_valid`; it will keep waiting on PD chunk fetches until they finish or the block becomes "too old". With local-only provisioning that was cheap; with network pulls it becomes a bandwidth and runtime amplification path for invalid blocks.

**References**: `crates/actors/src/validation_service/block_validation_task.rs:551`

**Claude's take**: This is a legitimate concern but bounded — the fetch tasks themselves have finite per-peer timeouts (the gossip client has a 5-second reqwest timeout). So "bandwidth amplification" is real but bounded per-block to `num_missing_chunks * num_assigned_peers * 5s`. Still worth noting in the spec.

#### Resolution: CancellationToken within `tokio::join!`

**Chosen fix**: Introduce a shared `tokio_util::sync::CancellationToken` across all six validation tasks inside `validate_block()`. Every task wrapper both **signals** the token on definitive failure and **listens** for cancellation from siblings. The `tokio::join!` structure is unchanged — cancellation propagates through the token, not through structural restructuring.

**How it works**:

1. `validate_block()` creates a `CancellationToken` before the `tokio::join!`.
2. Each of the 6 tasks is wrapped in a `tokio::select!` that races the original task against `cancel.cancelled()`.
3. When any task completes with a definitive failure (`ValidationResult::Invalid` or `Err`), the wrapper calls `cancel.cancel()`.
4. The token wakes all sibling wrappers' `cancelled()` futures. On the next `tokio::join!` poll, each wrapper's `select!` sees `cancelled()` ready and resolves immediately, **dropping the inner future**.
5. When `shadow_tx_task`'s inner future is dropped, the `ProvisionBlockChunks` oneshot receiver is dropped. PdService detects this and eagerly cancels the in-flight fetch via `abort_handle.abort()` or removes the pending retry via `retry_queue.remove()` (Issue #2's cascade).
6. `tokio::join!` resolves within one poll cycle. Cancelled tasks return `ValidationCancelled` sentinels; the real failure is surfaced by the existing "find first invalid" logic.

**Uniform wrapper for `ValidationResult` tasks**:

```rust
async fn with_cancel(
    fut: impl Future<Output = ValidationResult>,
    cancel: CancellationToken,
) -> ValidationResult {
    tokio::select! {
        result = fut => {
            if matches!(&result, ValidationResult::Invalid(_)) {
                cancel.cancel();
            }
            result
        }
        _ = cancel.cancelled() => {
            ValidationResult::Invalid(ValidationError::ValidationCancelled {
                reason: "sibling validation failed".into(),
            })
        }
    }
}
```

**Application in `validate_block()`**:

```rust
let cancel = CancellationToken::new();

// Fast tasks — uniform wrapping (signal + listen)
let recall_task     = with_cancel(recall_task,     cancel.clone());
let poa_task        = with_cancel(poa_task,        cancel.clone());
let seeds_task      = with_cancel(seeds_task,      cancel.clone());
let commitment_task = with_cancel(commitment_task, cancel.clone());
let data_txs_task   = with_cancel(data_txs_task,   cancel.clone());

// Shadow_tx — same pattern, different return type (eyre::Result)
let shadow_tx_task = {
    let cancel = cancel.clone();
    async move {
        tokio::select! {
            result = shadow_tx_task => {
                if result.is_err() { cancel.cancel(); }
                result
            }
            _ = cancel.cancelled() => {
                Err(eyre::eyre!("cancelled: sibling validation task failed"))
            }
        }
    }
};

// tokio::join! unchanged
let (recall, poa, shadow, seeds, commitment, data_txs) = tokio::join!(
    recall_task, poa_task, shadow_tx_task,
    seeds_task, commitment_task, data_txs_task
);
```

**Cancellation cascade for an invalid block** (e.g., PoA fails at t=50ms, PD fetch in progress):

```
t=50ms  PoA completes → Invalid → wrapper calls cancel.cancel()
t=50ms  Token wakes all sibling cancelled() futures
t=50ms  tokio::join! polls each wrapper:
          - shadow_tx: select! picks cancelled() → drops inner future
              → oneshot receiver dropped
              → PdService: abort_handle.abort() (Issue #2 cascade)
          - recall/seeds/commitment/data_txs: already completed (Valid)
              or: select! picks cancelled() → ValidationCancelled
t=50ms  tokio::join! resolves
        Result tuple: PoA=Invalid(real error), shadow=Err(cancelled), others=Valid/Cancelled
        Existing logic at line 629 surfaces the PoA error
```

**Phantom invalids**: When the token fires, still-running fast tasks return `ValidationCancelled` instead of their real result. This is harmless — the block is already definitively invalid, and the "find first invalid" logic surfaces the real error (the task that triggered cancellation completed with the actual error before calling `cancel.cancel()`). The `ValidationCancelled` sentinels are never reported as the primary failure reason.

**Dependency**: `tokio_util::sync::CancellationToken` — already on the dependency path via Issue #2's `tokio_util::time::DelayQueue`.

**No signature changes**: The token is created and consumed entirely within `validate_block()`. The `with_cancel` helper is a local async function (or closure). No changes to `shadow_transactions_are_valid`, `poa_is_valid`, or any other validation function signature.

#### Alternatives considered and rejected

**Approach A — `tokio::select!` racing fast-task group vs shadow_tx (structural split)**: Group the 5 fast tasks into one `tokio::join!` future, race it against `shadow_tx_task` via `tokio::select!`. If the fast group resolves first with any `Invalid`, `shadow_tx_task` is dropped. This achieves the same cancellation cascade and preserves parallelism for valid blocks. However, it introduces a "fast track / slow track" mental model that bifurcates the validation structure. The `tokio::join!` must be restructured into nested `select!` + `join!`, and the two branches have different control flow (one returns early, the other awaits the remaining future). The CancellationToken approach avoids this structural change — every task participates uniformly in the same protocol, and the `tokio::join!` is unchanged.

**Approach B — `tokio::spawn` + explicit `AbortHandle`**: Spawn `shadow_tx_task` as a detached tokio task to obtain an `AbortHandle`. Run fast tasks via `tokio::join!`, then abort the handle if any fails. This gives explicit cancellation control but requires `'static` bounds on the spawned future. `shadow_tx_task` captures `&self` (borrows from `BlockValidationTask`), so all referenced data would need to be cloned or Arc-wrapped. It also breaks structured concurrency — the spawned task is detached from the validation task's lifetime, creating cleanup concerns on panic paths.

**Approach C — Two-phase sequential (fast checks first, then shadow_tx)**: Run all 5 fast tasks to completion, check results, then only start `shadow_tx_task` if all passed. Simplest approach and eliminates wasted PD fetches entirely. However, it serializes validation — for valid blocks, total wall-clock time becomes `fast_tasks_time + shadow_tx_time` instead of `max(fast_tasks_time, shadow_tx_time)`. The shadow_tx path includes computation beyond PD fetches (expected shadow tx generation, execution data building) that currently overlaps with PoA and other fast tasks. This latency penalty on the happy path is unacceptable for block validation throughput.

### 4. Missing AbortHandles for fetch task cancellation (Important)

The spec says PdService will abort per-key fetch tasks when waiters disappear, but the proposed state does not actually retain anything abortable. `PdChunkFetchState` tracks waiters, attempts, and excluded peers, but not a `JoinSet` task ID or `AbortHandle`, so "abort fetch tasks in JoinSet" is not implementable as written. Related: receiver-drop cleanup is only observed on send or before respawn, so a cancelled block can remain attached to in-flight work for a full request timeout or longer.

**Claude's take**: Valid. `PdChunkFetchState` should store the `AbortHandle` returned by `JoinSet::spawn`. When all waiters for a key are removed, call `abort_handle.abort()` to cancel the in-flight fetch immediately.

### 5. Serving side is unthrottled (Important)

`/gossip/v2/pull_data` is a poor serving surface without additional protection. The synchronous pull path (`handle_get_data_sync`) bypasses the `DataRequestTracker` checks used by `handle_get_data`, and the proposed handler would perform storage lookup plus unpacking inline on the Actix server side. Frequent `PdChunk` requests can turn into an expensive unthrottled endpoint.

**References**: `crates/p2p/src/server.rs:1422`, `crates/p2p/src/gossip_data_handler.rs:875`

**Claude's take**: Valid concern. The fix should add rate-limiting for `PdChunk` requests specifically — either extend `DataRequestTracker` to cover pull_data, or add a semaphore similar to `chunk_semaphore` used for inbound chunk gossip. The unpacking work could also be offloaded to a blocking task pool.

### 6. "No explicit timeout" isn't actually bounded (Moderate)

`exit_if_block_is_too_old` is progress-based; it only cancels when the tip moves far enough and a block-state update wakes the waiter. If the network is stalled or the node is near tip but peers simply do not serve the chunk, the block validation can hang indefinitely while fetch rounds keep retrying.

**References**: `crates/actors/src/validation_service/block_validation_task.rs:174`

**Claude's take**: Partially valid. In practice, the mempool path doesn't need a timeout (tx gets evicted by Reth's pool maintenance). For validation, if the network is stalled, no new blocks arrive, so the node isn't doing useful work anyway. But a safety timeout (e.g., 60s) would be prudent as defense-in-depth. Could be added as a max retry count rather than wall-clock timeout.

### 7. Cache reference propagation missing for deduped waiters (Moderate)

`ChunkCache::insert` pins exactly one `B256` reference; additional consumers require `add_reference`. The spec's `on_fetch_done` pseudocode updates waiting tx/block state after insertion, but it does not say how all waiting blocks and txs acquire cache references. Without that, a later `TransactionRemoved` or `ReleaseBlockChunks` can drop the only reference while other consumers still depend on the chunk.

**References**: `crates/actors/src/pd_service/cache.rs:77`

**Claude's take**: This is a spec documentation gap, not a design flaw. The `on_fetch_done` handler must call `cache.insert(key, data, first_waiter_id)` then `cache.add_reference(key, other_waiter_id)` for each additional waiter. The spec should spell this out explicitly.

## Additional Suggestions from Codex

- Freeze peer-resolution inputs per provisioning request instead of re-reading `canonical_epoch_snapshot()` inside retries. Other validation code explicitly snapshots epoch data to avoid mid-validation races (e.g., `block_validation.rs:2177`).
- If there is already some PD-side proof that binds an access-list range to an expected `data_root` or path hash, finding 1 could be mitigated through that binding.
