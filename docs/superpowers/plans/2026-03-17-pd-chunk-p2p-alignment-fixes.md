# PD Chunk P2P Alignment Fixes — Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix 7 issues found in the PD chunk P2P pull implementation where the code diverges from the spec — dead gossip code, accept-on-trust verification, unused state machine fields, ignored tests, and debug log pollution.

**Architecture:** The PdService (irys-actors) manages chunk fetching with a 4-arm tokio::select! loop. A new `PdChunkFetcher` async trait in irys-types bridges the circular dependency between irys-actors and irys-p2p, allowing PdService to call into GossipClient's HTTP+gossip fetch without a direct crate dependency. Peer attribution on fetch results enables excluding bad peers on retry.

**Tech Stack:** Rust 1.93.0, async-trait, tokio JoinSet + DelayQueue, MDBX (via reth-db)

**Spec:** `docs/superpowers/specs/2026-03-17-pd-chunk-p2p-pull-alignment-fixes.md`

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `crates/types/src/chunk_provider.rs` | Modify | Add `PdChunkFetcher` trait, `PdChunkFetchSuccess`, `PdChunkFetchFailure` types |
| `crates/p2p/src/pd_chunk_fetcher.rs` | Create | `GossipPdChunkFetcher` impl wrapping `GossipClient` |
| `crates/p2p/src/lib.rs` | Modify | Add `pub mod pd_chunk_fetcher;` |
| `crates/p2p/src/gossip_client.rs` | Modify | Change `pull_pd_chunk_from_peers` return to include `SocketAddr` |
| `crates/actors/src/pd_service/fetch.rs` | Modify | Change `excluded_peers` to `SocketAddr`, add `serving_peer` to result |
| `crates/actors/src/pd_service.rs` | Modify | Replace `http_client` with `chunk_fetcher`, implement `generation`, peer exclusion, verification fix, log cleanup |
| `crates/chain/src/chain.rs` | Modify | Wire `GossipPdChunkFetcher` into PdService |
| `crates/chain-tests/src/programmable_data/pd_chunk_p2p_pull.rs` | Modify | Remove `#[ignore]` |

---

## Task 1: Downgrade `[PD_DEBUG]` Logs (Fix 8)

**Files:**
- Modify: `crates/actors/src/pd_service.rs`

- [ ] **Step 1: Replace all `info!("[PD_DEBUG]` with `debug!(`**

In `crates/actors/src/pd_service.rs`, replace every occurrence of `info!("[PD_DEBUG]` with `debug!("` (removing the `[PD_DEBUG] ` prefix from the format string). Affected lines: 152, 155-156, 194-195, 236-237, 276-277, 531-532, 542-543, 553, 557-558, 643-644, 1042-1043, 1053.

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p irys-actors`
Expected: Compiles with no errors

- [ ] **Step 3: Commit**

```bash
git add crates/actors/src/pd_service.rs
git commit -m "fix: downgrade [PD_DEBUG] logs from info! to debug!"
```

---

## Task 2: Add Publish-Only Comments (Fix 7)

**Files:**
- Modify: `crates/actors/src/pd_service.rs`

- [ ] **Step 1: Add comment at `specs_to_keys`**

At `crates/actors/src/pd_service.rs:625`, above `keys.insert(ChunkKey { ledger: 0, offset });`, add:

```rust
// PD is Publish-ledger-only by design — see CLAUDE.md
```

- [ ] **Step 2: Add comment at `resolve_peers_for_chunk`**

At `crates/actors/src/pd_service.rs:525`, above `let publish_ledger_id: u32 = DataLedger::Publish.into();`, add:

```rust
// PD is Publish-ledger-only by design — see CLAUDE.md
```

- [ ] **Step 3: Commit**

```bash
git add crates/actors/src/pd_service.rs
git commit -m "docs: add Publish-only design comments to PD chunk code"
```

---

## Task 3: Implement `generation` Counter Properly (Fix 5)

**Files:**
- Modify: `crates/actors/src/pd_service.rs:31-64` (struct fields)
- Modify: `crates/actors/src/pd_service.rs:84-103` (constructor)
- Modify: `crates/actors/src/pd_service.rs:730-742` (mempool spawn)
- Modify: `crates/actors/src/pd_service.rs:951-963` (block spawn)

- [ ] **Step 1: Add `next_generation` field to PdService struct**

At `crates/actors/src/pd_service.rs`, after `own_miner_address: IrysAddress` (line 63), add:

```rust
    /// Monotonically increasing counter for fetch lifecycle disambiguation.
    next_generation: u64,
```

- [ ] **Step 2: Initialize in constructor**

At `crates/actors/src/pd_service.rs:102`, after `own_miner_address,`, add:

```rust
            next_generation: 0,
```

- [ ] **Step 3: Use counter when creating PdChunkFetchState (mempool path)**

At `crates/actors/src/pd_service.rs:730-742`, before the `self.pending_fetches.insert(` call, add generation increment. Change `generation: 0,` (line 736) to:

```rust
                                    generation: {
                                        let g = self.next_generation;
                                        self.next_generation += 1;
                                        g
                                    },
```

Wait — this is inside an `else` block within a loop borrowing `self`. The increment needs to happen before the insert. Restructure: compute generation before the insert:

Replace lines 730-742:
```rust
                            let generation = self.next_generation;
                            self.next_generation += 1;
                            self.pending_fetches.insert(
                                key,
                                fetch::PdChunkFetchState {
                                    waiting_txs: HashSet::from([tx_hash]),
                                    waiting_blocks: HashSet::new(),
                                    attempt: 0,
                                    generation,
                                    excluded_peers: HashSet::new(),
                                    status: fetch::FetchPhase::Fetching,
                                    abort_handle: Some(abort_handle),
                                    retry_queue_key: None,
                                },
                            );
```

- [ ] **Step 4: Use counter when creating PdChunkFetchState (block validation path)**

At `crates/actors/src/pd_service.rs:951-963`, same pattern. Replace `generation: 0,` (line 957) by computing generation before insert:

```rust
                    let generation = self.next_generation;
                    self.next_generation += 1;
                    self.pending_fetches.insert(
                        *key,
                        fetch::PdChunkFetchState {
                            waiting_txs: HashSet::new(),
                            waiting_blocks: HashSet::from([block_hash]),
                            attempt: 0,
                            generation,
                            excluded_peers: HashSet::new(),
                            status: fetch::FetchPhase::Fetching,
                            abort_handle: Some(abort_handle),
                            retry_queue_key: None,
                        },
                    );
```

- [ ] **Step 5: Also update test_service() constructor**

At `crates/actors/src/pd_service.rs:1184-1205`, in `test_service()`, add `next_generation: 0,` after `own_miner_address: IrysAddress::default(),` (line 1202).

- [ ] **Step 6: Verify it compiles**

Run: `cargo check -p irys-actors --tests`
Expected: Compiles. The existing `on_retry_ready` generation check at line 478 now works correctly.

- [ ] **Step 7: Commit**

```bash
git add crates/actors/src/pd_service.rs
git commit -m "fix: implement generation counter for fetch lifecycle disambiguation"
```

---

## Task 4: Define `PdChunkFetcher` Trait in irys-types (Fix 1+4, Part A)

**Files:**
- Modify: `crates/types/src/chunk_provider.rs:1-89`

- [ ] **Step 1: Add trait and types to chunk_provider.rs**

At `crates/types/src/chunk_provider.rs`, add after the existing imports (line 7) a new import:

```rust
use std::net::SocketAddr;
```

Then after the `ChunkStorageProvider` trait (after line 53), add:

```rust
/// Error returned when a PD chunk fetch fails.
#[derive(Debug)]
pub struct PdChunkFetchFailure {
    pub message: String,
    /// API addresses of peers that were tried and failed.
    pub failed_peers: Vec<SocketAddr>,
}

/// Success result from a PD chunk fetch, including which peer served it.
#[derive(Debug)]
pub struct PdChunkFetchSuccess {
    pub chunk: ChunkFormat,
    /// API address of the peer that served this chunk (for attribution on
    /// verification failure).
    pub serving_peer: SocketAddr,
}

/// Fetches PD chunks from remote peers (HTTP + gossip fallback).
/// Defined in irys-types so irys-actors can use it without depending on irys-p2p.
#[async_trait::async_trait]
pub trait PdChunkFetcher: Send + Sync + 'static {
    /// Fetch a chunk from the given peers. Tries each peer in order.
    /// Returns the chunk and which peer served it, or an error with the list of
    /// failed peer addresses.
    async fn fetch_chunk(
        &self,
        peers: &[crate::PeerAddress],
        ledger: u32,
        offset: u64,
    ) -> Result<PdChunkFetchSuccess, PdChunkFetchFailure>;
}
```

- [ ] **Step 2: Verify irys-types compiles**

Run: `cargo check -p irys-types`
Expected: Compiles (async-trait is already a dependency of irys-types)

- [ ] **Step 3: Commit**

```bash
git add crates/types/src/chunk_provider.rs
git commit -m "feat: add PdChunkFetcher trait to irys-types"
```

---

## Task 5: Update fetch.rs Types (Fix 1+4, Part B)

**Files:**
- Modify: `crates/actors/src/pd_service/fetch.rs`

- [ ] **Step 1: Change `excluded_peers` from `IrysAddress` to `SocketAddr`**

In `crates/actors/src/pd_service/fetch.rs`:

Replace `use irys_types::{ChunkFormat, IrysAddress};` (line 1) with:
```rust
use irys_types::ChunkFormat;
use std::net::SocketAddr;
```

Change `PdChunkFetchError::AllPeersFailed` (lines 19-21) from:
```rust
    AllPeersFailed {
        excluded_peers: HashSet<IrysAddress>,
    },
```
to:
```rust
    AllPeersFailed {
        failed_peers: Vec<SocketAddr>,
    },
```

Add `serving_peer` to `PdChunkFetchResult` (lines 10-13):
```rust
pub(crate) struct PdChunkFetchResult {
    pub key: ChunkKey,
    /// Which peer served the chunk (for attribution on verification failure).
    pub serving_peer: Option<SocketAddr>,
    pub result: Result<ChunkFormat, PdChunkFetchError>,
}
```

Change `PdChunkFetchState::excluded_peers` (line 46) from:
```rust
    pub excluded_peers: HashSet<IrysAddress>,
```
to:
```rust
    pub excluded_peers: HashSet<SocketAddr>,
```

Change `RetryEntry::excluded_peers` (line 60) from:
```rust
    pub excluded_peers: HashSet<IrysAddress>,
```
to:
```rust
    pub excluded_peers: HashSet<SocketAddr>,
```

- [ ] **Step 2: Check it compiles (expect errors in pd_service.rs)**

Run: `cargo check -p irys-actors 2>&1 | head -40`
Expected: Errors in pd_service.rs about missing `serving_peer` field and type mismatches. These will be fixed in Task 6.

- [ ] **Step 3: Commit**

```bash
git add crates/actors/src/pd_service/fetch.rs
git commit -m "refactor: change excluded_peers to SocketAddr, add serving_peer to fetch result"
```

---

## Task 6: Update PdService to Use Trait + Peer Exclusion (Fix 1+4, Part C)

**Files:**
- Modify: `crates/actors/src/pd_service.rs`

This is the largest task. It touches the struct, constructor, all 3 spawn sites, `on_fetch_done`, `on_fetch_success`, `on_fetch_all_peers_failed`, `on_retry_ready`, `resolve_peers_for_chunk`, and the test module.

- [ ] **Step 1: Update imports**

At `crates/actors/src/pd_service.rs`, line 10, add `PdChunkFetcher` to the import:
```rust
use irys_types::chunk_provider::{ChunkStorageProvider, PdChunkFetcher, PdChunkMessage, PdChunkReceiver};
```

Add `use std::net::SocketAddr;` to the imports.

Remove `use irys_types::{..., IrysAddress, ...}` — keep `IrysAddress` only if still used (it is, for `own_miner_address`).

- [ ] **Step 2: Replace `http_client` field with `chunk_fetcher`**

In the `PdService` struct (line 50-51), replace:
```rust
    /// HTTP client for fetching chunks from peers.
    http_client: reqwest::Client,
```
with:
```rust
    /// Fetches PD chunks from remote peers (trait object bridging irys-p2p).
    chunk_fetcher: Arc<dyn PdChunkFetcher>,
```

- [ ] **Step 3: Update `spawn_service` signature**

At line 69-81, change the signature. Replace the current parameters after `ready_pd_txs`:
```rust
        peer_list: PeerList,
        block_tree: BlockTreeReadGuard,
```
to:
```rust
        peer_list: PeerList,
        chunk_fetcher: Arc<dyn PdChunkFetcher>,
        block_tree: BlockTreeReadGuard,
```

In the constructor (line 84-103), replace `http_client: reqwest::Client::new(),` (line 96) with:
```rust
            chunk_fetcher,
```

- [ ] **Step 4: Update `resolve_peers_for_chunk` to accept exclusion set**

At line 523, change the signature from:
```rust
    fn resolve_peers_for_chunk(&self, key: &ChunkKey) -> Vec<PeerAddress> {
```
to:
```rust
    fn resolve_peers_for_chunk(&self, key: &ChunkKey, exclude: &HashSet<SocketAddr>) -> Vec<PeerAddress> {
```

In the body, at the peer push site (line 554), wrap with exclusion check:
```rust
            if !exclude.contains(&peer.address.api) {
                peers.push(peer.address);
            }
```
(replacing the existing bare `peers.push(peer.address);`)

- [ ] **Step 5: Update all `resolve_peers_for_chunk` call sites**

There are 3 call sites. For initial fetches, pass `&HashSet::new()`:

Line 714: `let peers = self.resolve_peers_for_chunk(&key);`
→ `let peers = self.resolve_peers_for_chunk(&key, &HashSet::new());`

Line 934: `let peers = self.resolve_peers_for_chunk(key);`
→ `let peers = self.resolve_peers_for_chunk(key, &HashSet::new());`

Line 489: `let peers = self.resolve_peers_for_chunk(&entry.key);`
→ Update in step 8 (on_retry_ready uses excluded_peers).

- [ ] **Step 6: Update mempool spawn site (lines 714-742)**

Replace the spawn block. Change from `http_client.clone()` / `fetch_chunk_from_peers` to `chunk_fetcher.clone()` / `fetcher.fetch_chunk`:

```rust
                            let peers = self.resolve_peers_for_chunk(&key, &HashSet::new());
                            let fetcher = self.chunk_fetcher.clone();
                            let abort_handle = self.join_set.spawn(async move {
                                match AssertUnwindSafe(async {
                                    match fetcher.fetch_chunk(&peers, key.ledger, key.offset).await {
                                        Ok(success) => fetch::PdChunkFetchResult {
                                            key,
                                            serving_peer: Some(success.serving_peer),
                                            result: Ok(success.chunk),
                                        },
                                        Err(failure) => fetch::PdChunkFetchResult {
                                            key,
                                            serving_peer: None,
                                            result: Err(fetch::PdChunkFetchError::AllPeersFailed {
                                                failed_peers: failure.failed_peers,
                                            }),
                                        },
                                    }
                                })
                                .catch_unwind()
                                .await
                                {
                                    Ok(result) => result,
                                    Err(_panic) => fetch::PdChunkFetchResult {
                                        key,
                                        serving_peer: None,
                                        result: Err(fetch::PdChunkFetchError::AllPeersFailed {
                                            failed_peers: vec![],
                                        }),
                                    },
                                }
                            });
```

- [ ] **Step 7: Update block validation spawn site (lines 934-950)**

Same pattern as Step 6, using `self.chunk_fetcher.clone()`.

- [ ] **Step 8: Update `on_retry_ready` (lines 472-514)**

Replace the spawn block to use `chunk_fetcher` and pass excluded_peers:

```rust
    fn on_retry_ready(&mut self, entry: fetch::RetryEntry) {
        let Some(state) = self.pending_fetches.get_mut(&entry.key) else {
            return;
        };

        if state.generation != entry.generation {
            return;
        }

        if state.waiting_txs.is_empty() && state.waiting_blocks.is_empty() {
            self.pending_fetches.remove(&entry.key);
            return;
        }

        let mut peers = self.resolve_peers_for_chunk(&entry.key, &entry.excluded_peers);
        if peers.is_empty() {
            // All known peers excluded — retry full set (peers may have recovered)
            peers = self.resolve_peers_for_chunk(&entry.key, &HashSet::new());
            if peers.is_empty() {
                self.fail_pending_fetch(&entry.key);
                return;
            }
        }

        let key = entry.key;
        let fetcher = self.chunk_fetcher.clone();
        let abort_handle = self.join_set.spawn(async move {
            match AssertUnwindSafe(async {
                match fetcher.fetch_chunk(&peers, key.ledger, key.offset).await {
                    Ok(success) => fetch::PdChunkFetchResult {
                        key,
                        serving_peer: Some(success.serving_peer),
                        result: Ok(success.chunk),
                    },
                    Err(failure) => fetch::PdChunkFetchResult {
                        key,
                        serving_peer: None,
                        result: Err(fetch::PdChunkFetchError::AllPeersFailed {
                            failed_peers: failure.failed_peers,
                        }),
                    },
                }
            })
            .catch_unwind()
            .await
            {
                Ok(result) => result,
                Err(_panic) => fetch::PdChunkFetchResult {
                    key,
                    serving_peer: None,
                    result: Err(fetch::PdChunkFetchError::AllPeersFailed {
                        failed_peers: vec![],
                    }),
                },
            }
        });

        let state = self
            .pending_fetches
            .get_mut(&entry.key)
            .expect("entry confirmed present above");
        state.status = fetch::FetchPhase::Fetching;
        state.abort_handle = Some(abort_handle);
        state.retry_queue_key = None;
    }
```

- [ ] **Step 9: Update `on_fetch_done` to extract `serving_peer` (lines 151-190)**

```rust
    fn on_fetch_done(&mut self, result: Result<fetch::PdChunkFetchResult, tokio::task::JoinError>) {
        let fetch_result = match result {
            Ok(r) => r,
            Err(join_error) => {
                warn!("PD chunk fetch task panicked or was cancelled: {}", join_error);
                return;
            }
        };

        let key = fetch_result.key;
        let serving_peer = fetch_result.serving_peer;

        match fetch_result.result {
            Ok(chunk_format) => self.on_fetch_success(key, chunk_format, serving_peer),
            Err(fetch::PdChunkFetchError::AllPeersFailed { failed_peers }) => {
                self.on_fetch_all_peers_failed(key, failed_peers);
            }
            Err(fetch::PdChunkFetchError::VerificationFailed) => {
                warn!(?key, "Unexpected VerificationFailed from fetch task");
                self.on_fetch_all_peers_failed(key, vec![]);
            }
        }
    }
```

- [ ] **Step 10: Update `on_fetch_success` signature (lines 192-331)**

Change signature from:
```rust
    fn on_fetch_success(&mut self, key: ChunkKey, chunk_format: irys_types::ChunkFormat) {
```
to:
```rust
    fn on_fetch_success(&mut self, key: ChunkKey, chunk_format: irys_types::ChunkFormat, serving_peer: Option<SocketAddr>) {
```

The data_root mismatch path (lines 225-234) and derivation failure path (lines 241-251) will be updated in Tasks 8 and 9 respectively. For now, just update the signature.

- [ ] **Step 11: Update `on_fetch_all_peers_failed` signature (lines 334-383)**

Change from:
```rust
    fn on_fetch_all_peers_failed(
        &mut self,
        key: ChunkKey,
        excluded_peers: HashSet<irys_types::IrysAddress>,
    ) {
```
to:
```rust
    fn on_fetch_all_peers_failed(
        &mut self,
        key: ChunkKey,
        failed_peers: Vec<SocketAddr>,
    ) {
```

In the body, change line 368 from `excluded_peers: excluded_peers.clone(),` to `excluded_peers: state.excluded_peers.clone(),`.

Before the retry scheduling, accumulate failed peers (before line 362):
```rust
        state.excluded_peers.extend(failed_peers.iter().copied());
```

Change line 375 from `state.excluded_peers = excluded_peers;` to remove it (already accumulated above).

- [ ] **Step 12: Delete `fetch_chunk_from_peers` function (lines 1037-1101)**

Delete the entire standalone `fetch_chunk_from_peers` async fn. It's replaced by the trait.

- [ ] **Step 13: Update test_service() in test module (lines 1184-1205)**

Replace `http_client: reqwest::Client::new(),` (line 1196) with:
```rust
            chunk_fetcher: Arc::new(MockPdChunkFetcher),
```

Add the mock before `test_service()`:
```rust
    struct MockPdChunkFetcher;

    #[async_trait::async_trait]
    impl irys_types::chunk_provider::PdChunkFetcher for MockPdChunkFetcher {
        async fn fetch_chunk(
            &self,
            _peers: &[PeerAddress],
            _ledger: u32,
            _offset: u64,
        ) -> Result<
            irys_types::chunk_provider::PdChunkFetchSuccess,
            irys_types::chunk_provider::PdChunkFetchFailure,
        > {
            Err(irys_types::chunk_provider::PdChunkFetchFailure {
                message: "mock: no peers".to_string(),
                failed_peers: vec![],
            })
        }
    }
```

- [ ] **Step 14: Verify irys-actors compiles**

Run: `cargo check -p irys-actors --tests`
Expected: May fail due to chain.rs — that's OK, fixed in Task 7.

- [ ] **Step 15: Commit**

```bash
git add crates/actors/src/pd_service.rs
git commit -m "feat: replace http_client with PdChunkFetcher trait, implement peer exclusion"
```

---

## Task 7: Implement GossipPdChunkFetcher + Wire chain.rs (Fix 1+4, Part D)

**Files:**
- Create: `crates/p2p/src/pd_chunk_fetcher.rs`
- Modify: `crates/p2p/src/lib.rs`
- Modify: `crates/p2p/src/gossip_client.rs:2222-2332`
- Modify: `crates/chain/src/chain.rs:1849-1861`

- [ ] **Step 1: Modify `pull_pd_chunk_from_peers` to return peer SocketAddr**

At `crates/p2p/src/gossip_client.rs:2222`, change return type from:
```rust
    ) -> Result<ChunkFormat, PeerNetworkError> {
```
to:
```rust
    ) -> Result<(ChunkFormat, std::net::SocketAddr), PeerNetworkError> {
```

At line 2247 (HTTP success), change `return Ok(chunk_format);` to:
```rust
                        return Ok((chunk_format, peer.api));
```

At line 2290 (gossip success), change `return Ok(chunk_format);` to:
```rust
                        return Ok((chunk_format, peer.api));
```

- [ ] **Step 2: Create `crates/p2p/src/pd_chunk_fetcher.rs`**

```rust
use irys_domain::PeerList;
use irys_types::chunk_provider::{PdChunkFetchFailure, PdChunkFetchSuccess, PdChunkFetcher};
use irys_types::PeerAddress;

use crate::gossip_client::GossipClient;

/// Bridges the irys-actors ↔ irys-p2p dependency gap.
/// Wraps GossipClient + PeerList and implements the PdChunkFetcher trait
/// defined in irys-types, so PdService can fetch chunks via HTTP + gossip
/// without directly depending on irys-p2p.
pub struct GossipPdChunkFetcher {
    gossip_client: GossipClient,
    peer_list: PeerList,
}

impl GossipPdChunkFetcher {
    pub fn new(gossip_client: GossipClient, peer_list: PeerList) -> Self {
        Self {
            gossip_client,
            peer_list,
        }
    }
}

#[async_trait::async_trait]
impl PdChunkFetcher for GossipPdChunkFetcher {
    async fn fetch_chunk(
        &self,
        peers: &[PeerAddress],
        ledger: u32,
        offset: u64,
    ) -> Result<PdChunkFetchSuccess, PdChunkFetchFailure> {
        match self
            .gossip_client
            .pull_pd_chunk_from_peers(peers, ledger, offset, &self.peer_list)
            .await
        {
            Ok((chunk, serving_peer)) => Ok(PdChunkFetchSuccess {
                chunk,
                serving_peer,
            }),
            Err(e) => Err(PdChunkFetchFailure {
                message: e.to_string(),
                failed_peers: peers.iter().map(|p| p.api).collect(),
            }),
        }
    }
}
```

- [ ] **Step 3: Add module to `crates/p2p/src/lib.rs`**

After `mod types;` (approximately line 14), add:
```rust
pub mod pd_chunk_fetcher;
```

- [ ] **Step 4: Verify irys-p2p compiles**

Run: `cargo check -p irys-p2p`
Expected: Compiles

- [ ] **Step 5: Wire in chain.rs**

At `crates/chain/src/chain.rs`, before the `PdService::spawn_service` call (line 1849), add:

```rust
        let pd_chunk_fetcher: Arc<dyn irys_types::chunk_provider::PdChunkFetcher> =
            Arc::new(irys_p2p::pd_chunk_fetcher::GossipPdChunkFetcher::new(
                gossip_data_handler.gossip_client.clone(),
                peer_list_guard.clone(),
            ));
```

Note: `gossip_data_handler` is returned from `p2p_service.run()` at line 1761.

Update the `PdService::spawn_service` call to pass `pd_chunk_fetcher` after `peer_list_guard.clone()` and remove the parameters that are no longer needed. The new call should be:

```rust
        let pd_service_handle = irys_actors::pd_service::PdService::spawn_service(
            pd_chunk_rx,
            chunk_provider.clone(),
            runtime_handle.clone(),
            chunk_data_index.clone(),
            ready_pd_txs.clone(),
            peer_list_guard.clone(),
            pd_chunk_fetcher,
            block_tree_guard.clone(),
            block_index_guard.clone(),
            irys_db.clone(),
            config.consensus.num_chunks_in_partition,
            config.node_config.miner_address(),
        );
```

- [ ] **Step 6: Verify full workspace compiles**

Run: `cargo check --workspace --tests`
Expected: Compiles

- [ ] **Step 7: Run existing pd_service unit tests**

Run: `cargo nextest run -p irys-actors pd_service`
Expected: All existing tests pass

- [ ] **Step 8: Commit**

```bash
git add crates/p2p/src/pd_chunk_fetcher.rs crates/p2p/src/lib.rs crates/p2p/src/gossip_client.rs crates/chain/src/chain.rs
git commit -m "feat: implement GossipPdChunkFetcher and wire through chain.rs"
```

---

## Task 8: Wire VerificationFailed for Retry-on-Mismatch (Fix 6)

**Files:**
- Modify: `crates/actors/src/pd_service.rs`

- [ ] **Step 1: Update data_root mismatch path in `on_fetch_success`**

In `on_fetch_success`, find the data_root mismatch block (was lines 225-234). Replace:

```rust
                    warn!(
                        ?key,
                        ?chunk_data_root,
                        ?expected_data_root,
                        "Fetched chunk data_root mismatch — discarding"
                    );
                    // TODO: mark the peer as suspicious and retry from another peer
                    self.fail_pending_fetch(&key);
                    return;
```

with:

```rust
                    warn!(
                        ?key,
                        ?chunk_data_root,
                        ?expected_data_root,
                        ?serving_peer,
                        "Fetched chunk data_root mismatch — excluding peer and retrying"
                    );
                    let failed: Vec<SocketAddr> = serving_peer.into_iter().collect();
                    self.on_fetch_all_peers_failed(key, failed);
                    return;
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p irys-actors`
Expected: Compiles

- [ ] **Step 3: Commit**

```bash
git add crates/actors/src/pd_service.rs
git commit -m "fix: retry from different peers on data_root mismatch instead of permanent failure"
```

---

## Task 9: Reject on Derivation Failure (Fix 2)

**Files:**
- Modify: `crates/actors/src/pd_service.rs`

- [ ] **Step 1: Replace accept-on-trust with rejection**

In `on_fetch_success`, find the derivation error path (was lines 241-251). Replace:

```rust
            Err(e) => {
                // If we cannot derive the expected data_root (e.g., block not yet migrated),
                // log a warning and proceed — the chunk may still be valid.
                warn!(
                    ?key,
                    error = %e,
                    "Could not derive expected data_root for verification — accepting chunk on trust"
                );
                // TODO: Full merkle path + leaf hash verification as fallback
            }
```

with:

```rust
            Err(e) => {
                warn!(
                    ?key,
                    error = %e,
                    "Could not derive expected data_root — rejecting unverifiable chunk \
                     (PD only accesses migrated blocks, so this indicates missing MDBX state)"
                );
                self.fail_pending_fetch(&key);
                return;
            }
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p irys-actors`
Expected: Compiles

- [ ] **Step 3: Commit**

```bash
git add crates/actors/src/pd_service.rs
git commit -m "fix: reject unverifiable chunks instead of accepting on trust"
```

---

## Task 10: Remove `#[ignore]` and Run Integration Tests (Fix 3)

**Files:**
- Modify: `crates/chain-tests/src/programmable_data/pd_chunk_p2p_pull.rs`

- [ ] **Step 1: Remove all `#[ignore]` annotations and stale comments**

Remove `#[ignore = "requires PD chunk P2P pull implementation (Stage 3)"]` from all 4 test functions.

Remove the `#[expect(dead_code, reason = "fields reserved for Stage 3 P2P pull tests")]` attribute from `PdP2pTestContext` (line 31) — with tests no longer ignored, the struct fields are used.

Update the module-level doc comment (lines 9-11) — remove the sentence:
```
Currently all tests are `#[ignore]` because PdService cannot yet fetch chunks from peers (Stage 3).
```

Update the comment block at lines 274-281 to remove the stale "not yet implemented" / `#[ignore]` text.

- [ ] **Step 2: Run clippy on the full workspace**

Run: `cargo clippy --workspace --tests --all-targets 2>&1 | grep "warning:" | head -20`
Expected: No warnings in pd_service or fetch.rs (the `VerificationFailed` variant and `RetryEntry` fields are now either used or will be used by the retry path)

- [ ] **Step 3: Run the integration tests**

Run: `cargo nextest run -p irys-chain-tests test_pd_chunk_p2p`
Expected: Tests either pass or provide clear failure messages to debug.

If tests fail, debug using the @superpowers:systematic-debugging skill. Common modes:
- Timeout → peer resolution returning empty Vec → check `resolve_peers_for_chunk` with tracing
- Connection refused → peer API not ready → add polling wait
- Wrong data → unpacking with wrong entropy → check `ChunkConfig` alignment

- [ ] **Step 4: Commit**

```bash
git add crates/chain-tests/src/programmable_data/pd_chunk_p2p_pull.rs
git commit -m "test: remove #[ignore] from PD chunk P2P pull integration tests"
```

---

## Task 11: Final Verification

- [ ] **Step 1: Run full local checks**

Run: `cargo xtask local-checks`
Expected: All checks pass (fmt, check, clippy, unused-deps, typos)

- [ ] **Step 2: Fix any issues**

If `local-checks` fails, run `cargo xtask local-checks --fix` for auto-fixable issues and manually fix the rest.

- [ ] **Step 3: Run full test suite**

Run: `cargo xtask test`
Expected: No regressions

- [ ] **Step 4: Final commit if needed**

```bash
git commit -am "chore: fix lint issues from alignment fixes"
```
