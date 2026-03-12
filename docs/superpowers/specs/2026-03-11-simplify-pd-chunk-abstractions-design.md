# Design: Simplify PD Chunk Abstractions

**Date**: 2026-03-11
**Branch**: rob/reth-pd-mempool-2
**Status**: Approved

## Problem

After commit `c0f1ce2c` gave the executor the `ChunkDataIndex`, both EVM paths (payload builder and block executor) always use `ChunkSource::Manager` — reading chunks from the shared DashMap. This left several abstractions dead or vestigial:

1. `ChunkSource::Storage` variant — unreachable in production
2. `PdContext::new(chunk_provider)` — only used for `ChunkConfig` extraction
3. `RethChunkProvider` trait in the EVM path — EVM never calls it
4. `MockChunkProvider` in production code — exists only to provide `ChunkConfig`
5. `IrysEvmFactory.chunk_data_index: Option<_>` — always `Some`, `None` branch dead
6. `ConfigureChunkDataIndex` trait — single-use trait for payload builder wiring
7. `IrysEvmFactory.context: PdContext` — stored but never used for chunk reads

The trait is also misnamed: `RethChunkProvider` is consumed by `PdService` and the API server, not by Reth/EVM.

## Design

### Change 1: Flatten `PdContext`

**Delete** the `ChunkSource` enum entirely. `PdContext` holds `ChunkDataIndex` directly.

```rust
// BEFORE
enum ChunkSource {
    Manager { chunk_data_index: ChunkDataIndex },
    Storage(Arc<dyn RethChunkProvider>),
}

pub struct PdContext {
    tx_hash: Arc<RwLock<Option<B256>>>,
    access_list: Arc<RwLock<Vec<AccessListItem>>>,
    chunk_source: ChunkSource,
    chunk_config: ChunkConfig,
}

// AFTER
pub struct PdContext {
    tx_hash: Arc<RwLock<Option<B256>>>,
    access_list: Arc<RwLock<Vec<AccessListItem>>>,
    chunk_data_index: ChunkDataIndex,
    chunk_config: ChunkConfig,
}
```

- Single constructor: `PdContext::new(chunk_config: ChunkConfig, chunk_data_index: ChunkDataIndex)`
- `get_chunk()` does direct `self.chunk_data_index.get(...)` — no match dispatch
- `clone_for_new_evm()` stays (access list isolation), clones the `ChunkDataIndex` Arc
- Remove `use irys_types::chunk_provider::RethChunkProvider` from `context.rs`

**File**: `crates/irys-reth/src/precompiles/pd/context.rs`

### Change 2: Simplify `IrysEvmFactory`

```rust
// BEFORE
pub struct IrysEvmFactory {
    context: PdContext,                              // Storage-mode, only for .chunk_config()
    hardfork_config: Arc<IrysHardforkConfig>,
    chunk_data_index: Option<ChunkDataIndex>,         // always Some in prod
}
IrysEvmFactory::new(chunk_provider, hardfork_config)  // takes Arc<dyn RethChunkProvider>
    .with_chunk_data_index(index)                      // optional bolt-on

// AFTER
pub struct IrysEvmFactory {
    chunk_config: ChunkConfig,
    hardfork_config: Arc<IrysHardforkConfig>,
    chunk_data_index: ChunkDataIndex,                  // required
}
IrysEvmFactory::new(chunk_config, hardfork_config, chunk_data_index)
```

- `create_evm()` and `create_evm_with_inspector()`: replace the `match` on `Option` with direct `PdContext::new(self.chunk_config, self.chunk_data_index.clone())`
- Delete `with_chunk_data_index()` method
- Delete `ConfigureChunkDataIndex` trait and its impl block (also referenced in Change 4 — same deletion)
- Delete `IrysEvmConfig::with_chunk_data_index()` wrapper
- Delete `IrysBlockExecutorFactory::with_chunk_data_index()` wrapper
- Delete `#[cfg(test)] pub fn context(&self)` accessor (the `context` field no longer exists)
- Update `new_for_testing()` signature: `fn new_for_testing(chunk_data_index: ChunkDataIndex) -> Self`

**Files**: `crates/irys-reth/src/evm.rs`

### Change 3: Remove `chunk_provider` From EVM Wiring Chain

Remove `Arc<dyn RethChunkProvider>` from all EVM-path structs and functions. Replace with `ChunkConfig` where config is needed.

| Struct/Function | Change |
|---|---|
| `IrysEthereumNode` | Remove `chunk_provider` field, add `chunk_config: ChunkConfig` |
| `IrysExecutorBuilder` | Remove `chunk_provider` field, add `chunk_config: ChunkConfig` |
| `IrysExecutorBuilder::build_evm()` | `IrysEvmFactory::new(self.chunk_config, self.hardfork_config, self.chunk_data_index)` |
| `start_reth_node()` in `chain.rs` | Replace `chunk_provider` param with `chunk_config: ChunkConfig` |
| `run_node()` in `node.rs` | Replace `chunk_provider` param with `chunk_config: ChunkConfig` |
| `IrysPayloadBuilderBuilder::build_payload_builder()` | No longer calls `evm_config.with_chunk_data_index()`. The `chunk_data_index` is already baked into the `IrysEvmFactory` that comes through `IrysEvmConfig`. |

Add a convenience constructor:
```rust
// In crates/types/src/chunk_provider.rs
impl ChunkConfig {
    pub fn from_consensus(consensus: &ConsensusConfig) -> Self {
        Self {
            num_chunks_in_partition: consensus.num_chunks_in_partition,
            chunk_size: consensus.chunk_size,
            entropy_packing_iterations: consensus.entropy_packing_iterations as u8,
            chain_id: consensus.chain_id as u16,
        }
    }
}
```

**Production wiring** (`chain.rs`):
```rust
// BEFORE
let mock_provider = irys_types::chunk_provider::MockChunkProvider::new();
start_reth_node(exec, chainspec, config, latest_block,
    Arc::new(mock_provider), pd_chunk_tx, ready_pd_txs, chunk_data_index)

// AFTER
let chunk_config = ChunkConfig::from_consensus(&config.consensus);
start_reth_node(exec, chainspec, config, latest_block,
    chunk_config, pd_chunk_tx, ready_pd_txs, chunk_data_index)
```

**Files**: `crates/irys-reth/src/lib.rs`, `crates/chain/src/chain.rs`, `crates/reth-node-bridge/src/node.rs`

### Change 4: Payload Builder Wiring

Currently `IrysPayloadBuilderBuilder` receives `chunk_data_index` separately and bolts it onto the `evm_config` via the `ConfigureChunkDataIndex` trait:

```rust
// BEFORE (payload_builder_builder.rs)
let evm_config = evm_config.with_chunk_data_index(self.chunk_data_index.clone());
```

After Change 2, `IrysEvmFactory` already has `chunk_data_index` baked in (it's required at construction). Both executor and payload builder share the same `IrysEvmConfig` (cloned by reth's `ComponentsBuilder`). Since both paths now use the same `ChunkDataIndex` (the whole point of the c0f1ce2c fix), the payload builder no longer needs to bolt it on separately.

- Remove `chunk_data_index` field from `IrysPayloadBuilderBuilder`
- Remove the `with_chunk_data_index()` call in `build_payload_builder()`
- Remove the `+ ConfigureChunkDataIndex` trait bound from the `PayloadBuilderBuilder` impl (line 49)
- The `ConfigureChunkDataIndex` trait deletion itself is in Change 2 (same code, listed there for the evm.rs file scope)

The payload builder still keeps `ready_pd_txs` (that's a separate concern — gating which PD txs are ready for inclusion).

**Files**: `crates/irys-reth/src/payload_builder_builder.rs`, `crates/irys-reth/src/evm.rs`

### Change 5: Rename `RethChunkProvider` → `ChunkStorageProvider`

Simple rename of the trait. It's used by `PdService` (to fetch from storage) and by the domain `ChunkProvider` (the real implementation), not by Reth/EVM.

| Location | Change |
|---|---|
| `crates/types/src/chunk_provider.rs:22` | `pub trait ChunkStorageProvider` |
| `crates/domain/src/models/chunk_provider.rs:129` | `impl ChunkStorageProvider for ChunkProvider` |
| `crates/actors/src/pd_service.rs:6,28,38` | Update imports and field types |
| All `use` statements referencing `RethChunkProvider` | Update to `ChunkStorageProvider` |

**Files**: `crates/types/src/chunk_provider.rs`, `crates/domain/src/models/chunk_provider.rs`, `crates/actors/src/pd_service.rs`

### Change 6: Move `MockChunkProvider` to `pd_service` Tests

`MockChunkProvider` is currently in `crates/types/src/chunk_provider.rs` behind `#[cfg(any(test, feature = "test-utils"))]`. Its only legitimate consumer is `PdService` tests (which need a `ChunkStorageProvider` impl to test chunk provisioning).

- **Delete** `MockChunkProvider` from `crates/types/src/chunk_provider.rs`
- **Create** it in `crates/actors/src/pd_service.rs` inside the existing `#[cfg(test)] mod tests` block
- The mock implements `ChunkStorageProvider` (the renamed trait from `irys-types`)
- The `test-utils` feature stays in `irys-types` (it gates `NodeConfig::testing()`, `MempoolConfig::testing()`, etc.)

**Cargo.toml cleanup**:
- `crates/chain/Cargo.toml`: Move `irys-types` `test-utils` feature from `[dependencies]` to `[dev-dependencies]`
- `crates/reth-node-bridge/Cargo.toml`: Remove `features = ["test-utils"]` from `irys-reth` dependency (no longer needed — `MockChunkProvider` isn't used in the bridge)

### Change 7: Update EVM/Precompile Tests

Tests that currently use `MockChunkProvider` for EVM/precompile testing switch to pre-populating `ChunkDataIndex` directly.

Add a test helper in `crates/irys-reth/src/evm.rs` (inside `#[cfg(test)]`):
```rust
/// Creates a ChunkDataIndex pre-populated so that any (ledger, offset) lookup
/// returns zero-filled chunk bytes of the given size.
fn test_chunk_data_index() -> ChunkDataIndex {
    // Return an empty DashMap — tests that need chunks populate specific entries
    Arc::new(DashMap::new())
}

/// Inserts a zero-filled chunk at the given (ledger, offset) for testing.
fn insert_test_chunk(index: &ChunkDataIndex, ledger: u32, offset: u64, chunk_size: usize) {
    index.insert((ledger, offset), Arc::new(Bytes::from(vec![0u8; chunk_size])));
}
```

Tests updated:

| Test file | Current | After |
|---|---|---|
| `evm.rs` tests (`evm_rejects_eip4844...`, `evm_processes_normal_tx...`) | `MockChunkProvider` → `IrysEvmFactory::new_for_testing(mock)` | `IrysEvmFactory::new_for_testing(test_chunk_data_index())` |
| `precompile.rs` tests | `MockChunkProvider` → `IrysEvmFactory::new_for_testing(mock)` | Pre-populate `ChunkDataIndex` with chunks matching access list entries, pass to `new_for_testing()` |
| `read_bytes.rs` tests | `MockChunkProvider` → `PdContext::new(mock)` | `PdContext::new(chunk_config, test_chunk_data_index())` + populate needed entries |
| `pd_context_isolation.rs` | `MockChunkProvider` → `IrysEvmFactory::new(mock, ...)` | `IrysEvmFactory::new(chunk_config, hardfork, test_chunk_data_index())` + populate entries |
| `lib.rs` `setup_irys_reth()` | `MockChunkProvider` → `IrysEthereumNode { chunk_provider }` | `IrysEthereumNode { chunk_config }` — no mock needed |

## Abstractions Before vs After

### Before (current)
```
chain.rs: MockChunkProvider::new()
  → start_reth_node(Arc<dyn RethChunkProvider>, ...)
    → run_node(Arc<dyn RethChunkProvider>, ...)
      → IrysEthereumNode { chunk_provider: Arc<dyn RethChunkProvider> }
        → IrysExecutorBuilder { chunk_provider: Arc<dyn RethChunkProvider> }
          → IrysEvmFactory::new(Arc<dyn RethChunkProvider>, hardfork)
            → PdContext::new(provider) → ChunkSource::Storage(provider)
            → .with_chunk_data_index(index) → Option<ChunkDataIndex> = Some
              → create_evm() → match Option
                → Some → PdContext::new_with_manager(config, index) → ChunkSource::Manager
                → None → clone Storage PdContext  ← DEAD
```

### After (proposed)
```
chain.rs: ChunkConfig::from_consensus(&config.consensus)
  → start_reth_node(ChunkConfig, ...)
    → run_node(ChunkConfig, ...)
      → IrysEthereumNode { chunk_config: ChunkConfig }
        → IrysExecutorBuilder { chunk_config, chunk_data_index }
          → IrysEvmFactory::new(chunk_config, hardfork, chunk_data_index)
            → create_evm() → PdContext::new(config, index)
              → chunk_data_index.get(...)
```

**Removed**: `ChunkSource` enum, `PdContext::new(provider)`, `with_chunk_data_index()`, `ConfigureChunkDataIndex` trait, `Option<ChunkDataIndex>`, `MockChunkProvider` from types crate, `Arc<dyn RethChunkProvider>` from entire EVM path.

**Renamed**: `RethChunkProvider` → `ChunkStorageProvider` (stays in `irys-types`, used by `PdService` and domain `ChunkProvider`).

## Files Changed

| File | Changes |
|---|---|
| `crates/types/src/chunk_provider.rs` | Rename trait; add `ChunkConfig::from_consensus()`; delete `MockChunkProvider` |
| `crates/irys-reth/src/precompiles/pd/context.rs` | Delete `ChunkSource`; flatten `PdContext`; single constructor |
| `crates/irys-reth/src/evm.rs` | Simplify `IrysEvmFactory`; delete `ConfigureChunkDataIndex` trait; delete `with_chunk_data_index()` methods; update `create_evm()`/`create_evm_with_inspector()` |
| `crates/irys-reth/src/lib.rs` | Remove `chunk_provider` from `IrysEthereumNode`/`IrysExecutorBuilder`; add `chunk_config`; update `build_evm()`; update `setup_irys_reth()` test helper |
| `crates/irys-reth/src/payload_builder_builder.rs` | Remove `chunk_data_index` field; remove `with_chunk_data_index()` call |
| `crates/irys-reth/src/precompiles/pd/precompile.rs` | Update tests to use `ChunkDataIndex` |
| `crates/irys-reth/src/precompiles/pd/read_bytes.rs` | Update tests to use `ChunkDataIndex` |
| `crates/irys-reth/tests/pd_context_isolation.rs` | Update to use `ChunkDataIndex` |
| `crates/chain/src/chain.rs` | Replace `MockChunkProvider` with `ChunkConfig::from_consensus()` |
| `crates/chain/Cargo.toml` | Move `test-utils` from `[dependencies]` to `[dev-dependencies]` |
| `crates/reth-node-bridge/src/node.rs` | Replace `chunk_provider` param with `chunk_config` |
| `crates/reth-node-bridge/Cargo.toml` | Remove `test-utils` from `irys-reth` dependency |
| `crates/actors/src/pd_service.rs` | Rename `RethChunkProvider` usage; move `MockChunkProvider` into test module |
| `crates/domain/src/models/chunk_provider.rs` | Rename `RethChunkProvider` → `ChunkStorageProvider` in impl |

## Scope Clarification

The domain `ChunkProvider` struct (`crates/domain/src/models/chunk_provider.rs`) and its `Arc<ChunkProvider>` field on `IrysNodeCtx` (`chain.rs:108`), `ApiState`, and `IrysRethProviderInner` are **out of scope**. These serve the API server and PdService storage backend — they are not part of the EVM abstraction chain being simplified.

The `pd_chunk_sender: PdChunkSender` field on `IrysEthereumNode` is also **unchanged** — it's used by `IrysPoolBuilder` for PD transaction detection and is a separate concern.

## Risk Assessment

**Low risk.** All removed code is provably unreachable in production:
- `ChunkSource::Storage` is never matched in production (executor always has `chunk_data_index`)
- `MockChunkProvider::get_unpacked_chunk_by_ledger_offset()` is never called in production
- `ConfigureChunkDataIndex` trait has a single implementation and single call site

The rename (`RethChunkProvider` → `ChunkStorageProvider`) is mechanical and caught by the compiler.

Test changes switch from mock-backed chunk reads to DashMap-backed chunk reads — functionally equivalent since the mock returns zero-filled bytes and we populate the DashMap with zero-filled bytes.

## Known Pre-existing Issue (Out of Scope)

`ChunkConfig` stores `entropy_packing_iterations: u8` and `chain_id: u16`, but `ConsensusConfig` has these as `u32` and `u64` respectively. The `as u8` / `as u16` casts in `ChunkConfig::from_consensus()` are lossy — this is the same behavior as the existing `ChunkProvider::config()` impl (`domain/chunk_provider.rs:161`). Fixing the field widths is a separate concern and not part of this refactor.
