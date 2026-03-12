# Simplify PD Chunk Abstractions — Implementation Plan

## Overview

Remove dead/vestigial abstractions from the PD chunk path after commit `c0f1ce2c` made `ChunkDataIndex` the sole chunk source for both EVM paths. Delete `ChunkSource` enum, flatten `PdContext`, simplify `IrysEvmFactory`, remove `Arc<dyn RethChunkProvider>` from the EVM wiring chain, rename `RethChunkProvider` → `ChunkStorageProvider`, and move `MockChunkProvider` to `pd_service` tests.

**Design spec**: `docs/superpowers/specs/2026-03-11-simplify-pd-chunk-abstractions-design.md`

## Current State Analysis

Both EVM paths (payload builder and block executor) always use `ChunkSource::Manager` — reading chunks from the shared DashMap (`ChunkDataIndex`). This leaves several abstractions dead:

- `ChunkSource::Storage` variant — unreachable in production
- `PdContext::new(chunk_provider)` — only used for `ChunkConfig` extraction
- `RethChunkProvider` trait in the EVM path — EVM never calls it
- `MockChunkProvider` in production code — exists only to provide `ChunkConfig`
- `IrysEvmFactory.chunk_data_index: Option<_>` — always `Some`, `None` branch dead
- `ConfigureChunkDataIndex` trait — single-use trait for payload builder wiring
- `IrysEvmFactory.context: PdContext` — stored but never used for chunk reads

### Key Discoveries:
- `PdContext` struct at `crates/irys-reth/src/precompiles/pd/context.rs:31-42` has `chunk_source: ChunkSource` field
- `ChunkSource` enum at `context.rs:11-18` has `Manager` and `Storage` variants
- `IrysEvmFactory` at `crates/irys-reth/src/evm.rs:382-388` holds `context: PdContext`, `chunk_data_index: Option<ChunkDataIndex>`
- `ConfigureChunkDataIndex` trait at `evm.rs:366-378` — single impl, single consumer
- `IrysEthereumNode` at `crates/irys-reth/src/lib.rs:96-106` holds `chunk_provider: Arc<dyn RethChunkProvider>`
- `IrysExecutorBuilder` at `lib.rs:237-252` holds `chunk_provider: Arc<dyn RethChunkProvider>`
- `chain.rs:1154-1166` creates `MockChunkProvider::new()` just to pass config into EVM chain
- `RethChunkProvider` trait at `crates/types/src/chunk_provider.rs:22-32` — used by `PdService` and domain `ChunkProvider`, not by Reth/EVM
- `MockChunkProvider` at `types/chunk_provider.rs:72-75` gated by `#[cfg(any(test, feature = "test-utils"))]`

## Desired End State

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

### Verification:
- `cargo xtask check` passes
- `cargo xtask test` passes
- `cargo clippy --workspace --tests --all-targets` clean
- No `ChunkSource` references remain
- No `ConfigureChunkDataIndex` references remain
- No `RethChunkProvider` references remain (only `ChunkStorageProvider`)
- No `MockChunkProvider` in `crates/types/`

## What We're NOT Doing

- Changing the domain `ChunkProvider` struct or its `Arc<ChunkProvider>` usage in `IrysNodeCtx`, `ApiState`, `IrysRethProviderInner`
- Changing `pd_chunk_sender: PdChunkSender` on `IrysEthereumNode`
- Fixing the `ChunkConfig` field width mismatches (`entropy_packing_iterations: u8` vs `u32`, `chain_id: u16` vs `u64`)
- Changing `PdService` storage backend logic
- Modifying `ready_pd_txs` handling

## Implementation Approach

Two phases. Phase 1 is the core structural refactor — all interconnected type changes must happen atomically since changing `PdContext` constructors breaks `IrysEvmFactory` which breaks the wiring chain. Phase 2 is the independent rename + mock relocation + Cargo.toml cleanup.

---

## Phase 1: Core Structural Refactor

### Overview
Delete `ChunkSource` enum, flatten `PdContext`, simplify `IrysEvmFactory`, remove `chunk_provider` from EVM wiring chain, simplify payload builder wiring, and update all tests. Corresponds to design spec Changes 1–4 and 7.

### Changes Required:

#### 1.1 Add `ChunkConfig::from_consensus()`
**File**: `crates/types/src/chunk_provider.rs`

Add after the `ChunkConfig` struct definition (after line 18):

```rust
impl ChunkConfig {
    pub fn from_consensus(consensus: &crate::ConsensusConfig) -> Self {
        Self {
            num_chunks_in_partition: consensus.num_chunks_in_partition,
            chunk_size: consensus.chunk_size,
            entropy_packing_iterations: consensus.entropy_packing_iterations as u8,
            chain_id: consensus.chain_id as u16,
        }
    }
}
```

Note: The `as u8`/`as u16` casts match existing behavior in `domain/chunk_provider.rs:157-164`. This is a known pre-existing issue documented in the design spec as out of scope.

#### 1.2 Flatten `PdContext`
**File**: `crates/irys-reth/src/precompiles/pd/context.rs`

**Delete**:
- `ChunkSource` enum (lines 11-18)
- `Clone` impl for `ChunkSource` (lines 20-29)
- `PdContext::new(chunk_provider: Arc<dyn RethChunkProvider>)` constructor (lines 74-82)
- `use irys_types::chunk_provider::RethChunkProvider` import

**Modify `PdContext` struct** (lines 31-42):
```rust
// BEFORE
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

**Modify `Clone` impl** (lines 46-55) — replace `chunk_source: self.chunk_source.clone()` with `chunk_data_index: self.chunk_data_index.clone()`.

**Rename and simplify constructor** — rename `new_with_manager` to `new`:
```rust
// BEFORE (lines 60-70)
pub fn new_with_manager(
    chunk_config: ChunkConfig,
    chunk_data_index: irys_types::chunk_provider::ChunkDataIndex,
) -> Self { ... }

// AFTER
pub fn new(
    chunk_config: ChunkConfig,
    chunk_data_index: irys_types::chunk_provider::ChunkDataIndex,
) -> Self {
    Self {
        tx_hash: Arc::new(RwLock::new(None)),
        access_list: Arc::new(RwLock::new(Vec::new())),
        chunk_data_index,
        chunk_config,
    }
}
```

**Modify `clone_for_new_evm`** (lines 87-94) — replace `chunk_source: self.chunk_source.clone()` with `chunk_data_index: self.chunk_data_index.clone()`.

**Simplify `get_chunk`** (lines 129-144):
```rust
// BEFORE
pub fn get_chunk(&self, ledger: u32, offset: u64) -> eyre::Result<Option<Bytes>> {
    match &self.chunk_source {
        ChunkSource::Manager { chunk_data_index } => {
            match chunk_data_index.get(&(ledger, offset)) {
                Some(chunk_bytes) => Ok(Some((*chunk_bytes).clone())),
                None => {
                    tracing::warn!(...);
                    Ok(None)
                }
            }
        }
        ChunkSource::Storage(provider) => {
            provider.get_unpacked_chunk_by_ledger_offset(ledger, offset)
        }
    }
}

// AFTER
pub fn get_chunk(&self, ledger: u32, offset: u64) -> eyre::Result<Option<Bytes>> {
    match self.chunk_data_index.get(&(ledger, offset)) {
        Some(chunk_bytes) => Ok(Some((*chunk_bytes).clone())),
        None => {
            tracing::warn!(
                ledger,
                offset,
                "chunk not found in chunk_data_index"
            );
            Ok(None)
        }
    }
}
```

#### 1.3 Simplify `IrysEvmFactory`
**File**: `crates/irys-reth/src/evm.rs`

**Modify struct** (lines 382-388):
```rust
// BEFORE
pub struct IrysEvmFactory {
    context: PdContext,
    hardfork_config: Arc<irys_types::hardfork_config::IrysHardforkConfig>,
    chunk_data_index: Option<irys_types::chunk_provider::ChunkDataIndex>,
}

// AFTER
pub struct IrysEvmFactory {
    chunk_config: irys_types::chunk_provider::ChunkConfig,
    hardfork_config: Arc<irys_types::hardfork_config::IrysHardforkConfig>,
    chunk_data_index: irys_types::chunk_provider::ChunkDataIndex,
}
```

**Modify constructor** (lines 391-401):
```rust
// BEFORE
pub fn new(
    chunk_provider: Arc<dyn irys_types::chunk_provider::RethChunkProvider>,
    hardfork_config: Arc<irys_types::hardfork_config::IrysHardforkConfig>,
) -> Self { ... }

// AFTER
pub fn new(
    chunk_config: irys_types::chunk_provider::ChunkConfig,
    hardfork_config: Arc<irys_types::hardfork_config::IrysHardforkConfig>,
    chunk_data_index: irys_types::chunk_provider::ChunkDataIndex,
) -> Self {
    Self {
        chunk_config,
        hardfork_config,
        chunk_data_index,
    }
}
```

**Delete**:
- `with_chunk_data_index()` method (lines 403-410)
- `#[cfg(test)] pub fn context(&self)` accessor (lines 413-416)
- `ConfigureChunkDataIndex` trait and its impl block (lines 366-378)
- `IrysEvmConfig::with_chunk_data_index()` method (lines 350-363)
- `IrysBlockExecutorFactory::with_chunk_data_index()` method (lines 231-244)

**Modify `new_for_testing`** (lines 424-430):
```rust
// BEFORE
#[cfg(test)]
pub fn new_for_testing(
    chunk_provider: Arc<dyn irys_types::chunk_provider::RethChunkProvider>,
) -> Self {
    let hardfork_config = Arc::new(irys_types::config::ConsensusConfig::testing().hardforks);
    Self::new(chunk_provider, hardfork_config)
}

// AFTER
#[cfg(test)]
pub fn new_for_testing(chunk_data_index: irys_types::chunk_provider::ChunkDataIndex) -> Self {
    let consensus = irys_types::config::ConsensusConfig::testing();
    let hardfork_config = Arc::new(consensus.hardforks);
    let chunk_config = irys_types::chunk_provider::ChunkConfig::from_consensus(&consensus);
    Self::new(chunk_config, hardfork_config, chunk_data_index)
}
```

**Modify `create_evm`** (lines 443-484) — replace the `match &self.chunk_data_index` dispatch:
```rust
// BEFORE (lines 457-460)
let pd_context = match &self.chunk_data_index {
    Some(index) => PdContext::new_with_manager(self.context.chunk_config(), index.clone()),
    None => self.context.clone_for_new_evm(),
};

// AFTER
let pd_context = PdContext::new(self.chunk_config, self.chunk_data_index.clone());
```

**Modify `create_evm_with_inspector`** (lines 486-532) — same change at lines 504-508:
```rust
// BEFORE
let pd_context = match &self.chunk_data_index {
    Some(index) => PdContext::new_with_manager(self.context.chunk_config(), index.clone()),
    None => self.context.clone_for_new_evm(),
};

// AFTER
let pd_context = PdContext::new(self.chunk_config, self.chunk_data_index.clone());
```

#### 1.4 Remove `chunk_provider` from EVM wiring chain
**File**: `crates/irys-reth/src/lib.rs`

**Modify `IrysEthereumNode`** (lines 96-106):
```rust
// BEFORE
pub struct IrysEthereumNode {
    pub max_pd_chunks_per_block: u64,
    pub chunk_provider: Arc<dyn irys_types::chunk_provider::RethChunkProvider>,
    pub hardfork_config: Arc<irys_types::hardfork_config::IrysHardforkConfig>,
    pub pd_chunk_sender: irys_types::chunk_provider::PdChunkSender,
    pub ready_pd_txs: Arc<dashmap::DashSet<revm_primitives::B256>>,
    pub chunk_data_index: irys_types::chunk_provider::ChunkDataIndex,
}

// AFTER
pub struct IrysEthereumNode {
    pub max_pd_chunks_per_block: u64,
    pub chunk_config: irys_types::chunk_provider::ChunkConfig,
    pub hardfork_config: Arc<irys_types::hardfork_config::IrysHardforkConfig>,
    pub pd_chunk_sender: irys_types::chunk_provider::PdChunkSender,
    pub ready_pd_txs: Arc<dashmap::DashSet<revm_primitives::B256>>,
    pub chunk_data_index: irys_types::chunk_provider::ChunkDataIndex,
}
```

**Modify `IrysExecutorBuilder`** (lines 237-252):
```rust
// BEFORE
pub struct IrysExecutorBuilder {
    chunk_provider: Arc<dyn irys_types::chunk_provider::RethChunkProvider>,
    hardfork_config: Arc<irys_types::hardfork_config::IrysHardforkConfig>,
    chunk_data_index: irys_types::chunk_provider::ChunkDataIndex,
}

// AFTER
pub struct IrysExecutorBuilder {
    chunk_config: irys_types::chunk_provider::ChunkConfig,
    hardfork_config: Arc<irys_types::hardfork_config::IrysHardforkConfig>,
    chunk_data_index: irys_types::chunk_provider::ChunkDataIndex,
}
```

**Modify `components()`** (lines 148-168) — pass `chunk_config` instead of `chunk_provider` to `IrysExecutorBuilder`.

**Modify `build_evm()`** (lines 261-282):
```rust
// BEFORE
let factory = IrysEvmFactory::new(self.chunk_provider, self.hardfork_config)
    .with_chunk_data_index(self.chunk_data_index);

// AFTER
let factory = IrysEvmFactory::new(self.chunk_config, self.hardfork_config, self.chunk_data_index);
```

**File**: `crates/chain/src/chain.rs`

**Modify `start_reth_node`** (lines 343-372) — replace `chunk_provider: Arc<dyn irys_types::chunk_provider::RethChunkProvider>` with `chunk_config: irys_types::chunk_provider::ChunkConfig` in signature and pass-through.

**Modify call site** (lines 1154-1166):
```rust
// BEFORE
let mock_provider = irys_types::chunk_provider::MockChunkProvider::new();
let (node_handle, reth_node) = match start_reth_node(
    exec, reth_chainspec, config.clone(), latest_block_height,
    Arc::new(mock_provider), pd_chunk_tx.clone(), ready_pd_txs.clone(), chunk_data_index.clone(),
)

// AFTER
let chunk_config = irys_types::chunk_provider::ChunkConfig::from_consensus(&config.consensus);
let (node_handle, reth_node) = match start_reth_node(
    exec, reth_chainspec, config.clone(), latest_block_height,
    chunk_config, pd_chunk_tx.clone(), ready_pd_txs.clone(), chunk_data_index.clone(),
)
```

**File**: `crates/reth-node-bridge/src/node.rs`

**Modify `run_node`** (lines 100-268) — replace `chunk_provider: Arc<dyn irys_types::chunk_provider::RethChunkProvider>` with `chunk_config: irys_types::chunk_provider::ChunkConfig` in signature. Update the `IrysEthereumNode` construction at lines 233-241 to use `chunk_config` instead of `chunk_provider`.

#### 1.5 Simplify payload builder wiring
**File**: `crates/irys-reth/src/payload_builder_builder.rs`

**Remove `chunk_data_index` field** from `IrysPayloadBuilderBuilder` struct (line 27).

**Remove the `+ ConfigureChunkDataIndex` trait bound** from the `PayloadBuilderBuilder` impl (line 49 area).

**Remove the `with_chunk_data_index()` call** in `build_payload_builder()` (line 72):
```rust
// BEFORE
let evm_config = evm_config.with_chunk_data_index(self.chunk_data_index.clone());

// DELETE this line entirely — chunk_data_index is already baked into IrysEvmFactory
```

**Update `Debug` impl** (lines 30-39) — remove the `chunk_data_index` field line.

**Update construction in `components()`** (`lib.rs` lines 148-168) — remove `chunk_data_index` from `IrysPayloadBuilderBuilder` construction.

#### 1.6 Update tests

**File**: `crates/irys-reth/src/evm.rs` (test module, lines 1829+)

Add test helpers inside `#[cfg(test)] mod tests`:
```rust
/// Creates an empty ChunkDataIndex for testing.
fn test_chunk_data_index() -> irys_types::chunk_provider::ChunkDataIndex {
    Arc::new(DashMap::new())
}

/// Inserts a zero-filled chunk at the given (ledger, offset) for testing.
fn insert_test_chunk(
    index: &irys_types::chunk_provider::ChunkDataIndex,
    ledger: u32,
    offset: u64,
    chunk_size: usize,
) {
    index.insert((ledger, offset), Arc::new(Bytes::from(vec![0u8; chunk_size])));
}
```

Update tests that use `MockChunkProvider`:
- `evm_rejects_eip4844...` (line 1898): `IrysEvmFactory::new_for_testing(test_chunk_data_index())`
- `evm_processes_normal_tx...` (line 1938): same
- Remove `MockChunkProvider` import

**File**: `crates/irys-reth/src/precompiles/pd/precompile.rs` (test module, lines 138+)

Update `execute_precompile` helper (lines 172-194):
```rust
// BEFORE
let mock_chunk_provider = Arc::new(irys_types::chunk_provider::MockChunkProvider::new());
let factory = IrysEvmFactory::new_for_testing(mock_chunk_provider);

// AFTER
let chunk_data_index: irys_types::chunk_provider::ChunkDataIndex = Arc::new(DashMap::new());
// Pre-populate chunks for access list entries
for item in &access_list.0 {
    for key in &item.storage_keys {
        // Parse key to determine if it's a ChunkRead
        // Insert zero-filled chunk at appropriate (ledger, offset)
    }
}
let factory = IrysEvmFactory::new_for_testing(chunk_data_index);
```

Note: The precompile tests need chunks pre-populated in the `ChunkDataIndex` so that `get_chunk()` returns data. The existing `MockChunkProvider` returns zero-filled 256KB chunks for any request. We need to replicate this by inserting entries into the DashMap for the (ledger, offset) pairs derived from the access list entries used in each test.

Each test constructs access list entries with specific `ChunkRead` args containing `ledger` and `chunk_offset` fields. We need to insert corresponding zero-filled chunks. The easiest approach: pre-populate entries for ledger=0, offsets 0..N matching the test's access list. Check each test's access list construction to determine exact keys.

Update all individual tests that directly construct `MockChunkProvider` (lines 253, 279, 333, 359, 405, 461, 516) to use `test_chunk_data_index()` + pre-populate as needed.

**File**: `crates/irys-reth/src/precompiles/pd/read_bytes.rs` (test module, lines 365+)

Update `create_test_context` helper (lines 375-378):
```rust
// BEFORE
fn create_test_context() -> PdContext {
    let mock_provider = Arc::new(MockChunkProvider::new());
    PdContext::new(mock_provider)
}

// AFTER
fn create_test_context() -> PdContext {
    let chunk_config = irys_types::chunk_provider::ChunkConfig {
        num_chunks_in_partition: 100,
        chunk_size: 256_000,
        entropy_packing_iterations: 0,
        chain_id: 1,
    };
    let chunk_data_index: irys_types::chunk_provider::ChunkDataIndex = Arc::new(DashMap::new());
    PdContext::new(chunk_config, chunk_data_index)
}
```

Tests in `read_bytes.rs` that actually call `get_chunk` on the context will need chunks pre-populated. Check which tests do chunk reads and insert zero-filled entries for the appropriate keys.

**File**: `crates/irys-reth/tests/pd_context_isolation.rs`

Update test (lines 63-146):
```rust
// BEFORE (line 66-68)
let mock_chunk_provider = Arc::new(MockChunkProvider::new());
let hardfork_config = Arc::new(ConsensusConfig::testing().hardforks);
let factory = IrysEvmFactory::new(mock_chunk_provider, hardfork_config);

// AFTER
let chunk_data_index: irys_types::chunk_provider::ChunkDataIndex = Arc::new(DashMap::new());
let factory = IrysEvmFactory::new_for_testing(chunk_data_index);
```

Remove `MockChunkProvider` import. Add `DashMap` import.

**File**: `crates/irys-reth/src/lib.rs` (`setup_irys_reth` test helper, lines 3325-3396)

Update signature — remove `chunk_provider` parameter:
```rust
// BEFORE
pub async fn setup_irys_reth(
    num_nodes: &[Address],
    chain_spec: Arc<...>,
    is_dev: bool,
    attributes_generator: impl Fn(...) + ...,
    chunk_provider: Arc<dyn irys_types::chunk_provider::RethChunkProvider>,
) -> ...

// AFTER
pub async fn setup_irys_reth(
    num_nodes: &[Address],
    chain_spec: Arc<...>,
    is_dev: bool,
    attributes_generator: impl Fn(...) + ...,
) -> ...
```

Inside the function, replace `chunk_provider.clone()` with:
```rust
let chunk_config = irys_types::chunk_provider::ChunkConfig::from_consensus(
    &irys_types::config::ConsensusConfig::testing()
);
```

Update `IrysEthereumNode` construction to use `chunk_config` instead of `chunk_provider`.

Update all callers of `setup_irys_reth` — remove the `MockChunkProvider` argument.

### Success Criteria:

#### Automated Verification:
- [x] `cargo xtask check` passes
- [x] `cargo nextest run -p irys-reth` passes (unit + integration tests)
- [x] `cargo clippy --workspace --tests --all-targets` clean
- [x] `cargo fmt --all --check` clean
- [x] No `ChunkSource` references in codebase (grep confirms)
- [x] No `ConfigureChunkDataIndex` references in codebase (grep confirms)
- [x] `IrysEvmFactory` no longer has `context: PdContext` field or `Option<ChunkDataIndex>`
- [x] No `MockChunkProvider` usage in `crates/irys-reth/` or `crates/chain/` production code

**Implementation Note**: After completing this phase and all automated verification passes, pause here for review before proceeding to Phase 2.

---

## Phase 2: Rename + MockChunkProvider Relocation + Cargo.toml Cleanup

> **Phase 1 completed in commit `59fcc69d`.** Notable deviations from plan:
> - `IrysEvmFactory::new_for_testing` gate changed from `#[cfg(test)]` to `#[cfg(any(test, feature = "test-utils"))]` so integration tests can use it.
> - `pd_context_isolation.rs` integration test constructs `IrysEvmFactory::new()` directly (avoiding feature-gate issues).
> - Line numbers in Phase 2 below may be stale — read the actual files before editing.

### Overview
Rename `RethChunkProvider` → `ChunkStorageProvider` everywhere, move `MockChunkProvider` from `irys-types` to `pd_service` test module, and clean up Cargo.toml feature flags. Corresponds to design spec Changes 5-6.

### Changes Required:

#### 2.1 Rename `RethChunkProvider` → `ChunkStorageProvider`
**File**: `crates/types/src/chunk_provider.rs` (line 22)
```rust
// BEFORE
pub trait RethChunkProvider: Send + Sync + std::fmt::Debug {

// AFTER
pub trait ChunkStorageProvider: Send + Sync + std::fmt::Debug {
```

**File**: `crates/domain/src/models/chunk_provider.rs` (line 129)
```rust
// BEFORE
impl irys_types::chunk_provider::RethChunkProvider for ChunkProvider {

// AFTER
impl irys_types::chunk_provider::ChunkStorageProvider for ChunkProvider {
```

**File**: `crates/actors/src/pd_service.rs`
- Line 6: Update import `RethChunkProvider` → `ChunkStorageProvider`
- Line 28: Update field type `Arc<dyn RethChunkProvider>` → `Arc<dyn ChunkStorageProvider>`
- Line 38: Update parameter type in `spawn_service`

**All other `use` statements**: grep for `RethChunkProvider` and update.

#### 2.2 Move `MockChunkProvider` to `pd_service` tests
**File**: `crates/types/src/chunk_provider.rs`

**Delete** `MockChunkProvider` struct, `new()`, `Default` impl, and `RethChunkProvider` impl (lines 70-117, gated by `#[cfg(any(test, feature = "test-utils"))]`).

Also delete the re-export if present in the module's public API.

**File**: `crates/actors/src/pd_service.rs`

Inside the existing `#[cfg(test)] mod tests` block, add:
```rust
#[derive(Debug, Clone)]
struct MockChunkProvider {
    config: irys_types::chunk_provider::ChunkConfig,
    cached_chunk: Bytes,
}

impl MockChunkProvider {
    fn new() -> Self {
        let config = irys_types::chunk_provider::ChunkConfig {
            num_chunks_in_partition: 100,
            chunk_size: 256_000,
            entropy_packing_iterations: 0,
            chain_id: 1,
        };
        let cached_chunk = Bytes::from(vec![0_u8; 256_000]);
        Self { config, cached_chunk }
    }
}

impl irys_types::chunk_provider::ChunkStorageProvider for MockChunkProvider {
    fn get_unpacked_chunk_by_ledger_offset(
        &self,
        _ledger: u32,
        _ledger_offset: u64,
    ) -> eyre::Result<Option<Bytes>> {
        Ok(Some(self.cached_chunk.clone()))
    }

    fn config(&self) -> irys_types::chunk_provider::ChunkConfig {
        self.config
    }
}
```

Remove `use irys_types::chunk_provider::MockChunkProvider;` from the test module imports.

#### 2.3 Cargo.toml cleanup
**File**: `crates/chain/Cargo.toml`

Move `irys-types` `test-utils` feature from `[dependencies]` to `[dev-dependencies]`:
```toml
# [dependencies] section — change:
# BEFORE
irys-types = { workspace = true, features = ["test-utils"] }
# AFTER
irys-types = { workspace = true }

# [dev-dependencies] section — add:
irys-types = { workspace = true, features = ["test-utils"] }
```

Note: Check if `irys-types` already appears in `[dev-dependencies]`. If so, just add the feature there. If `test-utils` is only needed because of `MockChunkProvider` (which is now gone from this crate), it may not be needed at all — verify by compiling.

**File**: `crates/reth-node-bridge/Cargo.toml`

Remove `features = ["test-utils"]` from `irys-reth` dependency (line 20):
```toml
# BEFORE
irys-reth = { workspace = true, features = ["test-utils"] }
# AFTER
irys-reth = { workspace = true }
```

### Success Criteria:

#### Automated Verification:
- [x] `cargo xtask check` passes
- [x] `cargo xtask test` passes (full test suite)
- [x] `cargo clippy --workspace --tests --all-targets` clean
- [x] `cargo fmt --all --check` clean
- [x] No `RethChunkProvider` references in codebase (grep confirms — only `ChunkStorageProvider`)
- [x] No `MockChunkProvider` in `crates/types/src/chunk_provider.rs`
- [x] `MockChunkProvider` exists only in `crates/actors/src/pd_service.rs` test module
- [x] `chain/Cargo.toml` does not have `test-utils` in `[dependencies]` for `irys-types`
- [x] `reth-node-bridge/Cargo.toml` does not have `test-utils` for `irys-reth`

---

## Testing Strategy

### Unit Tests (updated in Phase 1):
- `evm.rs` tests: `evm_rejects_eip4844...`, `evm_processes_normal_tx...` — use `test_chunk_data_index()`
- `precompile.rs` tests: all 8 test cases — pre-populate `ChunkDataIndex` with chunks matching access list entries
- `read_bytes.rs` tests: `create_test_context()` uses `ChunkConfig` + `ChunkDataIndex` directly
- `pd_context_isolation.rs`: uses `IrysEvmFactory::new_for_testing(chunk_data_index)`
- `pd_service.rs` tests: use local `MockChunkProvider` (Phase 2)

### Integration Tests:
- Full node tests in `evm.rs` (`evm_pd_header_tx_charges_fees`, etc.) go through `setup_irys_reth` → exercise the full wiring chain
- `cargo nextest run -p irys-chain` for chain-level integration tests

### Verification Commands:
```sh
# Phase 1
cargo xtask check
cargo nextest run -p irys-reth
cargo clippy --workspace --tests --all-targets

# Phase 2
cargo xtask check
cargo xtask test
cargo clippy --workspace --tests --all-targets

# Post-completion grep checks
rg "ChunkSource" crates/
rg "ConfigureChunkDataIndex" crates/
rg "RethChunkProvider" crates/
rg "MockChunkProvider" crates/types/
```

## Risk Assessment

**Low risk.** All removed code is provably unreachable in production:
- `ChunkSource::Storage` is never matched in production (executor always has `chunk_data_index`)
- `MockChunkProvider::get_unpacked_chunk_by_ledger_offset()` is never called in production
- `ConfigureChunkDataIndex` trait has a single implementation and single call site

The rename (`RethChunkProvider` → `ChunkStorageProvider`) is mechanical and caught by the compiler.

Test changes switch from mock-backed chunk reads to DashMap-backed chunk reads — functionally equivalent since the mock returns zero-filled bytes and we populate the DashMap with zero-filled bytes.

## References

- Design spec: `docs/superpowers/specs/2026-03-11-simplify-pd-chunk-abstractions-design.md`
- Prior commit: `c0f1ce2c` (gave executor ChunkDataIndex so PD precompile matches payload builder)
