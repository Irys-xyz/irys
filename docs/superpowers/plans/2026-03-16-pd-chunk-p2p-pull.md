# PD Chunk P2P Pull — Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Enable PdService to fetch missing chunks from peers over the network so that nodes without local storage modules can validate blocks with PD transactions and provision mempool PD transactions.

**Architecture:** PdService gains a JoinSet for async fetch tasks and a DelayQueue for retry backoff. Fetch tasks try the existing public HTTP chunk API first, then fall back to a new gossip PdChunk pull variant. Chunks arrive packed, are unpacked locally, verified against locally-derived `data_root` from MDBX, and inserted into the PD cache. A CancellationToken in `validate_block()` ensures sibling task failures cancel in-flight PD fetches.

**Tech Stack:** Rust 1.93, tokio (JoinSet, select!), tokio_util (DelayQueue, CancellationToken), reqwest, irys_packing (unpack), MDBX (block index, tx headers)

**Spec:** `docs/superpowers/specs/2026-03-13-pd-chunk-p2p-pull-design.md`

---

## Stage Overview

The implementation is split into three stages. Each stage produces a working, testable artifact:

1. **Stage 1 — Integration Tests**: Write 4 failing integration tests that exercise the PD chunk P2P pull flow. Tests fail because PdService can't fetch remotely yet. Validates test harness, node topology, data upload, and PD tx construction.

2. **Stage 2 — Gossip Protocol Changes**: Add wire types, serving handler, gossip client method, and new `get_chunk_for_pd` trait method. After this stage, serving peers can respond to PD chunk requests.

3. **Stage 3 — PdService Fetch Logic & Validation**: Add JoinSet, DelayQueue, fetch tasks, `on_fetch_done`, retry logic, cancellation paths, CancellationToken, and wire everything into `chain.rs`. After this stage, the Stage 1 tests pass.

---

## Chunk 1: Integration Tests (Stage 1)

### File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `crates/chain-tests/src/programmable_data/pd_chunk_p2p_pull.rs` | Create | All 4 PD chunk P2P pull integration tests + shared setup helper |
| `crates/chain-tests/src/programmable_data/mod.rs` | Modify | Add `mod pd_chunk_p2p_pull;` |

### Task 1: Shared Setup Helper and Module Registration

**Files:**
- Create: `crates/chain-tests/src/programmable_data/pd_chunk_p2p_pull.rs`
- Modify: `crates/chain-tests/src/programmable_data/mod.rs`

- [ ] **Step 1: Add module declaration**

In `crates/chain-tests/src/programmable_data/mod.rs`, add:
```rust
mod pd_chunk_p2p_pull;
```

- [ ] **Step 2: Create test file with shared setup helper**

Create `crates/chain-tests/src/programmable_data/pd_chunk_p2p_pull.rs` with the shared setup context struct and helper function. Follow the pattern from `preloaded_chunk_table.rs:55-130`:

```rust
use alloy_primitives::{Address, B256};
use eyre::Result;
use irys_chain::IrysNodeCtx;
use irys_testing_utils::IrysNodeTest;
use irys_types::{
    irys::IrysSigner, config::NodeConfig, DataLedger, NodeMode,
};

/// Context returned by the shared setup helper for all PD P2P pull tests.
struct PdP2pTestContext {
    node_a: IrysNodeTest<IrysNodeCtx>,       // genesis, mining, has chunks
    node_b: IrysNodeTest<IrysNodeCtx>,       // peer, validator-only, no local chunks
    data_start_offset: u64,                   // global ledger offset of first uploaded chunk
    partition_index: u64,                     // data_start_offset / num_chunks_in_partition
    local_offset: u32,                        // data_start_offset % num_chunks_in_partition
    num_chunks_uploaded: u16,                 // total chunks available for tests
    chunk_size: u64,                          // bytes per chunk (32 in tests)
    num_chunks_in_partition: u64,             // partition size config
    pd_signer: IrysSigner,                    // funded account for PD tx submission
    data_signer: IrysSigner,                  // account used for data upload
}

/// Establishes two-node topology: Node A (genesis miner with data) + Node B (validator-only).
/// Uploads real data on Node A, waits for chunk migration, starts Node B, waits for sync.
async fn setup_pd_p2p_test() -> Result<PdP2pTestContext> {
    // Node A: genesis with small chunks, low migration depth
    let mut config = NodeConfig::testing();
    let chunk_size = 32u64;
    let num_chunks_in_partition = 10u64;
    config.consensus.get_mut().chunk_size = chunk_size;
    config.consensus.get_mut().block_migration_depth = 2;
    config.consensus.get_mut().num_chunks_in_partition = num_chunks_in_partition;
    config.consensus.get_mut().num_chunks_in_recall_range = 2;

    let data_signer = IrysSigner::random_signer(config.consensus.chain_id);
    let pd_signer = IrysSigner::random_signer(config.consensus.chain_id);

    // Fund both accounts in genesis
    // ... (follow pattern from preloaded_chunk_table.rs:67-78)

    let node_a = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("NODE_A", 30)
        .await;

    // Upload 16 chunks of real data (16 * 32 = 512 bytes)
    let num_chunks_uploaded = 16u16;
    let data_bytes = vec![0xAB_u8; (num_chunks_uploaded as u64 * chunk_size) as usize];
    // ... post publish data tx, upload chunks via HTTP, mine blocks past migration depth
    // ... wait_for_migrated_txs + sleep(2s) for chunk migration
    // ... record data_start_offset

    // Node B: peer, validator-only, no storage, not mining
    let peer_signer = IrysSigner::random_signer(config.consensus.chain_id);
    let peer_config = node_a.testing_peer_with_signer(&peer_signer);
    let node_b = IrysNodeTest::new(peer_config)
        .start_with_name("NODE_B")
        .await;

    // Sync gate: wait for Node B to sync to Node A's tip (migration-aware)
    // ... wait_until_height_confirmed or equivalent

    let partition_index = data_start_offset / num_chunks_in_partition;
    let local_offset = (data_start_offset % num_chunks_in_partition) as u32;

    Ok(PdP2pTestContext {
        node_a,
        node_b,
        data_start_offset,
        partition_index,
        local_offset,
        num_chunks_uploaded,
        chunk_size,
        num_chunks_in_partition,
        pd_signer,
        data_signer,
    })
}
```

Reference: `@crates/chain-tests/src/programmable_data/preloaded_chunk_table.rs:55-130` for the setup pattern, `@crates/chain-tests/src/programmable_data/pd_content_verification.rs` for multi-node peer validation pattern.

- [ ] **Step 3: Verify it compiles**

Run: `cargo check -p irys-chain-tests --tests`
Expected: Compiles (no tests yet, just setup helper)

- [ ] **Step 4: Commit**

```bash
git add crates/chain-tests/src/programmable_data/pd_chunk_p2p_pull.rs crates/chain-tests/src/programmable_data/mod.rs
git commit -m "test: add PD chunk P2P pull test file with shared setup helper"
```

### Task 2: Test 1 — Happy Path Single PD Transaction

**Files:**
- Modify: `crates/chain-tests/src/programmable_data/pd_chunk_p2p_pull.rs`

- [ ] **Step 1: Write the test**

```rust
/// Node A mines a block with 1 PD tx. Node B validates by fetching chunks from Node A.
#[test_log::test(tokio::test)]
async fn test_pd_chunk_p2p_happy_path() -> Result<()> {
    let ctx = setup_pd_p2p_test().await?;

    // Pre-test invariant: Node B has no local chunks
    assert!(ctx.node_b.node_ctx.chunk_provider
        .get_unpacked_chunk_by_ledger_offset(0, ctx.data_start_offset)
        .unwrap()
        .is_none());

    // Construct PD tx referencing 2 chunks at known offsets
    // ... use create_and_inject_pd_transaction pattern from preloaded_chunk_table.rs
    // Submit to Node A, mine a block

    // Assert Node B validates the block (canonical tips match)
    // Assert PD tx is in the validated block on Node B
    // Assert chunks are in Node B's ChunkDataIndex DashMap

    Ok(())
}
```

Reference: `@crates/chain-tests/src/programmable_data/preloaded_chunk_table.rs:160-250` for PD tx construction and mining pattern. Use `create_and_inject_pd_transaction_with_priority_fee` or manual access list construction with `ChunkRangeSpecifier`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p irys-chain-tests test_pd_chunk_p2p_happy_path`
Expected: FAIL — Node B rejects the block because PD chunks not available locally (current behavior: `"PD chunks not available locally for block validation"`)

- [ ] **Step 3: Commit**

```bash
git add crates/chain-tests/src/programmable_data/pd_chunk_p2p_pull.rs
git commit -m "test: add failing happy path test for PD chunk P2P pull"
```

### Task 3: Test 2 — Multiple PD Transactions in One Block

**Files:**
- Modify: `crates/chain-tests/src/programmable_data/pd_chunk_p2p_pull.rs`

- [ ] **Step 1: Write the test**

```rust
/// Node A mines a block with 3 PD txs referencing different chunk ranges.
/// Node B fetches all 6 chunks and validates.
#[test_log::test(tokio::test)]
async fn test_pd_chunk_p2p_multiple_txs() -> Result<()> {
    let ctx = setup_pd_p2p_test().await?;

    // Pre-test invariant
    // ... assert Node B has no local chunks

    // Construct 3 PD txs: offsets 0-1, 2-3, 4-5 (relative to data_start_offset)
    // Submit all to Node A, mine a block

    // Assert Node B validates, all 3 txs in block, all 6 chunks in ChunkDataIndex
    Ok(())
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p irys-chain-tests test_pd_chunk_p2p_multiple_txs`
Expected: FAIL — same reason as Test 1

- [ ] **Step 3: Commit**

```bash
git commit -am "test: add failing multi-tx test for PD chunk P2P pull"
```

### Task 4: Test 3 — Chunk Deduplication

**Files:**
- Modify: `crates/chain-tests/src/programmable_data/pd_chunk_p2p_pull.rs`

- [ ] **Step 1: Write the test**

```rust
/// PD tx T1 enters Node B's mempool (needs chunk X). Node A mines a block with
/// PD tx T2 also needing chunk X. Single fetch serves both waiters.
#[test_log::test(tokio::test)]
async fn test_pd_chunk_p2p_deduplication() -> Result<()> {
    let ctx = setup_pd_p2p_test().await?;

    // Pre-test invariant
    // ... assert Node B has no local chunks

    // Submit T1 directly to Node B's mempool (referencing chunk at offset X)
    // Submit T2 to Node A, mine a block containing T2
    // Node B receives block, both waiters should be served

    // Assert block validated on Node B
    // Assert T1 is Ready in Node B's provisioning state
    // Assert ready_pd_txs contains T1's hash
    // Assert chunk X in Node B's ChunkDataIndex
    Ok(())
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p irys-chain-tests test_pd_chunk_p2p_deduplication`
Expected: FAIL

- [ ] **Step 3: Commit**

```bash
git commit -am "test: add failing deduplication test for PD chunk P2P pull"
```

### Task 5: Test 4 — Mempool Path

**Files:**
- Modify: `crates/chain-tests/src/programmable_data/pd_chunk_p2p_pull.rs`

- [ ] **Step 1: Write the test**

```rust
/// PD tx enters Node B's mempool. Node B fetches chunks, tx transitions to Ready.
#[test_log::test(tokio::test)]
async fn test_pd_chunk_p2p_mempool_path() -> Result<()> {
    let ctx = setup_pd_p2p_test().await?;

    // Pre-test invariant
    // ... assert Node B has no local chunks

    // Submit PD tx referencing 2-3 chunks directly to Node B's mempool
    // Wait for PdService to fetch and provision

    // Assert ready_pd_txs contains the tx hash
    // Assert chunks in Node B's ChunkDataIndex
    // Assert chunk bytes match original data uploaded to Node A
    Ok(())
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p irys-chain-tests test_pd_chunk_p2p_mempool_path`
Expected: FAIL — PdService logs `"Chunk not found locally. P2P gossip not yet implemented"` and marks tx `PartiallyReady`

- [ ] **Step 3: Commit**

```bash
git commit -am "test: add failing mempool path test for PD chunk P2P pull"
```

---

## Chunk 2: Gossip Protocol Changes (Stage 2)

### File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `crates/types/src/gossip.rs` | Modify | Add `PdChunk` variants to request/response enums |
| `crates/types/src/chunk_provider.rs` | Modify | Add `get_chunk_for_pd` method to `ChunkStorageProvider` trait |
| `crates/domain/src/models/chunk_provider.rs` | Modify | Implement `get_chunk_for_pd` on `ChunkProvider` |
| `crates/p2p/src/gossip_data_handler.rs` | Modify | Handle `PdChunk` in `resolve_data_request` |
| `crates/p2p/src/gossip_client.rs` | Modify | Add `pull_pd_chunk_from_peers` method |
| `crates/p2p/src/gossip_service.rs` | Modify | Pass `ChunkProvider` into `GossipDataHandler` |

### Task 6: Wire Type Additions

**Files:**
- Modify: `crates/types/src/gossip.rs`

- [ ] **Step 1: Add PdChunk to GossipDataRequestV2**

At `crates/types/src/gossip.rs:339` (after the `Transaction(H256)` variant), add:

```rust
    PdChunk(u32, u64),  // (ledger_id, ledger_offset)
```

- [ ] **Step 2: Add to_v1() mapping for PdChunk**

In the `to_v1()` match block (around line 350), add:

```rust
    Self::PdChunk(..) => None,  // V1 peers cannot serve PD chunks
```

- [ ] **Step 3: Add PdChunk to GossipDataV2**

At `crates/types/src/gossip.rs:252` (after the `IngressProof` variant), add:

```rust
    PdChunk(ChunkFormat),
```

Add import for `ChunkFormat` at top of file if not present.

- [ ] **Step 4: Handle PdChunk in any GossipDataV2 match blocks**

Search for exhaustive matches on `GossipDataV2` in the file and add `PdChunk` arms. The `to_v1()` method for `GossipDataV2` should return `None` for `PdChunk` (V1 doesn't support it).

- [ ] **Step 5: Verify it compiles**

Run: `cargo check -p irys-types`
Expected: May have exhaustive match errors in other crates — fix those too.

Run: `cargo check --workspace`
Expected: Compiles. Fix any exhaustive match errors in `crates/p2p/`, `crates/actors/`, etc. by adding `GossipDataV2::PdChunk(_) => { /* TODO: handle PD chunk */ }` placeholder arms.

- [ ] **Step 6: Commit**

```bash
git commit -am "feat: add PdChunk variants to gossip wire types"
```

### Task 7: get_chunk_for_pd Trait Method

**Files:**
- Modify: `crates/types/src/chunk_provider.rs`
- Modify: `crates/domain/src/models/chunk_provider.rs`

- [ ] **Step 1: Add trait method**

In `ChunkStorageProvider` trait at `crates/types/src/chunk_provider.rs:33-43`, add:

```rust
    /// Returns a chunk in the best available format for PD serving:
    /// unpacked from MDBX CachedChunks if available, packed from storage module otherwise.
    /// No on-the-fly unpacking — only returns unpacked if already cached.
    fn get_chunk_for_pd(
        &self,
        ledger: u32,
        ledger_offset: u64,
    ) -> eyre::Result<Option<ChunkFormat>>;
```

Add `use crate::ChunkFormat;` import.

- [ ] **Step 2: Implement on ChunkProvider**

In `crates/domain/src/models/chunk_provider.rs`, implement the method:

```rust
fn get_chunk_for_pd(
    &self,
    ledger: u32,
    ledger_offset: u64,
) -> eyre::Result<Option<ChunkFormat>> {
    // TODO: Check MDBX CachedChunks table for unpacked version first
    // For now, fall back to packed from storage module
    let ledger = DataLedger::try_from(ledger)?;
    match self.get_chunk_by_ledger_offset(ledger, LedgerChunkOffset::from(ledger_offset))? {
        Some(packed) => Ok(Some(ChunkFormat::Packed(packed))),
        None => Ok(None),
    }
}
```

- [ ] **Step 3: Fix any other implementations of ChunkStorageProvider**

Search for other `impl ChunkStorageProvider` blocks (e.g., mock implementations in test code). Add the new method with a reasonable default (return `None` or delegate to packed).

Run: `cargo check --workspace --tests`

- [ ] **Step 4: Commit**

```bash
git commit -am "feat: add get_chunk_for_pd to ChunkStorageProvider trait"
```

### Task 8: Gossip Serving Handler — PdChunk Arm

**Files:**
- Modify: `crates/p2p/src/gossip_data_handler.rs`

- [ ] **Step 1: Add storage_provider field to GossipDataHandler**

At `crates/p2p/src/gossip_data_handler.rs:66-87`, add to the struct:

```rust
    pub storage_provider: Option<Arc<dyn ChunkStorageProvider>>,
```

Add import: `use irys_types::chunk_provider::ChunkStorageProvider;`

- [ ] **Step 2: Handle PdChunk in resolve_data_request**

In the `resolve_data_request` method (around line 886-973), add a match arm:

```rust
GossipDataRequestV2::PdChunk(ledger, offset) => {
    if let Some(ref provider) = self.storage_provider {
        match provider.get_chunk_for_pd(*ledger, *offset) {
            Ok(Some(chunk_format)) => Ok(Some(GossipDataV2::PdChunk(chunk_format))),
            Ok(None) => Ok(None),
            Err(e) => {
                tracing::warn!(ledger, offset, "Error serving PD chunk: {}", e);
                Ok(None)
            }
        }
    } else {
        Ok(None) // No storage provider configured
    }
}
```

- [ ] **Step 3: Update GossipDataHandler construction**

In `crates/p2p/src/gossip_service.rs` (around line 214-230), add `storage_provider: None` to the struct initialization. The actual wiring will happen in Stage 3 (Task in chain.rs).

- [ ] **Step 4: Verify it compiles**

Run: `cargo check -p irys-p2p`
Expected: Compiles

- [ ] **Step 5: Commit**

```bash
git commit -am "feat: handle PdChunk requests in gossip data handler"
```

### Task 9: GossipClient — pull_pd_chunk_from_peers Method

**Files:**
- Modify: `crates/p2p/src/gossip_client.rs`

- [ ] **Step 1: Add pull method**

Following the pattern of `pull_data_from_network` at line 1715, add a new method that accepts caller-supplied peer addresses:

```rust
/// Pull a PD chunk from specific peers (by API address first, gossip fallback).
/// Returns the first successful response.
pub async fn pull_pd_chunk_from_peers(
    &self,
    peers: &[PeerAddress],
    ledger: u32,
    offset: u64,
    peer_list: &PeerList,
) -> Result<ChunkFormat, PeerNetworkError> {
    let request = GossipDataRequestV2::PdChunk(ledger, offset);

    for peer in peers {
        // Try public API first (existing endpoint, always returns packed)
        let api_url = format!(
            "http://{}/v1/chunk/ledger/{}/{}",
            peer.api, ledger, offset
        );
        match reqwest::get(&api_url).await {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(chunk_format) = resp.json::<ChunkFormat>().await {
                    return Ok(chunk_format);
                }
            }
            _ => {} // Fall through to gossip
        }

        // Gossip fallback
        match self.pull_data_sync(&peer.gossip, &request, peer_list).await {
            Ok(Some(GossipDataV2::PdChunk(chunk_format))) => {
                return Ok(chunk_format);
            }
            _ => continue,
        }
    }

    Err(PeerNetworkError::AllPeersFailed)
}
```

Note: The exact implementation depends on existing patterns in `gossip_client.rs`. Reference `pull_data_from_network` (line 1715) and `pull_data_sync` for the gossip pull pattern. Adapt as needed for the actual error types and peer resolution.

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p irys-p2p`

- [ ] **Step 3: Commit**

```bash
git commit -am "feat: add pull_pd_chunk_from_peers to GossipClient"
```

### Task 10: Wire ChunkProvider into GossipDataHandler

**Files:**
- Modify: `crates/p2p/src/gossip_service.rs`
- Modify: `crates/p2p/src/lib.rs` or wherever `P2PService::run` is defined

- [ ] **Step 1: Add storage_provider parameter to P2PService**

The `P2PService` needs to accept an `Option<Arc<dyn ChunkStorageProvider>>` and pass it through to `GossipDataHandler`. Find where `GossipDataHandler` is constructed in `gossip_service.rs:214-230` and replace `storage_provider: None` with the passed-in value.

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p irys-p2p`

- [ ] **Step 3: Commit**

```bash
git commit -am "feat: wire ChunkProvider into GossipDataHandler via P2PService"
```

---

## Chunk 3: PdService Fetch Logic & Validation (Stage 3)

### File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `crates/actors/src/pd_service.rs` | Modify | Add JoinSet, DelayQueue, pending state, fetch spawning, on_fetch_done, on_retry_ready, 4-arm select! |
| `crates/actors/src/pd_service/provisioning.rs` | Modify | Add PartiallyReady→Ready transition callable from on_fetch_done |
| `crates/actors/src/pd_service/fetch.rs` | Create | Fetch task, PdChunkFetchResult, FetchPhase, RetryEntry, PdChunkFetchState, PendingBlockProvision, peer resolution, local data_root derivation |
| `crates/actors/src/validation_service/block_validation_task.rs` | Modify | Add CancellationToken + with_cancel wrapper in validate_block() |
| `crates/chain/src/chain.rs` | Modify | Pass new deps to PdService::spawn_service |

### Task 11: New Types — Fetch State Machine

**Files:**
- Create: `crates/actors/src/pd_service/fetch.rs`
- Modify: `crates/actors/src/pd_service/mod.rs` (if module structure exists, or inline in pd_service.rs)

- [ ] **Step 1: Define fetch types**

```rust
use irys_types::{ChunkFormat, ChunkKey, IrysAddress};
use alloy_primitives::B256;
use std::collections::HashSet;
use tokio::task::AbortHandle;
use tokio_util::time::delay_queue;

/// Result returned by a chunk fetch task in the JoinSet.
pub struct PdChunkFetchResult {
    pub key: ChunkKey,
    pub result: Result<ChunkFormat, PdChunkFetchError>,
}

#[derive(Debug)]
pub enum PdChunkFetchError {
    AllPeersFailed { excluded_peers: HashSet<IrysAddress> },
    VerificationFailed,
}

/// Phase of a single chunk key's fetch lifecycle.
#[derive(Debug)]
pub enum FetchPhase {
    Fetching,
    Backoff,
}

/// Tracks an in-flight or retrying fetch for a single ChunkKey.
pub struct PdChunkFetchState {
    pub waiting_txs: HashSet<B256>,
    pub waiting_blocks: HashSet<B256>,
    pub attempt: u32,
    pub generation: u64,
    pub excluded_peers: HashSet<IrysAddress>,
    pub status: FetchPhase,
    pub abort_handle: Option<AbortHandle>,
    pub retry_queue_key: Option<delay_queue::Key>,
}

/// Entry stored in the DelayQueue for scheduled retries.
pub struct RetryEntry {
    pub key: ChunkKey,
    pub attempt: u32,
    pub generation: u64,
    pub excluded_peers: HashSet<IrysAddress>,
}

/// Tracks a block validation waiting for chunks to be fetched.
pub struct PendingBlockProvision {
    pub remaining_keys: HashSet<ChunkKey>,
    pub all_keys: Vec<ChunkKey>,
    pub response: tokio::sync::oneshot::Sender<Result<(), Vec<(u32, u64)>>>,
}

pub const MAX_CHUNK_FETCH_RETRIES: u32 = 10;
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p irys-actors`

- [ ] **Step 3: Commit**

```bash
git commit -am "feat: add PD chunk fetch state machine types"
```

### Task 12: PdService Struct Additions

**Files:**
- Modify: `crates/actors/src/pd_service.rs`

- [ ] **Step 1: Add new fields to PdService**

Add to the `PdService` struct (line 23-32):

```rust
    // New fields for P2P chunk fetching
    join_set: JoinSet<PdChunkFetchResult>,
    retry_queue: DelayQueue<RetryEntry>,
    pending_fetches: HashMap<ChunkKey, PdChunkFetchState>,
    pending_blocks: HashMap<B256, PendingBlockProvision>,
    http_client: reqwest::Client,
    gossip_client: GossipClient,
    peer_list: PeerList,
    block_tree: BlockTreeReadGuard,
    block_index: BlockIndexReadGuard,
    db: DatabaseProvider,
    num_chunks_in_partition: u64,
    own_miner_address: IrysAddress,
```

- [ ] **Step 2: Update spawn_service signature**

Add the new parameters to `spawn_service()` (line 36-67). Initialize the new fields in the constructor.

- [ ] **Step 3: Verify it compiles**

Run: `cargo check -p irys-actors`
Expected: May fail due to callers not providing new args — that's OK, chain.rs wiring is Task 20.

- [ ] **Step 4: Commit**

```bash
git commit -am "feat: add fetch infrastructure fields to PdService"
```

### Task 13: Modified Event Loop — 4-Arm Select

**Files:**
- Modify: `crates/actors/src/pd_service.rs`

- [ ] **Step 1: Replace the start() method's select! loop**

Replace the existing 2-arm select! (line 69-94) with the 4-arm version:

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

- [ ] **Step 2: Add stub handlers**

```rust
fn on_fetch_done(&mut self, result: Result<PdChunkFetchResult, tokio::task::JoinError>) {
    // TODO: implement in Task 15
    tracing::warn!("on_fetch_done not yet implemented");
}

fn on_retry_ready(&mut self, entry: RetryEntry) {
    // TODO: implement in Task 16
    tracing::warn!("on_retry_ready not yet implemented");
}
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo check -p irys-actors`

- [ ] **Step 4: Commit**

```bash
git commit -am "feat: add 4-arm select loop to PdService with stub handlers"
```

### Task 14: Peer Resolution and Fetch Task

**Files:**
- Modify: `crates/actors/src/pd_service/fetch.rs` (or pd_service.rs)

- [ ] **Step 1: Implement resolve_peers_for_chunk**

```rust
fn resolve_peers_for_chunk(&self, key: &ChunkKey) -> Vec<PeerAddress> {
    let slot_index = key.offset / self.num_chunks_in_partition;
    let ledger_id = DataLedger::Publish;

    let epoch_snapshot = self.block_tree.canonical_epoch_snapshot();
    let assignments = &epoch_snapshot.partition_assignments.data_partitions;

    let mut peers = Vec::new();
    for (_hash, assignment) in assignments.iter() {
        if assignment.ledger_id == Some(ledger_id as u32)
            && assignment.slot_index == Some(slot_index as usize)
            && assignment.miner_address != self.own_miner_address
        {
            if let Some(peer) = self.peer_list.peer_by_mining_address(&assignment.miner_address) {
                peers.push(peer.address.clone());
            }
        }
    }
    peers
}
```

- [ ] **Step 2: Implement fetch task function**

```rust
/// Spawnable fetch task: tries public API first, gossip fallback.
async fn fetch_chunk_from_peers(
    key: ChunkKey,
    peers: Vec<PeerAddress>,
    http_client: reqwest::Client,
    gossip_client: GossipClient,
    peer_list: PeerList,
) -> PdChunkFetchResult {
    for peer in &peers {
        // Try public API first (existing endpoint)
        let api_url = format!(
            "http://{}/v1/chunk/ledger/{}/{}",
            peer.api, key.ledger, key.offset
        );
        if let Ok(resp) = http_client.get(&api_url).send().await {
            if resp.status().is_success() {
                if let Ok(chunk_format) = resp.json::<ChunkFormat>().await {
                    return PdChunkFetchResult { key, result: Ok(chunk_format) };
                }
            }
        }

        // Gossip fallback
        // ... use gossip_client to pull PdChunk variant
    }

    PdChunkFetchResult {
        key,
        result: Err(PdChunkFetchError::AllPeersFailed {
            excluded_peers: peers.iter().map(|p| /* miner address */).collect(),
        }),
    }
}
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo check -p irys-actors`

- [ ] **Step 4: Commit**

```bash
git commit -am "feat: implement peer resolution and fetch task for PD chunks"
```

### Task 15: on_fetch_done Handler

**Files:**
- Modify: `crates/actors/src/pd_service.rs`

- [ ] **Step 1: Implement local data_root derivation**

```rust
/// Derives expected data_root from local MDBX for a given (ledger, offset).
fn derive_expected_data_root(&self, key: &ChunkKey) -> eyre::Result<DataRoot> {
    let block_index = self.block_index.read();
    let bounds = block_index.get_block_bounds(
        DataLedger::Publish,
        LedgerChunkOffset::from(key.offset),
    )?;

    let db_tx = self.db.tx()?;
    let block_header = block_header_by_hash(&db_tx, &bounds.block_hash, false)?
        .ok_or_else(|| eyre::eyre!("Block header not found for height {}", bounds.height))?;

    let tx_ids = block_header
        .get_data_ledger_tx_ids_ordered(DataLedger::Publish)
        .unwrap_or(&[]);

    let mut running_offset = bounds.start_chunk_offset;
    for tx_id in tx_ids {
        let tx_header = tx_header_by_txid(&db_tx, tx_id)?
            .ok_or_else(|| eyre::eyre!("Tx header not found: {}", tx_id))?;
        let num_chunks = tx_header.data_size.div_ceil(self.chunk_size());
        if key.offset < running_offset + num_chunks {
            return Ok(tx_header.data_root);
        }
        running_offset += num_chunks;
    }
    Err(eyre::eyre!("Offset {} not found in block txs", key.offset))
}
```

- [ ] **Step 2: Implement on_fetch_done**

Replace the stub with the full implementation:
- Unpack if packed
- Derive expected data_root locally
- Verify data_root match + data_path + leaf hash
- Insert into cache with per-waiter references
- Notify waiting blocks (respond on oneshot if remaining_keys empty)
- Notify waiting txs (transition to Ready if missing_chunks empty)
- On error: check retry eligibility, schedule via DelayQueue

Reference: Design spec "Data Flow: Block Validation Path" section for the full on_fetch_done pseudocode.

- [ ] **Step 3: Verify it compiles**

Run: `cargo check -p irys-actors`

- [ ] **Step 4: Commit**

```bash
git commit -am "feat: implement on_fetch_done with local data_root verification"
```

### Task 16: on_retry_ready Handler

**Files:**
- Modify: `crates/actors/src/pd_service.rs`

- [ ] **Step 1: Implement on_retry_ready**

```rust
fn on_retry_ready(&mut self, entry: RetryEntry) {
    let Some(state) = self.pending_fetches.get_mut(&entry.key) else { return };

    // Stale generation
    if state.generation != entry.generation { return; }

    // Waiters vanished during backoff
    if state.waiting_txs.is_empty() && state.waiting_blocks.is_empty() {
        self.pending_fetches.remove(&entry.key);
        return;
    }

    // Re-resolve peers
    let peers = self.resolve_peers_for_chunk(&entry.key);
    let abort_handle = self.join_set.spawn(fetch_chunk_from_peers(
        entry.key,
        peers,
        self.http_client.clone(),
        self.gossip_client.clone(),
        self.peer_list.clone(),
    ));

    state.status = FetchPhase::Fetching;
    state.abort_handle = Some(abort_handle);
    state.retry_queue_key = None;
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p irys-actors`

- [ ] **Step 3: Commit**

```bash
git commit -am "feat: implement on_retry_ready for PD chunk fetch retries"
```

### Task 17: Cancellation Paths

**Files:**
- Modify: `crates/actors/src/pd_service.rs`

- [ ] **Step 1: Implement waiter cleanup with eager cancellation**

Add a helper method that checks when all waiters for a key are removed:

```rust
fn cancel_fetch_if_no_waiters(&mut self, key: &ChunkKey) {
    let Some(state) = self.pending_fetches.get(key) else { return };
    if !state.waiting_txs.is_empty() || !state.waiting_blocks.is_empty() {
        return;
    }

    let state = self.pending_fetches.remove(key).unwrap();
    match state.status {
        FetchPhase::Fetching => {
            if let Some(handle) = state.abort_handle {
                handle.abort();
            }
        }
        FetchPhase::Backoff => {
            if let Some(queue_key) = state.retry_queue_key {
                self.retry_queue.remove(&queue_key);
            }
        }
    }
}
```

- [ ] **Step 2: Integrate into existing handle_release_chunks and handle_release_block_chunks**

After removing a tx or block from waiting sets, call `cancel_fetch_if_no_waiters`.

- [ ] **Step 3: Handle dropped oneshot (block validation cancelled)**

In `on_fetch_done`, before sending on the oneshot, check `response.is_closed()`. If closed, clean up the pending block entry.

- [ ] **Step 4: Verify it compiles**

Run: `cargo check -p irys-actors`

- [ ] **Step 5: Commit**

```bash
git commit -am "feat: implement eager cancellation for PD chunk fetches"
```

### Task 18: Refactor handle_provision_chunks to Spawn Fetches

**Files:**
- Modify: `crates/actors/src/pd_service.rs`

- [ ] **Step 1: Modify handle_provision_chunks (mempool path)**

Where the current code logs `"Chunk not found locally. P2P gossip not yet implemented"` (line 225-231), replace with fetch spawning:

```rust
// Instead of just logging, spawn a fetch task
if !missing.is_empty() {
    for key in &missing {
        if self.pending_fetches.contains_key(key) {
            // Already being fetched — just add this tx as a waiter
            self.pending_fetches.get_mut(key).unwrap().waiting_txs.insert(tx_hash);
        } else {
            // Spawn new fetch
            let peers = self.resolve_peers_for_chunk(key);
            let abort_handle = self.join_set.spawn(fetch_chunk_from_peers(
                *key, peers, self.http_client.clone(),
                self.gossip_client.clone(), self.peer_list.clone(),
            ));
            self.pending_fetches.insert(*key, PdChunkFetchState {
                waiting_txs: HashSet::from([tx_hash]),
                waiting_blocks: HashSet::new(),
                attempt: 0,
                generation: 0,
                excluded_peers: HashSet::new(),
                status: FetchPhase::Fetching,
                abort_handle: Some(abort_handle),
                retry_queue_key: None,
            });
        }
    }
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p irys-actors`

- [ ] **Step 3: Commit**

```bash
git commit -am "feat: spawn fetch tasks on cache miss in handle_provision_chunks"
```

### Task 19: Refactor handle_provision_block_chunks to Spawn Fetches

**Files:**
- Modify: `crates/actors/src/pd_service.rs`

- [ ] **Step 1: Modify handle_provision_block_chunks (block validation path)**

Where the current code returns `Err(missing)` when chunks are not found locally (around line 340-363), replace with fetch spawning and oneshot holding:

- If no missing chunks: respond `Ok(())` immediately (existing behavior)
- If missing chunks: store the oneshot in `pending_blocks[block_hash]`, register each missing key in `pending_fetches`, spawn fetch tasks

- [ ] **Step 2: Add duplicate block_hash guard**

Before storing the oneshot, check `pending_blocks.contains_key(&block_hash)`. If duplicate, respond with `Err` on the second oneshot.

- [ ] **Step 3: Verify it compiles**

Run: `cargo check -p irys-actors`

- [ ] **Step 4: Commit**

```bash
git commit -am "feat: spawn fetch tasks on cache miss in handle_provision_block_chunks"
```

### Task 20: CancellationToken in validate_block

**Files:**
- Modify: `crates/actors/src/validation_service/block_validation_task.rs`

- [ ] **Step 1: Add with_cancel helper**

Before the `validate_block` method, add:

```rust
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
```

- [ ] **Step 2: Wrap all 6 tasks in validate_block**

At line 559 (before `tokio::join!`), create the token and wrap each task:

```rust
let cancel = CancellationToken::new();

let recall_task = with_cancel(recall_task, cancel.clone());
let poa_task = with_cancel(poa_task, cancel.clone());
let seeds_validation_task = with_cancel(seeds_validation_task, cancel.clone());
let commitment_ordering_task = with_cancel(commitment_ordering_task, cancel.clone());
let data_txs_validation_task = with_cancel(data_txs_validation_task, cancel.clone());

// shadow_tx_task has a different return type (eyre::Result), wrap separately
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
```

Add import: `use tokio_util::sync::CancellationToken;`

- [ ] **Step 3: Verify it compiles**

Run: `cargo check -p irys-actors`

- [ ] **Step 4: Run existing validation tests**

Run: `cargo nextest run -p irys-chain-tests validation`
Expected: All existing tests pass (CancellationToken is a no-op for valid blocks)

- [ ] **Step 5: Commit**

```bash
git commit -am "feat: add CancellationToken to validate_block for sibling task cancellation"
```

### Task 21: Wire New Deps in chain.rs

**Files:**
- Modify: `crates/chain/src/chain.rs`

- [ ] **Step 1: Pass new deps to PdService::spawn_service**

Find where `PdService::spawn_service` is called in `chain.rs` and add the new parameters:

```rust
let pd_handle = PdService::spawn_service(
    receivers.pd_chunk,
    chunk_provider.clone(),
    runtime_handle.clone(),
    chunk_data_index.clone(),
    ready_pd_txs.clone(),
    // New deps:
    reqwest::Client::new(),
    gossip_client.clone(),
    peer_list.clone(),
    block_tree_guard.clone(),
    block_index_guard.clone(),
    irys_db.clone(),
    config.consensus.num_chunks_in_partition,
    config.mining_address(),
);
```

- [ ] **Step 2: Pass ChunkProvider to P2PService**

Wire the `Arc<ChunkProvider>` into `P2PService` so it reaches `GossipDataHandler`.

- [ ] **Step 3: Verify everything compiles**

Run: `cargo check --workspace`
Expected: Full workspace compiles

- [ ] **Step 4: Commit**

```bash
git commit -am "feat: wire PD chunk fetch deps through chain.rs"
```

### Task 22: Run Integration Tests

- [ ] **Step 1: Run the Stage 1 tests**

Run: `cargo nextest run -p irys-chain-tests test_pd_chunk_p2p`
Expected: All 4 tests PASS (if everything is wired correctly)

- [ ] **Step 2: If tests fail, debug and fix**

Common failure modes:
- Timeout: PD fetch takes too long → check peer resolution, API connectivity
- Wrong data_root: data_root derivation logic error → check offset accumulation
- Unpack failure: wrong packing entropy → check PackedChunk metadata fields
- Oneshot closed: validation cancelled before fetch completes → check CancellationToken logic

- [ ] **Step 3: Run full test suite**

Run: `cargo xtask test`
Expected: No regressions in existing tests

- [ ] **Step 4: Run local checks**

Run: `cargo xtask local-checks --fix`
Expected: Clean (fmt, clippy, unused deps, typos)

- [ ] **Step 5: Final commit**

```bash
git commit -am "feat: PD chunk P2P pull integration tests passing"
```
