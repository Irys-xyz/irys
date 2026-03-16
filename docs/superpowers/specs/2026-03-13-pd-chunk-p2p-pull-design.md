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

**Implementation constraint: `exit_if_block_is_too_old` is progress-based and only fires when the tip advances.** If the network is stalled (no new blocks) and peers do not serve the chunk, validation can hang indefinitely while fetch rounds keep retrying. As defense-in-depth, PdService enforces a `MAX_CHUNK_FETCH_RETRIES` per chunk key. When exceeded, the chunk is failed permanently and the block validation completes with an error. This bounds retry behavior even when the progress-based cancellation cannot fire. See the `on_fetch_done` retry path for the implementation.

PdService implements a retry loop for failed fetches: on failure, it retries with a different assigned peer via a `DelayQueue`-based backoff mechanism. The loop continues until either all chunks arrive, the request is cancelled (block validation cancelled or tx removed from mempool), or `MAX_CHUNK_FETCH_RETRIES` is exceeded.

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

**Chosen: Public HTTP API (primary) with gossip pull fallback**

Four options were considered:

**(A) Fetch packed via existing API endpoint**: Reuse `GET /v1/chunk/ledger/{id}/{offset}`. No API changes, but returns packed (XOR-encrypted) data without `tx_path`. PdService would need to send unpacking requests to the packing service, adding latency and CPU load on the requesting node. No way to verify chunk is bound to the correct ledger position.

**(B) New unpacked-only API endpoint**: Add `GET /v1/chunk/ledger/{id}/{offset}/unpacked`. The serving node always unpacks before sending. Shifts CPU cost to the data holder on every request — unpacking is expensive (entropy recomputation via C/CUDA). Does not benefit from gossip infrastructure.

**(C) Gossip pull only**: Add `PdChunk(u32, u64)` to `GossipDataRequestV2`. Leverages gossip infrastructure (peer auth, scoring, circuit breakers). However, requires gossip handshake authentication — unstaked peers (validator-only nodes) may be unable to initiate pull requests.

**(D) Existing public HTTP API with gossip fallback** (chosen): Reuse the existing `GET /v1/chunk/ledger/{id}/{offset}` endpoint as-is — no API changes. The endpoint always returns `ChunkFormat::Packed`. The requesting node unpacks locally. If the public API fails (peer offline, network error), PdService falls back to gossip pull via `GossipDataRequestV2::PdChunk`. Chunk authenticity is verified locally by the requester via `data_root` derivation from its own MDBX (see Decision 8).

**Why existing public API primary**:
- **Zero API changes**: No new endpoints, no modified responses, fully backwards compatible with all existing nodes
- **No authentication barrier**: Public API requires no gossip handshake or staking verification. Unstaked validator-only nodes can fetch chunks without special configuration.
- **Follows DataSyncService precedent**: `DataSyncService` already fetches chunks via this exact endpoint. This is the established pattern for chunk retrieval.
- **HTTP caching friendly**: Can benefit from future caching infrastructure (reverse proxy, CDN)

**Why gossip fallback**:
- Gossip pull provides an authenticated fallback with peer scoring and circuit breakers
- Useful when the public API is unreachable but the gossip connection is established
- Reuses existing `/gossip/v2/pull_data` infrastructure

**Receiver-side unpacking**: The existing endpoint always returns `ChunkFormat::Packed`. PdService unpacks locally using `irys_packing::unpack()` with entropy derived from `(packing_address, partition_hash, chunk_offset, chain_id)`. The `PackedChunk` struct carries `packing_address` and `partition_hash`, so the receiver has all necessary inputs. The existing `block_validation.rs` already handles packed chunks in its fetch path — this is an established pattern.

**TODO: Future optimization — serve unpacked when cached.** The serving node's MDBX `CachedChunks` table (populated by `ChunkIngressService`) contains unpacked versions of recently-ingested chunks. A future update should extend the endpoint (or add a new one with a query parameter like `?format=auto`) to return `ChunkFormat::Unpacked` when available, avoiding unnecessary unpacking on the receiver. This change requires a backwards-compatible wire format (e.g., new endpoint or opt-in parameter) since existing consumers expect `ChunkFormat::Packed`.

### Decision 7: Peer selection strategy

**Chosen: Targeted by partition assignment**

Three strategies were considered:

**(A) Dumb fan-out**: Ask top N active peers, first to respond wins. Simple but wastes requests to peers that don't have the data.

**(B) Targeted by partition assignment** (chosen): Compute the partition from the ledger offset using `partition_index = offset / num_chunks_in_partition`. Look up assigned miners in `canonical_epoch_snapshot().partition_assignments.data_partitions` for the relevant `(ledger_id, slot_index)`. Resolve those miners to gossip addresses via `PeerList`. Request chunks from those specific peers.

**(C) Targeted with fallback**: Try assigned miners first, fall back to fan-out. More robust but more complex.

**Why targeted**: PD chunks live at specific ledger offsets that map deterministically to partition slots. The epoch snapshot's partition assignments tell us exactly which miners are responsible for storing that data. Requesting from those miners is both efficient (high hit rate) and fast (no wasted requests).

### Decision 8: Validation of pulled chunks

**Chosen: Local `data_root` derivation + `data_path` verification**

Gossip handshake authentication (signature + staking check) provides admission control but not consensus-critical correctness. A Byzantine or compromised staker can return a valid chunk from a different ledger position — it would pass `data_path` validation against its own `data_root` but deliver wrong data for the requested offset. Since PD execution feeds directly into EVM state transitions, the requesting node must verify that the returned chunk is bound to the correct `(ledger, offset)`.

The requester derives the expected `data_root` locally from its own canonical MDBX state — no server-supplied proof (`tx_path`) is needed:

```
ledger_offset → BlockIndex::get_block_bounds() → block height
  → block_header_by_hash() → block header with ordered tx IDs
    → get_data_ledger_tx_ids_ordered(ledger) → ordered tx IDs
      → tx_header_by_txid() for each → accumulate data_size.div_ceil(chunk_size)
        → find the DataTransactionHeader that contains the target offset
          → expected_data_root = tx.data_root (from local trusted DB)
            → validate_path(expected_data_root, data_path, chunk_byte_offset) → proves chunk membership
              → SHA256(unpacked_bytes) == leaf_hash → binds raw data to proof
```

This local derivation is cryptographically equivalent to the `tx_path` proof chain used in PoA validation (`block_validation.rs:1194`) — both establish the binding from `(ledger, offset)` → `data_root`. The difference is that instead of the server providing a merkle proof (`tx_path`) from `tx_root` to `data_root`, the requester looks up the `data_root` directly from its local MDBX. Since `persist_block()` writes block headers, tx headers, and BlockIndex entries atomically in one MDBX transaction (`block_migration_service.rs:242-296`), if `get_block_bounds()` succeeds, the tx headers are guaranteed to be present. Similar DB-loading logic already exists in `load_data_transactions()` (`block_tree.rs:1518`).

**Why local derivation over server-supplied `tx_path`**:
- **Simpler wire protocol**: The server returns just the chunk data (packed or unpacked), no proof
- **No extra server-side work**: The serving node doesn't need to read `tx_path` from SM submodule DB
- **Reuses existing infrastructure**: `get_block_bounds()`, `block_header_by_hash()`, `tx_header_by_txid()` are all existing methods
- **All nodes have the data**: `persist_block()` runs on ALL nodes unconditionally (no role/pledge/miner gating) — validator-only nodes have the same MDBX state
- **Same trust model**: Both `tx_root` (from BlockIndex) and `data_root` (from tx headers) come from the same canonical migrated state

**BlockIndex availability guarantee**: The BlockIndex always has the committing block when a PD tx references that offset. `persist_block()` atomically writes the block header, tx headers, and BlockIndex entry. Only afterward does `ChunkMigrationService::BlockMigrated` fire to write chunk data into storage modules. Since PD txs can only reference chunks that exist in storage, and chunks only exist in storage after migration, the committing block is guaranteed to be in the BlockIndex. (`PD-readable ⟹ BlockIndex present` is always true.)

**Batch optimization**: When fetching multiple chunks from the same block, the `get_block_bounds()` and tx header lookups can be cached — the `data_root` resolution only needs to happen once per DataTransaction boundary.

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
                         │    http_client: reqwest::Client             │
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
                         │  3. Resolve to peer addresses (PeerList)    │
                         │  4. Try public API first (existing endpoint):│
                         │     GET /v1/chunk/ledger/{id}/{off}         │
                         │     → returns ChunkFormat::Packed (always)  │
                         │  5. If API fails, fall back to gossip pull: │
                         │     /gossip/v2/pull_data (PdChunk variant)  │
                         │  6. Unpack locally via irys_packing::unpack │
                         │  7. Return PdChunkFetchResult               │
                         └──────────────────┬──────────────────────────┘
                                            │
                                            ▼
                         ┌─────────────────────────────────────────────┐
                         │  Serving Peer                               │
                         │                                             │
                         │  Public API: GET /v1/chunk/ledger/{id}/{off}│
                         │    (existing endpoint, no changes)          │
                         │    → always returns ChunkFormat::Packed     │
                         │                                             │
                         │  Gossip fallback: resolve_data_request      │
                         │    PdChunk(ledger, offset) =>               │
                         │      get_chunk_by_ledger_offset (packed)    │
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
validate_block():
  let cancel = CancellationToken::new();
  All 6 tasks wrapped with with_cancel(task, cancel.clone()):
    - Each task signals cancel.cancel() on definitive failure
    - Each task listens for cancel.cancelled() from siblings
  tokio::join!(
    with_cancel(recall_task, cancel),
    with_cancel(poa_task, cancel),
    shadow_tx_task (cancel-aware),  ◄── PD chunk provisioning happens here
    with_cancel(seeds_task, cancel),
    with_cancel(commitment_task, cancel),
    with_cancel(data_txs_task, cancel)
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
  3. Resolve to peer addresses (PeerList) — both API and gossip addresses
  4. For each assigned peer (in order):
     a. Try public API first: GET http://{peer.api}/v1/chunk/ledger/{id}/{off}
     b. If API fails (timeout, 404, network error): try gossip fallback
        gossip_client.pull_data(peer, PdChunk(ledger, offset))
     c. If Ok(Some(chunk_format)): return Ok(result)
     d. If both fail: try next peer
  5. If all peers exhausted: return Err(AllPeersFailed)
  │
  ▼
PdService::on_fetch_done (select! arm 3):
  On Ok(packed_chunk):
    1. Unpack locally via irys_packing::unpack() using (packing_address,
       partition_hash, chunk_offset, chain_id) from PackedChunk metadata
    2. Derive expected data_root locally from MDBX:
       a. bounds = block_index.get_block_bounds(DataLedger::Publish, key.offset)
       b. block_header = block_header_by_hash(bounds.block_hash)
       c. tx_ids = block_header.get_data_ledger_tx_ids_ordered(DataLedger::Publish)
       d. Walk tx_ids, load each DataTransactionHeader, accumulate
          data_size.div_ceil(chunk_size) to find the tx containing key.offset
       e. expected_data_root = matching_tx.data_root
    3. Verify chunk against expected data_root:
       a. Ensure chunk.data_root == expected_data_root
       b. validate_path(expected_data_root, &chunk.data_path, chunk_byte_offset_in_tx)
          → confirms chunk membership under data_root
       c. SHA256(unpacked_bytes) == leaf_hash → binds raw data to proof
       d. On verification failure: mark peer as invalid, retry with different peer
    2. Insert chunk bytes into cache with explicit reference per waiter:
       a. Pick first waiter (tx or block) as the initial reference:
          cache.insert(key, data, first_waiter_id)
       b. For each additional waiting block:
          cache.add_reference(key, block_hash)
       c. For each additional waiting tx:
          cache.add_reference(key, tx_hash)
       (This ensures every consumer holds a reference — a later
        TransactionRemoved or ReleaseBlockChunks cannot drop the only
        reference while other consumers still depend on the chunk.)
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
  On Err(AllPeersFailed { excluded_peers }):
    1. Check if any waiters remain (block oneshots not closed, txs not removed)
    2. If no waiters remain: remove from pending_fetches, skip retry
    3. If MAX_CHUNK_FETCH_RETRIES exceeded: fail_chunk_permanently(key)
    4. If waiters remain and retries not exceeded:
       a. Compute backoff delay: min(1s * 2^attempt, 30s)
       b. Insert RetryEntry into retry_queue (DelayQueue)
       c. Store delay_queue::Key in PdChunkFetchState
       d. Transition status to FetchPhase::Backoff

PdService::on_retry_ready (select! arm 4, fires when DelayQueue timer expires):
    1. Validate generation matches (skip if stale)
    2. Check waiters still exist (skip if all removed during backoff)
    3. Re-resolve peers via resolve_peers_for_chunk (picks up epoch changes)
    4. Spawn new fetch task into JoinSet
    5. Store AbortHandle in PdChunkFetchState
    6. Transition status to FetchPhase::Fetching
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
  → If no other waiters for those keys:
      Fetching → abort_handle.abort() (cancels in-flight fetch task)
      Backoff  → retry_queue.remove(&queue_key) (cancels pending timer)
  → cache.try_shrink_to_fit()

Case 2: Mempool transaction removed
────────────────────────────────────
pd_transaction_monitor fires TransactionRemoved
  → PdService::handle_release_chunks (existing logic)
  → Additionally: remove tx_hash from pending_fetches[key].waiting_txs
  → If no other waiters for those keys:
      Fetching → abort_handle.abort()
      Backoff  → retry_queue.remove(&queue_key)
  → Decrement cache references, evict unreferenced chunks
  → cache.try_shrink_to_fit()

Case 3: Sibling validation task fails (CancellationToken)
──────────────────────────────────────────────────────────
A sibling task (e.g., PoA) completes with Invalid
  → with_cancel wrapper calls cancel.cancel()
  → CancellationToken wakes all sibling cancelled() futures
  → shadow_tx_task's select! picks cancelled() → drops inner future
  → oneshot receiver dropped (was awaiting ProvisionBlockChunks response)
  │
  ▼
Same cascade as Case 1:
  PdService detects dropped oneshot → abort_handle.abort() or
  retry_queue.remove() → eager cleanup of in-flight work

with_cancel helper (used to wrap each validation task uniformly):

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

shadow_tx_task uses a similar pattern but with eyre::Result return type:

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

No signature changes: the token is created and consumed entirely within
validate_block(). No changes to shadow_transactions_are_valid, poa_is_valid,
or any other validation function signature.

Case 4: PdService shutdown
──────────────────────────
PdService select! loop exits on shutdown signal
  → JoinSet drops → all in-flight fetch tasks cancelled automatically
  → DelayQueue drops → all pending retry timers cancelled
  → No cleanup needed (structured concurrency)
```

## Internal State Changes

### New Types in PdService

```rust
/// Result returned by a chunk fetch task in the JoinSet.
struct PdChunkFetchResult {
    key: ChunkKey,
    result: Result<PackedChunk, PdChunkFetchError>,
}

/// Phase of a single chunk key's fetch lifecycle.
enum FetchPhase {
    /// An HTTP fetch task is running in the JoinSet.
    Fetching,
    /// Waiting for the backoff timer to expire in the DelayQueue.
    Backoff,
}

/// Tracks an in-flight or retrying fetch for a single ChunkKey.
struct PdChunkFetchState {
    /// Mempool transactions waiting on this chunk.
    waiting_txs: HashSet<B256>,
    /// Block validations waiting on this chunk.
    waiting_blocks: HashSet<B256>,
    /// Number of fetch attempts so far (incremented on each retry).
    attempt: u32,
    /// Distinguishes provisioning lifecycles. Incremented when a key
    /// re-enters Fetching after being cleaned up and reprovisioned.
    /// Prevents stale retry timers from acting on a new lifecycle.
    generation: u64,
    /// Peers that have been tried and failed (passed to the next fetch task).
    excluded_peers: HashSet<IrysAddress>,
    /// Current phase of the fetch lifecycle.
    status: FetchPhase,
    /// Handle to cancel the in-flight fetch task (when status == Fetching).
    /// Stored from JoinSet::spawn's return value. Used for eager cancellation
    /// when all waiters are removed — call abort_handle.abort() to cancel
    /// the in-flight HTTP request immediately.
    abort_handle: Option<AbortHandle>,
    /// Handle to cancel the pending retry timer (when status == Backoff).
    /// Used for eager cancellation via retry_queue.remove(&queue_key).
    retry_queue_key: Option<delay_queue::Key>,
}

/// Entry stored in the DelayQueue for scheduled retries.
struct RetryEntry {
    key: ChunkKey,
    attempt: u32,
    generation: u64,
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
    retry_queue: DelayQueue<RetryEntry>,
    pending_fetches: HashMap<ChunkKey, PdChunkFetchState>,
    pending_blocks: HashMap<B256, PendingBlockProvision>,
    http_client: reqwest::Client,      // for public API fetch (primary)
    gossip_client: GossipClient,       // for gossip pull (fallback)
    peer_list: PeerList,
    block_tree: BlockTreeReadGuard,
    block_index: BlockIndexReadGuard,  // for local data_root derivation (get_block_bounds)
    db: DatabaseProvider,              // for tx header lookups (block_header_by_hash, tx_header_by_txid)
    num_chunks_in_partition: u64,
}
```

### Modified Event Loop

Active fetches live in a `JoinSet`; retry timers live in a `tokio_util::time::DelayQueue` owned by the actor. This separates retry scheduling from fetch execution — retries are data in a queue, not detached tasks or sleeping sentinels mixed with fetch results.

```rust
async fn start(mut self) {
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

            Some(result) = self.join_set.join_next() => {
                self.on_fetch_done(result);
            }

            Some(expired) = self.retry_queue.next(), if !self.retry_queue.is_empty() => {
                self.on_retry_ready(expired.into_inner());
            }
        }
    }
}
```

**`on_fetch_done` retry path** (pseudocode):

```rust
fn on_fetch_done(&mut self, result: JoinResult<PdChunkFetchResult>) {
    let PdChunkFetchResult { key, result } = /* unwrap join result */;
    match result {
        Ok(packed_chunk) => { /* unpack locally, derive expected data_root from MDBX, verify, cache chunk, notify waiters */ }
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
            state.abort_handle = None; // no in-flight task during backoff
            state.excluded_peers = excluded_peers;
        }
    }
}
```

**`on_retry_ready` handler** (pseudocode):

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
    let abort_handle = self.join_set.spawn(
        fetch_chunk_from_peers(entry.key, peers, self.gossip_client.clone())
    );

    state.status = FetchPhase::Fetching;
    state.abort_handle = Some(abort_handle);
    state.retry_queue_key = None;
}
```

**Cancellation path**: When all waiters for a key are removed (tx removed + block released), PdService checks `status`:
- `Fetching` → call `abort_handle.abort()` to cancel the in-flight fetch task
- `Backoff` → call `self.retry_queue.remove(&queue_key)` to cancel the pending timer

Both are eager cancellation — no dangling tasks or timers survive waiter cleanup. Pending retries are entries in a queue, not spawned tasks. Queue length is bounded by the number of tracked chunk keys.

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

## Public API Endpoint

### Existing Route: `GET /v1/chunk/ledger/{ledger_id}/{offset}` (no changes)

PdService uses the existing public chunk endpoint as-is. No modifications to the endpoint, handler, or `ChunkStorageProvider` trait are needed.

The existing endpoint always returns `ChunkFormat::Packed(PackedChunk)` (`get_chunk.rs:34`). The `PackedChunk` includes all metadata needed by the requester: `data_root`, `data_size`, `data_path` (merkle proof), `packing_address`, `partition_hash`, `partition_offset`.

The requester unpacks locally using `irys_packing::unpack()` and verifies the chunk against a locally-derived `data_root` (see Decision 8).

**Implementation constraint: Rate limiting.** The existing chunk endpoint has no rate limiting. Bulk PD chunk fetching during block validation could overload peers. Options for future hardening: (a) add a semaphore similar to `chunk_semaphore` used for inbound chunk gossip, (b) leverage Actix middleware for per-peer rate limiting.

**TODO: Future optimization — serve unpacked when cached.** The serving node's MDBX `CachedChunks` table (populated by `ChunkIngressService`) contains unpacked versions of recently-ingested chunks. A future update should add a new endpoint or query parameter (e.g., `?format=auto`) to return `ChunkFormat::Unpacked` when available, avoiding unnecessary unpacking on the receiver. This must be backwards-compatible since existing consumers expect `ChunkFormat::Packed`.

## Gossip Protocol Changes (Fallback Path)

### Wire Type Additions (`crates/types/src/gossip.rs`)

The gossip pull path serves as a fallback when the public API is unreachable.

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

// Add to GossipDataV2 (response enum):
pub enum GossipDataV2 {
    // ... existing variants (ExecutionPayload, BlockHeader, BlockBody, Chunk, Transaction) ...
    PdChunk(PackedChunk),  // NEW: packed chunk (requester unpacks and verifies locally)
}
```

### Gossip Serving Handler (`crates/p2p/src/gossip_data_handler.rs`)

`resolve_data_request` gains a new match arm using the same `get_chunk_with_tx_path` trait method:

```rust
GossipDataRequestV2::PdChunk(ledger, offset) => {
    match self.storage_provider.get_chunk_by_ledger_offset(ledger, offset.into()) {
        Ok(Some(packed_chunk)) => {
            Some(GossipDataV2::PdChunk(ChunkFormat::Packed(packed_chunk)))
        }
        Ok(None) => None,
        Err(e) => {
            warn!(ledger, offset, "Error serving PD chunk: {}", e);
            None
        }
    }
}
```

This uses the existing `get_chunk_by_ledger_offset` method (no trait changes). Wiring `Arc<ChunkProvider>` into `GossipDataHandler` is still needed. The `ChunkProvider` is already constructed in `chain.rs` and can be passed through `P2PService::run` into `GossipDataHandler::new`. `GossipDataHandler` is in `crates/p2p` and only needs the `ChunkStorageProvider` trait (from `irys-types`, which `p2p` already depends on).

## Cache Insertion: PackedChunk to Bytes Conversion

The fetch always returns a `PackedChunk` (from both the public API and gossip fallback). The `ChunkCache` stores `Arc<Bytes>` (raw unpacked chunk data), and the `ChunkDataIndex` DashMap maps `(u32, u64) → Arc<Bytes>`.

When a pulled chunk passes verification (local `data_root` derivation + `data_path` + leaf hash), PdService extracts the raw bytes for cache insertion:

```rust
// Unpack locally using entropy from PackedChunk metadata
// PackedChunk carries packing_address and partition_hash
let unpacked = irys_packing::unpack(&packed_chunk, &packing_config)?;

let data: Arc<Bytes> = Arc::new(Bytes::from(unpacked.bytes));
// Insert with explicit reference per waiter (see on_fetch_done):
self.cache.insert(key, data.clone(), first_waiter_id);
for other_waiter in remaining_waiters {
    self.cache.add_reference(key, other_waiter);
}
// cache.insert internally mirrors to the ChunkDataIndex DashMap
```

The `data_path`, `data_root`, and `data_size` from the `PackedChunk` are used only for verification and then discarded. This matches how the existing local storage path works: `ChunkStorageProvider::get_unpacked_chunk_by_ledger_offset` returns raw `Bytes`, and `cache.insert` stores `Arc::new(chunk_bytes)`.

**TODO: When the serving endpoint is updated to return unpacked chunks from MDBX cache (see Decision 6 TODO), the unpacking step can be skipped for `ChunkFormat::Unpacked` responses.**

## Validation of Pulled Chunks

When PdService receives a `PackedChunk` from a peer (via public API or gossip fallback), it unpacks locally, then verifies the chunk is authentic for the requested `(ledger, offset)` before inserting into the cache. Verification uses the requester's own canonical MDBX state — no server-supplied proof is needed.

**Verification pseudocode in `on_fetch_done`**:

```rust
// 0. Unpack locally (endpoint always returns packed)
let unpacked = irys_packing::unpack(&packed_chunk, &packing_config)?;
let unpacked_bytes = unpacked.bytes;
let data_root = packed_chunk.data_root;
let data_path = packed_chunk.data_path;

// 1. Derive expected data_root locally from MDBX
let bounds = block_index.get_block_bounds(DataLedger::Publish, LedgerChunkOffset::from(key.offset))?;
let block_header = block_header_by_hash(&db_tx, &bounds.block_hash)?;
let tx_ids = block_header.get_data_ledger_tx_ids_ordered(DataLedger::Publish)?;

// Walk tx headers, accumulating chunk counts to find the tx containing key.offset
let mut running_offset = bounds.start_chunk_offset;
let mut expected_data_root = None;
for tx_id in tx_ids {
    let tx_header = tx_header_by_txid(&db_tx, tx_id)?;
    let num_chunks = tx_header.data_size.div_ceil(chunk_size);
    if key.offset < running_offset + num_chunks {
        expected_data_root = Some(tx_header.data_root);
        break;
    }
    running_offset += num_chunks;
}
let expected_data_root = expected_data_root.ok_or("offset not found in block txs")?;

// 2. Verify chunk's data_root matches the locally derived expected value
ensure!(data_root == expected_data_root);

// 3. Verify data_path: expected_data_root → chunk bytes
let data_path_result = validate_path(expected_data_root.0, &data_path, chunk_end_byte)?;
ensure!(SHA256(unpacked_bytes) == data_path_result.leaf_hash);
```

If any verification step fails, the chunk is rejected, the peer is marked as having returned invalid data, and the fetch retries with a different peer.

**Batch optimization**: The `get_block_bounds` result and tx header lookups can be cached when fetching multiple chunks from the same block — the `data_root` resolution only needs to happen once per `DataTransaction` boundary.

## File Change Summary

| File | Change |
|---|---|
| `crates/types/src/gossip.rs` | Add `GossipDataRequestV2::PdChunk(u32, u64)` request variant, `GossipDataV2::PdChunk(PackedChunk)` response variant, `to_v1()` returns `None` |
| `crates/types/src/chunk_provider.rs` | No changes (existing `get_chunk_by_ledger_offset` returns `PackedChunk`, used as-is) |
| `crates/api-server/src/routes/get_chunk.rs` | No changes (existing endpoint used as-is) |
| `crates/p2p/src/gossip_data_handler.rs` | Handle `PdChunk` in `resolve_data_request` using existing `get_chunk_by_ledger_offset`; add `storage_provider: Arc<dyn ChunkStorageProvider>` field |
| `crates/p2p/src/gossip_client.rs` | Add `pull_data_from_peers` method that accepts a pre-selected list of peer addresses (instead of using `top_active_peers`). Follows the same retry/handshake pattern as `pull_data_from_network` but with caller-supplied peers. |
| `crates/p2p/src/gossip_service.rs` | Pass `ChunkProvider` through to `GossipDataHandler` construction |
| `crates/actors/src/pd_service.rs` | Add `JoinSet`, `DelayQueue<RetryEntry>`, `pending_fetches`, `pending_blocks`, `own_miner_address`, new deps (`reqwest::Client`, `GossipClient`, `PeerList`, `BlockTreeReadGuard`, `BlockIndexReadGuard`, `DatabaseProvider`, `ConsensusConfig`). Refactor `handle_provision_chunks` and `handle_provision_block_chunks` to spawn fetch tasks on miss. Add `on_fetch_done` handler with local `data_root` derivation from MDBX, `irys_packing::unpack()`, `DelayQueue`-based retry logic. Add `on_retry_ready` handler. Add third and fourth `select!` arms for `join_set.join_next()` and `retry_queue.next()`. |
| `crates/actors/src/validation_service/block_validation_task.rs` | Add `CancellationToken` + `with_cancel` wrapper in `validate_block()` to cancel sibling tasks (including PD fetches) when any task fails definitively. No signature changes to individual validation functions. |
| `crates/actors/src/pd_service/provisioning.rs` | Add `PartiallyReady -> Ready` transition method callable from `on_fetch_done`. Extend `TxProvisioningState` or add helper for checking/clearing `missing_chunks`. |
| `crates/actors/src/pd_service/cache.rs` | No structural changes (insert/remove/reference tracking already work) |
| `crates/chain/src/chain.rs` | Pass `reqwest::Client`, `GossipClient`, `PeerList`, `BlockTreeReadGuard`, `BlockIndexReadGuard`, `DatabaseProvider`, `ConsensusConfig`, `own_miner_address` to `PdService::spawn_service` |
| `crates/chain-tests/src/programmable_data/pd_chunk_p2p_pull.rs` | **New file.** Integration tests for PD chunk P2P pull: happy path, multiple PD txs, chunk deduplication, mempool path. Shared `setup_pd_p2p_test()` helper. |
| `crates/chain-tests/src/programmable_data/mod.rs` | Add `mod pd_chunk_p2p_pull;` |

## Edge Cases

### Chunk needed by both mempool tx and block validation simultaneously

Handled by `pending_fetches` deduplication. A single `ChunkKey` entry tracks both `waiting_txs` and `waiting_blocks`. Only one fetch task is spawned. When the chunk arrives, both the tx's `missing_chunks` and the block's `remaining_keys` are updated.

### Oneshot receiver dropped before response sent

When block validation is cancelled, the oneshot receiver in `shadow_transactions_are_valid` is dropped. PdService detects this when it attempts `response.send(Ok(()))` — the `send` returns `Err`. PdService then cleans up: removes the `PendingBlockProvision` entry, decrements cache references for any chunks already fetched for that block, and removes the block hash from all `pending_fetches` entries. This mirrors the existing cleanup path in `handle_provision_block_chunks` when the oneshot fails (lines 352-363).

### All assigned peers fail to serve a chunk

The fetch task uses a **return-and-respawn** retry model with `DelayQueue`-based backoff. Each fetch task attempts all assigned peers once. If all fail, the task returns `Err(PdChunkFetchError::AllPeersFailed { excluded_peers })` to PdService via `join_set.join_next()`. The `on_fetch_done` handler then:

1. Checks whether waiters still exist for the chunk key — if not, cleans up and skips retry
2. Checks whether `MAX_CHUNK_FETCH_RETRIES` has been exceeded — if so, fails permanently
3. Computes backoff delay: `min(1s * 2^attempt, 30s)`
4. Inserts a `RetryEntry` into the actor-owned `DelayQueue` and transitions the fetch state to `FetchPhase::Backoff`
5. When the `DelayQueue` timer fires (via the `retry_queue.next()` select arm), `on_retry_ready` re-reads `resolve_peers_for_chunk` (picks up any epoch changes or peers coming online) and spawns a new fetch task into the JoinSet

This return-and-respawn model (vs. internal infinite retry within the task) ensures PdService can observe intermediate failures, update `FetchPhase`, and decide whether to continue retrying based on whether waiters still exist. If all waiters for a key have been removed (block cancelled, tx removed) between retry rounds, PdService simply skips the respawn. The `DelayQueue` approach avoids blocking the actor loop during backoff — retries are data in a queue, not sleeping tasks.

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

## Integration Tests

### Test File

`crates/chain-tests/src/programmable_data/pd_chunk_p2p_pull.rs` — added to the existing PD test module (`mod.rs`).

### Shared Setup Helper

A reusable setup function that establishes the two-node topology used by all test cases:

```rust
struct PdP2pTestContext {
    node_a: IrysNodeTest<Started>,       // genesis, mining, has chunks
    node_b: IrysNodeTest<Started>,       // peer, validator-only, no local chunks
    data_start_offset: u64,              // global ledger offset of first uploaded chunk
    partition_index: u64,                // data_start_offset / num_chunks_in_partition
    local_offset: u32,                   // data_start_offset % num_chunks_in_partition
    num_chunks_uploaded: u16,            // total chunks available for tests
    chunk_size: u64,                     // bytes per chunk (32 in tests)
    num_chunks_in_partition: u64,        // partition size config
    pd_signer: PrivateKeySigner,         // funded account for PD tx submission
    data_signer: PrivateKeySigner,       // account used for data upload
}

/// Establishes the two-node topology and uploads real data on Node A.
async fn setup_pd_p2p_test() -> PdP2pTestContext { ... }
```

**Node A (genesis, miner, data holder)**:
- `IrysNodeTest::new_genesis(config)` with `NodeConfig::testing()` (Sprite hardfork active from genesis)
- Staked, pledged, mining-capable, with storage modules
- Small chunk size (32 bytes) for manageable test data
- `block_migration_depth` set low (e.g., 2) to speed up chunk migration
- `num_chunks_in_partition` set small (e.g., 10) for tractable partition layout — this controls how `resolve_peers_for_chunk` computes `slot_index = offset / num_chunks_in_partition`

**Data upload on Node A**:
- Post a publish data transaction with enough chunks to cover all test scenarios (e.g., 16 chunks × 32 bytes = 512 bytes)
- Upload unpacked chunks via HTTP `/v1/chunk` API
- Mine blocks until chunks migrate to storage modules (wait for `block_migration_depth` blocks past the data tx)
- Wait for `ChunkMigrationService` to write chunks to storage modules using the `wait_for_migrated_txs()` pattern followed by a brief sleep (e.g., 2 seconds) to account for async chunk migration writes — this follows the established pattern in `pd_content_verification.rs`
- Record `data_start_offset` from BlockIndex for constructing PD access lists
- Decompose into partition-relative coordinates for `ChunkRangeSpecifier` construction:
  ```rust
  let partition_index = data_start_offset / num_chunks_in_partition;
  let local_offset = (data_start_offset % num_chunks_in_partition) as u32;
  ```

**Node B (peer, validator-only, no local chunks)**:
- `IrysNodeTest::new(genesis.testing_peer_with_signer(&peer_signer))` — NOT staked, NOT pledged, NO storage modules, NOT mining
- Connected to Node A via trusted peers (gossip + API)
- P2P chunk fetch enabled (new PdService configuration)
- No data is uploaded to or packed on Node B — its `ChunkStorageProvider` naturally returns `Ok(None)` for all ledger offsets since no storage modules contain the relevant data

**No gossip authentication concern**: The primary fetch path uses the public HTTP API (`GET /v1/chunk/ledger/{id}/{offset}/pd`), which requires no authentication or staking. Node B (unstaked, validator-only) can fetch PD chunks from Node A's public API without any special gossip configuration. The gossip fallback path is not required for the integration tests.

**Sync gate**: Wait for Node B to sync to Node A's current tip using `wait_until_height_confirmed` (or equivalent migration-aware gate) — not just block tree tip sync. This ensures Node B's BlockIndex and DataTransactionHeaders are populated for the data-carrying block, which is required for local `data_root` derivation during chunk verification in `on_fetch_done`.

**Pre-test invariant**: Before each test's PD-specific logic, assert that Node B's `ChunkStorageProvider::get_unpacked_chunk_by_ledger_offset` returns `Ok(None)` for the test's target offsets. This proves the P2P fetch path is actually exercised — without this check, tests could pass vacuously if chunks were found locally.

### Test 1: Happy Path — Single PD Transaction Block Validation

**Topology**: Node A (genesis, mining, has chunks) → Node B (peer, validator-only, no chunks locally)

**Setup**: Call `setup_pd_p2p_test()`. Node A has migrated chunks at known ledger offsets.

**Test case**:
1. Assert pre-test invariant: Node B has no local chunks at target offsets
2. Construct a PD transaction referencing 2 chunks at known offsets (using `ChunkRangeSpecifier` with `partition_index` and `local_offset` from `PdP2pTestContext`)
3. Submit the PD tx to Node A's mempool
4. Node A mines a block including the PD tx (use `future_or_mine_on_timeout()` pattern)
5. Node B receives the block via gossip
6. Node B's PdService detects missing chunks during `handle_provision_block_chunks`
7. Node B fetches chunks from Node A via `GossipDataRequestV2::PdChunk`
8. Node B validates the block successfully (full proof chain: tx_path + data_path + leaf hash)

**Assertions**:
- Node B's canonical tip matches Node A's block hash (block validated and accepted)
- The PD tx is included in the validated block on Node B (check block body)
- Nodes remain in sync — no fork, no rejection
- Fetched chunks are present in Node B's `ChunkDataIndex` DashMap (the shared mirror the EVM precompile reads from)

### Test 2: Multiple PD Transactions in One Block

**Topology**: Node A (genesis, mining, has chunks) → Node B (peer, validator-only, no chunks locally)

**Setup**: Call `setup_pd_p2p_test()`. The uploaded data provides enough chunks for multiple non-overlapping ranges.

**Test case**:
1. Assert pre-test invariant: Node B has no local chunks at target offsets
2. Construct 3 PD transactions, each referencing different chunk ranges (partition-relative offsets from `local_offset`):
   - TX1: offsets 0-1 (2 chunks)
   - TX2: offsets 2-3 (2 chunks)
   - TX3: offsets 4-5 (2 chunks)
3. Submit all 3 PD txs to Node A's mempool
4. Node A mines a block including all 3 PD txs
5. Node B receives and validates the block, fetching all 6 chunks from Node A via parallel fetch tasks

**Assertions**:
- Node B's canonical tip matches Node A's
- All 3 PD txs are in the validated block on Node B
- All 6 chunks are present in Node B's `ChunkDataIndex` DashMap
- Nodes remain in sync

### Test 3: Chunk Deduplication — Mempool TX and Block Validation Share a Fetch

**Topology**: Node A (genesis, mining, has chunks) → Node B (peer, validator-only, no chunks locally)

**Setup**: Call `setup_pd_p2p_test()`.

**Test case**:
1. Assert pre-test invariant: Node B has no local chunks at target offset X
2. Construct PD transaction T1 referencing chunk at offset X
3. Submit T1 directly to Node B's mempool (via Node B's RPC, not Node A)
4. T1 triggers `NewTransaction` in Node B → PdService detects missing chunks and starts fetching chunk X from Node A
5. Construct PD transaction T2 (different tx, same chunk at offset X) and submit to Node A's mempool
6. Node A mines a block containing T2
7. Node B receives the block → `ProvisionBlockChunks` for T2 either hits the cache (if fetch already completed) or joins the existing pending fetch as a second waiter
8. Both T1 (mempool waiter) and T2 (block waiter) are served

**Assertions**:
- Node B validates the block successfully (block waiter for T2 satisfied)
- T1 transitions to `Ready` in Node B's provisioning state (tx waiter for T1 satisfied)
- `ready_pd_txs` contains T1's tx hash
- Chunk X is present in Node B's `ChunkDataIndex` DashMap

**Timing note**: The relative timing of the fetch completion vs. block arrival matters. If the fetch completes before the block arrives, `ProvisionBlockChunks` hits the cache directly (cache hit path). If the block arrives while the fetch is still in-flight, the block's chunk key is added to `pending_fetches[X].waiting_blocks` (dedup path). Both outcomes are correct — the test asserts the end state regardless of which path executes.

### Test 4: Mempool Path — Fetch Chunks for Pending Transaction

**Topology**: Node A (genesis, mining, has chunks) → Node B (peer, validator-only, no chunks locally)

**Setup**: Call `setup_pd_p2p_test()`.

**Test case**:
1. Assert pre-test invariant: Node B has no local chunks at target offsets
2. Construct a PD transaction referencing 2-3 chunks at known offsets
3. Submit the PD tx directly to Node B's mempool (via Node B's RPC)
4. Node B's PdService detects missing chunks via `handle_provision_chunks`, marks the tx as `PartiallyReady`
5. PdService spawns fetch tasks into the JoinSet for each missing chunk
6. Fetch tasks pull chunks from Node A via `GossipDataRequestV2::PdChunk`
7. As each chunk arrives: proof chain verified, chunk inserted into cache, `missing_chunks` decremented
8. When last chunk arrives: tx transitions from `PartiallyReady` → `Ready`, tx_hash inserted into `ready_pd_txs`

**Assertions**:
- `ready_pd_txs` contains the tx hash (tx is available for block building)
- Fetched chunks are present in Node B's `ChunkDataIndex` DashMap
- The chunk bytes in `ChunkDataIndex` match the original data uploaded to Node A (byte-level verification)

### Future Test Scenarios

The following scenarios are valuable but out of scope for the initial implementation. They should be added as the implementation matures:

- **Retry after peer failure**: Node A's gossip temporarily disabled → Node B's fetch fails, retries with backoff via `DelayQueue`, eventually succeeds when gossip re-enabled. Exercises the `AllPeersFailed` → `on_retry_ready` path.
- **Block cancellation during fetch**: While Node B is fetching PD chunks, the tip advances past the block being validated → `exit_if_block_is_too_old` fires → fetch cancelled via `abort_handle.abort()`.
- **Sibling validation cancellation (CancellationToken)**: Another validation task fails (e.g., invalid PoA) while PD fetch is in-flight → `cancel.cancel()` fires → shadow_tx_task dropped → oneshot dropped → PdService cleanup.
- **Proof verification failure**: Serving peer returns a valid chunk from the wrong ledger position → tx_path verification fails → peer marked invalid, retry with different peer.
