# Codex Second Opinion: PD Chunk P2P Pull Design Spec

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

### 3. `tokio::join!` doesn't cancel sibling tasks (Important)

The cancellation model ignores the more common case where another validation branch already made the block invalid. `validate_block()` uses `tokio::join!` across six tasks, so a failing POA/recall/fees task does not cancel `shadow_transactions_are_valid`; it will keep waiting on PD chunk fetches until they finish or the block becomes "too old". With local-only provisioning that was cheap; with network pulls it becomes a bandwidth and runtime amplification path for invalid blocks.

**References**: `crates/actors/src/validation_service/block_validation_task.rs:551`

**Claude's take**: This is a legitimate concern but bounded — the fetch tasks themselves have finite per-peer timeouts (the gossip client has a 5-second reqwest timeout). So "bandwidth amplification" is real but bounded per-block to `num_missing_chunks * num_assigned_peers * 5s`. Still worth noting in the spec.

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
