# Plan: Wire Mock Chunk Provider for End-to-End PD Transactions

## Goal

Make Programmable Data (PD) transactions work end-to-end on a single-node environment using `MockChunkProvider` (zeroed chunks). A PD transaction submitted via RPC should be: detected by the mempool monitor, have its chunks pre-provisioned by `PdChunkManager`, pass the readiness gate in the payload builder, be included in a block, and execute the PD precompile successfully.

## Success Metric

An integration test in `crates/chain/tests/programmable_data/` that:
1. Starts a single `IrysNodeTest` genesis node with Sprite hardfork active
2. Deploys a contract that calls the PD precompile
3. Submits a PD transaction with a valid access list
4. Mines a block containing the PD transaction
5. Verifies the transaction was included and executed without panics
6. Verifies the precompile returned data (256KB of zeros from mock)

---

## Current Architecture

```
User submits PD tx → Mempool → pd_transaction_monitor (100ms poll)
                                    ↓
                              PdChunkMessage::NewTransaction
                                    ↓
                            PdChunkManager (tokio task)
                              - Fetches chunks from storage_provider
                              - Marks tx Ready
                                    ↓
                      Payload builder: CombinedTransactionIterator
                              - Sends IsReady query → gets response
                              - If ready + within budget → include tx
                                    ↓
                            EVM execution (IrysEvm)
                              - PD fee deduction
                              - Calls PD precompile
                              - Precompile reads chunks via PdContext
```

### What Works Today
- `PdChunkMessage` channel creation and `PdChunkManager` spawn (`chain.rs:864-881`)
- Mempool monitor detection of PD transactions (`mempool.rs:482+`)
- `PdChunkManager` provisioning with mock data (returns zeros for any offset)
- PD fee deduction in `IrysEvm::transact_raw()` (`evm.rs:617+`)
- PD precompile dispatch and byte reading (`precompile.rs`, `read_bytes.rs`)
- Access list encoding/decoding (`pd_tx.rs`, `range_specifier.rs`)

### What's Broken
1. **`blocking_recv()` panics in async context** — the payload builder iterator and PdContext both use `tokio::oneshot::blocking_recv()` which panics when called from within a tokio runtime
2. **`PdContext::new_with_manager()` is never called** — EVM always uses direct storage mode, bypassing the PdChunkManager cache entirely

---

## Step-by-Step Implementation

### Step 1: Fix `blocking_recv()` in `CombinedTransactionIterator::next()`

**Problem:** `tokio::sync::oneshot::Receiver::blocking_recv()` panics if called from within an async runtime. The payload builder runs inside reth's async task executor.

**File:** `crates/irys-reth/src/payload.rs`, lines 554-599

**Current code (line 574):**
```rust
if let Ok(ready) = resp_rx.blocking_recv() {
```

**Fix:** Wrap in `tokio::task::block_in_place()` which tells tokio to move the current task off the worker thread, allowing blocking:
```rust
if let Ok(ready) = tokio::task::block_in_place(|| resp_rx.blocking_recv()) {
```

`block_in_place` is the standard pattern for calling blocking code from within a multi-threaded tokio runtime. It moves the blocking operation to a blocking thread while freeing the async worker.

**Verification:** This change alone should prevent the panic. The `IsReady` message is sent to `PdChunkManager`, which responds on the oneshot sender.

---

### Step 2: Fix `blocking_recv()` in `PdContext::get_chunk()`

**Problem:** Same `blocking_recv()` issue, but in the precompile's chunk fetching path. This path is currently dead code (`new_with_manager()` is never called), but will be exercised after Step 4.

**File:** `crates/irys-reth/src/precompiles/pd/context.rs`, lines 128-131

**Current code:**
```rust
let chunk = resp_rx
    .blocking_recv()
    .map_err(|e| eyre::eyre!("Failed to receive chunk response: {}", e))?;
```

**Fix:**
```rust
let chunk = tokio::task::block_in_place(|| resp_rx.blocking_recv())
    .map_err(|e| eyre::eyre!("Failed to receive chunk response: {}", e))?;
```

---

### Step 3: Add `pd_chunk_sender` to `IrysEvmFactory` for Payload Building

**Problem:** Both the block executor and payload builder share the same `IrysEvmConfig` (cloned in reth's `ComponentsBuilder` at `~/.cargo/git/checkouts/reth-irys-*/324bfb8/crates/node/builder/src/components/builder.rs:388-392`). The `IrysEvmFactory` inside always uses `PdContext::new(chunk_provider)` → `ChunkSource::Storage`. There is no way to differentiate the payload building path.

**Design:** Add an optional `pd_chunk_sender` to `IrysEvmFactory`. When present, `create_evm()` uses `PdContext::new_with_manager()` instead of `PdContext::new()`. The executor builder sets it to `None`; the payload builder sets it after receiving the cloned config.

#### 3a. Modify `IrysEvmFactory`

**File:** `crates/irys-reth/src/evm.rs`

**Current struct (lines 337-340):**
```rust
pub struct IrysEvmFactory {
    context: PdContext,
    hardfork_config: Arc<irys_types::hardfork_config::IrysHardforkConfig>,
}
```

**Change to:**
```rust
pub struct IrysEvmFactory {
    context: PdContext,
    hardfork_config: Arc<irys_types::hardfork_config::IrysHardforkConfig>,
    /// When set, creates PdContext in Manager mode for payload building.
    /// When None, uses the Storage-backed PdContext (block validation).
    pd_chunk_sender: Option<irys_types::chunk_provider::PdChunkSender>,
}
```

**Modify `IrysEvmFactory::new()` (lines 343-352):**
```rust
pub fn new(
    chunk_provider: Arc<dyn irys_types::chunk_provider::RethChunkProvider>,
    hardfork_config: Arc<irys_types::hardfork_config::IrysHardforkConfig>,
) -> Self {
    let context = PdContext::new(chunk_provider);
    Self {
        context,
        hardfork_config,
        pd_chunk_sender: None,
    }
}
```

**Add a setter method:**
```rust
/// Configure for payload building mode — chunks fetched from PdChunkManager cache.
pub fn with_pd_chunk_sender(mut self, sender: irys_types::chunk_provider::PdChunkSender) -> Self {
    self.pd_chunk_sender = Some(sender);
    self
}
```

**Modify `create_evm()` (line 408) and `create_evm_with_inspector()` (line 450):**

Where `self.context.clone_for_new_evm()` is called, change to:
```rust
// Use manager-backed context for payload building, storage-backed for validation
let pd_context = match &self.pd_chunk_sender {
    Some(sender) => PdContext::new_with_manager(sender.clone(), self.context.chunk_config()),
    None => self.context.clone_for_new_evm(),
};
```

This requires exposing `chunk_config` from `PdContext`:

**File:** `crates/irys-reth/src/precompiles/pd/context.rs`

Add a public getter:
```rust
pub fn chunk_config(&self) -> ChunkConfig {
    self.chunk_config
}
```

**Update `Clone` for `IrysEvmFactory`:** The derived `Clone` should work since `Option<PdChunkSender>` is `Clone` (PdChunkSender = `UnboundedSender` which is `Clone`). Verify the `Debug` derive also still works — `PdChunkSender` does not implement `Debug`, so the existing manual or derived Debug impl may need updating.

**Note on `new_for_testing()` (line 367):** Leave it as-is (no `pd_chunk_sender`), since tests use the storage path.

#### 3b. Wire `pd_chunk_sender` into `IrysEvmConfig` for Payload Building

The challenge is that reth's `ComponentsBuilder` clones the same `evm_config` to both executor and payload builder (at `builder.rs:388-392`). We cannot change the reth fork code. Instead, we set the `pd_chunk_sender` on the `IrysEvmConfig` clone AFTER it reaches the payload builder.

**File:** `crates/irys-reth/src/payload_builder_builder.rs`

The `build_payload_builder()` method (lines 55-74) receives `evm_config: Evm` as a parameter. We need to modify the `evm_config` clone that the payload builder receives.

**Approach:** Add a method to `IrysEvmConfig` that sets `pd_chunk_sender` on its inner `IrysEvmFactory`:

**File:** `crates/irys-reth/src/evm.rs`

Add to `IrysEvmConfig`:
```rust
impl IrysEvmConfig {
    /// Configure for payload building mode — PD chunks fetched from manager cache.
    pub fn with_pd_chunk_sender(mut self, sender: irys_types::chunk_provider::PdChunkSender) -> Self {
        self.executor_factory = self.executor_factory.with_pd_chunk_sender(sender);
        self
    }
}
```

This requires `IrysBlockExecutorFactory` to also forward the setter:

**File:** `crates/irys-reth/src/evm.rs`

Add to `IrysBlockExecutorFactory` (find its definition — it wraps `IrysEvmFactory`):
```rust
impl IrysBlockExecutorFactory {
    pub fn with_pd_chunk_sender(mut self, sender: irys_types::chunk_provider::PdChunkSender) -> Self {
        self.evm_factory = self.evm_factory.with_pd_chunk_sender(sender);
        self
    }
}
```

**Then in `payload_builder_builder.rs` (line 66), modify `build_payload_builder()`:**

```rust
async fn build_payload_builder(
    self,
    ctx: &BuilderContext<Node>,
    pool: Pool,
    evm_config: Evm,
) -> eyre::Result<Self::PayloadBuilder> {
    let conf = ctx.payload_builder_config();
    let chain = ctx.chain_spec().chain();
    let gas_limit = conf.gas_limit_for(chain);

    // Configure evm_config for payload building — use PdChunkManager for chunk fetching
    let evm_config = evm_config.with_pd_chunk_sender(self.pd_chunk_sender.clone());

    Ok(crate::payload::IrysPayloadBuilder::new(
        ctx.provider().clone(),
        pool,
        evm_config,
        EthereumBuilderConfig::new().with_gas_limit(gas_limit),
        self.max_pd_chunks_per_block,
        self.hardforks,
        self.pd_chunk_sender,
    ))
}
```

**Important:** The `evm_config` passed to the payload builder is already a clone (reth clones it at `builder.rs:392`). So mutating it here does NOT affect the executor's copy. The executor retains `pd_chunk_sender: None` → `ChunkSource::Storage`. The payload builder gets `pd_chunk_sender: Some(...)` → `ChunkSource::Manager`.

---

### Step 4: Write the Integration Test

**File:** `crates/chain/tests/programmable_data/pd_mock_e2e.rs` (new file)

**Pattern to follow:** `crates/chain/tests/programmable_data/pd_chunk_limit.rs`

**Test outline:**

```rust
use irys_chain::IrysNodeCtxFields;
use irys_testing_utils::utils::IrysNodeTest;
use irys_types::config::NodeConfig;
use irys_types::range_specifier::ChunkRangeSpecifier;
use irys_types::U200;
use irys_reth::pd_tx::{build_pd_access_list, prepend_pd_header_v1_to_calldata, PdHeaderV1, sum_pd_chunks_in_access_list};
use alloy_primitives::U256;
use alloy_consensus::TxEip1559;
use alloy_eips::eip2930::AccessList;
use test_log::test;

/// Test that a PD transaction flows end-to-end on a single node with mock chunks:
/// mempool → monitor → PdChunkManager → payload builder → EVM → precompile
#[test_log::test(tokio::test)]
async fn heavy_test_pd_mock_e2e_single_node() -> eyre::Result<()> {
    let seconds_to_wait = 120;

    // 1. Start a single genesis node with Sprite active from genesis
    let config = NodeConfig::testing();
    let genesis_node = IrysNodeTest::new_genesis(config)
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // 2. Create a PD transaction
    //    - Use partition_index=0, offset=0, chunk_count=1 (minimal)
    //    - The access list tells the system which chunks to provision
    let chunk_spec = ChunkRangeSpecifier {
        partition_index: U200::ZERO,
        offset: 0,
        chunk_count: 1,
    };
    let access_list = build_pd_access_list(std::iter::once(chunk_spec));
    let chunks = sum_pd_chunks_in_access_list(&access_list);
    assert_eq!(chunks, 1);

    // 3. Build PD header with fees
    let header = PdHeaderV1 {
        max_priority_fee_per_chunk: U256::from(1_000_000_000_000_000_u64),
        max_base_fee_per_chunk: U256::from(1_000_000_000_000_000_u64),
    };
    let calldata = prepend_pd_header_v1_to_calldata(&header, &[]);

    // 4. Build, sign, and submit the transaction
    //    (Follow the pattern from pd_chunk_limit.rs — create TxEip1559,
    //     sign with LocalSigner, inject via rpc.inject_tx())
    // ... [see pd_chunk_limit.rs lines 60-117 for exact pattern]

    // 5. Wait for the mempool monitor to detect the PD tx (>100ms poll interval)
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // 6. Mine a block
    let (block, _block_eth_payload, _) = genesis_node
        .node_ctx
        .mine_block_without_gossip()
        .await?;

    // 7. Verify the PD transaction was included in the block
    //    Check block.txs contains our tx hash
    assert!(
        block.txs.len() > 0,
        "Block should contain at least the PD transaction"
    );

    // 8. (Optional) Query the tx receipt to verify execution succeeded
    //    Use reth_node_adapter.rpc to call eth_getTransactionReceipt

    Ok(())
}
```

**Note:** The exact transaction construction, signing, and submission follows the established pattern in `pd_chunk_limit.rs`. The key differences are:
- Single node (not two-node setup)
- We expect the tx to be successfully included (not rejected)
- We verify execution succeeded via receipt status

**Register the test module:** Add `mod pd_mock_e2e;` to the appropriate `mod.rs` or test root file in `crates/chain/tests/programmable_data/`.

---

### Step 5: Verify `IrysBlockExecutorFactory` Structure

Before implementing Step 3b, verify the exact structure of `IrysBlockExecutorFactory` — it wraps `IrysEvmFactory` and needs to forward the `with_pd_chunk_sender` call.

**File:** `crates/irys-reth/src/evm.rs`

Find `IrysBlockExecutorFactory` definition and confirm it has an `evm_factory: IrysEvmFactory` field (or similar). The setter needs to reach through to the inner factory.

Also check if `IrysEvmConfig` has direct access to `executor_factory` — the `with_pd_chunk_sender` chain is:
```
IrysEvmConfig → executor_factory: IrysBlockExecutorFactory → evm_factory: IrysEvmFactory → pd_chunk_sender
```

If the fields are private, add setters at each level.

---

## File Change Summary

| File | Change | Lines |
|------|--------|-------|
| `crates/irys-reth/src/payload.rs` | Wrap `blocking_recv()` in `block_in_place()` | ~574 |
| `crates/irys-reth/src/precompiles/pd/context.rs` | Wrap `blocking_recv()` in `block_in_place()`, add `chunk_config()` getter | ~130, new |
| `crates/irys-reth/src/evm.rs` | Add `pd_chunk_sender` field to `IrysEvmFactory`, modify `create_evm`/`create_evm_with_inspector`, add `with_pd_chunk_sender()` to `IrysEvmConfig` and `IrysBlockExecutorFactory` | ~337-462, new methods |
| `crates/irys-reth/src/payload_builder_builder.rs` | Call `evm_config.with_pd_chunk_sender()` in `build_payload_builder()` | ~66 |
| `crates/chain/tests/programmable_data/pd_mock_e2e.rs` | New integration test | new file |

## What This Does NOT Change

- **`chain.rs` MockChunkProvider sites** — left as-is. The mock is sufficient for single-node dev. Wiring the real `ChunkProvider` is a separate follow-up task.
- **`Lock`/`Unlock` messages** — not wired. Not needed for basic flow.
- **`start_provisioning` Ready-on-failure** — not fixed. Mock never fails.
- **`MockChunkProvider::config()` values** — left as-is. Mock returns data for any offset regardless.

## Verification Checklist

- [ ] `cargo check -p irys-reth` passes after Steps 1-3
- [ ] `cargo test -p irys-reth` passes (existing unit tests still work)
- [ ] `cargo nextest run -p irys-chain heavy_test_pd_mock_e2e_single_node` passes
- [ ] No panics from `blocking_recv()` in logs
- [ ] Logs show: "PdChunkManager started", "Starting PD chunk provisioning", "PD chunks ready"
- [ ] Block contains the PD transaction (verified in test assertion)
