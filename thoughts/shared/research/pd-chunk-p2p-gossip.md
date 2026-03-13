# Research: PD Chunk P2P Gossip — Current State & Prior Work

**Date**: 2026-03-13
**Git Commit**: 95483eeb (feat/pd)
**Branch**: feat/pd

## Research Question

How could P2P gossiping of PD chunks look if we don't have a chunk locally? What prior WIP work exists on `feat-pd-chunks` and `dmac/pd-chunk-service`, and is it reusable?

## Summary

The current PD service (`feat/pd`) handles local-only chunk retrieval. When a chunk is missing locally, it logs a warning and marks the transaction as `PartiallyReady` — no network fetch occurs. Two prior branches contain WIP work on P2P chunk gossip:

1. **`feat-pd-chunks`** — Contains substantial, well-structured P2P gossip infrastructure for PD chunks: a new `ProtocolVersion::VersionPD` (9000), a `PdChunkMessage` gossip type, a `/pd_chunks` gossip route, a `handle_pd_chunk` handler with timestamp validation, and full broadcast/receive wiring. However, it **deletes** the current `PdService` actor entirely (the branch predates or diverged from the PdService implementation).

2. **`dmac/pd-chunk-service`** — A small WIP branch off `feat/pd-resolve` with skeleton `PDService` actor using `ProvisionChunks`/`LockChunks`/`UnlockChunks` messages and two empty LRU modules. This is superseded by the current PdService on `feat/pd`.

The `feat-pd-chunks` P2P layer work is the most relevant — its gossip infrastructure can be extracted and integrated with the current PdService. The dmac branch is fully superseded.

## Detailed Findings

### 1. Current PD Service on `feat/pd` — The Gap

The PD service handles four message types (`crates/types/src/chunk_provider.rs:51-67`):
- `NewTransaction` — provision chunks for a mempool PD tx
- `TransactionRemoved` — release chunks when tx leaves mempool
- `ProvisionBlockChunks` — load chunks for validating a peer's block (with oneshot response)
- `ReleaseBlockChunks` — release chunks after validation

**The gap** is in `handle_provision_chunks()` at `crates/actors/src/pd_service.rs:224-231`:
```rust
Ok(None) => {
    warn!(
        "Chunk not found locally. P2P gossip not yet implemented — chunk will be unavailable."
    );
    missing.insert(*key);
}
```

The same gap exists in `handle_provision_block_chunks()` at lines 322-329. Missing chunks result in `PartiallyReady` state (mempool path) or `Err(missing)` response (validation path).

The `ProvisioningState` enum (`provisioning.rs:8-15`) already has `PartiallyReady { found, total }` with the doc comment "Some chunks unavailable (P2P not yet implemented)."

### 2. feat-pd-chunks Branch — P2P Gossip Infrastructure (REUSABLE)

This branch introduces a **push-based gossip** model for PD chunks, following the same pattern as existing chunk/block/tx gossip. Key components:

#### 2a. Protocol Version: `VersionPD` (9000)

`crates/types/src/version.rs`:
```rust
VersionPD = 9000,
```
Added to `supported_versions()` and `from_u32()`. This means PD chunk gossip uses a separate protocol version negotiated during handshake.

#### 2b. Gossip Types

`crates/types/src/gossip.rs:12-18`:
```rust
pub struct PdChunkMessage {
    pub chunk: UnpackedChunk,
    pub range_specifier: ChunkRangeSpecifier,
    pub timestamp: u64,
}
```

The message carries the full `UnpackedChunk` (256KB data + merkle proof) plus the `ChunkRangeSpecifier` (partition_index, offset, chunk_count) so the receiver knows WHERE in the PD data layout this chunk belongs. The timestamp enables freshness validation.

New enum variants added to `version_pd` module:
- `GossipDataVersionPD::PdChunk(PdChunkMessage)` — the data envelope
- `GossipBroadcastMessageVersionPD` — the broadcast wrapper (replaces V2 broadcast for VersionPD peers)
- `GossipRequestVersionPD<T>` — the request envelope (converts to V2 internally)
- `GossipCacheKey::PdChunk(ChunkPathHash, ChunkRangeSpecifier)` — dedup cache key

#### 2c. Route and Handler

`crates/p2p/src/types.rs:286`:
```rust
PdChunk,  // maps to "/pd_chunks"
```

`crates/p2p/src/server.rs:991-1028` — `handle_pd_chunk_version_pd()`:
- Accepts `GossipRequestVersionPD<PdChunkMessage>`
- Checks sync state (gossip reception enabled)
- Validates peer via `check_peer_v2()`
- Delegates to `GossipDataHandler::handle_pd_chunk()`

`crates/p2p/src/gossip_data_handler.rs:181-265` — `handle_pd_chunk()`:
- **Timestamp validation**: rejects chunks older than 5 minutes or more than 30 seconds in the future
- **Range specifier validation**: rejects `chunk_count == 0`
- **Chunk ingestion**: passes the `UnpackedChunk` to `ChunkIngressService` (same path as regular chunk gossip)
- **Dedup cache**: records the `PdChunk(chunk_path_hash, range_specifier)` cache key on success

#### 2d. Broadcast / Client

`crates/p2p/src/gossip_client.rs:1237-1247`:
```rust
GossipDataVersionPD::PdChunk(pd_chunk) => {
    self.send_data_internal(
        &peer.address.gossip,
        GossipRoutes::PdChunk,
        pd_chunk,
        ProtocolVersion::VersionPD,
    ).await
}
```

`crates/p2p/src/gossip_service.rs:108,131`:
The entire broadcast system uses `GossipBroadcastMessageVersionPD` as the broadcast message type, with conversion helpers `to_v2()` and `to_v1()` for backward compatibility (PdChunk variants return `None` for V1/V2 conversion — only VersionPD peers receive PD chunks).

#### 2e. What's MISSING in feat-pd-chunks

1. **The PdService actor is deleted** on that branch — the diff shows `crates/actors/src/pd_service.rs` as a deleted file. The branch predates the current PdService implementation.
2. **No wiring from gossip handler to PdService** — `handle_pd_chunk()` sends chunks to `ChunkIngressService` (storage layer ingestion) but does NOT notify the PdService that a missing chunk has arrived.
3. **No pull/request mechanism** — the implementation is purely push-based broadcast. There's no way for a node to REQUEST specific PD chunks it's missing from a peer. The `GossipDataRequestV2` enum on feat-pd-chunks does NOT have a PdChunk variant for the `/pull_data` route.
4. **No chain.rs wiring** — the PD chunk gossip broadcast is not connected to any triggering event (e.g., when a node provisions chunks for a PD tx, it should broadcast them to peers).

### 3. dmac/pd-chunk-service Branch (SUPERSEDED)

Commit `c0ed847a` off `feat/pd-resolve`. Contains a skeleton PDService with:
- Three messages: `ProvisionChunks(ChunkRangeSpecifier)`, `LockChunks(ChunkRangeId)`, `UnlockChunks(ChunkRangeId)`
- Two empty module files: `chunk_range_request_lru.rs`, `unpacked_chunks_lru.rs`
- Handler bodies are log-only stubs with comments describing a lifecycle:
  - Chunk states: Requested → Provisioned
  - Request states: Requesting → Provisioned (Locked/Unlocked) → Expired
  - TTL-based expiry with reset on re-request

This design is **fully superseded** by the current PdService on `feat/pd`, which uses:
- Reference-counted pinning instead of TTL-based expiry
- `NewTransaction`/`TransactionRemoved` instead of `ProvisionChunks`
- `ProvisionBlockChunks`/`ReleaseBlockChunks` (with oneshot response) instead of `LockChunks`/`UnlockChunks`
- A working LRU cache with shared DashMap index instead of empty module stubs

### 4. Existing P2P Infrastructure (For Context)

The P2P layer uses HTTP-based gossip. Key existing patterns relevant to PD chunk gossip:

**Push gossip** (`/gossip/chunk`, `/gossip/block`, `/gossip/transaction`):
- Peer pushes data → handler validates → processes → records in GossipCache → broadcasts to other peers
- Rate-limited by semaphore (`chunk_semaphore`)
- Deduplication via GossipCache (prevents re-processing seen data)

**Pull/request** (`/gossip/get_data`, `/gossip/pull_data`):
- `get_data`: "do you have X?" → receiver pushes X back if found
- `pull_data`: "give me X" → returns X in HTTP response
- Keyed by `GossipDataRequestV2` variants (BlockHeader, BlockBody, ExecutionPayload, Chunk, Transaction)
- **Chunk variant always returns `Ok(None)`** — chunks are NOT served via pull

**DataSyncService** (background replication):
- Fetches **packed** chunks from peers via `GET /v1/chunk/ledger/{id}/{offset}` (API server, not gossip)
- Uses `ChunkOrchestrator` with per-partition request tracking
- Request states: Pending → Requested → Completed (or retry with excluded peers)
- Health-score-based peer selection with bandwidth tracking

### 5. How P2P PD Chunk Gossip Could Be Integrated

Based on the analysis of both the current PdService and the feat-pd-chunks work, here's what each component's role would be:

**From feat-pd-chunks (extract & adapt):**
- `PdChunkMessage` gossip type (chunk + range_specifier + timestamp)
- `ProtocolVersion::VersionPD` and the version negotiation changes
- `GossipRoutes::PdChunk` → `/pd_chunks` route
- `GossipDataVersionPD::PdChunk` variant and broadcast support
- `GossipCacheKey::PdChunk` for dedup
- `handle_pd_chunk()` handler with timestamp/range validation
- Error variants: `PdChunkTimestampExpired`, `PdChunkTimestampFuture`, `PdChunkInvalidRangeSpecifier`

**New wiring needed (not in any branch):**
- `handle_pd_chunk()` must notify PdService when a chunk arrives (not just ChunkIngressService)
- PdService needs a new message variant (e.g., `ChunkArrivedFromPeer { key: ChunkKey, data: Arc<Bytes> }`)
- PdService must update `PartiallyReady` → `Ready` when all missing chunks arrive
- Broadcast trigger: when PdService provisions chunks for a new tx, broadcast the chunks to VersionPD peers
- (Optional) Pull mechanism: add `PdChunk(ledger, offset)` to `GossipDataRequestV2` for targeted fetch

## Architecture Diagram

```
Current (feat/pd):
  Mempool → PdChunkMessage::NewTransaction → PdService
                                                ↓
                                        storage_provider.get_unpacked_chunk_by_ledger_offset()
                                                ↓
                                        Found? → ChunkCache → ChunkDataIndex (DashMap) → EVM precompile
                                        Missing? → PartiallyReady (dead end)

With P2P gossip (feat-pd-chunks infrastructure + new wiring):
  Mempool → PdChunkMessage::NewTransaction → PdService
                                                ↓
                                        storage_provider.get_unpacked_chunk_by_ledger_offset()
                                                ↓
                                        Found? → ChunkCache → broadcast to peers (push)
                                        Missing? → PartiallyReady → wait for gossip
                                                                        ↑
  Peer broadcasts PdChunkMessage → /pd_chunks route → handle_pd_chunk()
                                                          ↓
                                                  ChunkIngressService (storage)
                                                          +
                                                  PdService::ChunkArrivedFromPeer → cache → check if any tx becomes Ready
```

## Code References

- `crates/actors/src/pd_service.rs:224-231` — "P2P gossip not yet implemented" gap (mempool path)
- `crates/actors/src/pd_service.rs:322-329` — Same gap (block validation path)
- `crates/actors/src/pd_service/provisioning.rs:14` — `PartiallyReady` doc: "P2P not yet implemented"
- `crates/actors/src/pd_service/cache.rs:1-395` — ChunkCache with reference tracking and shared DashMap index
- `crates/types/src/chunk_provider.rs:51-67` — PdChunkMessage enum (current, no network variants)
- `crates/types/src/chunk_provider.rs:32-43` — ChunkStorageProvider trait (local-only)
- `crates/p2p/src/server.rs` — All gossip route handlers
- `crates/p2p/src/gossip_data_handler.rs` — Data processing handlers
- `crates/p2p/src/gossip_client.rs` — Outbound gossip sending
- `crates/p2p/src/gossip_service.rs` — Broadcast loop
- `crates/p2p/src/types.rs:261-303` — GossipRoutes enum
- `crates/types/src/gossip.rs` — All gossip types (V1, V2, VersionPD)

## Branch References

- `origin/feat-pd-chunks` (tip: `7a7ac5da`) — P2P gossip infrastructure for PD chunks. **Reusable P2P layer**, but PdService is deleted.
- `origin/dmac/pd-chunk-service` (tip: `c0ed847a`) — Skeleton PD chunk service. **Fully superseded** by current PdService.
- `origin/feat-atomic-cache-updates` — May contain related cache work (not investigated).

## Reusability Assessment

| Component | Source Branch | Reusable? | Notes |
|---|---|---|---|
| `PdChunkMessage` gossip type | feat-pd-chunks | Yes | Chunk + range_specifier + timestamp — good design |
| `ProtocolVersion::VersionPD` | feat-pd-chunks | Yes | Clean version negotiation |
| `/pd_chunks` route + handler | feat-pd-chunks | Yes | Timestamp validation, rate limiting, peer checks |
| `GossipDataVersionPD::PdChunk` | feat-pd-chunks | Yes | Broadcast envelope with dedup |
| `GossipCacheKey::PdChunk` | feat-pd-chunks | Yes | Dedup cache key |
| `handle_pd_chunk()` handler | feat-pd-chunks | Partially | Needs to also notify PdService (currently only does ChunkIngress) |
| PD chunk → PdService notification | — | Must build | New message variant + state transition logic |
| Pull/request mechanism | — | Must build | Optional: add PdChunk to GossipDataRequestV2 |
| Broadcast trigger | — | Must build | PdService → broadcast channel on successful provision |
| dmac PDService skeleton | dmac/pd-chunk-service | No | Fully superseded by current PdService |
| dmac LRU modules | dmac/pd-chunk-service | No | Empty stubs, replaced by ChunkCache |
