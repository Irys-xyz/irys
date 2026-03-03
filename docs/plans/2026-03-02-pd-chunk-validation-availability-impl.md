# PD Chunk Validation Data Availability — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Ensure the Reth executor has PD chunk data available during `new_payload` block execution, so validators don't falsely reject valid blocks containing PD transactions.

**Architecture:** Route the executor through the existing PD service (same as payload builder) by giving `IrysExecutorBuilder` a `pd_chunk_sender`. The validation service provisions chunks into the PD service cache before calling `submit_payload_to_reth`, and releases them after.

**Tech Stack:** Rust, tokio channels (mpsc/oneshot), reth engine API, irys-actors PD service

---

### Task 1: Make `extract_pd_chunk_specs` Public

Move the function from private in `mempool.rs` to public in `pd_tx.rs` so both mempool and validation can use it.

**Files:**
- Modify: `crates/irys-reth/src/pd_tx.rs` (add function)
- Modify: `crates/irys-reth/src/mempool.rs:455-472` (replace with re-export)

**Step 1: Move function to `pd_tx.rs`**

Add to the end of `crates/irys-reth/src/pd_tx.rs`:

```rust
use irys_types::range_specifier::{ChunkRangeSpecifier, PdAccessListArg};
use irys_types::precompile::PD_PRECOMPILE_ADDRESS;

/// Extracts PD chunk range specifiers from a transaction's access list.
///
/// Returns all `ChunkRangeSpecifier` entries found under the PD precompile address.
/// Invalid encodings are logged as warnings and skipped.
pub fn extract_pd_chunk_specs(
    access_list: &alloy_eips::eip2930::AccessList,
) -> Vec<ChunkRangeSpecifier> {
    access_list
        .0
        .iter()
        .filter(|item| item.address == PD_PRECOMPILE_ADDRESS)
        .flat_map(|item| item.storage_keys.iter())
        .filter_map(|key| match PdAccessListArg::decode(&key.0) {
            Ok(PdAccessListArg::ChunkRead(spec)) => Some(spec),
            Ok(PdAccessListArg::ByteRead(_)) => None,
            Err(e) => {
                tracing::warn!("Invalid PD access list key encoding, skipping: {}", e);
                None
            }
        })
        .collect()
}
```

Note: Check if the imports (`ChunkRangeSpecifier`, `PdAccessListArg`, `PD_PRECOMPILE_ADDRESS`) are already imported at the top of `pd_tx.rs`. If so, don't duplicate them.

**Step 2: Update `mempool.rs` to use the moved function**

In `crates/irys-reth/src/mempool.rs`, replace the private `extract_pd_chunk_specs` function (lines 451-472) with a re-import:

```rust
use crate::pd_tx::extract_pd_chunk_specs;
```

Remove the old function body and the now-unused imports that were only used by it (check carefully — `PdAccessListArg` and `PD_PRECOMPILE_ADDRESS` may be used elsewhere in `mempool.rs`).

**Step 3: Verify compilation**

Run: `cargo check -p irys-reth`
Expected: compiles cleanly

**Step 4: Commit**

```
feat: make extract_pd_chunk_specs public in pd_tx module
```

---

### Task 2: Add Block-Level Provisioning Messages to `PdChunkMessage`

**Files:**
- Modify: `crates/types/src/chunk_provider.rs:42-71`

**Step 1: Add new variants**

Add two new variants to the `PdChunkMessage` enum in `crates/types/src/chunk_provider.rs` (after the `GetChunk` variant, before the closing `}`):

```rust
    /// Provision chunks needed for validating a peer block.
    /// Loads chunks from local storage into cache.
    /// Responds with Ok(()) when all chunks are cached, or Err with list of missing (ledger, offset).
    ProvisionBlockChunks {
        block_hash: B256,
        chunk_specs: Vec<ChunkRangeSpecifier>,
        response: oneshot::Sender<Result<(), Vec<(u32, u64)>>>,
    },
    /// Release chunks provisioned for a block after validation completes.
    ReleaseBlockChunks { block_hash: B256 },
```

**Step 2: Verify compilation**

Run: `cargo check -p irys-types -p irys-actors`
Expected: compiler error in `pd_service.rs` — non-exhaustive match in `handle_message`. This is expected and will be fixed in Task 3.

**Step 3: Commit**

```
feat: add ProvisionBlockChunks and ReleaseBlockChunks to PdChunkMessage
```

---

### Task 3: Implement Block-Level Provisioning Handlers in PD Service

**Files:**
- Modify: `crates/actors/src/pd_service.rs`
- Test: `crates/actors/src/pd_service.rs` (add tests inline or in a test submodule)

**Step 1: Write the failing test for `handle_provision_block_chunks`**

Add a test module or extend existing tests. Create a test in a new file `crates/actors/src/pd_service/tests.rs` (or inline in `pd_service.rs`):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::chunk_provider::MockChunkProvider;
    use irys_types::range_specifier::ChunkRangeSpecifier;
    use tokio::sync::{mpsc, oneshot};

    /// Helper: create a PdService for testing with a mock provider.
    fn test_service() -> (PdService, irys_types::chunk_provider::PdChunkSender) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (_, block_state_rx) = tokio::sync::broadcast::channel(16);
        let provider = Arc::new(MockChunkProvider::new());
        let (_, shutdown) = reth::tasks::shutdown::signal();
        let service = PdService {
            shutdown,
            msg_rx: rx,
            cache: ChunkCache::with_default_capacity(),
            tracker: ProvisioningTracker::new(),
            storage_provider: provider,
            block_state_rx,
            current_height: None,
            block_tracker: HashMap::new(),
        };
        (service, tx)
    }

    #[test]
    fn test_provision_block_chunks_loads_into_cache() {
        let (mut service, _tx) = test_service();
        let block_hash = B256::with_last_byte(0xAA);
        let specs = vec![ChunkRangeSpecifier {
            partition_index: 0,
            offset: 0,
            chunk_count: 3,
        }];

        let (resp_tx, resp_rx) = oneshot::channel();
        service.handle_provision_block_chunks(block_hash, specs, resp_tx);

        let result = resp_rx.blocking_recv().unwrap();
        assert!(result.is_ok(), "All chunks should be available from mock provider");

        // Verify chunks are in cache
        let key0 = ChunkKey { ledger: 0, offset: 0 };
        let key1 = ChunkKey { ledger: 0, offset: 1 };
        let key2 = ChunkKey { ledger: 0, offset: 2 };
        assert!(service.cache.contains(&key0));
        assert!(service.cache.contains(&key1));
        assert!(service.cache.contains(&key2));

        // Verify block is tracked
        assert!(service.block_tracker.contains_key(&block_hash));
    }

    #[test]
    fn test_release_block_chunks_removes_references() {
        let (mut service, _tx) = test_service();
        let block_hash = B256::with_last_byte(0xBB);
        let specs = vec![ChunkRangeSpecifier {
            partition_index: 0,
            offset: 0,
            chunk_count: 2,
        }];

        // Provision
        let (resp_tx, _) = oneshot::channel();
        service.handle_provision_block_chunks(block_hash, specs, resp_tx);

        // Release
        service.handle_release_block_chunks(&block_hash);

        // Block tracker should be empty
        assert!(!service.block_tracker.contains_key(&block_hash));
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p irys-actors -- pd_service::tests::test_provision_block_chunks`
Expected: FAIL — `handle_provision_block_chunks` method does not exist, `block_tracker` field does not exist

**Step 3: Add `block_tracker` field and implement handlers**

In `crates/actors/src/pd_service.rs`:

1. Add import at top:
```rust
use std::collections::HashMap;
```

2. Add field to `PdService` struct (after `current_height`):
```rust
    block_tracker: HashMap<B256, Vec<ChunkKey>>,
```

3. Initialize in `spawn_service` (in the `Self { ... }` block):
```rust
    block_tracker: HashMap::new(),
```

4. Add match arms in `handle_message`:
```rust
            PdChunkMessage::ProvisionBlockChunks {
                block_hash,
                chunk_specs,
                response,
            } => {
                self.handle_provision_block_chunks(block_hash, chunk_specs, response);
            }
            PdChunkMessage::ReleaseBlockChunks { block_hash } => {
                self.handle_release_block_chunks(&block_hash);
            }
```

5. Implement the handlers:

```rust
    /// Provision chunks needed for validating a peer block.
    /// Loads chunks from local storage into cache, pins them with block_hash as reference.
    fn handle_provision_block_chunks(
        &mut self,
        block_hash: B256,
        chunk_specs: Vec<ChunkRangeSpecifier>,
        response: oneshot::Sender<Result<(), Vec<(u32, u64)>>>,
    ) {
        let required_chunks = self.specs_to_keys(&chunk_specs);
        let chunk_keys: Vec<ChunkKey> = required_chunks.iter().copied().collect();

        debug!(
            block_hash = %block_hash,
            total_chunks = chunk_keys.len(),
            "Provisioning PD chunks for block validation"
        );

        let mut missing = Vec::new();

        for key in &chunk_keys {
            if self.cache.contains(key) {
                self.cache.add_reference(key, block_hash);
            } else {
                match self
                    .storage_provider
                    .get_unpacked_chunk_by_ledger_offset(key.ledger, key.offset)
                {
                    Ok(Some(chunk)) => {
                        self.cache.insert(*key, Arc::new(chunk), block_hash);
                    }
                    Ok(None) => {
                        warn!(
                            block_hash = %block_hash,
                            ledger = key.ledger,
                            offset = key.offset,
                            "Chunk not found locally for block validation"
                        );
                        missing.push((key.ledger, key.offset));
                    }
                    Err(e) => {
                        warn!(
                            block_hash = %block_hash,
                            ledger = key.ledger,
                            offset = key.offset,
                            error = %e,
                            "Failed to fetch chunk from storage for block validation"
                        );
                        missing.push((key.ledger, key.offset));
                    }
                }
            }
        }

        self.block_tracker.insert(block_hash, chunk_keys);

        let result = if missing.is_empty() {
            Ok(())
        } else {
            Err(missing)
        };
        let _ = response.send(result);

        debug!(
            block_hash = %block_hash,
            cached_chunks = self.cache.len(),
            "Block chunk provisioning complete"
        );
    }

    /// Release chunks provisioned for a block after validation completes.
    fn handle_release_block_chunks(&mut self, block_hash: &B256) {
        if let Some(chunk_keys) = self.block_tracker.remove(block_hash) {
            for key in &chunk_keys {
                let unreferenced = self.cache.remove_reference(key, block_hash);
                if unreferenced {
                    self.cache.remove(key);
                }
            }
            trace!(
                block_hash = %block_hash,
                released_keys = chunk_keys.len(),
                "Released block validation chunks"
            );
        }
    }
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p irys-actors -- pd_service::tests`
Expected: PASS

**Step 5: Verify full compilation**

Run: `cargo check -p irys-actors`
Expected: compiles cleanly

**Step 6: Commit**

```
feat: implement block-level chunk provisioning in PD service
```

---

### Task 4: Wire `pd_chunk_sender` into `IrysExecutorBuilder`

**Files:**
- Modify: `crates/irys-reth/src/lib.rs:151-154` (components method) and `232-268` (IrysExecutorBuilder)

**Step 1: Add `pd_chunk_sender` field to `IrysExecutorBuilder`**

In `crates/irys-reth/src/lib.rs`, modify the struct at line 232:

```rust
pub struct IrysExecutorBuilder {
    chunk_provider: Arc<dyn irys_types::chunk_provider::RethChunkProvider>,
    hardfork_config: Arc<irys_types::hardfork_config::IrysHardforkConfig>,
    pd_chunk_sender: irys_types::chunk_provider::PdChunkSender,
}
```

Update the Debug impl (line 237-243) to include the new field:

```rust
impl std::fmt::Debug for IrysExecutorBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IrysExecutorBuilder")
            .field("chunk_provider", &"<Arc<dyn RethChunkProvider>>")
            .field("hardfork_config", &self.hardfork_config)
            .field("pd_chunk_sender", &"<sender>")
            .finish()
    }
}
```

**Step 2: Use sender in `build_evm`**

In `build_evm()` (line 253-268), change line 257 from:

```rust
let evm_factory = IrysEvmFactory::new(self.chunk_provider, self.hardfork_config);
```

to:

```rust
let evm_factory = IrysEvmFactory::new(self.chunk_provider, self.hardfork_config)
    .with_pd_chunk_sender(self.pd_chunk_sender);
```

**Step 3: Pass sender in `components()`**

In the `components()` method (line 148-162), update the executor construction at line 151:

```rust
.executor(IrysExecutorBuilder {
    chunk_provider: self.chunk_provider.clone(),
    hardfork_config: self.hardfork_config.clone(),
    pd_chunk_sender: self.pd_chunk_sender.clone(),
})
```

**Step 4: Verify compilation**

Run: `cargo check -p irys-reth -p irys-reth-node-bridge -p irys-chain`
Expected: compiles cleanly (the `IrysEthereumNode` struct already has `pd_chunk_sender` and passes it through `run_node`)

**Step 5: Commit**

```
feat: route executor through PD service for chunk access
```

---

### Task 5: Thread `pd_chunk_sender` Through Validation Service

**Files:**
- Modify: `crates/actors/src/validation_service.rs:79-104` (ValidationServiceInner) and `109-176` (spawn_service)
- Modify: `crates/actors/src/validation_service/block_validation_task.rs:48-54` (BlockValidationTask struct)
- Modify: `crates/chain/src/chain.rs:1645-1658` (spawn call site)

**Step 1: Add `pd_chunk_sender` to `ValidationServiceInner`**

In `crates/actors/src/validation_service.rs`, add to `ValidationServiceInner` struct (after `chain_sync_state` at line 103):

```rust
    /// PD chunk sender for provisioning chunks during block validation
    pub(crate) pd_chunk_sender: irys_types::chunk_provider::PdChunkSender,
```

**Step 2: Add parameter to `spawn_service`**

In `spawn_service` signature (line 109-122), add parameter after `chain_sync_state`:

```rust
    pub fn spawn_service(
        block_index_guard: BlockIndexReadGuard,
        block_tree_guard: BlockTreeReadGuard,
        mempool_guard: MempoolReadGuard,
        vdf_state_readonly: VdfStateReadonly,
        config: &Config,
        service_senders: &ServiceSenders,
        reth_node_adapter: IrysRethNodeAdapter,
        db: DatabaseProvider,
        execution_payload_provider: ExecutionPayloadCache,
        rx: UnboundedReceiver<Traced<ValidationServiceMessage>>,
        runtime_handle: tokio::runtime::Handle,
        chain_sync_state: ChainSyncState,
        pd_chunk_sender: irys_types::chunk_provider::PdChunkSender,
    ) -> (TokioServiceHandle, Arc<AtomicBool>) {
```

Thread it into `ValidationServiceInner` construction (line 141, after `chain_sync_state`):

```rust
                        pd_chunk_sender,
```

**Step 3: Add to `BlockValidationTask`**

In `crates/actors/src/validation_service/block_validation_task.rs`, add field to the struct at line 48:

```rust
pub(super) struct BlockValidationTask {
    pub sealed_block: Arc<SealedBlock>,
    pub service_inner: Arc<ValidationServiceInner>,
    pub block_tree_guard: BlockTreeReadGuard,
    pub skip_vdf_validation: bool,
    pub parent_span: tracing::Span,
}
```

No new field needed here — `BlockValidationTask` already has `service_inner: Arc<ValidationServiceInner>`, and `ValidationServiceInner` now has `pd_chunk_sender`. Access it via `self.service_inner.pd_chunk_sender`.

**Step 4: Update call site in `chain.rs`**

In `crates/chain/src/chain.rs`, at the `ValidationService::spawn_service` call (line 1645), add `pd_chunk_sender` parameter. You'll need to clone the `pd_chunk_tx` (created at line 866) and pass it:

```rust
        let (validation_handle, validation_enabled) = ValidationService::spawn_service(
            block_index_guard.clone(),
            block_tree_guard.clone(),
            mempool_guard.clone(),
            vdf_state_readonly.clone(),
            &config,
            &service_senders,
            reth_node_adapter.clone(),
            irys_db.clone(),
            execution_payload_cache.clone(),
            receivers.validation_service,
            runtime_handle.clone(),
            sync_state.clone(),
            pd_chunk_tx.clone(),
        );
```

Note: `pd_chunk_tx` is the sender created at line 866 (`let (pd_chunk_tx, pd_chunk_rx) = ...`). The variable name in scope at line 1645 may differ — check the `init_services` function signature to see what name the sender is passed as. If it's not directly accessible, you'll need to thread it through `init_services_thread` → `init_services` (similar to how `pd_chunk_rx` is already threaded). Check whether `pd_chunk_tx` is already available in the `init_services` scope, or if it needs to be passed as an additional parameter.

Important: `pd_chunk_tx` is created in the outer scope (line 866) and `pd_chunk_rx` is passed through to `init_services`. You need to also pass `pd_chunk_tx.clone()` through. Look at how `pd_chunk_rx` is threaded and do the same for a cloned sender.

**Step 5: Verify compilation**

Run: `cargo check -p irys-actors -p irys-chain --tests`
Expected: compiles cleanly

**Step 6: Commit**

```
feat: thread pd_chunk_sender through validation service
```

---

### Task 6: Provision Chunks Before `submit_payload_to_reth`

**Files:**
- Modify: `crates/actors/src/block_validation.rs:1318-1466` (`shadow_transactions_are_valid`)
- Modify: `crates/actors/src/validation_service/block_validation_task.rs:436-463` (call site)
- Modify: `crates/actors/src/validation_service/block_validation_task.rs:591-618` (post-submit release)

**Step 1: Add `pd_chunk_sender` parameter to `shadow_transactions_are_valid`**

In `crates/actors/src/block_validation.rs`, add parameter to the function signature at line 1318:

```rust
pub async fn shadow_transactions_are_valid(
    config: &Config,
    block_tree_guard: &BlockTreeReadGuard,
    mempool_guard: &MempoolReadGuard,
    parent_block: &IrysBlockHeader,
    block: &IrysBlockHeader,
    db: &DatabaseProvider,
    payload_provider: ExecutionPayloadCache,
    parent_epoch_snapshot: Arc<EpochSnapshot>,
    parent_ema_snapshot: Arc<EmaSnapshot>,
    current_ema_snapshot: Arc<EmaSnapshot>,
    parent_commitment_snapshot: Arc<CommitmentSnapshot>,
    block_index: BlockIndex,
    transactions: &BlockTransactions,
    pd_chunk_sender: &irys_types::chunk_provider::PdChunkSender,
) -> eyre::Result<ExecutionData> {
```

**Step 2: Collect chunk specs during PD budget validation**

In the PD budget validation section (lines 1431-1466), modify to also collect chunk specs. Add the import at the top of the file:

```rust
use irys_reth::pd_tx::extract_pd_chunk_specs;
```

Modify the loop:

```rust
    // 2.5. Validate PD chunk budget (only if Sprite hardfork is active)
    let block_timestamp = irys_types::UnixTimestamp::from_secs(block_timestamp_sec as u64);
    let mut pd_chunk_specs: Vec<irys_types::range_specifier::ChunkRangeSpecifier> = Vec::new();
    if let Some(max_pd_chunks) = config
        .consensus
        .hardforks
        .max_pd_chunks_per_block_at(block_timestamp)
    {
        let mut total_pd_chunks: u64 = 0;

        for tx in evm_block.body.transactions.iter() {
            let input = tx.input();
            if let Ok(Some(_header)) = detect_and_decode_pd_header(input) {
                if let Some(access_list) = tx.access_list() {
                    let chunks = sum_pd_chunks_in_access_list(access_list);
                    total_pd_chunks = total_pd_chunks.saturating_add(chunks);
                    pd_chunk_specs.extend(extract_pd_chunk_specs(access_list));
                }
            }
        }

        if total_pd_chunks > max_pd_chunks {
            tracing::debug!(
                block_hash = %block.block_hash,
                evm_block_hash = %block.evm_block_hash,
                total_pd_chunks,
                max_pd_chunks,
                "Rejecting block: exceeds maximum PD chunks per block",
            );
            eyre::bail!(
                "Block exceeds maximum PD chunks per block: {} > {}",
                total_pd_chunks,
                max_pd_chunks
            );
        }
    }
```

**Step 3: Provision chunks before returning execution data**

After the budget validation and before the shadow transaction extraction (before the line `// 3. Extract shadow transactions`), add:

```rust
    // 2.6. Provision PD chunks into PD service cache for block execution
    if !pd_chunk_specs.is_empty() {
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        pd_chunk_sender
            .send(irys_types::chunk_provider::PdChunkMessage::ProvisionBlockChunks {
                block_hash: block.block_hash,
                chunk_specs: pd_chunk_specs,
                response: resp_tx,
            })
            .map_err(|_| eyre::eyre!("PD service channel closed"))?;

        match resp_rx.await.map_err(|_| eyre::eyre!("PD service response channel dropped"))? {
            Ok(()) => {
                tracing::debug!(
                    block_hash = %block.block_hash,
                    "PD chunks provisioned for block validation"
                );
            }
            Err(missing) => {
                eyre::bail!(
                    "Block {} references {} PD chunks not available locally",
                    block.block_hash,
                    missing.len()
                );
            }
        }
    }
```

**Step 4: Update the call site in `block_validation_task.rs`**

In `crates/actors/src/validation_service/block_validation_task.rs`, update the `shadow_transactions_are_valid` call at line 436-450 to pass the new parameter:

```rust
            shadow_transactions_are_valid(
                config,
                &self.block_tree_guard,
                &self.service_inner.mempool_guard,
                &parent_block,
                block,
                &self.service_inner.db,
                self.service_inner.execution_payload_provider.clone(),
                parent_epoch_snapshot,
                parent_ema_snapshot,
                current_ema_snapshot,
                parent_commitment_snapshot,
                block_index,
                sealed_block_for_shadow.transactions(),
                &self.service_inner.pd_chunk_sender,
            )
```

**Step 5: Add release after `submit_payload_to_reth`**

In `block_validation_task.rs`, after the `submit_payload_to_reth` call (around lines 595-618), add a release that fires regardless of success/failure. Modify the section to:

```rust
            (
                ValidationResult::Valid,
                ValidationResult::Valid,
                ValidationResult::Valid,
                ValidationResult::Valid,
                ValidationResult::Valid,
            ) => {
                tracing::debug!("All consensus validations successful, submitting to reth");

                let reth_result = submit_payload_to_reth(
                    self.sealed_block.header(),
                    &self.service_inner.reth_node_adapter,
                    execution_data,
                )
                .instrument(tracing::error_span!(
                    "reth_submission",
                    block.hash = %self.sealed_block.header().block_hash,
                    block.height = %self.sealed_block.header().height
                ))
                .await;

                // Release PD chunks provisioned for this block (fire-and-forget)
                let _ = self.service_inner.pd_chunk_sender.send(
                    irys_types::chunk_provider::PdChunkMessage::ReleaseBlockChunks {
                        block_hash: self.sealed_block.header().block_hash,
                    },
                );

                match reth_result {
                    Ok(()) => {
                        tracing::debug!("Reth execution layer validation successful");
                        ValidationResult::Valid
                    }
                    Err(err) => {
                        tracing::error!(custom.error = ?err, "Reth execution layer validation failed");
                        ValidationResult::Invalid(ValidationError::ExecutionLayerFailed(
                            err.to_string(),
                        ))
                    }
                }
            }
```

Also add release in the failure path (the `_ =>` arm around line 620) since `shadow_transactions_are_valid` may have provisioned chunks even if other validations failed:

```rust
            _ => {
                tracing::debug!("Consensus validation failed, not submitting to reth");

                // Release any PD chunks that may have been provisioned
                let _ = self.service_inner.pd_chunk_sender.send(
                    irys_types::chunk_provider::PdChunkMessage::ReleaseBlockChunks {
                        block_hash: self.sealed_block.header().block_hash,
                    },
                );

                // ... rest of existing failure handling
```

**Step 6: Verify compilation**

Run: `cargo check -p irys-actors -p irys-chain --tests`
Expected: compiles cleanly

**Step 7: Commit**

```
feat: provision PD chunks before block validation execution
```

---

### Task 7: Fix Reth Node to Use Real ChunkProvider (Optional Cleanup)

There's a TODO at `crates/chain/src/chain.rs:1220-1221`:
```rust
// TODO: Use real ChunkProvider (aka PD Chunk Cache) instead of mock
let mock_provider = irys_types::chunk_provider::MockChunkProvider::new();
```

This means the executor's `IrysEvmFactory` receives a `MockChunkProvider` which returns zero-filled chunks. With the PD service routing (Task 4), the executor will use `ChunkSource::Manager` (PD service) instead of `ChunkSource::Storage`, so the mock provider is less critical. However, for correctness if the PD service is unavailable, we should pass the real provider.

**Files:**
- Modify: `crates/chain/src/chain.rs:1207-1230` (`start_reth_thread`)

**Step 1: Thread real `chunk_provider` to `start_reth_thread`**

Add `chunk_provider: Arc<dyn RethChunkProvider>` parameter to `start_reth_thread` and replace the mock:

In the function at line 1196, add the parameter. Then at the call site (check where `start_reth_thread` is called), pass the real `chunk_provider`.

Note: The real `chunk_provider` is created inside `init_services` (around line 1780 of `chain.rs`). However, `start_reth_thread` is called from a different code path. If the real provider isn't available at that point, skip this task for now — the PD service routing makes the mock provider a fallback-only path.

**Step 2: Evaluate whether to skip**

If threading the real provider requires significant refactoring of the startup order, skip this task. The executor now routes through PD service (ChunkSource::Manager), so the fallback to ChunkSource::Storage with MockProvider only matters if the PD service channel is broken.

**Step 3: Commit (if changes made)**

```
chore: replace mock ChunkProvider with real provider in reth node
```

---

### Task 8: Run Full Checks

**Step 1: Format**

Run: `cargo fmt --all`

**Step 2: Clippy**

Run: `cargo clippy --workspace --tests --all-targets`
Fix any warnings.

**Step 3: Test**

Run: `cargo xtask test`
Verify no regressions.

**Step 4: Commit any fixes**

```
chore: fix clippy warnings and formatting
```

---

## Summary of File Changes

| File | Task | Change |
|------|------|--------|
| `crates/irys-reth/src/pd_tx.rs` | 1 | Add `extract_pd_chunk_specs` (public) |
| `crates/irys-reth/src/mempool.rs` | 1 | Replace private fn with re-import |
| `crates/types/src/chunk_provider.rs` | 2 | Add `ProvisionBlockChunks`, `ReleaseBlockChunks` variants |
| `crates/actors/src/pd_service.rs` | 3 | Add `block_tracker`, `handle_provision_block_chunks`, `handle_release_block_chunks` |
| `crates/irys-reth/src/lib.rs` | 4 | Add `pd_chunk_sender` to `IrysExecutorBuilder`, wire in `components()` and `build_evm()` |
| `crates/actors/src/validation_service.rs` | 5 | Add `pd_chunk_sender` to `ValidationServiceInner` and `spawn_service` |
| `crates/actors/src/validation_service/block_validation_task.rs` | 5, 6 | Pass sender to `shadow_transactions_are_valid`, add release after submit |
| `crates/actors/src/block_validation.rs` | 6 | Add `pd_chunk_sender` param, collect specs, provision before return |
| `crates/chain/src/chain.rs` | 5, 7 | Pass `pd_chunk_sender` to validation service, optionally fix mock provider |
