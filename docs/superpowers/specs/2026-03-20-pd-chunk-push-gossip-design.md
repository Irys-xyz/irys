# PD Chunk Push Gossip Design

**Date**: 2026-03-20
**Branch**: `feat/pd`
**Issues**: [#1231](https://github.com/Irys-xyz/irys/issues/1231), [#1217](https://github.com/Irys-xyz/irys/issues/1217)

## Problem

PD chunk fetching is pull-only: when a node needs chunks for PD transaction validation, it queries the ~10 partition assignees who store that data. In a 100+ validator network, all validators needing the same chunks simultaneously creates a thundering herd on those few assignees.

## Solution

After a node validates a block containing PD transactions, it proactively pushes the PD chunks to peers via gossip. This turns every partition assignee into a secondary source after validation, spreading load across the network. Pull-based fetch remains the correctness fallback.

## Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Push trigger | `BlockStateUpdated` event from `BlockTreeService` | Fires after full validation; gossip already uses broadcast events |
| Event payload | Enriched `BlockStateUpdated` enum with full block data | Self-contained; no consumer lookups needed |
| Wire format | `PdChunkBatch { block_hash, chunks: Vec<PdChunkPush> }` | Batch per block; one HTTP request per peer |
| Dedup | `GossipCacheKey::PdChunk(u32, u64)` per-chunk only | Per-chunk granularity; no batch-level dedup (partial batches must not suppress later complementary batches) |
| Receiving storage | MDBX `CachedChunks` direct write (bypass `cache_chunk_verified`) | Push chunks don't need ingress proof tracking |
| Serving from cache | Implement `get_chunk_for_pd` TODO to check `CachedChunks` | Makes push recipients serveable via pull |
| Peer selection | All peers sorted by score | Same pattern as block broadcast; GossipCache handles dedup |
| Verification | `derive_chunk_verification_info` via MDBX BlockIndex | PD chunks reference data from blocks migrated at depth 6+; any synced receiver has them indexed |
| Push orchestration | P2PService subscribes to `BlockStateUpdated` | Natural owner of gossip logic |

## Architecture

### Push Flow (Sender)

```
BlockTreeService::on_block_validation_finished
  → BlockStateUpdated::Valid { block_header, block_body }
    → P2PService receives event via block_state_events broadcast channel
      → Fetch evm_block from ExecutionPayloadCache using block_header.evm_block_hash
      → extract_pd_chunk_specs_from_block(&evm_block) → Vec<ChunkRangeSpecifier>
      → specs_to_ledger_offsets(specs, num_chunks_in_partition) → Vec<(u32, u64)>
        (partition_index * num_chunks_in_partition + offset, for each chunk in range)
      → Skip if no offsets (no PD chunks in block)
      → For each (ledger, offset):
          Try storage modules → PackedChunk → unpack → UnpackedChunk
          Else try CachedChunks MDBX → reconstruct UnpackedChunk
          Else skip
      → Skip if no chunks collected (node has no local data for these offsets)
      → Construct PdChunkBatch { block_hash, chunks }
      → Send as GossipBroadcastMessageV2 onto gossip_broadcast channel
        → Broadcast loop delivers to all peers via /gossip/v2/pd_chunk_batch
```

### Receive Flow (Receiver)

```
POST /gossip/v2/pd_chunk_batch
  → GossipDataHandler::handle_pd_chunk_batch
    → Sync check (drop if syncing)
    → Per-peer rate limit check (dedicated PD batch rate limiter, not DataRequestTracker)
    → Reject if ledger != DataLedger::Publish for any chunk (enforce PD-only constraint)
    → For each PdChunkPush { ledger, offset, chunk }:
        Check GossipCache: PdChunk(ledger, offset) already seen → skip this chunk
        derive_chunk_verification_info(block_index, db, chunk_size, Publish, offset)
          → data_root, data_size, tx_chunk_offset
          (if offset is beyond migrated range → skip this chunk)
        Verify chunk.data_root == expected_data_root
        Compute target_byte_offset using data_size (same logic as pd_service.rs:268,
          handling rightmost chunk edge case — do NOT use tx_chunk_offset * chunk_size)
        Verify validate_path(data_root, &chunk.data_path, target_byte_offset)
        Verify sha256(chunk.chunk) == leaf_hash
        Compute chunk_path_hash = hash_data_path(&chunk.data_path)
        Check CachedChunks for existing chunk_path_hash → skip if duplicate
        Write to CachedChunks + CachedChunksIndex
        Record seen: GossipCacheKey::PdChunk(ledger, offset) for this chunk
    → Return Accepted
```

**Note on block_hash**: The `block_hash` in `PdChunkBatch` is the NEW block being validated (not yet migrated to BlockIndex). It is NOT used for verification — each chunk is independently verified against migrated on-chain state via `derive_chunk_verification_info`. The `block_hash` is informational only (logging, tracing). Batch-level dedup by `block_hash` is intentionally NOT used because senders may send partial batches (only chunks they have locally), and batch-level dedup would suppress later complementary batches from other senders.

### Serving Flow (Pull from Cache)

```
GossipDataRequestV2::PdChunk(ledger, offset)
  OR PdService::handle_provision_block_chunks needs a chunk
    → ChunkProvider::get_chunk_for_pd(ledger, offset)
      → derive_chunk_verification_info → data_root, tx_chunk_offset
      → CachedChunksIndex(data_root, tx_chunk_offset) → chunk_path_hash
      → CachedChunks(chunk_path_hash) → CachedChunk { chunk, data_path }
      → If found → return ChunkFormat::Unpacked
      → Else → fall back to storage module → ChunkFormat::Packed
```

### Epidemic Spread

```
Block producer (has chunks in storage)
  → validates block N → pushes PdChunkBatch to all peers

Partition assignee A (has chunks in storage)
  → validates block N → pushes PdChunkBatch to all peers

Peer B (no storage partition, received push from A)
  → chunks land in CachedChunks
  → validates block N → reads from CachedChunks → pushes PdChunkBatch to all peers

Peer C (missed all pushes)
  → pulls from any peer via get_chunk_for_pd → CachedChunks hit on Peer B
```

## Type Changes

### New Types (`crates/types/src/gossip.rs`)

```rust
/// A single PD chunk with ledger coordinates, for push gossip.
/// Note: `ledger` is always `DataLedger::Publish` (0) per the permanent PD design constraint.
/// The field is retained for wire format self-description and consistency with `GossipDataRequestV2::PdChunk(u32, u64)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PdChunkPush {
    pub ledger: u32,
    pub offset: u64,
    pub chunk: UnpackedChunk,
}

/// A batch of PD chunks from a single block, pushed after validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PdChunkBatch {
    pub block_hash: BlockHash,
    pub chunks: Vec<PdChunkPush>,
}
```

### `GossipDataV2` Change

Add `PdChunkBatch(PdChunkBatch)` variant for push broadcast. Keep `PdChunk(ChunkFormat)` for pull responses only. The pull-request side (`GossipDataRequestV2::PdChunk(u32, u64)`) stays unchanged. Implement `pre_serialize_for_broadcast` and `send_data` for the new `PdChunkBatch` variant; the existing `PdChunk` variant continues to return `None`/`Rejected` in broadcast paths (it is never placed on the broadcast channel).

### `GossipCacheKey` Change

Add one variant for per-chunk dedup. No batch-level dedup key — partial batches from different senders must not suppress each other:

```rust
pub enum GossipCacheKey {
    Chunk(ChunkPathHash),
    Transaction(IrysTransactionId),
    Block(BlockHash),
    ExecutionPayload(B256),
    IngressProof(H256),
    PdChunk(u32, u64),          // per-chunk dedup
}
```

### `BlockStateUpdated` Change

Convert from struct to enum. The current struct has `state: ChainState` and `discarded: bool` — the enum removes the boolean by making valid/invalid explicit variants. The `ChainState` field is preserved on `Valid` for existing consumers (e.g. `BlockValidationTracker`).

```rust
pub enum BlockStateUpdated {
    Valid {
        block_hash: BlockHash,
        height: u64,
        state: ChainState,
        block_header: Arc<IrysBlockHeader>,
        block_body: Arc<BlockBody>,
    },
    Invalid {
        block_hash: BlockHash,
        height: u64,
        validation_result: ValidationResult,
    },
}
```

**Note on EVM block data**: `BlockTreeService::on_block_validation_finished` does not have access to the `ExecutionPayloadCache` and receives only `block_hash` + `validation_result` via `BlockValidationFinished`. Rather than threading the EVM block through the block tree or adding `ExecutionPayloadCache` as a dependency, the P2PService fetches the EVM block lazily using `block_header.evm_block_hash` from the `ExecutionPayloadCache` it already has access to (passed to `GossipDataHandler` at construction). This keeps the event lean and avoids cloning a potentially large EVM block across the broadcast channel. The `block_header` and `block_body` are already available in the block tree cache as `BlockMetadata.block` (the `SealedBlock`).

Convenience methods for consumer migration:

```rust
impl BlockStateUpdated {
    pub fn block_hash(&self) -> &BlockHash { ... }
    pub fn height(&self) -> u64 { ... }
    pub fn is_valid(&self) -> bool { matches!(self, Self::Valid { .. }) }
    pub fn is_invalid(&self) -> bool { matches!(self, Self::Invalid { .. }) }
}
```

**Known consumers requiring update:**
- `crates/actors/src/block_producer/block_validation_tracker.rs:113` — uses `block_state_rx.recv()`, ignores event contents. Minimal change.
- `crates/chain-tests/src/utils.rs:4084` — `read_block_from_state` checks `event.block_hash`, `event.discarded`, `event.validation_result`. Needs refactoring to match on enum.
- `crates/chain-tests/src/utils.rs:4137-4140` — `wait_for_block_event` takes `Fn(&BlockStateUpdated) -> bool` predicates. All callsites need updating to use helper methods or match arms.
- `crates/chain-tests/src/utils.rs:976-988` — `wait_until_block_events_idle` uses the receiver generically. Minimal change.
- `crates/actors/src/validation_service/block_validation_task.rs:227` — subscribes to detect queued block validation completion.

## Shared Utilities

### `extract_pd_chunk_specs_from_block` (`crates/irys-reth/src/pd_tx.rs`)

New function consolidating duplicated iteration across `block_validation.rs`, `pd_pricing/base_fee.rs`, and the new gossip push path:

```rust
pub fn extract_pd_chunk_specs_from_block(
    evm_block: &reth_ethereum_primitives::Block,
) -> Vec<ChunkRangeSpecifier> {
    let mut all_specs = Vec::new();
    for tx in &evm_block.body.transactions {
        let is_pd = detect_and_decode_pd_header(tx.input())
            .ok()
            .and_then(|opt| opt)
            .is_some();
        if is_pd {
            let specs = extract_pd_chunk_specs(tx.access_list());
            all_specs.extend(specs);
        }
    }
    all_specs
}
```

### `specs_to_ledger_offsets` (shared with `PdService::specs_to_keys`)

Converts `ChunkRangeSpecifier`s (partition-relative) to absolute `(ledger, offset)` pairs. Currently implemented inline in `PdService::specs_to_keys` (`pd_service.rs:702-748`). Extracted as a shared utility:

```rust
pub fn specs_to_ledger_offsets(
    specs: &[ChunkRangeSpecifier],
    num_chunks_in_partition: u64,
) -> Vec<(u32, u64)> {
    // For each spec: base = partition_index * num_chunks_in_partition + spec.offset
    // Emit (DataLedger::Publish as u32, base + i) for i in 0..spec.chunk_count
}
```

Requires `ConsensusConfig::num_chunks_in_partition` — callers obtain this from their config reference.

### `derive_chunk_verification_info` (extract from `pd_service.rs` to `crates/database` or `crates/domain`)

Extracted as a free function usable by `PdService`, `GossipDataHandler`, and `ChunkProvider`:

```rust
pub struct ChunkVerificationInfo {
    pub expected_data_root: H256,
    pub data_size: u64,
    pub tx_chunk_offset: u64,
}

pub fn derive_chunk_verification_info(
    block_index: &BlockIndex,
    db: &DatabaseProvider,
    chunk_size: u64,
    ledger: DataLedger,
    offset: u64,
) -> eyre::Result<ChunkVerificationInfo>
```

Resolution chain: `(ledger, offset)` → binary search `IrysBlockIndexItems` → `MigratedBlockHashes` → `IrysBlockHeaders` (tx_ids) → linear walk `IrysDataTxHeaders` (data_root, data_size) → match offset to tx.

## Changes by Crate

### `crates/types`

- `gossip.rs`: Add `GossipDataV2::PdChunkBatch(PdChunkBatch)` variant (keep `PdChunk(ChunkFormat)` for pull responses)
- `gossip.rs`: Add `PdChunkPush`, `PdChunkBatch` structs
- `gossip.rs`: Add `GossipCacheKey::PdChunk(u32, u64)` (no batch-level key)
- `gossip.rs`: Convert `BlockStateUpdated` from struct to `Valid`/`Invalid` enum
- Update all `BlockStateUpdated` consumers to match on the new enum

### `crates/irys-reth`

- `pd_tx.rs`: Add `extract_pd_chunk_specs_from_block`
- Optionally refactor `block_validation.rs` and `pd_pricing/base_fee.rs` to use the new utility

### `crates/database` or `crates/domain`

- Extract `derive_chunk_verification_info` as shared free function

### `crates/p2p`

- `gossip_service.rs`: P2PService subscribes to `block_state_events`; new `select!` arm in `spawn_broadcast_task` handles `BlockStateUpdated::Valid`
- `gossip_client.rs`: Implement `pre_serialize_for_broadcast` for `PdChunkBatch`
- `gossip_client.rs`: Implement `send_data` for `PdChunkBatch` (V2 send, V1 reject)
- `gossip_data_handler.rs`: New `handle_pd_chunk_batch` handler
- `cache.rs`: Add `pd_chunks` moka cache to `GossipCache` (per-chunk dedup only, no batch-level cache)
- `server.rs`: New route `/gossip/v2/pd_chunk_batch`
- `types.rs`: Add `GossipRoutes::PdChunkBatch` variant
- P2PService gains `ChunkStorageProvider`, `BlockIndexReadGuard`, `DatabaseProvider`, `ConsensusConfig` dependencies

### `crates/domain`

- `chunk_provider.rs`: Implement `get_chunk_for_pd` — check `CachedChunks` via `derive_chunk_verification_info` → `CachedChunksIndex` → `CachedChunks` before falling back to packed storage

### `crates/actors`

- `block_tree_service.rs`: Emit `BlockStateUpdated::Valid` with full block data
- `pd_service.rs`: Replace internal `derive_chunk_verification_info` with shared utility
- `services.rs`: No new channels — reuses existing `block_state_events` broadcast

### `crates/chain-tests`

- `utils.rs`: Update `read_block_from_state`, `wait_for_block_event`, and predicate-based event waiters to match on `BlockStateUpdated` enum variants instead of struct fields

### Unchanged

- `CachedChunks` / `CachedChunksIndex` table schemas
- Pull-based PD chunk fetch (`pull_pd_chunk_from_peers`)
- PD `ChunkCache` / `ChunkDataIndex` (in-memory EVM cache)
- `ChunkIngressService` (regular chunk ingress)

## Verification Model

### Sender-Side

The sender only pushes chunks it can read from local storage modules or MDBX `CachedChunks`. These are already verified data — storage module chunks were packed/unpacked through the packing pipeline, and `CachedChunks` entries were written after verification on receipt.

### Receiver-Side

The receiver independently verifies each pushed chunk:

1. **Cross-reference on-chain state**: `derive_chunk_verification_info` resolves `(ledger, offset)` to `(data_root, data_size, tx_chunk_offset)` from the MDBX BlockIndex. This is authoritative — the block has been migrated (depth 6+) and the tx headers are committed.
2. **Merkle proof**: `validate_path(data_root, data_path, target_byte_offset)` verifies the chunk's position in the transaction's Merkle tree. The `target_byte_offset` is computed using `data_size` (from trusted MDBX state) to correctly handle the rightmost chunk edge case — reusing the same logic as `pd_service.rs:268`, NOT a naive `tx_chunk_offset * chunk_size`.
3. **Leaf hash**: `sha256(chunk_bytes) == path_result.leaf_hash` verifies the chunk content matches the proof.
4. **Ledger constraint**: The receiver rejects any `PdChunkPush` where `ledger != DataLedger::Publish`, enforcing the permanent PD design constraint at the gossip layer.

If any verification fails, the individual chunk is skipped. A partially-valid batch is accepted for its valid chunks.

### Why BlockIndex Is Sufficient

PD transactions can only reference chunks from the Publish ledger at offsets that are already in the migrated BlockIndex. Migration happens at `block_migration_depth = 6` blocks behind the canonical tip. By the time a block with PD transactions is validated and the push fires, the referenced chunks are from blocks migrated long ago. Any peer that is caught up enough to receive real-time gossip (not in sync mode) has already migrated those blocks.

If a receiver's BlockIndex lookup fails (peer is too far behind), the pushed chunks are dropped. The peer will fall back to pull-based fetch when it catches up.

## Broadcast Wiring

The `BlockStateUpdated` event is received in the `spawn_broadcast_task` loop via a second `select!` arm. The `block_state_events` `broadcast::Receiver` is obtained via `service_senders.subscribe_block_state_updates()` in `P2PService::run()` and passed into `spawn_broadcast_task` alongside the existing `mempool_data_receiver`.

On `BlockStateUpdated::Valid`, the handler spawns a detached task (same pattern as the existing `mempool_data_receiver` arm) that:
1. Fetches the EVM block from `ExecutionPayloadCache` via `block_header.evm_block_hash`
2. Extracts PD chunk specs from the EVM block
3. Converts to ledger offsets
4. Reads chunk data from storage/cache
5. If no chunks collected, returns early
6. Constructs `PdChunkBatch`
7. Wraps as `GossipBroadcastMessageV2` with `data: GossipDataV2::PdChunkBatch(batch)`. The broadcast key uses a synthetic `GossipCacheKey::Block(block_hash)` for the outer broadcast loop's `peers_that_have_seen` filtering — this is a coarse hint only. Per-chunk dedup via `GossipCacheKey::PdChunk` is the authoritative dedup layer, applied on the receiver side and after successful per-peer send
8. Calls `service.broadcast_data(message, &peer_list)` directly — same inline invocation pattern as the existing `mempool_data_receiver` arm

The chunk assembly and broadcast delivery happen in the same detached task. This avoids feeding back into the `mempool_data_receiver` channel and keeps the two `select!` arms independent.

## Reorg Behavior

If a block is validated, PD chunks are pushed, and then a reorg discards that block:

- Pushed chunks remain in receivers' `CachedChunks` MDBX tables. This is **harmless**: the chunks are still valid data (they reference already-migrated Publish ledger offsets that are unaffected by the reorg). The `CachedChunks` entries will be evicted naturally by the existing `CacheService` pruning during epoch processing.
- The replacement block on the canonical chain may contain different PD transactions referencing different chunks. Those chunks will be pushed independently after the replacement block validates.
- Stale cache entries do not affect correctness: `get_chunk_for_pd` returns valid chunk data regardless of which block triggered the push.

## CachedChunks Data Lifetime and Integrity

Pushed PD chunks land in MDBX `CachedChunks` without `CachedDataRoots` entries (bypassing `cache_chunk_verified`). This creates orphan rows not governed by the existing `CacheService` pruning logic (which walks `CachedDataRoots`).

### Duplicate suppression

Before writing a chunk, the handler checks if `chunk_path_hash` already exists in `CachedChunks` (a simple `tx.get::<CachedChunks>(chunk_path_hash)` point lookup). If present, the write is skipped. This prevents repeated pushes from creating duplicate `CachedChunksIndex` rows with fresh timestamps. Combined with the `GossipCache` per-chunk seen-state, most duplicates are rejected before reaching the DB.

### Eviction

The existing `CacheService::prune_chunks_without_active_ingress_proofs` scans `CachedDataRoots` and only prunes chunks associated with known data roots. Push-only entries lack a `CachedDataRoots` parent, so they are invisible to this pruner.

To handle eviction, extend the `CacheService` prune cycle to also scan `CachedChunks` for entries whose `data_root` has no `CachedDataRoots` entry — these are push-only orphans. Apply a time-based threshold (e.g., entries older than `min_chunk_age_secs`) for deletion. This piggybacks on the existing `OnBlockMigrated` trigger and reuses the `get_cache_size::<CachedChunks>` capacity check.

Alternatively, a simpler initial approach: the `CachedChunks` table is already bounded by `max_cache_size_bytes`, and the capacity check fires on every `OnBlockMigrated` event. When the cache exceeds `prune_at_capacity_percent`, the existing pruner deletes chunks without active ingress proofs. Push-only entries have no ingress proofs, so they will be pruned first when capacity pressure exists. This provides implicit eviction without new scanning logic.

## Backward Compatibility

The `GossipDataV2::PdChunk(ChunkFormat)` variant being replaced was introduced on the `feat/pd` branch and has never been deployed to any production or testnet network. Both `pre_serialize_for_broadcast` and `send_data` return no-op/rejection for this variant — no peer has ever sent or received a `PdChunk` push message. The replacement with `PdChunkBatch` is a safe wire-format change.

The pull-request side (`GossipDataRequestV2::PdChunk(u32, u64)` and its response carrying `GossipDataV2::PdChunk(ChunkFormat)`) needs to remain functional. The pull response should continue to use `ChunkFormat` — this means the `GossipDataV2` enum needs both the `PdChunkBatch` variant (for push broadcast) and a pull-response variant. Options:
- Keep `PdChunk(ChunkFormat)` as a separate variant alongside `PdChunkBatch` — used only in pull responses, never in broadcast
- Use `PdChunkBatch` for both push and pull (wrapping single-chunk pulls in a batch of size 1)

The first option is cleaner — the push and pull paths serve different purposes and have different payloads. `PdChunk(ChunkFormat)` is the pull-response type, `PdChunkBatch(PdChunkBatch)` is the push-broadcast type.

## Scoring

The existing `handle_score` function treats all `Ok(_)` as success. For PD chunk batch push:

- Successful delivery (peer accepts the batch): normal score increase via `ScoreIncreaseReason::DataRequest`
- Peer rejects (unsupported, rate-limited): no score change
- Network error: mark peer offline via circuit breaker

No special scoring treatment needed — the existing pattern applies.

## Bandwidth Considerations

A block with 100 PD chunks at 256KB each = ~25MB per batch. With the batch-per-peer model, each peer receives one HTTP request containing the full batch. The broadcast loop's `broadcast_batch_size` and `broadcast_batch_throttle_interval` provide natural rate limiting across peers.

The `GossipCache` dedup ensures each peer receives each chunk at most once per 5-minute window. As validators validate and re-push, the seen-set grows, and subsequent pushers skip already-notified peers.

## Rate Limiting

The `/gossip/v2/pd_chunk_batch` endpoint needs a **dedicated per-peer rate limiter** — the existing `DataRequestTracker` is for pull requests and is per-handler-clone (not shared). The PD batch rate limiter:

- Tracks per-peer batch count within a sliding time window
- Rejects with `GossipError::RateLimited` when exceeded
- Applies BEFORE any per-chunk verification to bound CPU cost

**Cross-sender amplification**: `GossipCache` is local per node, so it cannot prevent multiple validators from sending the same batch to the same peer. In practice, validators process blocks sequentially — later validators find most peers already marked "seen" by earlier pushers via per-chunk `PdChunk(u32, u64)` entries. The receiver-side per-chunk dedup (`GossipCache` check before verification) ensures even if multiple batches arrive, each chunk is verified and stored at most once. The bandwidth cost of receiving duplicate batches is bounded by the per-peer rate limiter.

**Request body size limit**: The actix-web `JsonConfig` already limits request bodies to 100MB (`server.rs:1606`). For PD batches, a tighter limit could be enforced based on `max_pd_chunks_per_block * chunk_size` from the hardfork config.
