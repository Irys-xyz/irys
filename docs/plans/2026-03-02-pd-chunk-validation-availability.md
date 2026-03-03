# PD Chunk Data Availability for Block Validation

## Problem

When a validating node receives a block from a peer containing PD (Programmable Data) transactions, Reth's `new_payload` must execute the block to verify the state root. During EVM execution, the PD precompile fetches chunk data via `PdContext`.

If the node hasn't synced the referenced chunks from the permanent ledger, the precompile returns `ChunkNotFound`, the transaction reverts, the state root won't match the block header, and the block is incorrectly rejected as `Invalid`.

This is a **data availability problem**: validators need PD chunk data available locally before `new_payload` execution. Without it, valid blocks get rejected, causing potential consensus splits.

Note: the malicious producer case (referencing non-existent or wrong chunks) is already handled correctly — the state root mismatch between what the producer computed and what the validator computes will cause `new_payload` to return `Invalid`.

## Decision

**Approach: On-Demand Storage Loading in PD Service** — the PD service's `GetChunk` handler falls back to loading from local storage on cache miss. This makes Reth fully responsible for the chunk lifecycle: the PD precompile requests chunks via `PdContext` → `GetChunk` message → PD service loads from storage transparently. No Reth fork changes needed.

Previous approaches (superseded):
1. CL-side pre-provisioning in `shadow_transactions_are_valid` before `submit_payload_to_reth` — split chunk lifecycle ownership between CL and EL
2. Reth fork pre-execution hooks on `ConfigureEngineEvm` — required modifying the Reth fork for a simple cache-miss fallback

Alternatives considered:
- **Per-block chunk map via side channel** — isolated but duplicates fetching logic, no caching benefit
- **Per-transaction provisioning in `IrysEvm::transact_raw`** — no Reth fork changes but adds complexity without benefit over on-demand loading

## POC Scope

- Chunk source: local storage only (perm ledger data assumed available)
- If chunks are not found in storage: `GetChunk` returns `None`, PD precompile returns `ChunkNotFound`, tx reverts, state root mismatches → block correctly marked `Invalid`
- Network gossiping of PD chunks is not yet implemented
- Each cache miss during EVM execution triggers a synchronous storage read — acceptable for local disk reads (~256KB per chunk)

## Execution Pipeline Analysis

The `new_payload` execution flow in Reth:

```
on_new_payload → try_insert_payload → insert_block_or_payload
  → BasicEngineValidator::validate_payload
    → validate_block_with_state
      → execute_block
        → evm_config.create_executor()   → IrysBlockExecutor
        → execute_transactions
          → executor.apply_pre_execution_changes()
          → for tx in txs: executor.execute_transaction(tx)
              → IrysEvm::transact_raw()
                → PD precompile → PdContext::get_chunk() → GetChunk msg → PD service
          → executor.finish()
      → verify state root
```

Key facts:
- `IrysExecutorBuilder` wires `pd_chunk_sender` via `build_evm()` → `IrysEvmFactory::with_pd_chunk_sender()`
- Both payload builder and block executor use `ChunkSource::Manager` mode (PD service cache)
- During EVM execution, the PD precompile calls `PdContext::get_chunk()` which sends a `GetChunk` message to the PD service
- The PD service now loads from storage on cache miss, making pre-provisioning unnecessary for block validation

## Design

### 1. On-Demand Storage Loading in PD Service (Core Change)

Modified `handle_get_chunk` in `crates/actors/src/pd_service.rs` to fall back to storage on cache miss:

```rust
fn handle_get_chunk(&mut self, ledger: u32, offset: u64) -> Option<Arc<Bytes>> {
    let key = ChunkKey { ledger, offset };

    // Fast path: cache hit
    if let Some(chunk) = self.cache.get(&key) {
        return Some(chunk);
    }

    // Cache miss — load from storage on demand
    match self.storage_provider.get_unpacked_chunk_by_ledger_offset(ledger, offset) {
        Ok(Some(chunk)) => {
            let data = Arc::new(chunk);
            self.cache.insert_unreferenced(key, data.clone());
            Some(data)
        }
        Ok(None) => None,   // chunk not available locally
        Err(e) => None,      // storage error
    }
}
```

Added `insert_unreferenced` to `ChunkCache` — inserts a chunk without any transaction reference. The chunk can be evicted by LRU at any time, but the caller holds an `Arc<Bytes>` so the data is safe.

### 2. Executor Wiring (Already Implemented)

`IrysExecutorBuilder` has `pd_chunk_sender` field, wired through `build_evm()` → `IrysEvmFactory::with_pd_chunk_sender()`. Both payload builder and executor use `ChunkSource::Manager`.

### 3. PD Service — Block-Level Provisioning Messages (Already Implemented, Future Use)

Two variants on `PdChunkMessage` remain for future proactive batch-provisioning (e.g., P2P):
- `ProvisionBlockChunks` — bulk-load chunks for a block
- `ReleaseBlockChunks` — release block-level references

These are not used during normal block validation (on-demand loading handles it), but provide the infrastructure for future network-based provisioning.

### 4. Removed CL-Side Provisioning

Removed provisioning code from the CL validation path:
- `block_validation.rs`: Removed section 2.6 (`ProvisionBlockChunks` send) and `pd_chunk_specs` collection. Kept section 2.5 (PD budget validation only)
- `block_validation_task.rs`: Removed all 3 `ReleaseBlockChunks` sends
- `validation_service.rs`: Removed `pd_chunk_sender` field from `ValidationServiceInner` and `spawn_service` parameter
- `chain.rs`: Removed `pd_chunk_sender` threading through `init_services_thread` → `init_services` → validation service

## Files Modified

| File | Change |
|------|--------|
| `crates/actors/src/pd_service.rs` | `handle_get_chunk` falls back to storage on cache miss |
| `crates/actors/src/pd_service/cache.rs` | Add `insert_unreferenced` method to `ChunkCache` |
| `crates/actors/src/block_validation.rs` | Remove section 2.6 (provisioning), `pd_chunk_specs` collection, and `pd_chunk_sender` param |
| `crates/actors/src/validation_service/block_validation_task.rs` | Remove 3 `ReleaseBlockChunks` sends |
| `crates/actors/src/validation_service.rs` | Remove `pd_chunk_sender` field and param |
| `crates/chain/src/chain.rs` | Remove `pd_chunk_sender` threading for validation service |

**No Reth fork changes needed.**

## How It Works End-to-End

### Block Building (payload builder path)
1. PD tx enters mempool → `NewTransaction` → PD service provisions chunks from storage into cache
2. Payload builder locks chunks via `Lock` → executes PD tx → chunks read from cache
3. Block produced and propagated

### Block Validation (new_payload path)
1. Validator receives block via gossip, submits to Reth via `new_payload`
2. Reth's `validate_block_with_state` executes the block
3. For each PD tx, the PD precompile calls `PdContext::get_chunk()`
4. `GetChunk` message → PD service → cache hit (fast) or cache miss → load from storage
5. Chunk returned to EVM, execution continues
6. State root verified → `Valid` or `Invalid`

### Missing Chunks
- If chunk not in storage: `GetChunk` returns `None` → precompile returns `ChunkNotFound` → tx reverts → state root mismatch → block correctly marked `Invalid`
- This is the correct behavior for both malicious producers (referencing non-existent chunks) and data availability gaps

## Future Work (Not in This PR)

- **Network fetching**: When `GetChunk` finds neither cache nor storage, the PD service could try P2P fetch before returning `None`. With timeout and retry semantics.
- **Proactive batch provisioning**: Use the existing `ProvisionBlockChunks` message for pre-execution hooks (if added to the Reth fork later) for batch-fetching from P2P.
- **Chunk gossiping**: Proactive propagation of PD chunks across the network.
- **Cache tuning**: Separate cache budgets for validation vs payload building if contention arises.
