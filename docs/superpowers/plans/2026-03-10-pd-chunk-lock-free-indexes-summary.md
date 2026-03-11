# PD Chunk Lock-Free Indexes — Implementation Summary

**Plan:** `docs/superpowers/plans/2026-03-10-pd-chunk-lock-free-indexes.md`
**Branch:** `rob/reth-pd-mempool-2`
**Base commit:** `fa85d3b642d0a5a8c97f564844b8ca4e8f6082a5`
**Final commit:** `9199706d`

---

## Goal

Eliminate all `blocking_recv()` calls in the PD payload building and EVM execution paths by replacing channel-based request-reply with shared concurrent data structures.

## Architecture

Two shared concurrent structures replace the blocking channels:

1. **`ready_pd_txs`** (`Arc<DashSet<B256>>`) — PdService writes when chunks are fully provisioned; the payload builder reads with `.contains()` instead of sending `IsReady` and blocking on a oneshot channel.

2. **`ChunkDataIndex`** (`Arc<DashMap<(u32, u64), Arc<Bytes>>>`) — PdService populates during provisioning (mirrored from the LRU cache); the PD precompile reads directly instead of sending `GetChunk` and blocking.

PdService remains the single authority for both structures. The Reth mempool is the single authority on transaction liveness.

## Commits

| SHA | Message |
|-----|---------|
| `e1877b63` | `feat(types): add ChunkDataIndex type; simplify PdChunkMessage to 4 variants` |
| `2fdd0a29` | `refactor(provisioning): remove TTL, Lock/Unlock, is_ready — pool owns tx liveness` |
| `36033ce9` | `refactor(cache): remove lock_count, add shared ChunkDataIndex mirroring` |
| `f4c882b7` | `feat(pd-service): write ready_pd_txs directly; remove TTL, lock, block_state subscription` |
| `714af8d8` | `feat(mempool): event-driven monitor with pool listeners instead of polling` |
| `648596f5` | `chore: clippy and fmt fixes for Phase 1` |
| `86d14770` | `feat(pd-context): replace blocking GetChunk channel with lock-free ChunkDataIndex reads` |
| `463b6458` | `chore: fix tests and clippy for Phase 2` |
| `526d5e4d` | `fix(tests): expose ready_pd_txs to test infrastructure for PD transaction readiness` |
| `9199706d` | `chore: remove dead pd_chunk_sender from IrysPayloadBuilderBuilder` |

## What Was Removed

| Removed | Replaced By |
|---------|-------------|
| `PdChunkMessage::IsReady` | `ready_pd_txs.contains()` |
| `PdChunkMessage::GetChunk` | `ChunkDataIndex.get()` |
| `PdChunkMessage::Lock` / `Unlock` | Dead code (no senders existed) |
| `ProvisioningState::Locked` | N/A |
| TTL expiry (`expire_at_height`, `TTL_BLOCKS`, `current_height`) | Reth pool owns tx liveness |
| `block_state_rx` subscription in PdService | N/A (only used for TTL) |
| `lock_count` in `CachedChunkEntry` | N/A |
| `lock_chunks` / `unlock_chunks` on ChunkCache | N/A |
| `handle_is_ready` / `handle_lock` / `handle_unlock` / `handle_get_chunk` / `handle_block_state_update` on PdService | Direct DashSet/DashMap writes |
| 100ms polling in mempool monitor | Event-driven pool listeners |
| `PdChunkSender` in `PdContext` / `IrysEvmFactory` | `ChunkDataIndex` |
| `ConfigurePdChunkSender` trait | `ConfigureChunkDataIndex` |

## Files Modified

| File | Changes |
|------|---------|
| `crates/types/src/chunk_provider.rs` | Added `ChunkDataIndex` type alias; reduced `PdChunkMessage` from 7 to 4 variants |
| `crates/types/Cargo.toml` | Added `dashmap` dependency |
| `crates/actors/src/pd_service/provisioning.rs` | Removed `Locked` state, TTL fields, `is_ready`/`lock`/`unlock`/`expire_at_height` methods |
| `crates/actors/src/pd_service/cache.rs` | Removed `lock_count`; added `shared_index` field mirroring inserts/removes/evictions |
| `crates/actors/src/pd_service.rs` | Added `ready_pd_txs`; removed `block_state_rx`/`current_height`/lock handlers/TTL; writes DashSet on provision/release |
| `crates/irys-reth/src/mempool.rs` | Event-driven monitor using `new_transactions_listener_for()` and `all_transactions_event_listener()` |
| `crates/irys-reth/src/payload.rs` | Replaced blocking `IsReady` with `ready_pd_txs.contains()`; removed `pd_chunk_sender` from iterator |
| `crates/irys-reth/src/precompiles/pd/context.rs` | `ChunkSource::Manager` holds `ChunkDataIndex`; lock-free DashMap reads |
| `crates/irys-reth/src/evm.rs` | Replaced `pd_chunk_sender` with `chunk_data_index` in `IrysEvmFactory`; renamed trait |
| `crates/irys-reth/src/payload_builder_builder.rs` | Replaced `pd_chunk_sender` with `ready_pd_txs` + `chunk_data_index` |
| `crates/irys-reth/src/lib.rs` | Added `ready_pd_txs` + `chunk_data_index` to `IrysEthereumNode`; updated test infrastructure |
| `crates/reth-node-bridge/src/node.rs` | Threaded `ready_pd_txs` + `chunk_data_index` through `run_node` |
| `crates/chain/src/chain.rs` | Created shared state (`DashMap::with_capacity(16_384)`, `DashSet`); wired to PdService and reth |
| `crates/irys-reth/Cargo.toml` | Added `dashmap`, moved `futures` from dev-deps to deps |
| `crates/chain/Cargo.toml` | Added `dashmap` |
| `crates/reth-node-bridge/Cargo.toml` | Added `dashmap` |

## Design Decisions

1. **Executor uses Storage mode, not ChunkDataIndex.** The reth executor (for block validation) reads chunks from the storage provider directly. Only the payload builder (for block building) uses the shared `ChunkDataIndex`. This is because the executor validates peer blocks where chunks may not be in the local cache.

2. **`CombinedTransactionIterator` uses `continue` (not `mark_invalid`).** Reth's `BestTransactionFilter` calls `mark_invalid` which blocks ALL subsequent txs from the same sender. The iterator intentionally uses `continue` (skip without mark_invalid) to preserve sender ordering for mixed PD/non-PD senders.

3. **Fail-closed readiness.** If `ready_pd_txs` doesn't contain a tx hash, the tx is skipped. This is safer than the old fail-open behavior (where channel errors defaulted to "ready"). Tests explicitly pre-populate the DashSet.

4. **`Arc<Bytes>` mid-execution safety.** If PdService removes a DashMap entry mid-EVM-execution, any `Arc<Bytes>` already retrieved via `.get()` remains valid (Arc keeps data alive). Subsequent `.get()` calls for other chunks in the same tx could return `None`, but this is safe — the tx was removed from the pool, so the EVM execution will be discarded anyway.

## Verification

- `cargo clippy --workspace --tests --all-targets` — zero warnings
- `cargo nextest run -p irys-types -p irys-actors -p irys-reth` — 796 tests passed, 0 failed
- `cargo check --workspace --tests` — clean
