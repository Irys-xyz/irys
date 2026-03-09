---
date: 2026-03-09T14:30:00+0000
researcher: rob
git_commit: 08827966
branch: rob/reth-pd-mempool-2
repository: irys
topic: "PD Chunk Pipeline: Architecture Audit — Performance, Correctness, Concurrency"
tags: [audit, pd-chunks, reth, mempool, block-building, validation, precompile, concurrency]
status: complete
last_updated: 2026-03-09
last_updated_by: rob
---

# PD Chunk Pipeline — Architecture Audit Report

**Date**: 2026-03-09
**Branch**: rob/reth-pd-mempool-2
**Scope**: Rust performance, correctness, concurrency, antipatterns across the full PD subsystem

---

## Critical Issues (Fix Immediately)

### 1. Zero Gas Metering for PD Precompile I/O

**File**: `crates/irys-reth/src/precompiles/pd/read_bytes.rs:345`

Every PD precompile call costs a flat 5000 gas regardless of how many chunks it reads. `gas_used` is always returned as `0` from `read_bytes_range`. An attacker can read 65,535 chunks (~16GB) for 5000 gas — the primary EVM anti-DoS mechanism is completely bypassed.

```rust
// read_bytes.rs:344-349
Ok(PrecompileOutput {
    gas_used: 0,       // <--- Always zero regardless of work done
    gas_refunded: 0,
    bytes: extracted,
    reverted: false,
})
```

The `_gas_limit` parameter passed to `read_bytes_range_by_index` and `read_partial_byte_range` is never consulted (note the `_` prefix).

**Recommendation**: Charge gas proportional to the number of chunks fetched and bytes returned. Validate gas budget *before* performing I/O work.

### 2. Unbounded Memory Allocation from `chunk_count`

**File**: `crates/irys-reth/src/precompiles/pd/read_bytes.rs:280`

```rust
Vec::with_capacity((*chunk_count as u64 * chunk_size) as usize);
```

`chunk_count` is `u16` (max 65535). With 256KB chunks, this allocates **~16GB** in a single call, with no gas check preventing it (see #1). The EVM's access list gas cost model limits to ~15,000 keys at 30M gas, but combined with zero gas metering this is fully exploitable.

**Recommendation**: Add a hard cap on chunk count per precompile call. Use `checked_mul` and validate against gas budget before allocation.

### 3. `block_tracker` HashMap Never Cleaned Up

**File**: `crates/actors/src/pd_service.rs:33`

```rust
block_tracker: HashMap<B256, Vec<ChunkKey>>,
```

If `ReleaseBlockChunks` is never sent (caller bug, crash during validation, dropped channel), the block_tracker entry and its pinned cache chunks persist **forever**. No TTL exists for block tracker entries, unlike the provisioning tracker which has `expire_at_height`.

The chunk references in the cache for these blocks also persist forever (pinned by the block_hash reference in `referencing_txs`), preventing eviction and contributing to #4.

**Recommendation**: Add a TTL mechanism for `block_tracker` entries, or at minimum clean up entries older than N blocks in `handle_block_state_update`.

### 4. LRU Cache Capacity Grows But Never Shrinks

**File**: `crates/actors/src/pd_service/cache.rs:174-180`

```rust
} else {
    // All entries are pinned — grow capacity to avoid evicting a pinned entry.
    // The capacity will shrink back naturally as pinned entries are released and evicted.
    let new_cap =
        NonZeroUsize::new(self.chunks.cap().get() + 1).expect("cap + 1 must be non-zero");
    self.chunks.resize(new_cap);
}
```

The comment claims "capacity will shrink back naturally" — this is **false**. No code anywhere calls `resize()` to reduce the capacity. Once `make_room_for_insert` grows the LRU capacity, it stays at the enlarged size permanently.

Each cached 256KB chunk entry is `Arc<Bytes>`, so growing from 16K to 32K entries adds ~4GB of allowed memory. Combined with #3, this creates monotonic memory growth proportional to runtime.

**Recommendation**: After each removal, check if `self.chunks.len() < DEFAULT_CACHE_CAPACITY` and resize down. Add a hard ceiling (e.g., 2x default).

### 5. `ReleaseBlockChunks` Not Sent on Cancellation/Panic

**File**: `crates/actors/src/validation_service.rs`

Multiple paths skip cleanup:
- JoinSet panic handling (lines 301-337) does not send `ReleaseBlockChunks` for the panicked task's block
- Service shutdown (lines 207-209) drops the JoinSet without cleanup
- Validation cancellation paths

These all leak `block_tracker` entries (feeding #3).

**Recommendation**: Implement RAII guard pattern (`PdChunkGuard`) that sends `ReleaseBlockChunks` on drop.

---

## High Severity

### 6. Synchronous Disk I/O Blocks the Async Runtime

**File**: `crates/actors/src/pd_service.rs:248-285` (and lines 411-441)

```rust
self.storage_provider
    .get_unpacked_chunk_by_ledger_offset(key.ledger, key.offset)
```

`get_unpacked_chunk_by_ledger_offset` performs disk I/O (reading from storage modules) and CPU-intensive `irys_packing::unpack()` (SHA256-based entropy computation) **synchronously** inside a `tokio::select!` loop. The `RethChunkProvider` trait has no `async` methods.

For a PD transaction requiring 100 chunks at 256KB each, this blocks the thread for tens of milliseconds while reading 25MB from disk. During this time, no messages are processed — the `select!` loop is blocked, creating head-of-line blocking for all other PdService operations including latency-sensitive `IsReady` and `GetChunk` queries.

**Recommendation**: Either:
1. Use `tokio::task::spawn_blocking` for the I/O portions
2. Make the trait async
3. Run PdService on a dedicated thread (not on the tokio runtime)

### 7. No Timeout on `blocking_recv` — Can Hang Forever

**File**: `crates/irys-reth/src/precompiles/pd/context.rs:136`, `crates/irys-reth/src/payload.rs:575`

Both the EVM precompile and payload builder use `block_in_place(|| resp_rx.blocking_recv())` with **no timeout**. If PdService is slow, deadlocked, or panicked, these block indefinitely — hanging block production.

```rust
// context.rs:136
let chunk = tokio::task::block_in_place(|| resp_rx.blocking_recv())
    .map_err(|e| eyre::eyre!("Failed to receive chunk response: {}", e))?;

// payload.rs:574-575
if let Ok(false) = tokio::task::block_in_place(|| resp_rx.blocking_recv()) {
```

Note: The `block_in_place` usage itself is correct — Reth's payload builder runs on tokio's blocking thread pool via `spawn_blocking`, so `block_in_place` is semantically a no-op. No deadlock risk from that pattern. The issue is purely the missing timeout.

**Recommendation**: Add timeouts — 100ms for `IsReady`, 1-5s for `GetChunk`. Treat timeouts as "not ready" / execution failure.

### 8. Divide-by-Zero in `translate_offset`

**File**: `crates/types/src/range_specifier.rs:253`

```rust
let additional_chunks = full_offset.div(chunk_size);  // panics if chunk_size == 0
let new_byte_offset = U18::try_from(full_offset % chunk_size)?;  // also panics
```

If `chunk_size == 0`, line 253 panics with a division by zero. While `ChunkConfig.chunk_size` is typically 256000, there is no guard at any call site. The `.div()` call uses `std::ops::Div` which is just the `/` operator — no checked division.

**Recommendation**: Use `NonZeroU64` for `chunk_size` in `ChunkConfig` to make this a compile-time guarantee, or add an explicit check at the top of `translate_offset`.

### 9. MockChunkProvider Permanently Baked Into Executor

**File**: `crates/chain/src/chain.rs:1146-1153`

```rust
let mock_provider = irys_types::chunk_provider::MockChunkProvider::new();
let (node_handle, reth_node) = match start_reth_node(
    // ...
    Arc::new(mock_provider),  // <-- mock passed here, never replaced
)
```

The `IrysExecutorBuilder` receives a `MockChunkProvider` (returns zero-filled chunks) that is **never replaced** with the real provider. Block validation through the executor path gets incorrect chunk data.

The real `ChunkProvider` is only created much later at `chain.rs:1813`. The TODO at line 1145 confirms this is known but unresolved.

**Impact**: During block validation via Reth's executor, the `PdContext` in `Storage` mode calls `mock_provider.get_unpacked_chunk_by_ledger_offset()`, which returns zero-filled chunks instead of real data.

**Recommendation**: Either (a) the executor's `IrysEvmFactory` should receive a `pd_chunk_sender` so it fetches from the PdService cache during validation, or (b) the real `ChunkProvider` should be passed to `start_reth_node` instead of the mock.

---

## Medium Severity

### 10. Mempool Monitor Polling Misses Short-Lived Transactions

**File**: `crates/irys-reth/src/mempool.rs:457-574`

The `pd_transaction_monitor` uses 100ms `tokio::time::interval` polling + `pool.all_transactions()` diff against a local `HashSet<B256>`. Any PD tx that enters and leaves the pool within one tick is invisible:

1. **Fast replacement**: User submits PD tx, then within <100ms submits a replacement (same sender + nonce, higher gas). Original PD tx evicted before next poll.
2. **Rapid block inclusion**: On fast chain cadence, PD tx enters pending pool and is included in a block within one poll interval.
3. **Eviction under load**: Under high mempool contention, transactions discarded within a single tick.

The `PdChunkManager` never receives `NewTransaction` for these transactions.

**Recommendation**: Switch to `all_transactions_event_listener()` — a stream-based API Reth already provides. Eliminates the polling gap and reduces CPU overhead from repeated full-pool scans + PD header parsing of every transaction.

### 11. `biased` Select Starves Broadcast Channel

**File**: `crates/actors/src/pd_service.rs:75-110`

```rust
tokio::select! {
    biased;
    _ = &mut self.shutdown => { ... }
    msg = self.msg_rx.recv() => { ... }
    result = self.block_state_rx.recv() => { ... }
}
```

The `biased` keyword means branches are checked in order. The `msg_rx` (unbounded channel) is checked before `block_state_rx` (broadcast). Under sustained message traffic:
1. `current_height` never advances
2. TTL expiration (`expire_at_height`) never fires
3. Stale provisioning entries accumulate indefinitely

The broadcast channel has a finite buffer. If events accumulate, the `RecvError::Lagged(n)` path (line 101) fires — the service logs a warning but those heights are lost.

**Recommendation**: Either remove `biased`, move `block_state_rx` above `msg_rx`, or drain the broadcast first before processing messages.

### 12. Priority Inversion in Single-Channel Architecture

**File**: `crates/types/src/chunk_provider.rs:84`

All 4 producers (mempool monitor, payload builder, precompile, validator) share one unbounded FIFO channel. A bulk `ProvisionBlockChunks` with disk I/O queues behind all pending `NewTransaction` messages. The latency-sensitive `IsReady`/`GetChunk` queries from the payload builder are blocked behind bulk operations with no priority mechanism.

**Recommendation**: Separate read-only queries from provisioning commands — use two channels or a priority channel so `IsReady`/`GetChunk` are never blocked behind `NewTransaction`/`ProvisionBlockChunks` I/O.

### 13. Silent Fallthrough on Channel Failure Includes Unverified Txs

**File**: `crates/irys-reth/src/payload.rs:565-582`

```rust
if self.pd_chunk_sender.send(PdChunkMessage::IsReady { ... }).is_ok() {
    if let Ok(false) = tokio::task::block_in_place(|| resp_rx.blocking_recv()) {
        continue; // Skip this transaction
    }
}
// Falls through to budget check and inclusion
```

Three failure modes all silently fall through to **include** the transaction:
1. `send()` returns `Err` (receiver dropped / PdService shut down)
2. `blocking_recv()` returns `Err(RecvError)` (oneshot sender dropped)
3. The `if let Ok(false)` pattern only matches `Ok(false)` — `Err(_)` falls through

**Recommendation**: Treat all channel failures as "not ready":

```rust
let is_ready = {
    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
    self.pd_chunk_sender
        .send(PdChunkMessage::IsReady { tx_hash, response: resp_tx })
        .is_ok()
        && tokio::task::block_in_place(|| resp_rx.blocking_recv()).unwrap_or(false)
};
if !is_ready { continue; }
```

### 14. Head-of-Line Blocking in Deferred Queue

**File**: `crates/irys-reth/src/payload.rs:454-465`

```rust
fn pop_ready_deferred(...) -> Option<...> {
    if let Some(candidate) = self.deferred.front() {
        let chunks = chunk_counter(candidate.as_ref());
        if self.can_fit(chunks) {
            return self.deferred.pop_front();
        }
    }
    None
}
```

Only checks the **front** element. A single large PD tx (e.g., 600 chunks) that doesn't fit permanently blocks smaller txs behind it (200, 100 chunks) that would fit.

**Recommendation**: Scan the deferred queue for the first transaction that fits:

```rust
fn pop_ready_deferred(...) -> Option<...> {
    let pos = self.deferred.iter().position(|candidate| {
        self.can_fit(chunk_counter(candidate.as_ref()))
    })?;
    self.deferred.remove(pos)
}
```

### 15. Priority Ordering Mixes Incomparable Units

**File**: `crates/irys-reth/src/mempool.rs:409-442`

```rust
let effective = gas_tip.saturating_add(pd_total_tip);
```

`gas_tip` is in **wei per gas** (a per-unit rate). `pd_total_tip` is a **total amount** (per-chunk fee × chunk count, in scaled-1e18 IRYS tokens). Adding these produces a number not meaningful in any single unit system.

The ordering is technically transitive (won't crash or produce cycles), but economically wrong — PD fee stuffing gets priority at below-market gas rates.

**Recommendation**: Either normalize both to a common unit (e.g., total value in wei) or use a two-level comparator.

### 16. Malformed PD Headers Bypass Validator Prefilter

**File**: `crates/irys-reth/src/mempool.rs:270-284`

When `detect_and_decode_pd_header` returns `Err` (magic matches but deserialization fails), the `Ok(Some(_))` pattern doesn't match, so the tx passes through to the regular ETH validator without PD fee checks.

Compare with shadow tx detection (lines 252-267) which correctly rejects on `Err(_)` as well.

**Recommendation**: Handle `Err` the same as shadow tx detection — reject malformed PD headers.

### 17. `ii()` Inclusive Interval with Half-Open Iteration

**File**: `crates/irys-reth/src/precompiles/pd/read_bytes.rs:60-74, 267-282`

```rust
// Line 74: builds inclusive-inclusive interval [start, start+chunk_count]
Ok(LedgerChunkRange::from(ledger_chunk_offset_ii!(start, end)))

// Lines 267-282: iterates with half-open range
for ledger_offset in *ledger_start..*ledger_end {
```

Works by accident: `*start..*end` = `start..(start+chunk_count)` produces the correct count. But the `LedgerChunkRange` claims to be inclusive, containing `chunk_count + 1` points. Any consumer using nodit's inclusive semantics would process one extra element.

**Recommendation**: Use `ie()` (inclusive-exclusive) instead of `ii()`.

### 18. VDF Notify 5-Second Stall

**File**: `crates/actors/src/validation_service.rs:350`

A documented-but-unsolved bug. The 5-second periodic `notify_one()` is a workaround for a `Notify` permit consumption race between `VdfScheduler::submit` and the main service loop's `notified()` call. This adds up to 5 seconds of latency per VDF task start.

**Recommendation**: Replace `Notify` with `tokio::sync::watch` channel or `mpsc(1)`.

### 19. `LruCache::get_mut` Promotes on Reference Removal

**File**: `crates/actors/src/pd_service/cache.rs:114-121`

```rust
pub fn remove_reference(&mut self, key: &ChunkKey, tx_hash: &B256) -> bool {
    if let Some(entry) = self.chunks.get_mut(key) {  // <-- promotes to MRU
        entry.referencing_txs.remove(tx_hash);
        // ...
    }
}
```

`get_mut()` promotes the entry to the MRU position. When a transaction is released, all referenced chunks get promoted — making them the *last* candidates for eviction. Semantically backwards: just-unreferenced chunks should be the *most* appropriate eviction candidates.

Same issue in `lock_chunks` and `unlock_chunks` (lines 124-138).

**Recommendation**: Use `peek_mut()` instead of `get_mut()` — the `lru` crate provides this for exactly this purpose.

### 20. `lock()` Returns `true` for Expired/Unknown PD Transactions

**File**: `crates/actors/src/pd_service.rs:341-350`

```rust
fn handle_lock(&mut self, tx_hash: &B256) -> bool {
    if !self.tracker.lock(tx_hash) { return false; }
    // tracker.lock() returns true for unknown tx hashes ("allow lock for non-PD transactions")
    if let Some(state) = self.tracker.get(tx_hash) {
        self.cache.lock_chunks(&state.required_chunks);
    }
    true  // Returns true even when no chunks were actually locked
}
```

If a PD transaction was expired by TTL but the caller still tries to lock it, the caller gets `true` and proceeds to execute, but the chunks may have been evicted from the cache. The subsequent `GetChunk` calls would then return `None`, causing execution failures.

### 21. `parse_access_list` Silently Succeeds with Zero Valid Entries

**File**: `crates/irys-reth/src/precompiles/pd/utils.rs:15-41`

An attacker can submit a non-empty access list where all storage keys are garbage. `parse_access_list` succeeds returning empty vectors. The precompile then fails with a confusing "not found" error rather than "invalid encoding" error.

The `Err(_) => continue` pattern silently swallows decode errors.

**Recommendation**: Return an error if no valid PD entries were found after parsing. Log individual decode failures.

### 22. `clone_for_new_evm` Copies Stale Transaction State

**File**: `crates/irys-reth/src/precompiles/pd/context.rs:80-87`

```rust
pub fn clone_for_new_evm(&self) -> Self {
    Self {
        tx_hash: Arc::new(RwLock::new(*self.tx_hash.read())),
        access_list: Arc::new(RwLock::new(self.access_list.read().clone())),
        chunk_source: self.chunk_source.clone(),
        chunk_config: self.chunk_config,
    }
}
```

Copies current `tx_hash` and `access_list` into the new context. Correctness depends on the caller remembering to update the transaction state after cloning via `set_current_tx`/`update_access_list`. No compile-time enforcement.

If the caller forgets to call `update_access_list`, the new EVM uses the previous transaction's access list, enabling a cross-transaction data leak or incorrect chunk access.

### 23. Full Access List Clone Per Precompile Call

**File**: `crates/irys-reth/src/precompiles/pd/context.rs:152-154`

```rust
pub fn read_access_list(&self) -> Vec<AccessListItem> {
    self.access_list.read().clone()
}
```

Every precompile invocation deep-clones the entire access list (potentially large `Vec<B256>` storage keys) under a read lock. On the EVM execution hot path.

**Recommendation**: Use `Arc<Vec<AccessListItem>>` so `read_access_list` returns a cheap `Arc::clone`.

---

## Low Severity

### 24. Unbounded Channel — No Backpressure

**File**: `crates/types/src/chunk_provider.rs:84`

```rust
pub type PdChunkSender = mpsc::UnboundedSender<PdChunkMessage>;
```

Four producers can enqueue messages without limit. All senders use `let _ = sender.send(...)` which silently discards errors if the channel is closed but never applies backpressure.

**Recommendation**: Switch to bounded channel with reasonable capacity (e.g., 1024). Requires senders to handle `SendError` or use `try_send`.

### 25. `saturating_add/sub` on `lock_count` Silently Masks Bugs

**File**: `crates/actors/src/pd_service/cache.rs:127, 136`

```rust
entry.lock_count = entry.lock_count.saturating_add(1);
entry.lock_count = entry.lock_count.saturating_sub(1);
```

If `lock_count` reaches `u32::MAX`, saturating_add stops incrementing. If unlock is called more than lock, saturating_sub clamps to 0. Both represent programming errors that should be loud.

**Recommendation**: Use `debug_assert!` or logging instead of silent saturation.

### 26. `register` Silently Drops Different Chunk Sets for Same tx_hash

**File**: `crates/actors/src/pd_service/provisioning.rs:56-74`

Uses `entry.or_insert_with(...)` — if called with an existing tx_hash but different `required_chunks`, the new chunk set is silently discarded.

### 27. `PartiallyReady` State is Terminal — No Recovery Path

**File**: `crates/actors/src/pd_service/provisioning.rs:18-19`

Once a transaction enters `PartiallyReady`, no code path ever transitions it to `Ready`. The transaction stays until TTL expiration, and the chunks that *were* successfully fetched remain referenced in cache, consuming memory.

### 28. PdService Shuts Down Before Reth Consumers

**File**: `crates/chain/src/chain.rs:1950-1988`

PdService is at shutdown position 4 (Storage operations) while Reth is at position 8. If payload builder or EVM precompile are mid-execution when PdService shuts down, their `blocking_recv` calls will receive `RecvError`.

### 29. Hardcoded `ledger: 0` Everywhere

**Files**: `crates/actors/src/pd_service.rs:194`, `crates/irys-reth/src/precompiles/pd/read_bytes.rs:284`

```rust
keys.insert(ChunkKey { ledger: 0, offset });
context.get_chunk(0, ledger_offset)
```

No multi-ledger support. Not enforced at the type level or documented.

### 30. O(n) Eviction Scan on Every Insert at Capacity

**File**: `crates/actors/src/pd_service/cache.rs:159-181`

When the cache is at capacity (steady-state under load), every `insert` performs a full reverse iteration over the LRU to find an evictable entry. With 16,384 entries, this is O(n) per insert.

**Recommendation**: Maintain a count of pinned entries. If `pinned_count == capacity`, skip the scan. Alternatively, maintain a secondary set of unpinned keys for O(1) eviction.

### 31. `PdArgsEncWrapper::from` Panics on Invalid Input

**File**: `crates/types/src/range_specifier.rs:146, 164`

```rust
impl<T: PdAccessListArgSerde> From<&[u8; 32]> for PdArgsEncWrapper<T> {
    fn from(bytes: &[u8; 32]) -> Self {
        Self(T::decode(bytes).expect("Invalid byte encoding"))  // panics
    }
}
```

Both `From` implementations use `.expect()`. If any caller uses these with untrusted data, the node crashes.

**Recommendation**: Replace `From` with `TryFrom`.

### 32. Potential `usize` Overflow in Bounds Check

**File**: `crates/irys-reth/src/precompiles/pd/read_bytes.rs:318`

```rust
if offset_usize + length_usize > bytes.len() {
```

If `offset_usize + length_usize` overflows `usize`, this wraps around and the check incorrectly passes. Extremely unlikely on 64-bit, but defensive programming calls for `checked_add`.

### 33. Broadcast Lag Discards Block Heights, Delays TTL Expiry

**File**: `crates/actors/src/pd_service.rs:101-102`

```rust
Err(broadcast::error::RecvError::Lagged(n)) => {
    warn!(skipped = n, "PdService lagged on block state events");
}
```

Missing block state events means `current_height` is stale, causing TTL entries to expire later than intended.

**Recommendation**: On lag, query current block height from an authoritative source and run `expire_at_height` to catch up.

### 34. Double `detect_and_decode_pd_header` Call in Validator

**File**: `crates/irys-reth/src/mempool.rs:270-320`

The `prefilter_tx` method calls `detect_and_decode_pd_header(input)` **twice** for every transaction when Sprite is active, and `is_sprite_active()` is also called twice (each reading `SystemTime::now()`).

**Recommendation**: Compute once, branch on results.

### 35. `HashSet<B256>` Per Cache Entry is Allocation-Heavy

**File**: `crates/actors/src/pd_service/cache.rs:23`

Each `CachedChunkEntry` contains a heap-allocated `HashSet`. Typically referenced by 1-3 transactions. A `SmallVec<[B256; 2]>` would avoid heap allocation for the common case.

### 36. Unnecessary `HashSet` Clone on Provision

**File**: `crates/actors/src/pd_service.rs:230`

```rust
self.tracker.register(tx_hash, required_chunks.clone(), self.current_height);
```

The `HashSet` is cloned before passing to `register`, then immediately iterated. Could be avoided by restructuring to pass ownership.

### 37. Silently Dropped Oneshot Responses

**File**: `crates/actors/src/pd_service.rs:129, 133, 144`

```rust
let _ = response.send(ready);
```

Using `let _` discards the `Result`. If the receiver timed out, the response is silently lost with no visibility into timeout rates.

### 38. `SenderId::from(0)` TODO for Shadow Transactions

**File**: `crates/irys-reth/src/payload.rs:393`

```rust
transaction_id: TransactionId::new(SenderId::from(0), tx.nonce()),
// todo - recheck the SenderId::from(0)
```

If two shadow transactions have the same nonce, their `TransactionId` will collide. Should be resolved.

---

## Top Architectural Recommendations

### 1. Separate Read-Only Queries from Provisioning Commands

Use two channels (or a priority channel) so `IsReady`/`GetChunk` are never blocked behind `NewTransaction`/`ProvisionBlockChunks` I/O. The latency-sensitive payload builder path should never wait behind bulk chunk provisioning.

### 2. Move Disk I/O to `spawn_blocking`

The PdService should remain an async message router; chunk fetching/unpacking should happen on a blocking thread pool with completion callbacks. This eliminates the head-of-line blocking that currently allows a single bulk provisioning request to stall all queries.

### 3. Add RAII Guard for `ReleaseBlockChunks`

Wrap the block hash in a guard that sends the release message on drop, eliminating all cleanup-path bugs across validation cancellation, panics, and shutdown.

### 4. Add Timeouts to All `blocking_recv` Calls

100ms for `IsReady`, 1-5s for `GetChunk`. Treat timeouts as "not ready" / execution failure. This prevents PdService unavailability from cascading into block production hangs.

### 5. Replace Mempool Polling with Event Stream

`all_transactions_event_listener()` eliminates the 100ms temporal gap and reduces CPU overhead from repeated full-pool scans with PD header parsing of every transaction.

### 6. Implement Gas Metering for the PD Precompile

Charge per-chunk and per-byte gas, validated *before* performing I/O. This is the primary defense against DoS through the precompile.

---

## Verified Non-Issues

- **`block_in_place` deadlock risk**: No deadlock. Reth's payload builder runs on tokio's blocking thread pool; `block_in_place` is a no-op there.
- **CombinedTransactionIterator duplicate yielding**: Not possible. Shadow txs are popped from VecDeque, pool txs consumed from iterator, deferred txs moved between VecDeque and pool. No overlap between the three sources.
- **PdChunkBudget overflow**: `saturating_add` prevents arithmetic overflow. `U256` multiplication uses `saturating_mul`. Budget accounting is internally consistent.
- **Payload ID hash collisions**: PayloadId is 8 bytes (per Ethereum Engine API spec). Birthday collision probability is ~2^32. Per-node, per-session namespace makes this acceptable.
- **`known_pd_txs` memory growth in monitor**: Replaced wholesale each tick; bounded by pool size.

---

## Summary by Component

| Component | Critical | High | Medium | Low |
|-----------|----------|------|--------|-----|
| PD Precompile | 2 (#1, #2) | 1 (#8) | 5 (#17, #21, #22, #23, #17) | 3 (#31, #32, #29) |
| PdService / Cache | 2 (#3, #4) | 1 (#6) | 3 (#11, #19, #20) | 6 (#25, #26, #27, #30, #35, #36) |
| Payload Builder | — | 1 (#7) | 3 (#13, #14, #15) | 1 (#38) |
| Mempool Monitor | — | — | 2 (#10, #16) | 1 (#34) |
| Validation Service | 1 (#5) | — | 1 (#18) | — |
| Channel Wiring | — | 1 (#9) | 1 (#12) | 3 (#24, #28, #33) |
