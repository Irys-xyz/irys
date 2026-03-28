# PD Chunk Optimistic Push — Implementation Design

**Date**: 2026-03-27
**Branch**: `rob/gossip-push-pd-chunks-v2`
**Issues**: [#1231](https://github.com/Irys-xyz/irys/issues/1231), [#1217](https://github.com/Irys-xyz/irys/issues/1217)
**Based On**: `docs/superpowers/specs/2026-03-24-pd-chunk-optimistic-targeted-push-design.md`
**Status**: Implementation design

## Executive Summary

This document specifies the implementation of optimistic targeted push for PD chunk data. When a PD transaction enters the EVM mempool, nodes that have the referenced chunk data push it to a small subset of peers. By the time a block including that PD transaction arrives for validation, most validators already have the chunks cached locally — eliminating the validation-time pull hotspot.

This is an **additive** optimization. The existing pull-from-assignees path remains the fallback and catch-up mechanism. Optimistic push only accelerates the hot path for mempool-live PD transactions.

## Scope

### In Scope

- Per-chunk optimistic push from PdService when PD transactions are detected in the EVM mempool
- New gossip route for receiving pushed PD chunks
- Receiver-side validation using the block index (no mempool awareness required)
- Inbound push tracking for duplicate suppression
- Onward re-push triggered by EVM mempool PD tx arrival
- Configurable fanout parameter

### Out of Scope

- Historical block catch-up (unchanged — pull from partition holders)
- Per-peer byte budgets (can be added later if abuse becomes a problem)
- Batching or fragment assembly (each chunk is pushed individually)
- Changes to the CL data transaction chunk ingress path
- Changes to the PD precompile or EVM execution path

## Assumptions

- PD transactions always reference already-migrated data (part of the block index). `data_root` and `data_size` are derivable from `(ledger, offset)` via the block index at mempool time.
- The PdService LRU `ChunkCache` and its mirrored `ChunkDataIndex` DashMap are the authoritative fast-path cache for PD chunk data during EVM execution.
- Partition assignees can provide both unpacked chunk bytes and merkle proofs (`data_path`) via the existing `get_chunk_for_pd` method on `ChunkStorageProvider`.

## Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Target cache | PdService LRU + ChunkDataIndex DashMap | PD precompile reads from ChunkDataIndex; landing chunks here directly eliminates validation-time pulls |
| Push granularity | Per-chunk, no batching | Simplest model; each chunk pushed as soon as provisioned; no fragment assembly overhead |
| Chunk key | `(ledger: u32, offset: u64)` | PdService's native coordinate system; avoids translation between content-keyed and offset-keyed spaces |
| Peer selection | k/2 top-scored + k/2 random, excluding known sources and partition assignees | Balances reliability and spread; avoids pushing to nodes that already have the data |
| Receiver anti-spam | Block index validation only | PD always references migrated data; receiver derives `data_root` from block index and verifies merkle proof; no mempool awareness needed; sidesteps EVM mempool propagation race |
| Onward push trigger | EVM mempool PD tx arrival | Creates a clean loop: tx arrives → provision (cache hit or pull) → push to k peers |
| Wire format | `PdChunkPush { ledger, offset, chunk_bytes, data_path }` — unpacked, no sender hints | Receiver derives all verification info from block index; no trust in sender-provided metadata |
| Gossip route | New `POST /gossip/v2/pd_chunk_push` | Dedicated route with its own body cap; PD push goes to PdService, CL chunk ingress goes to ChunkIngressService |
| Inbound push tracking | moka cache, `(ledger, offset) → Set<PeerId>`, 5-min TTL | Same pattern as existing GossipCache; prevents pushing chunks back to peers that sent them |
| Fanout config | `pd_optimistic_push_fanout` in `P2PGossipConfig`, default 4 | Configurable; set to 0 to disable |

## Wire Format

### `PdChunkPush`

```rust
/// A single unpacked PD chunk pushed optimistically before block validation.
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

No `data_root`, `data_size`, or `tx_id` fields. The receiver derives all verification info from the block index using `(ledger, offset)`.

### Gossip Route

- **Route**: `POST /gossip/v2/pd_chunk_push`
- **Request wrapper**: `GossipRequestV2<PdChunkPush>` (standard V2 wrapper with peer identity)
- **Body cap**: 768 KiB. Base64 encoding of 256 KiB chunk yields ~349 KiB; add base64 data_path, `GossipRequestV2` envelope (peer identity, signatures), and JSON structural overhead. 768 KiB provides safe headroom. Applied as a route-level `web::JsonConfig` override, not a change to the global 100 MiB app-level limit.
- **Response**: standard gossip response (accepted / rejected with reason)

## PdService Changes

### New Message Variant

```rust
pub enum PdChunkMessage {
    // ... existing variants unchanged ...

    /// An optimistically pushed PD chunk from a peer.
    OptimisticPush {
        peer_id: IrysPeerId,
        push: PdChunkPush,
    },
}
```

### Cache Change: Store `data_path` Alongside Chunk Bytes

The PdService LRU cache currently stores entries of type `CachedChunkEntry { data: Arc<Bytes>, referencing_txs: HashSet<B256>, cached_at: Instant }`. Add a `data_path` field:

```rust
pub struct CachedChunkEntry {
    pub data: Arc<Bytes>,
    pub data_path: Option<Base64>,  // NEW — Some when obtained via get_chunk_for_pd, optimistic push, or P2P fetch
    pub referencing_txs: HashSet<B256>,
    pub cached_at: Instant,
}
```

The `ChunkDataIndex` DashMap (read by the PD precompile) continues to hold `Arc<Bytes>` only — `data_path` is needed for outbound push and verification, not for EVM execution.

### Cache Insert for Optimistic Push: Unreferenced Entries

Optimistically pushed chunks have no associated `tx_hash` at insertion time — the PD transaction may not have reached this node's EVM mempool yet. Add a new method to `ChunkCache`:

```rust
/// Insert a chunk with no transaction reference. The entry is eligible for
/// immediate LRU eviction, which is correct for speculative push data.
/// When a NewTransaction or ProvisionBlockChunks message arrives later,
/// add_reference() promotes the entry.
fn insert_unreferenced(&mut self, key: ChunkKey, data: Arc<Bytes>, data_path: Base64) -> bool
```

This creates a `CachedChunkEntry` with an empty `referencing_txs` set. Unreferenced entries are evicted first under LRU pressure, which is the correct behavior — speculative data should yield to demanded data.

**Already-exists behavior:** If the chunk is already in the LRU (e.g., from a prior optimistic push or a `NewTransaction` provisioning), `insert_unreferenced` is a no-op and returns `false`. It does not overwrite existing references or data. Exception: if the existing entry has `data_path: None` and the new push has `Some(data_path)`, the `data_path` field is filled in (so the chunk becomes pushable to other peers).

### New Dependency: Outbound Push Capability

`PdService` currently has no outbound gossip capability. Add a new trait and inject it at construction:

```rust
pub trait PdChunkPusher: Send + Sync {
    /// Push a PD chunk to a peer. Fire-and-forget (detached tokio task).
    /// peer_id is needed for circuit breaker checks.
    fn push_pd_chunk(&self, peer_id: IrysPeerId, peer_addr: &PeerAddress, push: &PdChunkPush);
}
```

`GossipClient` implements this trait. `PdService::spawn_service` takes an `Arc<dyn PdChunkPusher>` parameter. This keeps PdService decoupled from the gossip client internals and testable with mocks.

### Local Storage Path Change

When provisioning chunks from local storage (partition assignee path), PdService calls `storage_provider.get_chunk_for_pd(ledger, offset)` instead of `get_unpacked_chunk_by_ledger_offset`. This returns `ChunkFormat::Packed(PackedChunk)` including `data_path`. PdService must then unpack the bytes locally using `irys_packing::unpack` — the same pattern already used in `on_fetch_success` for packed P2P responses — and stores both `chunk_bytes` and `data_path` in the LRU.

For P2P-fetched chunks, `on_fetch_success` already has `data_path` from the `ChunkFormat` response — it passes it through to the cache insert instead of discarding it.

### Inbound Push Tracker

```rust
pub struct InboundPushTracker {
    /// (ledger, offset) → set of peers that pushed this chunk to us.
    cache: moka::sync::Cache<(u32, u64), Arc<RwLock<HashSet<IrysPeerId>>>>,
}
```

- `time_to_live`: 5 minutes (matches `GOSSIP_CACHE_TTL`)
- `record_inbound(ledger, offset, peer_id)` — called after accepting a valid optimistic push
- `get_known_sources(ledger, offset) → HashSet<IrysPeerId>` — called when selecting outbound push targets

Lives as a field on `PdService`. Not shared with other services.

Memory estimate: 10,000 active chunk offsets x (16 bytes key + ~5 peer IDs x 32 bytes) ≈ ~1.7 MiB.

## Sender Flow

Triggered when `PdChunkMessage::NewTransaction` arrives (PD tx detected in EVM mempool):

```
PdService::handle_provision_chunks:
  specs_to_keys(chunk_specs) → HashSet<ChunkKey>
  for each ChunkKey(ledger, offset):
      // Synchronous provision paths (cache hit or local storage)
      if cache.contains(ledger, offset):
          // already have it — push immediately
          schedule_outbound_push(ledger, offset)
      else if let Some(chunk_format) = storage_provider.get_chunk_for_pd(ledger, offset):
          // local storage hit — unpack, insert, push
          (chunk_bytes, data_path) = unpack(chunk_format)
          cache.insert(key, chunk_bytes, data_path, tx_hash)
          schedule_outbound_push(ledger, offset)
      else:
          // async P2P pull — push happens in on_fetch_success callback
          join_set.spawn(chunk_fetcher.fetch_chunk(peers, ledger, offset))

  // ... existing on_fetch_success handler (runs in later event loop iteration):
  on_fetch_success(key, chunk_format):
      (chunk_bytes, data_path) = verify_and_unpack(chunk_format)
      cache.insert(key, chunk_bytes, data_path, tx_hash)
      schedule_outbound_push(key.ledger, key.offset)  // push after async pull completes

schedule_outbound_push(ledger, offset):
  if pd_optimistic_push_fanout == 0: return
  excluded_ids = inbound_tracker.get_known_sources(ledger, offset)
                 ∪ resolve_partition_assignee_ids(ledger, offset)
  // resolve_partition_assignee_ids returns IrysPeerId values
  // by looking up miner addresses in the epoch snapshot
  // then resolving to IrysPeerId via peer_list
  k = pd_optimistic_push_fanout
  candidates = peer_list.all_peers_with_score()
               // returns Vec<(IrysPeerId, PeerAddress, f64)>
               // new PeerList method needed
               .filter(|(id, _, _)| !excluded_ids.contains(id))
  peers = select_push_targets(k, candidates)
          // k/2 from top-scored, k/2 random
          // produces Vec<(IrysPeerId, PeerAddress)>
  for each (peer_id, peer_addr) in peers:
      chunk_pusher.push_pd_chunk(peer_id, peer_addr, PdChunkPush {
          ledger, offset, chunk_bytes, data_path
      })
      // fire-and-forget detached tokio task
      // respects circuit breaker (keyed by peer_id)
```

## Receiver Flow

```
POST /gossip/v2/pd_chunk_push arrives:
  check body cap (768 KiB)
  deserialize GossipRequestV2<PdChunkPush>
  validate peer identity
  send PdChunkMessage::OptimisticPush { peer_id, push } to PdService

PdService::handle_optimistic_push(peer_id, push):
  // 0. Ledger constraint — PD operates exclusively on the Publish ledger
  if push.ledger != DataLedger::Publish:
      reject (non-Publish ledger)

  // 1. Block index validation
  info = derive_chunk_verification_info(push.ledger, push.offset)
  if info is None:
      reject (invalid ledger offset)

  // 2. Merkle proof verification
  // Same logic as on_fetch_success
  num_chunks = info.data_size.div_ceil(chunk_size)
  last_chunk_offset = num_chunks - 1
  target_byte_offset = if info.tx_chunk_offset == last_chunk_offset:
      info.data_size - 1
  else:
      (info.tx_chunk_offset + 1) * chunk_size - 1
  path_result = validate_path(info.expected_data_root, &push.data_path, target_byte_offset)
  if path_result is Err:
      reject (bad merkle proof)

  // 3. Leaf hash verification
  if sha256(push.chunk_bytes) != path_result.leaf_hash:
      reject (bad chunk data)

  // 4. Cache insert (unreferenced — no tx_hash at this point)
  cache.insert_unreferenced(
      ChunkKey { ledger: push.ledger, offset: push.offset },
      Arc::new(push.chunk_bytes),
      push.data_path
  )
  // LRU insert mirrors to ChunkDataIndex DashMap automatically
  // Entry is evictable under LRU pressure until a NewTransaction
  // or ProvisionBlockChunks adds a reference

  // 5. Record inbound source
  inbound_tracker.record_inbound(push.ledger, push.offset, peer_id)
```

## Onward Re-Push

When a PD tx enters a peer's EVM mempool, the same `NewTransaction` flow triggers. If the chunks were already received via optimistic push (Phase 3 in the lifecycle), they are LRU cache hits. The standard sender flow fires and pushes to that peer's own k targets.

This creates bounded epidemic spread:
- Seed nodes push to k peers on first seeing the PD tx in mempool
- Those peers push to their own k peers when the PD tx reaches their mempool
- Spread continues until all peers with the PD tx in mempool have pushed
- Inbound tracker prevents pushing back to known sources
- Partition assignees are excluded as targets (they have the data on disk)

## Validation-Time Behavior

When `PdChunkMessage::ProvisionBlockChunks` arrives during block validation:

```
handle_provision_block_chunks:
  for each ChunkKey:
      LRU cache hit (from optimistic push) → add block reference → done
      LRU miss → fall back to existing P2P pull path (unchanged)
  PdBlockGuard holds references until block validation completes
  PD precompile reads from ChunkDataIndex DashMap (unchanged)
```

No changes to the block validation path itself. Optimistic push just increases the LRU hit rate.

Note: `handle_provision_block_chunks` continues to call `get_unpacked_chunk_by_ledger_offset` for its local storage fallback (not `get_chunk_for_pd`). Block validation does not need `data_path` — it does not push chunks outward. Only the `NewTransaction` provisioning path switches to `get_chunk_for_pd`.

## Configuration

### New Field in `P2PGossipConfig`

```rust
/// Number of peers to push each PD chunk to.
/// Peers are selected as k/2 top-scored + k/2 random,
/// excluding known sources and partition assignees.
/// Set to 0 to disable optimistic push entirely.
/// Default: 4
pub pd_optimistic_push_fanout: u32,
```

### Hardcoded Constants

| Constant | Value | Rationale |
|----------|-------|-----------|
| Route body cap | 768 KiB | Base64-encoded 256 KiB chunk (~349 KiB) + data_path + GossipRequestV2 envelope + JSON overhead. Safety invariant, not a tuning knob |
| Inbound tracker TTL | 5 minutes | Matches `GOSSIP_CACHE_TTL`. Moka handles cleanup automatically |

### Feature Disable

Setting `pd_optimistic_push_fanout = 0` disables all outbound push. Inbound push acceptance remains on — accepting valid pushed chunks is free and benefits the receiver regardless of whether it re-pushes.

## Interaction With Existing Systems

### Block Validation

Unchanged. `ProvisionBlockChunks` checks the LRU first (existing behavior). Optimistic push increases hit rate. Misses fall back to pull.

### PdBlockGuard Reference Counting

Unchanged. Optimistically pushed chunks enter the LRU with no block reference. When `ProvisionBlockChunks` finds a cache hit, it adds the block reference at that point. Guard releases on drop.

### LRU Eviction

Unchanged. If the LRU evicts a chunk under capacity pressure, the chunk is gone. The inbound tracker may still have entries for it (harmless — the stale tracker entries are only used for outbound target exclusion, and the chunk would be re-pulled or re-accepted via push if needed during validation).

### Partition Assignees

An assignee stores packed data on disk. When it sees a PD tx in the mempool, `handle_provision_chunks` calls `get_chunk_for_pd` (returns packed + data_path), unpacks, inserts into LRU, then pushes. The unpack cost is paid once; the bytes serve both local execution and outbound push.

### Circuit Breaker

Outbound push respects the existing `CircuitBreakerManager`. If a peer's breaker is open, the push is skipped for that peer. No changes to circuit breaker logic.

### CL Data Transaction Chunks

Completely unaffected. The CL chunk ingress path (`/gossip/v2/chunk` → `ChunkIngressService` → MDBX `CachedChunks`) is separate from the PD optimistic push path (`/gossip/v2/pd_chunk_push` → `PdService` → LRU ChunkCache).

## Guardrails

1. **Bounded fanout.** Configurable, default 4. Set to 0 to disable.
2. **No sender hints in wire format.** Receiver derives all verification info from block index.
3. **Route-specific body cap.** 768 KiB hard limit on the push route.
4. **Block index validation.** Receiver rejects pushes for offsets that don't map to confirmed data.
5. **Full merkle proof + leaf hash verification.** Same verification as existing P2P pull path.
6. **Fire-and-forget outbound.** Push failure does not affect the sender's provisioning or block building.
7. **Pull fallback always available.** Optimistic push is additive; existing pull path handles all misses.

## What This Design Does Not Solve

- **Historical block catch-up.** By the time a syncing node reaches an old block, the optimistic push wave is gone. Catch-up remains pull-from-assignees.
- **Very large PD transactions.** No per-content size cap is enforced. If a PD tx references hundreds of chunks, all get pushed. A future enhancement could skip optimistic push for txs above a chunk count threshold.
- **Bandwidth abuse.** No per-peer byte budget. If a peer spams `pd_chunk_push` with valid-but-unwanted chunks, they consume LRU capacity. Mitigation: the block index gate ensures only valid confirmed chunks are accepted, and LRU eviction handles capacity. Per-peer budgets can be added later.
- **Guaranteed zero-wait validation.** If the mempool-to-block window is shorter than the push propagation time, some validators will miss the wave and fall back to pull. This is by design.
