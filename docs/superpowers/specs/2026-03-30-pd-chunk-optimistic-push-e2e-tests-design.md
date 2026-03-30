# PD Chunk Optimistic Push — E2E Integration Test Design

**Date**: 2026-03-30
**Branch**: `rob/gossip-push-pd-chunks-v2`
**Issues**: [#1231](https://github.com/Irys-xyz/irys/issues/1231), [#1217](https://github.com/Irys-xyz/irys/issues/1217)
**Depends On**: `docs/superpowers/specs/2026-03-27-pd-chunk-optimistic-push-implementation-design.md`
**Status**: Design

## Summary

Three E2E integration tests validating the PD chunk optimistic push feature across a 3-node network. Each test exercises a distinct code path in `PdService::handle_optimistic_push` (`crates/actors/src/pd_service.rs:804-909`):

| Test | Code Path | Key Assertion |
|------|-----------|---------------|
| Happy path | Full verification → `insert_unreferenced` | Chunks arrive in Observer cache BEFORE block gossip |
| Cache-hit shortcut | Early exit at `cache.contains()` (line 822) | Data integrity preserved after duplicate push |
| Pending fetch reconciliation | Fetch abort + waiter notification (lines 883-898) | Block validates despite Observer having no fetchable peer |

## Node Topology

```
                 ┌──────────────┐
                 │   Genesis    │ ← Has partition assignments
                 │   (Node A)   │ ← Stores raw chunk data
                 │              │ ← Source of optimistic push
                 └──┬───────┬───┘
          sync      │       │     push_pd_chunk (manual)
                    │       │
         ┌──────────▼──┐  ┌─▼──────────────┐
         │ Block Prod  │  │   Observer      │ ← NOT staked
         │  (Node B)   │  │   (Node C)      │ ← NOT pledged
         │ staked+assgn│  │  validator-only  │ ← Push target
         └─────────────┘  └─────────────────┘
```

### Node Roles

**Genesis (Node A):** Full miner. Uploads data, mines blocks until migration, stores packed chunks in storage modules. Provisioning PD transactions triggers `schedule_outbound_push` to connected peers. `pd_optimistic_push_fanout = 4` (default) for Tests 1-2; `= 0` for Test 3 (manual push control).

**Block Producer (Node B):** Staked peer with partition assignments (via `testing_peer_with_assignments`). Has its own partition data but does NOT store the specific PD data uploaded by Genesis. Provides a realistic multi-node topology and serves as a peer in Observer's network for Test 3.

**Observer (Node C):** Peer node started via `testing_peer_with_signer` — not staked, not pledged, no mining. Syncs chain state via gossip. Primary target for push assertions.

### Why 3 Nodes

- Tests 1 & 2: Genesis pushes to Observer. Block Producer exists in the peer list and may receive pushes (exercising peer selection), but assertions focus on Observer.
- Test 3: Observer's `trusted_peers` is overridden to point to Node B (not Genesis). When Observer tries to P2P-fetch PD chunks, it contacts Node B, which doesn't have the data. This creates a stuck fetch that the manual push from Genesis reconciles.

## Shared Test Infrastructure

### Context Struct: `PdPushTestContext`

Modeled on `PdP2pTestContext` (`pd_chunk_p2p_pull.rs:28-57`). Holds all 3 nodes, uploaded data metadata, and signer accounts.

### Setup Function: `setup_pd_push_test(fanout: u32)`

Follows `setup_pd_p2p_test()` (`pd_chunk_p2p_pull.rs:69-215`):

1. `NodeConfig::testing()` with `chunk_size=32`, `block_migration_depth=2`, `num_chunks_in_partition=10`, `num_chunks_in_recall_range=2`
2. Set `pd_optimistic_push_fanout = fanout` (caller controls per-test)
3. Create 5 funded signers: `data_signer`, `peer_signer`, `observer_signer`, `pd_signer`, `pd_signer_2`
4. Start Genesis, upload 16x32B data, mine until migrated, confirm chunk in storage
5. Start Block Producer via `testing_peer_with_assignments` (stake + pledge + epoch)
6. Start Observer via `testing_peer_with_signer` (no stake, no pledge, validator-only)
7. Sync Observer to Genesis block index height

### Helper: `build_and_inject_real_pd_tx()`

Copied from `pd_chunk_p2p_pull.rs:225-277`. Builds a signed `TxEip1559` with real `ChunkRangeSpecifier` and injects via `rpc.inject_tx()`.

## Test 1: Happy Path — `heavy3_pd_chunk_optimistic_push_happy_path`

### Preconditions

- Genesis has chunks in storage, `pd_optimistic_push_fanout = 4`
- Observer has synced block index (can verify pushes against MDBX)
- Observer does NOT have chunks in `chunk_data_index`

### Flow

```
Genesis mempool ← PD tx injected
    │
    ▼ handle_provision_chunks
Genesis storage → cache.insert_with_data_path → schedule_outbound_push
    │
    ▼ push_pd_chunk (fire-and-forget HTTP POST)
Observer gossip server → handle_pd_chunk_push
    │
    ▼ PdChunkMessage::OptimisticPush
Observer PdService → handle_optimistic_push
    │
    ├─ derive_chunk_verification_info (block index lookup)
    ├─ validate_path (merkle proof)
    ├─ sha256(chunk_bytes) == leaf_hash
    └─ cache.insert_unreferenced → chunk_data_index.insert
    │
    ▼ (ASSERTION POINT: chunk in Observer cache BEFORE block gossip)

Genesis mines block → gossip to Observer
    │
    ▼
Observer validates block (cache hit for PD chunks)
    │
    ▼ (ASSERTION POINT: canonical tip matches)
```

### Key Assertion

`wait_for_pd_chunk_in_cache` succeeds on Observer BEFORE `gossip_block_to_peers` is called. This proves chunks arrived via optimistic push, not P2P fetch during block validation.

## Test 2: Cache-Hit Shortcut — `heavy3_pd_chunk_optimistic_push_cache_hit_shortcut`

### Preconditions

- Same as Test 1 setup
- First push has already delivered chunk to Observer's cache

### Flow

```
Genesis ← PD tx T1 injected (pd_signer)
    │
    ▼ push → Observer caches chunk
    │
    ▼ WAIT: chunk confirmed in Observer's chunk_data_index
    │
Genesis ← PD tx T2 injected (pd_signer_2, SAME chunk offset)
    │
    ▼ push → Observer receives second push for same (ledger, offset)

Observer PdService::handle_optimistic_push:
    cache.contains(&key) → true
    inbound_push_tracker.record_inbound(...)
    return  ← EARLY EXIT, no MDBX lookup, no merkle verification
    │
    ▼ (ASSERTION POINT: chunk still in cache, bytes unchanged)

Genesis mines block with T1 → gossip → Observer validates
    │
    ▼ (ASSERTION POINT: block validates, chunk functional)
```

### Key Assertion

After the second push, `chunk_data_index` still has the chunk with identical bytes. The early exit at `pd_service.rs:822-826` preserves the existing cache entry without re-verification. Block validation confirms the chunk remains usable.

### Why Two Signers

`pd_signer` for T1, `pd_signer_2` for T2. Same signer with different nonces would cause nonce conflicts when T1 is included in a block (T2's nonce would become stale). Different signers avoids this.

## Test 3: Pending Fetch Reconciliation — `heavy3_pd_chunk_optimistic_push_reconciles_pending_fetch`

### Preconditions

- Genesis: `pd_optimistic_push_fanout = 0` (no automatic push — we push manually)
- Observer's `trusted_peers` overridden to point to Node B (not Genesis)
- Node B has partition assignments but does NOT store the specific PD chunks

### Flow

```
Genesis ← PD tx injected, provisions from storage (no push, fanout=0)
    │
    ▼ mine_block_without_gossip

Genesis → gossip block to Node B → Node B validates and auto-gossips to Observer
    │
    ▼
Observer receives block → ProvisionBlockChunks → PdService
    │
    ├─ cache.contains(&key) → false
    ├─ pending_fetches: no entry for this key
    └─ spawn fetch task → tries Node B (Observer's only peer) → FAILS
        (Node B doesn't have PD data for this offset)
    │
    ▼ fetch enters retry/backoff, PendingBlockProvision holds oneshot open

Genesis gossip_client → push_pd_chunk(observer_peer_id, observer_addr, push_msg)
    │
    ▼ HTTP POST to Observer's /gossip/v2/pd_chunk_push

Observer PdService::handle_optimistic_push:
    cache.contains → false (not cached yet)
    derive_chunk_verification_info → Ok (block index has this offset)
    validate_path → Ok
    sha256 check → Ok
    cache.insert_unreferenced → chunk enters cache
    │
    ▼ RECONCILIATION (pd_service.rs:883-898):
    pending_fetches.remove(&key) → Some(fetch_state)
    retry_queue.try_remove → cancel retry timer
    fetch_state.handle.abort() → kill in-flight fetch
    cache.add_reference(&key, block_hash) → pin chunk for block validation
    notify_chunk_waiters → pending.remaining_keys now empty
    pending.response.send(Ok(())) → UNBLOCK block validation
    │
    ▼
Observer BlockValidation continues → validates block → canonical tip advances
    │
    ▼ (ASSERTION POINT: block validates, canonical tip matches Genesis)
```

### Key Assertion

Without the manual push, block validation would TIME OUT on Observer because its only peer (Node B) doesn't have the PD data. The push is the only way the chunk can arrive. Block validation success proves the reconciliation path worked: the push inserted the chunk, reconciled with the pending fetch, added cache references for the waiting block, and woke the validation waiter via the oneshot.

### Topology Override

```rust
// After testing_peer_with_signer (which sets trusted_peers to Genesis):
observer_config.trusted_peers = vec![PeerAddress {
    api: format!("127.0.0.1:{}", node_b.cfg.http.public_port).parse().unwrap(),
    gossip: format!("127.0.0.1:{}", node_b.cfg.gossip.public_port).parse().unwrap(),
    execution: RethPeerInfo::default(),
}];
```

### Manual Push Construction

```rust
// Get packed chunk from Genesis storage
let packed = match genesis.node_ctx.chunk_provider.get_chunk_for_pd(0, offset)? {
    Some(ChunkFormat::Packed(p)) => p,
    other => panic!("expected Packed, got {:?}", other),
};

// Unpack (signature: unpack(&PackedChunk, iterations, chunk_size, chain_id) -> UnpackedChunk)
let unpacked = irys_packing::unpack(
    &packed,
    genesis.node_ctx.config.consensus.entropy_packing_iterations,
    genesis.node_ctx.config.consensus.chunk_size as usize,
    genesis.node_ctx.config.consensus.chain_id,
);

// Construct push message
let push_msg = PdChunkPush {
    ledger: 0,
    offset: data_start_offset,
    chunk_bytes: unpacked.bytes,
    data_path: packed.data_path,
};

// Send directly to Observer via Genesis's gossip client
let observer_peer_id = observer.node_ctx.config.peer_id();
let observer_addr = observer.cfg.peer_address();
genesis.node_ctx.gossip_data_handler.gossip_client
    .push_pd_chunk(observer_peer_id, &observer_addr, &push_msg);
```

## Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Test file location | `crates/chain-tests/src/programmable_data/pd_chunk_optimistic_push.rs` | Follows existing PD test organization |
| Setup function | Parameterized by `fanout: u32` | Tests 1-2 use `fanout=4`, Test 3 uses `fanout=0` |
| Helper duplication | Copy `build_and_inject_real_pd_tx` | Avoids cross-module coupling; function is self-contained |
| Test 3 topology | Observer trusts Node B only | Guarantees fetch fails, forcing reconciliation path |
| Test 3 push method | `gossip_client.push_pd_chunk()` | Accessible via `IrysNodeCtx.gossip_data_handler.gossip_client` (both `pub`) |
| Test naming | `heavy3_` prefix | Follows convention for 3-node tests |
| Chunk verification | Byte-level comparison against uploaded `data_bytes` | Same pattern as `pd_chunk_p2p_pull.rs:692-702` |

## What These Tests Do NOT Cover

- **Peer selection algorithm**: `select_push_targets` is covered by unit tests in `pd_service/push.rs`
- **Inbound push tracker dedup**: Internal to PdService, covered by unit tests
- **Circuit breaker behavior**: Covered by gossip_client tests
- **LRU eviction under pressure**: Covered by cache unit tests in `pd_service/cache.rs`
- **Invalid push rejection**: Covered by unit tests (bad merkle proof, bad leaf hash, non-Publish ledger)
