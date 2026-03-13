# PD Chunk P2P Pull — Design Specification

**Date**: 2026-03-13
**Branch**: rob/pd-chunk-gossiping (off feat/pd)
**Status**: Draft

## Problem Statement

The PD (Programmable Data) service manages an in-memory chunk cache that the EVM's PD precompile reads from during transaction execution. Today, PdService is local-only: it loads chunks exclusively from the node's own StorageModules via `ChunkStorageProvider`. When a chunk is not available locally, two things break:

1. **Block validation**: A peer's block containing PD transactions is rejected as invalid. `shadow_transactions_are_valid` sends `ProvisionBlockChunks` to PdService, which responds `Err(missing)`. The block fails with `ValidationError::ShadowTransactionInvalid` — permanently, with no retry. This means a node that doesn't store the relevant data partition cannot validate blocks with PD transactions.

2. **Block building**: When a PD transaction enters the Reth mempool, `pd_transaction_monitor` fires `NewTransaction` exactly once. PdService marks the tx as `PartiallyReady` if chunks are missing. The tx is never added to `ready_pd_txs`, so the payload builder's `CombinedTransactionIterator` silently skips it on every block build attempt. There is no mechanism to re-attempt provisioning — the tx sits in `PartiallyReady` until Reth evicts it from the mempool.

Both paths need PdService to be able to fetch missing chunks from peers over the network.

## Design Decisions and Alternatives Considered

### Decision 1: Where does the blocking await happen?

**Chosen: CL/Irys side, in `shadow_transactions_are_valid`**

The block validation pipeline has two sides: the Irys consensus layer (CL) and the Reth execution layer (EL). The question was whether to block waiting for chunks on the CL side before submitting to Reth, or on the Reth side during EVM execution.

**Why not Reth side**: The PD precompile runs inside `IrysEvm::transact_raw` — fully synchronous EVM execution. `PdContext::get_chunk()` does a lock-free `DashMap::get()` and immediately returns `Ok(None)` if the chunk is missing, which causes `PdPrecompileError::ChunkNotFound`, the transaction reverts, and Reth marks the block as `PayloadStatusEnum::Invalid`. There is no async machinery inside Reth's block executor to await network fetches. Injecting one would require fundamental changes to Reth's execution model.

**Why CL side works**: `shadow_transactions_are_valid` already contains an unbounded `.await` for `wait_for_payload` (fetching EVM execution payloads from peers, with up to 10 retries at 5-second intervals). The concurrent validation `JoinSet` has no concurrency cap — blocking one block's validation task doesn't stall others. And `exit_if_block_is_too_old` races against `validate_block`, cancelling it if the tip advances more than `block_tree_depth` (50) blocks ahead, providing a natural safety net.

**Block building is unaffected**: The payload builder already gates PD tx inclusion via `ready_pd_txs.contains()`. Only transactions whose chunks are fully provisioned are included. No blocking is needed or desired in the block building path.

### Decision 2: Timeout vs. auto-cancellation

**Chosen: No explicit timeout — rely on the validation service's existing cancellation mechanism**

The validation pipeline already has `exit_if_block_is_too_old`, which races against `validate_block` via `futures::future::select`. If the canonical tip advances more than `block_tree_depth` blocks beyond the block being validated, the validation task is cancelled with `ValidationError::ValidationCancelled`. This provides a natural upper bound on how long a fetch can block.

Adding an explicit timeout would be redundant and would require choosing an arbitrary duration. The auto-cancellation mechanism is adaptive — it fires based on chain progress, not wall-clock time.

PdService implements a retry loop for failed fetches: on failure, it retries with a different assigned peer. The loop continues until either all chunks arrive or the request is cancelled (block validation cancelled or tx removed from mempool).

### Decision 3: Push vs. Pull for missing PD chunks

**Chosen: Pull (request-response)**

The `feat-pd-chunks` branch contains WIP work on push-based PD chunk gossip (broadcast to all `VersionPD` peers). We chose pull instead for the following reasons:

- **Efficiency**: Push broadcasts chunks to all peers regardless of whether they need them. Pull requests only the specific chunks that are missing, from peers that are likely to have them.
- **Latency**: A node validating a block or building a block needs specific chunks now. Waiting for a peer to happen to push them is non-deterministic. Pull gives the requesting node control over timing.
- **Bandwidth**: PD chunks are 256KB each. Broadcasting every PD chunk to every peer is expensive. Pull is targeted.
- **Simplicity**: The gossip pull mechanism (`/gossip/v2/pull_data`) already exists with peer authentication, handshake, and score tracking. Adding a new request variant is minimal work.

Push-based gossip may be added later as an optimization (proactive replication), but pull is the necessary foundation for correctness.

### Decision 4: Reuse DataSyncService vs. PdService owns fetching

**Chosen: PdService owns fetching directly**

DataSyncService was evaluated as a candidate for reuse since it already pulls chunks from peers. The analysis revealed deep structural incompatibilities:

**Coupling to StorageModules**: DataSyncService's `ChunkOrchestrator` requires a local `StorageModule` instance. `ChunkOrchestrator::new` panics without a `partition_assignment`. Queue population reads SM entropy intervals. Throttling reads SM disk throughput. PD chunk requests are for arbitrary `(ledger, offset)` pairs that may not correspond to any local StorageModule.

**Peer selection by slot**: `get_best_available_peers` filters peers by `(ledger_id, slot_index)` matching a local SM's assignment. PD chunks need peers selected by the partition that contains the requested offset — a different lookup entirely.

**Completion path hardwired to SM writes**: `on_chunk_completed` unconditionally calls `sm.write_data_chunk()` and falls back to `ChunkIngressService`. PD chunks need to go into PdService's `ChunkCache` and `ChunkDataIndex` DashMap, not into StorageModules.

**Tick-based, not responsive**: DataSyncService operates on a 250ms tick cycle. PD chunk fetching for block validation needs immediate dispatch — a validation task is blocking on a oneshot response.

**Packed chunks only**: The `GET /v1/chunk/ledger/{id}/{offset}` API endpoint returns `ChunkFormat::Packed` (XOR-encrypted data). PD chunks need unpacked bytes. Adding unpacking adds latency and CPU load.

**Cross-contamination**: Sharing `PeerBandwidthManager` would mix SM sync health scores with PD fetch results, distorting both.

**Priority contention**: DataSyncService uses `UnpackingPriority::Background`. PD chunk fetching is latency-sensitive (a block validation is waiting). Sharing the unpacking queue could delay PD responses.

**Why PdService is the right owner**:
- PdService already tracks exactly which chunks are missing per tx (`TxProvisioningState.missing_chunks`)
- PdService needs to atomically transition `PartiallyReady -> Ready` and update `ready_pd_txs` + `ChunkDataIndex`
- Adding an intermediary service would be a pass-through that complicates the state machine without adding value

### Decision 5: Concurrency model within PdService

**Chosen: Structured concurrency with JoinSet owned by PdService**

PdService is currently a single-threaded actor — its `select!` loop processes messages synchronously. Three options were considered for adding async chunk fetching:

**(A) Spawn-and-callback via channel**: Spawn detached tokio tasks for HTTP fetches, each sending results back via a new internal channel. PdService's `select!` loop gains a second arm. Follows the DataSyncService pattern.

**(B) Dedicated fetcher service**: A separate long-lived `PdChunkFetcherService` task owns the HTTP client. PdService sends fetch requests, receives results. More isolation but adds cross-service coordination.

**(C) JoinSet / FuturesUnordered owned by PdService** (chosen): PdService owns a `JoinSet<PdChunkFetchResult>`. The `select!` loop polls `join_set.join_next()` alongside the message channel. This follows the same pattern as `ValidationCoordinator` which uses `JoinSet<ConcurrentValidationResult>`.

**Why JoinSet**:
- No extra channel needed — results come back directly typed
- Structured concurrency — tasks are owned by the service, automatically cancelled on shutdown when the JoinSet drops
- Clean integration with `tokio::select!` via `join_next()`
- Precedent in the codebase (`ValidationCoordinator.concurrent_tasks`)

### Decision 6: Transport mechanism for pulling chunks

**Chosen: Gossip pull via `/gossip/v2/pull_data` with new `PdChunk` request variant**

Three options were considered:

**(A) Fetch packed via existing API endpoint**: Reuse `GET /v1/chunk/ledger/{id}/{offset}`. No API changes, but returns packed (XOR-encrypted) data. PdService would need to send unpacking requests to the packing service, adding latency and CPU load on the requesting node.

**(B) New unpacked chunk API endpoint**: Add `GET /v1/chunk/ledger/{id}/{offset}/unpacked`. The serving node unpacks before sending. Shifts CPU cost to the data holder. Requires a new API route and doesn't benefit from gossip infrastructure (peer auth, handshake, scoring).

**(C) Gossip pull mechanism** (chosen): Add `PdChunk(u32, u64)` to `GossipDataRequestV2`. The serving peer handles it in `resolve_data_request` by calling a new `get_full_unpacked_chunk_by_ledger_offset` method on `ChunkStorageProvider` that returns the full `UnpackedChunk` (including data bytes, merkle proof/data_path, data_root, and data_size). The existing `get_unpacked_chunk_by_ledger_offset` only returns raw `Bytes` (discards the proof), so a new method is needed. Returns the chunk in the existing `GossipDataV2::Chunk` envelope.

**Why gossip pull**:
- Leverages existing peer authentication (handshake + staking verification)
- Leverages existing peer score tracking (health scores, circuit breakers)
- No new HTTP routes — reuses `/gossip/v2/pull_data`
- The serving peer unpacks using its own locally-available entropy — no extra round trips
- V1 peers gracefully excluded (`PdChunk.to_v1()` returns `None`)

### Decision 7: Peer selection strategy

**Chosen: Targeted by partition assignment**

Three strategies were considered:

**(A) Dumb fan-out**: Ask top N active peers, first to respond wins. Simple but wastes requests to peers that don't have the data.

**(B) Targeted by partition assignment** (chosen): Compute the partition from the ledger offset using `partition_index = offset / num_chunks_in_partition`. Look up assigned miners in `canonical_epoch_snapshot().partition_assignments.data_partitions` for the relevant `(ledger_id, slot_index)`. Resolve those miners to gossip addresses via `PeerList`. Request chunks from those specific peers.

**(C) Targeted with fallback**: Try assigned miners first, fall back to fan-out. More robust but more complex.

**Why targeted**: PD chunks live at specific ledger offsets that map deterministically to partition slots. The epoch snapshot's partition assignments tell us exactly which miners are responsible for storing that data. Requesting from those miners is both efficient (high hit rate) and fast (no wasted requests).

### Decision 8: Validation of pulled chunks

**Chosen: Validate merkle proofs on pulled chunks**

The serving peer is authenticated via the gossip handshake (signature + staking check), but we add merkle proof validation as a defense-in-depth measure. The pulled `UnpackedChunk` includes `data_path` (merkle inclusion proof) which can be validated against the expected `data_root` to confirm the chunk data is authentic.

This follows the same principle used in `ChunkIngressService::handle_chunk_ingress_message`, which validates merkle proofs on all incoming chunks regardless of source.

## Architecture

### Component Diagram

```
                          PdService (single actor, owns JoinSet)
                         ┌─────────────────────────────────────────────┐
                         │                                             │
  pd_transaction_monitor │  select! {                                  │
  ───NewTransaction────> │    shutdown,                                │
                         │    msg_rx.recv() => handle_message(),       │
  shadow_txs_are_valid   │    join_set.join_next() => on_fetch_done(), │
  ──ProvisionBlockChunks>│  }                                         │
                         │                                             │
  pd_transaction_monitor │  State:                                     │
  ──TransactionRemoved──>│    cache: ChunkCache (LRU + DashMap mirror) │
                         │    tracker: ProvisioningTracker             │
  PdBlockGuard::drop()   │    pending_fetches: HashMap<ChunkKey, ...>  │
  ──ReleaseBlockChunks──>│    pending_blocks: HashMap<B256, ...>       │
                         │    join_set: JoinSet<PdChunkFetchResult>    │
                         │                                             │
                         │  New deps:                                  │
                         │    gossip_client: GossipClient              │
                         │    peer_list: PeerList                      │
                         │    block_tree: BlockTreeReadGuard           │
                         │    consensus_config: ConsensusConfig        │
                         └──────────────────┬──────────────────────────┘
                                            │ spawns fetch tasks
                                            ▼
                         ┌─────────────────────────────────────────────┐
                         │  Fetch Task (runs in JoinSet)               │
                         │                                             │
                         │  1. Compute partition from (ledger, offset) │
                         │  2. Look up assigned miners (epoch snapshot)│
                         │  3. Resolve to gossip addresses (PeerList)  │
                         │  4. Pull via /gossip/v2/pull_data           │
                         │     (GossipDataRequestV2::PdChunk)          │
                         │  5. Validate merkle proof on response       │
                         │  6. Retry with next peer on failure         │
                         │  7. Return PdChunkFetchResult               │
                         └──────────────────┬──────────────────────────┘
                                            │
                                            ▼
                         ┌─────────────────────────────────────────────┐
                         │  Serving Peer (gossip_data_handler)         │
                         │                                             │
                         │  resolve_data_request:                      │
                         │    PdChunk(ledger, offset) =>               │
                         │      storage_provider                       │
                         │        .get_unpacked_chunk_by_ledger_offset │
                         │      => GossipDataV2::Chunk(unpacked)       │
                         └─────────────────────────────────────────────┘
```

### Data Flow: Block Validation Path

```
Peer block arrives with PD transactions
  │
  ▼
BlockDiscoveryService::block_discovered (pre-validation)
  │
  ▼
BlockTreeService::on_block_prevalidated
  │
  ▼
ValidationService::ValidateBlock
  │
  ▼
BlockValidationTask::execute_concurrent
  │
  ├── futures::select(validate_block(), exit_if_block_is_too_old())
  │
  ▼
validate_block() → tokio::join!(
    recall_task,
    poa_task,
    shadow_tx_task,    ◄── PD chunk provisioning happens here
    seeds_task,
    commitment_task,
    data_txs_task
)
  │
  ▼
shadow_transactions_are_valid:
  1. wait_for_payload (existing, may block for EVM payload)
  2. Extract PdChunkSpecs from block's PD transactions
  3. Send ProvisionBlockChunks { block_hash, chunk_specs, response } to PdService
  4. .await on oneshot response  ◄── BLOCKS HERE until chunks fetched
  │                                   or cancelled by exit_if_block_is_too_old
  ▼
PdService::handle_provision_block_chunks:
  1. Convert specs to ChunkKeys via specs_to_keys()
  2. For each key:
     a. Cache hit → add_reference(key, block_hash)
     b. Cache miss, local storage hit → cache.insert, add to DashMap
     c. Cache miss, local storage miss → add to missing set
  3. If no missing chunks → respond Ok(()), create block_tracker entry
  4. If missing chunks:
     a. Store oneshot in pending_blocks[block_hash]
     b. For each missing key: register in pending_fetches, spawn fetch task
     c. Return (don't respond yet — oneshot held open)
  │
  ▼ (async, via JoinSet)
Fetch tasks execute:
  1. Compute partition_index = offset / num_chunks_in_partition
  2. Look up assigned miners from epoch snapshot
  3. For each assigned peer (in order):
     a. gossip_client.pull_data(peer, PdChunk(ledger, offset))
     b. If Ok(Some(chunk)): validate merkle proof, return Ok(chunk)
     c. If Ok(None) or Err: try next peer
  4. If all peers exhausted: sleep briefly, retry from the beginning
  │
  ▼
PdService::on_fetch_done (select! arm 3):
  On Ok(chunk):
    1. Validate merkle proof on the received UnpackedChunk
    2. Insert chunk bytes into cache (cache.insert → DashMap mirror updated)
    3. Remove key from pending_fetches, consuming the PdChunkFetchState
    4. For each waiting block in the consumed state's waiting_blocks:
       a. Remove key from pending_blocks[block_hash].remaining_keys
       b. If remaining_keys is empty:
          - Respond Ok(()) on the stored oneshot
          - Move chunk_keys to block_tracker (same as today's success path)
    5. For each waiting tx in the consumed state's waiting_txs:
       a. Remove key from tracker[tx_hash].missing_chunks
       b. If missing_chunks is empty:
          - Transition state to Ready
          - Insert tx_hash into ready_pd_txs
  On Err(AllPeersFailed):
    1. Check if any waiters remain (block oneshots not closed, txs not removed)
    2. If waiters remain: update status, sleep, respawn fetch task
    3. If no waiters remain: remove from pending_fetches, skip respawn
  │
  ▼
shadow_transactions_are_valid receives Ok(()) on the oneshot
  → Creates PdBlockGuard (RAII, sends ReleaseBlockChunks on drop)
  → Continues with shadow tx structural validation
  → Returns (execution_data, pd_guard)
  │
  ▼
All 6 tasks complete via tokio::join!
  → If all Valid: submit_payload_to_reth (Reth executes, precompile reads DashMap)
  → PdBlockGuard drops → ReleaseBlockChunks → cache references decremented
```

### Data Flow: Mempool Path

```
PD transaction enters Reth mempool
  │
  ▼
pd_transaction_monitor detects PD header + access list
  → PdChunkMessage::NewTransaction { tx_hash, chunk_specs }
  │
  ▼
PdService::handle_provision_chunks:
  1. Duplicate guard: if tracker.get(&tx_hash).is_some() → return
  2. Convert specs to ChunkKeys
  3. Register in tracker (state = Provisioning)
  4. For each key:
     a. Cache hit → add_reference
     b. Local storage hit → cache.insert
     c. Local storage miss → add to missing set
  5. If no missing → state = Ready, ready_pd_txs.insert(tx_hash)
  6. If missing:
     a. state = PartiallyReady { found, total }
     b. For each missing key: register in pending_fetches, spawn fetch task
     c. Return (tx stays PartiallyReady)
  │
  ▼ (async, via JoinSet — same as block validation path)
Fetch tasks execute and return results
  │
  ▼
PdService::on_fetch_done:
  → Insert chunk into cache
  → Update all waiting txs
  → When last missing chunk arrives for a tx:
     state = Ready, ready_pd_txs.insert(tx_hash)
  │
  ▼
Next block build attempt:
  CombinedTransactionIterator::next()
    → ready_pd_txs.contains(&tx_hash) → true
    → PD transaction included in block
    → EVM executes → PD precompile reads from DashMap → succeeds
```

### Data Flow: Cancellation

```
Case 1: Block validation cancelled (tip advanced too far)
─────────────────────────────────────────────────────────
exit_if_block_is_too_old resolves first in futures::select
  → validate_block future dropped
  → oneshot receiver dropped (shadow_tx_task was awaiting it)
  │
  ▼
PdService tries to send Ok(()) on stored oneshot → SendError
  → Remove pending_blocks[block_hash] entry
  → Decrement cache references for any already-fetched chunks
  → Remove block_hash from pending_fetches[key].waiting_blocks
  → If no other waiters for those keys → abort fetch tasks in JoinSet
  → cache.try_shrink_to_fit()

Case 2: Mempool transaction removed
────────────────────────────────────
pd_transaction_monitor fires TransactionRemoved
  → PdService::handle_release_chunks (existing logic)
  → Additionally: remove tx_hash from pending_fetches[key].waiting_txs
  → If no other waiters for those keys → abort fetch tasks in JoinSet
  → Decrement cache references, evict unreferenced chunks
  → cache.try_shrink_to_fit()

Case 3: PdService shutdown
──────────────────────────
PdService select! loop exits on shutdown signal
  → JoinSet drops → all in-flight fetch tasks cancelled automatically
  → No cleanup needed (structured concurrency)
```

## Internal State Changes

### New Types in PdService

```rust
/// Result returned by a chunk fetch task in the JoinSet.
struct PdChunkFetchResult {
    key: ChunkKey,
    result: Result<UnpackedChunk, PdChunkFetchError>,
}

/// Tracks an in-flight fetch for a single ChunkKey.
struct PdChunkFetchState {
    /// Mempool transactions waiting on this chunk.
    waiting_txs: HashSet<B256>,
    /// Block validations waiting on this chunk.
    waiting_blocks: HashSet<B256>,
    /// Number of fetch attempts so far (incremented on each respawn).
    attempt: u32,
    /// Peers that have been tried and failed (passed to the next fetch task).
    excluded_peers: HashSet<IrysAddress>,
}

/// Tracks a block validation waiting for chunks to be fetched.
struct PendingBlockProvision {
    /// Chunks still missing for this block.
    remaining_keys: HashSet<ChunkKey>,
    /// All chunk keys for this block (for reference cleanup).
    all_keys: Vec<ChunkKey>,
    /// The oneshot sender to respond on when all chunks arrive.
    response: oneshot::Sender<Result<(), Vec<(u32, u64)>>>,
}
```

### Modified PdService Struct

```rust
pub struct PdService {
    // Existing fields
    shutdown: Shutdown,
    msg_rx: PdChunkReceiver,
    cache: ChunkCache,
    tracker: ProvisioningTracker,
    storage_provider: Arc<dyn ChunkStorageProvider>,
    block_tracker: HashMap<B256, Vec<ChunkKey>>,
    ready_pd_txs: Arc<DashSet<B256>>,

    // New fields
    join_set: JoinSet<PdChunkFetchResult>,
    pending_fetches: HashMap<ChunkKey, PdChunkFetchState>,
    pending_blocks: HashMap<B256, PendingBlockProvision>,
    gossip_client: GossipClient,
    peer_list: PeerList,
    block_tree: BlockTreeReadGuard,
    num_chunks_in_partition: u64,
}
```

### Modified Event Loop

```rust
async fn start(mut self) {
    loop {
        tokio::select! {
            biased;

            _ = &mut self.shutdown => { break; }

            msg = self.msg_rx.recv() => {
                match msg {
                    Some(message) => self.handle_message(message),
                    None => break,
                }
            }

            Some(result) = self.join_set.join_next() => {
                match result {
                    Ok(fetch_result) => self.on_fetch_done(fetch_result),
                    Err(join_error) => {
                        // Task panicked or was cancelled
                        warn!("PD chunk fetch task failed: {}", join_error);
                    }
                }
            }
        }
    }
}
```

### Partition-to-Peer Resolution

**Ledger ID**: Currently, `specs_to_keys()` hardcodes `ledger: 0` for all PD chunks (`pd_service.rs:155`). All PD chunk data lives on the Publish ledger (ledger 0). The `GossipDataRequestV2::PdChunk(u32, u64)` carries the ledger ID for forward-compatibility, but in practice it will be 0. The `resolve_peers_for_chunk` method uses the Publish ledger (`DataLedger::Publish`) for partition assignment lookups.

**Partition assignment lookup**: The `PartitionAssignments::data_partitions` is a `BTreeMap<PartitionHash, PartitionAssignment>` keyed by partition hash, not by index. To find which miners are assigned to the partition containing a given offset, we must iterate the entries and filter by matching `ledger_id` and computed `slot_index`. The `slot_index` is derived from the ledger offset: `slot_index = offset / num_chunks_in_partition`. Each `PartitionAssignment` has a `slot_index` field that can be compared.

**Self-peer filtering**: The resolved peer list must exclude the node's own miner address to avoid requesting chunks from itself.

```rust
fn resolve_peers_for_chunk(&self, key: &ChunkKey) -> Vec<PeerAddress> {
    let slot_index = key.offset / self.num_chunks_in_partition;
    let ledger_id = DataLedger::Publish;

    let epoch_snapshot = self.block_tree.canonical_epoch_snapshot();
    let assignments = &epoch_snapshot.partition_assignments.data_partitions;

    let mut peers = Vec::new();
    for (_hash, assignment) in assignments.iter() {
        if assignment.ledger_id == Some(ledger_id)
            && assignment.slot_index == slot_index
            && assignment.miner_address != self.own_miner_address  // filter self
        {
            if let Some(peer) = self.peer_list.peer_by_mining_address(&assignment.miner_address) {
                peers.push(peer.address.clone());
            }
        }
    }
    peers
}
```

**Epoch snapshot choice**: We use `canonical_epoch_snapshot()` (the snapshot from the canonical chain tip) rather than a fork-specific snapshot. This is correct because we are looking up which miners *store* data, not validating consensus. Storage assignments are determined by the canonical chain regardless of which fork a block being validated is on.

### Fetch Deduplication

When multiple transactions or blocks need the same chunk:

```
NewTransaction(tx_A) needs chunks [C1, C2, C3]
  → C1: not in pending_fetches → spawn fetch, register tx_A as waiter
  → C2: not in pending_fetches → spawn fetch, register tx_A as waiter
  → C3: not in pending_fetches → spawn fetch, register tx_A as waiter

ProvisionBlockChunks(block_X) needs chunks [C2, C4]
  → C2: already in pending_fetches (InFlight) → just add block_X as waiter
  → C4: not in pending_fetches → spawn fetch, register block_X as waiter

When C2 fetch completes:
  → Insert into cache
  → Update tx_A: remove C2 from missing_chunks (2 remaining: C1, C3)
  → Update block_X: remove C2 from remaining_keys (1 remaining: C4)
```

## Gossip Protocol Changes

### Wire Type Additions (`crates/types/src/gossip.rs`)

```rust
// Add to GossipDataRequestV2:
pub enum GossipDataRequestV2 {
    ExecutionPayload(B256),
    BlockHeader(BlockHash),
    BlockBody(BlockHash),
    Chunk(ChunkPathHash),
    Transaction(H256),
    PdChunk(u32, u64),  // NEW: (ledger_id, ledger_offset)
}

// V1 compatibility:
impl GossipDataRequestV2 {
    pub fn to_v1(&self) -> Option<GossipDataRequestV1> {
        match self {
            // ... existing variants ...
            Self::PdChunk(..) => None,  // V1 peers cannot serve PD chunks
        }
    }
}
```

### Serving Handler (`crates/p2p/src/gossip_data_handler.rs`)

`resolve_data_request` gains a new match arm:

```rust
GossipDataRequestV2::PdChunk(ledger, offset) => {
    match self.storage_provider.get_full_unpacked_chunk_by_ledger_offset(ledger, offset) {
        Ok(Some(unpacked_chunk)) => {
            Some(GossipDataV2::Chunk(Arc::new(unpacked_chunk)))
        }
        Ok(None) => None,  // We don't have this chunk
        Err(e) => {
            warn!(ledger, offset, "Error serving PD chunk: {}", e);
            None
        }
    }
}
```

**New trait method required**: The existing `ChunkStorageProvider::get_unpacked_chunk_by_ledger_offset` returns only raw `Bytes` — it discards the merkle proof (`data_path`), `data_root`, and `data_size` during unpacking (`chunk_provider.rs:130-155`). A new method `get_full_unpacked_chunk_by_ledger_offset` is needed that returns the full `UnpackedChunk` including the merkle proof. The implementation follows the same path as the existing method but preserves the `data_path` from the storage module's index and includes `data_root` and `data_size` from `DataRootInfo`. This is essential for the requesting node to validate the merkle proof on the pulled chunk.

```rust
// New method on ChunkStorageProvider trait:
fn get_full_unpacked_chunk_by_ledger_offset(
    &self,
    ledger: u32,
    ledger_offset: u64,
) -> eyre::Result<Option<UnpackedChunk>>;
```

The concrete implementation in `ChunkProvider` (`crates/domain/src/models/chunk_provider.rs`) would call `generate_full_chunk` on the storage module (which already returns all the metadata including `data_path`), then unpack the chunk bytes via `irys_packing::unpack()`, and assemble the full `UnpackedChunk` with the preserved merkle proof.

This requires wiring a `ChunkStorageProvider` (specifically `Arc<ChunkProvider>`) into `GossipDataHandler`. The `ChunkProvider` is already constructed in `chain.rs` and can be passed through `P2PService::run` into `GossipDataHandler::new`. Note that `GossipDataHandler` is in `crates/p2p` and `ChunkProvider` is in `crates/domain`, but `GossipDataHandler` only needs the `ChunkStorageProvider` trait (from `irys-types`, which `p2p` already depends on). The concrete `ChunkProvider` is injected from `chain.rs`.

### No New Route Needed

The existing `/gossip/v2/pull_data` endpoint handles all `GossipDataRequestV2` variants. Adding `PdChunk` to the enum is sufficient. The `handle_get_data_sync` handler calls `resolve_data_request` and returns the result — no route registration changes.

## Cache Insertion: UnpackedChunk to Bytes Conversion

The gossip pull returns a full `UnpackedChunk` (data bytes + merkle proof + metadata). The `ChunkCache` stores `Arc<Bytes>` (just the raw chunk data), and the `ChunkDataIndex` DashMap maps `(u32, u64) → Arc<Bytes>`.

When a pulled chunk passes validation, PdService extracts the raw bytes for cache insertion:

```rust
let data: Arc<Bytes> = Arc::new(Bytes::from(unpacked_chunk.bytes));
self.cache.insert(key, data, reference_id);
// cache.insert internally mirrors to the ChunkDataIndex DashMap
```

The merkle proof (`data_path`), `data_root`, and `data_size` are used only for validation and then discarded. This matches how the existing local storage path works: `ChunkStorageProvider::get_unpacked_chunk_by_ledger_offset` returns raw `Bytes`, and `cache.insert` stores `Arc::new(chunk_bytes)`.

## Validation of Pulled Chunks

When PdService receives an `UnpackedChunk` from a peer via the pull mechanism, it performs merkle proof validation before inserting into the cache:

1. **Merkle path validation**: Call `validate_path(data_root, &chunk.data_path, target_offset)` to verify the chunk's inclusion proof against the expected data root.
2. **Leaf hash check**: Verify `SHA256(chunk.bytes) == leaf_hash` from the validated path.
3. **Data size check**: If the chunk's `data_size` is known, verify it's consistent.

This follows the same validation pattern used in `ChunkIngressService::handle_chunk_ingress_message` (steps 5-6 of its pipeline). If validation fails, the chunk is rejected, the peer is marked as having returned invalid data, and the fetch retries with a different peer.

## File Change Summary

| File | Change |
|---|---|
| `crates/types/src/gossip.rs` | Add `GossipDataRequestV2::PdChunk(u32, u64)` variant, `to_v1()` returns `None` |
| `crates/types/src/chunk_provider.rs` | Add `get_full_unpacked_chunk_by_ledger_offset` method to `ChunkStorageProvider` trait, returning `eyre::Result<Option<UnpackedChunk>>` |
| `crates/domain/src/models/chunk_provider.rs` | Implement `get_full_unpacked_chunk_by_ledger_offset` on `ChunkProvider`: call `generate_full_chunk` on the SM, unpack the bytes, assemble full `UnpackedChunk` preserving `data_path`, `data_root`, `data_size` |
| `crates/p2p/src/gossip_data_handler.rs` | Handle `PdChunk` in `resolve_data_request`; add `storage_provider: Arc<dyn ChunkStorageProvider>` field |
| `crates/p2p/src/gossip_client.rs` | Add `pull_data_from_peers` method that accepts a pre-selected list of peer addresses (instead of using `top_active_peers`). Follows the same retry/handshake pattern as `pull_data_from_network` but with caller-supplied peers. |
| `crates/p2p/src/gossip_service.rs` | Pass `ChunkProvider` through to `GossipDataHandler` construction |
| `crates/actors/src/pd_service.rs` | Add `JoinSet`, `pending_fetches`, `pending_blocks`, `own_miner_address`, new deps (`GossipClient`, `PeerList`, `BlockTreeReadGuard`, `ConsensusConfig`). Refactor `handle_provision_chunks` and `handle_provision_block_chunks` to spawn fetch tasks on miss. Add `on_fetch_done` handler with return-and-respawn retry logic. Add third `select!` arm for `join_set.join_next()`. |
| `crates/actors/src/pd_service/provisioning.rs` | Add `PartiallyReady -> Ready` transition method callable from `on_fetch_done`. Extend `TxProvisioningState` or add helper for checking/clearing `missing_chunks`. |
| `crates/actors/src/pd_service/cache.rs` | No structural changes (insert/remove/reference tracking already work) |
| `crates/chain/src/chain.rs` | Pass `GossipClient`, `PeerList`, `BlockTreeReadGuard`, `ConsensusConfig`, `own_miner_address` to `PdService::spawn_service` |

## Edge Cases

### Chunk needed by both mempool tx and block validation simultaneously

Handled by `pending_fetches` deduplication. A single `ChunkKey` entry tracks both `waiting_txs` and `waiting_blocks`. Only one fetch task is spawned. When the chunk arrives, both the tx's `missing_chunks` and the block's `remaining_keys` are updated.

### Oneshot receiver dropped before response sent

When block validation is cancelled, the oneshot receiver in `shadow_transactions_are_valid` is dropped. PdService detects this when it attempts `response.send(Ok(()))` — the `send` returns `Err`. PdService then cleans up: removes the `PendingBlockProvision` entry, decrements cache references for any chunks already fetched for that block, and removes the block hash from all `pending_fetches` entries. This mirrors the existing cleanup path in `handle_provision_block_chunks` when the oneshot fails (lines 352-363).

### All assigned peers fail to serve a chunk

The fetch task uses a **return-and-respawn** retry model. Each fetch task attempts all assigned peers once. If all fail, the task returns `Err(PdChunkFetchError::AllPeersFailed { excluded_peers })` to PdService via `join_set.join_next()`. The `on_fetch_done` handler then:

1. Updates `pending_fetches[key].status` to `Retrying { attempt: n+1, excluded_peers }`
2. Sleeps briefly (e.g., 1 second, scaling with attempt count up to a cap)
3. Re-reads `resolve_peers_for_chunk` (picks up any epoch changes or peers coming online)
4. Spawns a new fetch task into the JoinSet

This return-and-respawn model (vs. internal infinite retry within the task) ensures PdService can observe intermediate failures, update `FetchStatus`, and decide whether to continue retrying based on whether waiters still exist. If all waiters for a key have been removed (block cancelled, tx removed) between retry rounds, PdService simply skips the respawn.

**Mempool tx retry bound**: For mempool transactions, retries continue until either the chunk is fetched or the transaction is removed from the Reth pool (which fires `TransactionRemoved` via `pd_transaction_monitor`). Reth's pool maintenance evicts transactions based on `max_queued_lifetime`, providing a natural upper bound. PdService does not need its own timeout for this path.

**Block validation retry bound**: For block validations, retries continue until the chunk arrives or the oneshot receiver is dropped (block validation cancelled by `exit_if_block_is_too_old`). PdService detects the dropped receiver when it checks `response.is_closed()` before spawning a retry.

The fetch task terminates after one pass through available peers:
- The chunk is successfully fetched → return `Ok(chunk)`
- All peers tried, all failed → return `Err(AllPeersFailed)`
- The task is aborted by JoinSet (PdService shutdown or explicit abort)

### Chunk arrives via gossip push while a pull is in-flight

If the codebase later adds push-based PD chunk gossip, a chunk could arrive via gossip and be written to storage while a pull fetch is in-flight for the same key. The pull fetch would return a duplicate chunk. PdService handles this gracefully: `cache.insert` for a key that already exists simply adds the reference without duplicating data (existing behavior at `cache.rs:85-91`).

### PdService message queue backpressure during fetch storms

PdService uses an unbounded channel (`mpsc::UnboundedSender`). A large number of `ProvisionBlockChunks` messages could queue up. However, this is the existing design and is bounded by the rate of block arrival. The JoinSet tasks run concurrently on the tokio runtime and do not block PdService's message processing — results are harvested via `join_next()` in the select loop.

### Duplicate ProvisionBlockChunks for the same block_hash

If two `ProvisionBlockChunks` messages arrive for the same `block_hash` (e.g., same block discovered via multiple gossip paths), the second message is rejected. Before storing the oneshot in `pending_blocks`, PdService checks if `pending_blocks.contains_key(&block_hash)`. If it does, the second oneshot is responded to with `Err` immediately — the first request is already in-flight and will complete. This prevents overwriting the first oneshot sender and losing it.

### Partition assignment changes during fetch

If the epoch transitions while fetch tasks are in-flight, the assigned miners for a partition may change. In-flight tasks continue with the stale peer list. If those peers no longer serve the data, the fetches fail and retry. On retry, `resolve_peers_for_chunk` reads the current epoch snapshot, getting the updated assignments. This is self-correcting.

## Observability

The PD chunk fetch path is latency-sensitive (blocks validation) and should be instrumented:

- **`pd_chunk_fetch_duration_ms`** — histogram of time from fetch task spawn to successful result, labeled by source (mempool vs. block_validation)
- **`pd_chunk_fetch_attempts`** — counter of fetch attempts, labeled by outcome (success, peer_not_found, peer_error, validation_failed)
- **`pd_chunk_fetch_retries`** — counter of retry respawns per chunk key
- **`pd_chunks_pending`** — gauge of currently in-flight fetch tasks (JoinSet size)
- **`pd_provision_cache_hit_rate`** — ratio of cache hits to total chunk lookups during provision
- **`pd_provision_local_storage_hit_rate`** — ratio of local storage hits to cache misses
- **`pd_tx_partially_ready_count`** — gauge of mempool txs in `PartiallyReady` state
- **`pd_block_provision_wait_ms`** — histogram of time a block validation waits on the oneshot (from `ProvisionBlockChunks` send to `Ok(())` response)

These follow the existing metrics patterns in the codebase (e.g., `record_gossip_chunk_processing_duration`, `record_gossip_chunk_received`).
