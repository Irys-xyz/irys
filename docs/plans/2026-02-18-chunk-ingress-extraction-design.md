# Design: Extract ChunkIngressService from Mempool

## Problem

The mempool handles ~1,615 lines of chunk-related code (chunk validation, caching, storage writes, ingress proof generation/validation, gossip) that has no dependency on core mempool transaction state. This creates unnecessary lock contention on `AtomicMempoolState`, mixes concerns, and makes the mempool harder to reason about.

## Solution

Create a new `ChunkIngressService` actor that owns all chunk and ingress proof processing. The mempool's only remaining link is a one-way fire-and-forget message when a data TX is ingested, triggering pending chunk promotion.

## Architecture

```
Before:                              After:

POST /chunk --> Mempool              POST /chunk --> ChunkIngressService
P2P chunk   --> Mempool              P2P chunk   --> ChunkIngressService
DataSync    --> Mempool              DataSync    --> ChunkIngressService
BlockValid  --> Mempool              BlockValid  --> ChunkIngressService
P2P proof   --> Mempool              P2P proof   --> ChunkIngressService
                                     Mempool --notify--> ChunkIngressService
                                       (ProcessPendingChunks on data TX ingested)
```

## New Message Enum

```rust
pub enum ChunkIngressMessage {
    IngestChunk(UnpackedChunk, oneshot::Sender<Result<(), ChunkIngressError>>),
    IngestChunkFireAndForget(UnpackedChunk),
    IngestIngressProof(IngressProof, oneshot::Sender<Result<(), IngressProofError>>),
    ProcessPendingChunks(DataRoot),
}
```

## New Service Struct

```rust
pub struct ChunkIngressServiceInner {
    pub block_tree_read_guard: BlockTreeReadGuard,
    pub config: Config,
    pub exec: TaskExecutor,
    pub irys_db: DatabaseProvider,
    pub service_senders: ServiceSenders,
    pub storage_modules_guard: StorageModulesReadGuard,
    pub recent_valid_chunks: RwLock<LruCache<ChunkPathHash, ()>>,
    pub pending_chunks: RwLock<PriorityPendingChunks>,
}
```

## What Moves

| From | To |
|---|---|
| `mempool_service/chunks.rs` | `chunk_ingress_service/chunks.rs` |
| `mempool_service/ingress_proofs.rs` | `chunk_ingress_service/ingress_proofs.rs` |
| `mempool_service/pending_chunks.rs` | `chunk_ingress_service/pending_chunks.rs` |
| `MempoolState.recent_valid_chunks` | `ChunkIngressServiceInner` |
| `MempoolState.pending_chunks` | `ChunkIngressServiceInner` |
| `Inner.storage_modules_guard` | `ChunkIngressServiceInner` |

## Mempool Changes

- Remove `IngestChunk`, `IngestChunkFireAndForget`, `IngestIngressProof` from `MempoolServiceMessage`
- Remove `recent_valid_chunks`, `pending_chunks` from `MempoolState`
- Remove `storage_modules_guard` from `Inner`
- In `data_txs.rs::postprocess_data_ingress()`: send `ProcessPendingChunks(data_root)` to chunk ingress service
- Remove chunk/proof methods from `MempoolState`

## ServiceSenders Changes

Add `chunk_ingress: UnboundedSender<ChunkIngressMessage>` to `ServiceSendersInner` and corresponding receiver to `ServiceReceivers`.

## Caller Updates

| Caller | Change |
|---|---|
| `api-server/routes/post_chunk.rs` | Send to `chunk_ingress` instead of `mempool` |
| `p2p/gossip_data_handler.rs` | Route chunks + proofs via `ChunkIngressFacade` |
| `data_sync_service.rs` | Send `IngestChunkFireAndForget` to `chunk_ingress` |
| `block_validation.rs` | Send `IngestChunk` to `chunk_ingress` |
| `MempoolFacade` trait | Move chunk/proof methods to `ChunkIngressFacade` |
| `chain/tests/utils.rs` | Update test utilities |

## Coupling Bridge

One-way only: Mempool sends `ProcessPendingChunks(data_root)` to ChunkIngressService when a data TX header is ingested. No reverse dependency.

## Access Pattern Analysis

| Caller | Needs response? | Pattern |
|---|---|---|
| POST /chunk API | Yes (HTTP 200/400) | Request-response via oneshot |
| P2P gossip chunk | Yes (error mapping) | Request-response via oneshot |
| DataSyncService | No | Fire and forget |
| BlockValidationService | Yes (500ms timeout) | Request-response, tolerates timeout |
| P2P gossip proof | Yes (error mapping) | Request-response via oneshot |
| Mempool pending chunks | No | Fire and forget |

This mixed pattern (4/6 need responses) justifies the actor + oneshot approach over a pure fire-and-forget channel.
