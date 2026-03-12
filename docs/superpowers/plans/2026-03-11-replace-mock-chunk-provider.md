# Fix Executor Gas Mismatch — Give Executor the ChunkDataIndex

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix gas mismatch between payload building and block validation by giving the reth executor access to the same `ChunkDataIndex` DashMap that the payload builder uses.

**Architecture:** Pass `chunk_data_index` through `IrysExecutorBuilder` → `IrysEvmFactory`, so both the payload builder and executor create Manager-mode `PdContext` (reading chunks from the shared DashMap). The `MockChunkProvider` remains only as a vehicle for `ChunkConfig` extraction — it is never used for actual chunk reads.

**Tech Stack:** Rust, existing `ChunkDataIndex`/`IrysEvmFactory`/`ConfigureChunkDataIndex` types

---

## Bug Summary

**Symptom:** Integration tests fail with `block gas used mismatch: got 62608, expected 122308`.

**Root cause:** The reth executor's `IrysEvmFactory` has `chunk_data_index: None`, so it uses `ChunkSource::Storage(MockChunkProvider)` during block validation. The `MockChunkProvider` returns zero-filled 256KB chunks. The payload builder uses `ChunkSource::Manager(ChunkDataIndex)` which has real chunk data. Different data → different EVM execution → different gas → block rejected.

**Fix:** Pass `chunk_data_index` to `IrysExecutorBuilder` so the executor also uses `ChunkSource::Manager`. Both paths read identical data from the DashMap → identical gas.

**Why the DashMap is always populated for the executor:**
- **Self-built blocks:** PdService populates the DashMap during `NewTransaction` handling (mempool monitoring). Chunks remain pinned by tx_hash reference.
- **Peer blocks:** The validation service sends `ProvisionBlockChunks` to PdService and **awaits the response** before sending `newPayload` to reth. PdService fetches chunks from storage into the DashMap before the executor runs.

**Failing tests:**
- `programmable_data::pd_content_verification::heavy_test_pd_content_verification`
- `programmable_data::preloaded_chunk_table::heavy_test_pd_headerless_access_list_reverts`
- `programmable_data::preloaded_chunk_table::heavy_test_pd_multi_tx_single_block`
- `programmable_data::preloaded_chunk_table::heavy_test_pd_single_chunk_read::case_1_offset_4`
- `programmable_data::preloaded_chunk_table::heavy_test_pd_single_chunk_read::case_2_offset_3`

## Key Context

### Current Flow (broken)

```
Payload builder:
  IrysPayloadBuilderBuilder.build_payload()
    → evm_config.with_chunk_data_index(index)        ← sets chunk_data_index: Some(...)
    → IrysEvmFactory creates PdContext::new_with_manager()
    → ChunkSource::Manager reads from DashMap         ← real chunk data ✓

Executor:
  IrysExecutorBuilder.build_evm()
    → IrysEvmFactory::new(mock_provider, hardfork)    ← chunk_data_index: None
    → IrysEvmFactory creates self.context.clone_for_new_evm()
    → ChunkSource::Storage reads from MockChunkProvider ← zero-filled data ✗
```

### Fixed Flow

```
Payload builder:  (unchanged)
  → ChunkSource::Manager reads from DashMap           ← real chunk data ✓

Executor:
  IrysExecutorBuilder.build_evm()
    → IrysEvmFactory::new(mock_provider, hardfork)
        .with_chunk_data_index(self.chunk_data_index)  ← NEW: sets chunk_data_index: Some(...)
    → IrysEvmFactory creates PdContext::new_with_manager()
    → ChunkSource::Manager reads from DashMap           ← real chunk data ✓
```

### Key Code Locations

| Location | Current Role |
|----------|-------------|
| `crates/irys-reth/src/lib.rs:95-106` | `IrysEthereumNode` struct — already has `chunk_data_index` field |
| `crates/irys-reth/src/lib.rs:237-240` | `IrysExecutorBuilder` struct — missing `chunk_data_index` field |
| `crates/irys-reth/src/lib.rs:155-158` | `components()` — constructs `IrysExecutorBuilder` without `chunk_data_index` |
| `crates/irys-reth/src/lib.rs:258-275` | `build_evm()` — creates `IrysEvmFactory` without setting `chunk_data_index` |
| `crates/irys-reth/src/evm.rs:404-410` | `IrysEvmFactory::with_chunk_data_index()` — builder method, already exists |
| `crates/irys-reth/src/payload_builder_builder.rs:69-72` | Payload builder calls `with_chunk_data_index()` — reference for how to do it |
| `crates/chain/src/chain.rs:1154-1155` | `MockChunkProvider` creation — stays (needed for `ChunkConfig`) |

### Files Modified

| File | Changes |
|------|---------|
| `crates/irys-reth/src/lib.rs` | Add `chunk_data_index` to `IrysExecutorBuilder`; wire through `components()` and `build_evm()` |
| `crates/chain/src/chain.rs` | Update TODO comment (mock now intentional for ChunkConfig) |

---

## Chunk 1: Wire ChunkDataIndex to Executor

### Task 1: Add `chunk_data_index` to `IrysExecutorBuilder`

**Files:**
- Modify: `crates/irys-reth/src/lib.rs:237-240` (struct definition)
- Modify: `crates/irys-reth/src/lib.rs:242-248` (Debug impl)
- Modify: `crates/irys-reth/src/lib.rs:155-158` (`components()` construction)
- Modify: `crates/irys-reth/src/lib.rs:258-275` (`build_evm()` implementation)

- [x] **Step 1: Add `chunk_data_index` field to `IrysExecutorBuilder` struct**

Current (`lib.rs:236-240`):
```rust
#[derive(Clone)]
pub struct IrysExecutorBuilder {
    chunk_provider: Arc<dyn irys_types::chunk_provider::RethChunkProvider>,
    hardfork_config: Arc<irys_types::hardfork_config::IrysHardforkConfig>,
}
```

New:
```rust
#[derive(Clone)]
pub struct IrysExecutorBuilder {
    chunk_provider: Arc<dyn irys_types::chunk_provider::RethChunkProvider>,
    hardfork_config: Arc<irys_types::hardfork_config::IrysHardforkConfig>,
    chunk_data_index: irys_types::chunk_provider::ChunkDataIndex,
}
```

- [x] **Step 2: Update the Debug impl**

Current (`lib.rs:242-248`):
```rust
impl std::fmt::Debug for IrysExecutorBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IrysExecutorBuilder")
            .field("chunk_provider", &"<Arc<dyn RethChunkProvider>>")
            .field("hardfork_config", &self.hardfork_config)
            .finish()
    }
}
```

New:
```rust
impl std::fmt::Debug for IrysExecutorBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IrysExecutorBuilder")
            .field("chunk_provider", &"<Arc<dyn RethChunkProvider>>")
            .field("hardfork_config", &self.hardfork_config)
            .field("chunk_data_index", &"<ChunkDataIndex>")
            .finish()
    }
}
```

- [x] **Step 3: Pass `chunk_data_index` when constructing `IrysExecutorBuilder` in `components()`**

Current (`lib.rs:155-158`):
```rust
            .executor(IrysExecutorBuilder {
                chunk_provider: self.chunk_provider.clone(),
                hardfork_config: self.hardfork_config.clone(),
            })
```

New:
```rust
            .executor(IrysExecutorBuilder {
                chunk_provider: self.chunk_provider.clone(),
                hardfork_config: self.hardfork_config.clone(),
                chunk_data_index: self.chunk_data_index.clone(),
            })
```

- [x] **Step 4: Use `chunk_data_index` in `build_evm()`**

Current (`lib.rs:258-275`):
```rust
    async fn build_evm(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::EVM> {
        let evm_config = EthEvmConfig::new(ctx.chain_spec());

        let spec = ctx.chain_spec();
        // Note: IrysExecutorBuilder does not set chunk_data_index on the factory —
        // the executor uses Storage mode (direct chunk provider). Only the payload
        // builder sets chunk_data_index via IrysPayloadBuilderBuilder.
        let evm_factory = IrysEvmFactory::new(self.chunk_provider, self.hardfork_config);
        let evm_config = evm::IrysEvmConfig {
            inner: evm_config,
            assembler: IrysBlockAssembler::new(ctx.chain_spec()),
            executor_factory: evm::IrysBlockExecutorFactory::new(
                RethReceiptBuilder::default(),
                spec,
                evm_factory,
            ),
        };
        Ok(evm_config)
    }
```

New:
```rust
    async fn build_evm(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::EVM> {
        let evm_config = EthEvmConfig::new(ctx.chain_spec());

        let spec = ctx.chain_spec();
        // Both executor and payload builder use ChunkDataIndex for PD chunk reads.
        // PdService populates the DashMap before EVM execution in both paths:
        //   - Self-built blocks: populated during NewTransaction handling
        //   - Peer blocks: populated during ProvisionBlockChunks (awaited before newPayload)
        let evm_factory = IrysEvmFactory::new(self.chunk_provider, self.hardfork_config)
            .with_chunk_data_index(self.chunk_data_index);
        let evm_config = evm::IrysEvmConfig {
            inner: evm_config,
            assembler: IrysBlockAssembler::new(ctx.chain_spec()),
            executor_factory: evm::IrysBlockExecutorFactory::new(
                RethReceiptBuilder::default(),
                spec,
                evm_factory,
            ),
        };
        Ok(evm_config)
    }
```

- [x] **Step 5: Verify it compiles**

Run: `cargo check -p irys-reth`
Expected: Clean compile.

- [x] **Step 6: Commit**

```bash
git add crates/irys-reth/src/lib.rs
git commit -m "fix(reth): give executor ChunkDataIndex so PD precompile matches payload builder

The executor was using ChunkSource::Storage(MockChunkProvider) which returns
zero-filled chunks, while the payload builder used ChunkSource::Manager with
real chunk data from the DashMap. This caused a gas mismatch during block
validation. Both paths now read from the shared ChunkDataIndex."
```

---

### Task 2: Update the TODO comment in chain.rs

**Files:**
- Modify: `crates/chain/src/chain.rs:1154-1155`

- [x] **Step 1: Replace the outdated TODO comment**

Current (`chain.rs:1154-1155`):
```rust
        // TODO: Use real ChunkProvider (aka PD Chunk Cache) instead of mock
        let mock_provider = irys_types::chunk_provider::MockChunkProvider::new();
```

New:
```rust
        // MockChunkProvider is used only for ChunkConfig extraction by IrysEvmFactory.
        // Actual chunk reads go through ChunkDataIndex (DashMap), not through this provider.
        let mock_provider = irys_types::chunk_provider::MockChunkProvider::new();
```

- [x] **Step 2: Commit**

```bash
git add crates/chain/src/chain.rs
git commit -m "docs(chain): clarify MockChunkProvider role after executor ChunkDataIndex fix"
```

---

### Task 3: Verify the fix

- [x] **Step 1: Run the previously failing integration tests**

Run: `cargo nextest run -p irys-chain-tests heavy_test_pd_content_verification heavy_test_pd_headerless_access_list_reverts heavy_test_pd_multi_tx_single_block heavy_test_pd_single_chunk_read`

Expected: All 5 tests pass (no more gas mismatch).

- [x] **Step 2: Run broader PD test suite**

Run: `cargo nextest run -p irys-chain-tests programmable_data`

Expected: All programmable_data tests pass.

- [x] **Step 3: Run clippy and fmt**

Run: `cargo clippy --workspace --tests --all-targets && cargo fmt --all`

Expected: No new warnings.

---

## Why MockChunkProvider Stays

`IrysEvmFactory::new(chunk_provider, hardfork_config)` creates a base `PdContext::new(chunk_provider)` which extracts `ChunkConfig` via `chunk_provider.config()`. Even when `chunk_data_index` is set (Manager mode), `PdContext::new_with_manager(self.context.chunk_config(), index)` still needs that config.

The MockChunkProvider's config values (256KB chunks, 100 chunks/partition) are adequate because the payload builder already uses the same mock-derived config and produces correct results. Both paths share the same `IrysEvmFactory` lineage, so they get the same `ChunkConfig`.

A future cleanup could extract `ChunkConfig` as a standalone parameter, removing the need for any `RethChunkProvider` in the executor path entirely.

---

## Troubleshooting

### "Gas mismatch persists"

If the gas mismatch continues after this fix, the `ChunkConfig` from `MockChunkProvider` may differ from the real consensus config in a way that affects precompile gas calculation. Diagnostic: compare `MockChunkProvider::config()` output with `ConsensusConfig::testing()` values. If they differ, either update MockChunkProvider or pass a real ChunkProvider (see the storage-module-threading approach in git history of this plan file).

### "Chunk not found in shared index" warnings during validation

This would mean the DashMap was not populated before the executor ran. Check that `ProvisionBlockChunks` is being sent and awaited before `newPayload` in the validation path (`crates/actors/src/block_validation.rs:1471-1507`).
