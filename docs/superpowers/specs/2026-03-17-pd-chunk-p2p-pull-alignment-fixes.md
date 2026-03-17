# PD Chunk P2P Pull — Alignment Fixes

**Date**: 2026-03-17
**Source**: Joint Claude + Codex review of `rob/pd-chunk-gossiping-code` vs spec
**Validated by**: Codex (gpt-5.4) — see "Codex Validation" section
**Status**: Implementation spec (v2 — post-validation corrections applied)
**Branch**: `rob/pd-chunk-gossiping-code`

## Context

Review of the PD chunk P2P pull implementation revealed 8 issues where the code
diverges from the spec, contains dead code, or has incomplete state machine
plumbing. This spec addresses all of them in a dependency-ordered sequence.

### Key Design Constraint

**PD only accesses chunks from migrated blocks.** A data transaction must first
be promoted (part of the Publish ledger), then the containing block must be
migrated (at least `block_migration_depth` = 6 blocks deep). This means
`derive_expected_data_root()` should always succeed for valid PD chunk requests
— if it fails, the data is genuinely absent from MDBX, not merely "not yet
migrated." This makes strict rejection on derivation failure safe.

## Decisions (from brainstorm + Codex validation)

| # | Issue | Decision |
|---|-------|----------|
| 1 | Gossip fallback dead code | **C**: Move fetch into irys-p2p via trait bridge in irys-types |
| 2 | Verification degrades to trust | **A**: Reject chunk on derivation failure (strict) — safe because PD only accesses migrated blocks |
| 3 | Integration tests `#[ignore]` | **A**: Remove `#[ignore]`, run, see what passes |
| 4 | `excluded_peers` non-functional | **A**: Implement properly (merged into Fix 1 for trait error type) |
| 5 | `generation` never incremented | **Keep & implement** — Codex identified a stale-timer bug if removed |
| 6 | `VerificationFailed` never constructed | **B**: Use it — retry from different peers on mismatch, **with peer attribution** |
| 7 | Hardcoded Publish ledger | No change — PD is Publish-only forever (documented in CLAUDE.md) |
| 8 | `[PD_DEBUG]` log levels | Downgrade to `debug!`/`trace!` |

---

## Codex Validation (2026-03-17)

Codex validated the original spec and flagged 3 issues that are corrected in
this v2:

1. **Fix 2 liveness concern (DISMISSED)**: Codex flagged that
   `derive_expected_data_root` depends on migrated BlockIndex state and could
   fail for recent blocks. However, PD chunks only reference data from blocks
   that have already been migrated (`block_migration_depth` = 6). Strict
   rejection is safe.

2. **Fix 5 stale-timer bug (ACCEPTED)**: Removing `generation` allows a stale
   retry timer to act on a newly-created `pending_fetches` entry for the same
   `ChunkKey`. Lifecycle: key K in pending → all waiters vanish → entry removed
   → new tx needs K → fresh entry → stale timer fires → spawns duplicate fetch
   on new entry. Fix: keep `generation`, implement with a service-level counter.

3. **Fix 6 incomplete peer attribution (ACCEPTED)**: Retrying with empty
   `failed_peers` doesn't exclude the malicious peer. Fix: plumb
   `serving_peer: Option<SocketAddr>` through `PdChunkFetchResult` so the
   data_root mismatch path can exclude the offending peer.

4. **Fix 1 signature (ACCEPTED)**: `PdService` still needs `peer_list` for
   `resolve_peers_for_chunk`. `spawn_service` must take both `peer_list` and
   `chunk_fetcher`.

5. **Implementation order (ACCEPTED)**: Fix 1 and Fix 4 both touch the trait
   error type. Merge them so the trait is defined once with its final signature.

---

## Fix 1+4: Bridge Gossip Fallback via Trait + Implement `excluded_peers` (High)

These fixes are merged because both touch the `PdChunkFetcher` trait definition.
The trait is defined once with its final error type.

### Problem

`irys-actors` cannot depend on `irys-p2p` (the dependency goes the other way:
`irys-p2p` → `irys-actors`). The existing `fetch_chunk_from_peers` in
`pd_service.rs:1037` is HTTP-only. Meanwhile, the complete HTTP+gossip
implementation `GossipClient::pull_pd_chunk_from_peers` at
`gossip_client.rs:2222` is fully written but never called.

Additionally, `excluded_peers` is stored but never read. Retries hit the same
failing peers. The fetch function returns `excluded_peers: HashSet::new()` always.

### Step 1: Define trait and error type in `irys-types`

**File**: `crates/types/src/chunk_provider.rs`

Add after the existing `ChunkStorageProvider` trait:

```rust
use std::net::SocketAddr;

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
/// Defined here in irys-types so irys-actors can use it without depending on irys-p2p.
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

### Step 2: Implement trait in `irys-p2p`

**File**: `crates/p2p/src/pd_chunk_fetcher.rs` (new file)

```rust
use irys_domain::PeerList;
use irys_types::chunk_provider::{PdChunkFetchFailure, PdChunkFetchSuccess, PdChunkFetcher};
use irys_types::PeerAddress;

use crate::gossip_client::GossipClient;

pub struct GossipPdChunkFetcher {
    gossip_client: GossipClient,
    peer_list: PeerList,
}

impl GossipPdChunkFetcher {
    pub fn new(gossip_client: GossipClient, peer_list: PeerList) -> Self {
        Self { gossip_client, peer_list }
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
        // Delegate to existing pull_pd_chunk_from_peers which already has
        // HTTP + gossip fallback. Adapt the result to include peer attribution.
        //
        // NOTE: pull_pd_chunk_from_peers currently returns ChunkFormat without
        // identifying which peer served it. It needs a small change to return
        // the serving peer's SocketAddr. Modify it to track `last_tried_peer`
        // and return it on success.
        //
        // For now, collect failed peers from the iteration:
        let mut failed_peers = Vec::new();
        match self
            .gossip_client
            .pull_pd_chunk_from_peers(peers, ledger, offset, &self.peer_list)
            .await
        {
            Ok((chunk, serving_peer)) => Ok(PdChunkFetchSuccess {
                chunk,
                serving_peer,
            }),
            Err(e) => {
                failed_peers.extend(peers.iter().map(|p| p.api));
                Err(PdChunkFetchFailure {
                    message: e.to_string(),
                    failed_peers,
                })
            }
        }
    }
}
```

**Also modify `pull_pd_chunk_from_peers`** in `gossip_client.rs` to return
`Result<(ChunkFormat, SocketAddr), PeerNetworkError>` instead of
`Result<ChunkFormat, PeerNetworkError>`. On success, return `peer.api` alongside
the chunk. This is a minimal change — add `peer.api` to both the HTTP success
path (line ~2247) and the gossip success path (line ~2290).

### Step 3: Change `excluded_peers` type in fetch.rs

**File**: `crates/actors/src/pd_service/fetch.rs`

```rust
// BEFORE:
use irys_types::IrysAddress;
pub excluded_peers: HashSet<IrysAddress>,  // in PdChunkFetchState and RetryEntry

// AFTER:
use std::net::SocketAddr;
pub excluded_peers: HashSet<SocketAddr>,  // in PdChunkFetchState and RetryEntry
```

Also update `PdChunkFetchError::AllPeersFailed`:
```rust
AllPeersFailed {
    failed_peers: Vec<SocketAddr>,  // was HashSet<IrysAddress>
},
```

Add `serving_peer` to `PdChunkFetchResult`:
```rust
pub struct PdChunkFetchResult {
    pub key: ChunkKey,
    /// Which peer served the chunk (for attribution on verification failure).
    pub serving_peer: Option<SocketAddr>,
    pub result: Result<ChunkFormat, PdChunkFetchError>,
}
```

### Step 4: Update PdService to use trait

**File**: `crates/actors/src/pd_service.rs`

Replace:
```rust
http_client: reqwest::Client,
```
With:
```rust
chunk_fetcher: Arc<dyn PdChunkFetcher>,
```

**Keep `peer_list`** — it is still needed for `resolve_peers_for_chunk`.

Update `spawn_service` signature to take **both** `peer_list: PeerList` and
`chunk_fetcher: Arc<dyn PdChunkFetcher>`. Remove `http_client: reqwest::Client`.

Update all three spawn sites (mempool path ~line 716, block path ~line 934,
retry path ~line 492) to use:

```rust
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

### Step 5: Filter excluded peers in `resolve_peers_for_chunk`

**File**: `crates/actors/src/pd_service.rs`

Change `resolve_peers_for_chunk` to accept an exclusion set:

```rust
fn resolve_peers_for_chunk(
    &self,
    key: &ChunkKey,
    exclude: &HashSet<SocketAddr>,
) -> Vec<PeerAddress> {
    // ... existing logic ...
    // Add filter after the existing assignment match:
    if !exclude.contains(&peer.address.api) {
        peers.push(peer.address);
    }
}
```

Update all call sites to pass `&HashSet::new()` for the initial fetch and
`&entry.excluded_peers` for retries.

### Step 6: Use excluded_peers in `on_retry_ready`

```rust
fn on_retry_ready(&mut self, entry: fetch::RetryEntry) {
    // ... existing guards ...

    let peers = self.resolve_peers_for_chunk(&entry.key, &entry.excluded_peers);
    if peers.is_empty() {
        // All known peers are excluded — re-resolve without exclusions
        // (peers may have recovered)
        let peers = self.resolve_peers_for_chunk(&entry.key, &HashSet::new());
        if peers.is_empty() {
            // No peers at all — fail permanently
            self.fail_pending_fetch(&entry.key);
            return;
        }
    }
    // ... spawn fetch ...
}
```

### Step 7: Accumulate failed peers in `on_fetch_all_peers_failed`

```rust
fn on_fetch_all_peers_failed(&mut self, key: ChunkKey, failed_peers: Vec<SocketAddr>) {
    let Some(state) = self.pending_fetches.get_mut(&key) else { return };
    // Accumulate — don't replace
    state.excluded_peers.extend(failed_peers.iter().copied());
    // ... rest of retry scheduling ...
}
```

### Step 8: Wire in chain.rs

**File**: `crates/chain/src/chain.rs`

At PdService spawn site (~line 1849):

```rust
let pd_chunk_fetcher = Arc::new(irys_p2p::pd_chunk_fetcher::GossipPdChunkFetcher::new(
    gossip_client.clone(),
    peer_list_guard.clone(),
));

let pd_service_handle = PdService::spawn_service(
    pd_chunk_rx,
    chunk_provider.clone(),
    runtime_handle.clone(),
    chunk_data_index.clone(),
    ready_pd_txs.clone(),
    peer_list_guard.clone(),   // kept — used by resolve_peers_for_chunk
    pd_chunk_fetcher,          // NEW — replaces http_client
    block_tree_guard.clone(),
    block_index_guard.clone(),
    irys_db.clone(),
    config.consensus.num_chunks_in_partition,
    config.node_config.miner_address(),
);
```

### Step 9: Delete dead code

- Delete the standalone `fetch_chunk_from_peers` function from `pd_service.rs:1037-1101`
- The `pull_pd_chunk_from_peers` method on `GossipClient` is now called (via
  the trait impl), so it's no longer dead code

### Step 10: Update tests

The `PdService` unit tests at the bottom of `pd_service.rs` (the `#[cfg(test)]`
module) construct `PdService` directly. Add a `MockPdChunkFetcher` that
implements `PdChunkFetcher` for tests:

```rust
struct MockPdChunkFetcher;

#[async_trait::async_trait]
impl PdChunkFetcher for MockPdChunkFetcher {
    async fn fetch_chunk(
        &self,
        _peers: &[PeerAddress],
        _ledger: u32,
        _offset: u64,
    ) -> Result<PdChunkFetchSuccess, PdChunkFetchFailure> {
        Err(PdChunkFetchFailure {
            message: "mock: no peers".to_string(),
            failed_peers: vec![],
        })
    }
}
```

---

## Fix 2: Reject Chunk on Verification Failure (High)

### Problem

`on_fetch_success` at `pd_service.rs:241-251` accepts chunks without
verification when `derive_expected_data_root()` returns `Err`.

### Why strict rejection is safe

PD only accesses chunks from migrated blocks (at least `block_migration_depth`
= 6 blocks deep). The data transaction must be promoted to the Publish ledger
and the containing block must be migrated before any PD tx can reference its
chunks. Therefore `derive_expected_data_root()` should always succeed for
legitimate PD chunk requests. If it fails, the data is genuinely absent from
MDBX — not "not yet migrated."

### Fix

**File**: `crates/actors/src/pd_service.rs`

Replace the `Err(e)` arm in `on_fetch_success` (lines 241-251):

```rust
// BEFORE:
Err(e) => {
    warn!(
        ?key,
        error = %e,
        "Could not derive expected data_root for verification — accepting chunk on trust"
    );
}

// AFTER:
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

---

## Fix 3: Remove `#[ignore]` from Integration Tests (Medium)

### Problem

All 4 tests in `pd_chunk_p2p_pull.rs` have `#[ignore]` with the comment
"requires PD chunk P2P pull implementation (Stage 3)", but Stage 3 is
implemented.

### Fix

**File**: `crates/chain-tests/src/programmable_data/pd_chunk_p2p_pull.rs`

Remove `#[ignore = "..."]` from all 4 test functions:
- `test_pd_chunk_p2p_basic_block_validation` (~line 298)
- `test_pd_chunk_p2p_multi_tx` (~line 416)
- `test_pd_chunk_p2p_deduplication` (~line 529)
- `test_pd_chunk_p2p_mempool_path` (~line 640)

Run: `cargo nextest run -p irys-chain-tests test_pd_chunk_p2p`

If tests fail, debug and fix. Common failure modes:
- Timeout → peer resolution returning empty Vec (check resolve_peers_for_chunk)
- Wrong data → chunk unpacking with wrong entropy params
- Connection refused → peer HTTP server not ready, may need longer sleep or polling

---

## Fix 5: Implement `generation` Properly (Medium)

### Problem

`generation` is always 0 and never incremented. The Codex review identified
that simply removing it creates a stale-timer bug: a retry timer from a previous
lifecycle can act on a newly-created `pending_fetches` entry for the same
`ChunkKey`, spawning a duplicate fetch.

### Fix

Keep `generation` in both `PdChunkFetchState` and `RetryEntry`. Add a
service-level counter to PdService and increment it for each new fetch state.

**File**: `crates/actors/src/pd_service.rs`

Add field to `PdService`:
```rust
/// Monotonically increasing counter for fetch lifecycle disambiguation.
next_generation: u64,
```

Initialize in `spawn_service`:
```rust
next_generation: 0,
```

When creating a new `PdChunkFetchState` (3 sites: mempool path, block path,
retry re-spawn), use and increment the counter:

```rust
let generation = self.next_generation;
self.next_generation += 1;

self.pending_fetches.insert(key, fetch::PdChunkFetchState {
    // ...
    generation,
    // ...
});
```

When constructing `RetryEntry`, use the state's generation:
```rust
let retry_entry = fetch::RetryEntry {
    key,
    attempt: next_attempt,
    generation: state.generation,
    excluded_peers: state.excluded_peers.clone(),
};
```

The existing stale-generation check in `on_retry_ready` now works correctly:
```rust
if state.generation != entry.generation {
    return;  // Stale timer from a previous lifecycle — discard
}
```

---

## Fix 6: Use `VerificationFailed` for Retry-on-Mismatch with Peer Attribution (Medium)

### Problem

`PdChunkFetchError::VerificationFailed` is declared but never constructed. A
data_root mismatch calls `fail_pending_fetch()` directly (permanent failure),
even though the mismatch may be caused by a single Byzantine peer.

### Fix

When data_root doesn't match, exclude the serving peer and retry from different
peers via the normal retry path. The `serving_peer` field added in Fix 1+4
provides the attribution needed.

**File**: `crates/actors/src/pd_service.rs`

Change `on_fetch_done` to extract `serving_peer`:

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
            // Should not be returned by fetch task — verification happens in
            // on_fetch_success. Keep arm for completeness.
            warn!(?key, "Unexpected VerificationFailed from fetch task");
            self.on_fetch_all_peers_failed(key, vec![]);
        }
    }
}
```

Change the data_root mismatch path in `on_fetch_success`:

```rust
fn on_fetch_success(
    &mut self,
    key: ChunkKey,
    chunk_format: ChunkFormat,
    serving_peer: Option<SocketAddr>,
) {
    // ... unpack, extract data_root ...

    match self.derive_expected_data_root(&key) {
        Ok(expected_data_root) => {
            if chunk_data_root != expected_data_root {
                warn!(
                    ?key,
                    ?chunk_data_root,
                    ?expected_data_root,
                    ?serving_peer,
                    "Fetched chunk data_root mismatch — excluding peer and retrying"
                );
                // Exclude the serving peer and retry from different peers
                let failed = serving_peer.into_iter().collect();
                self.on_fetch_all_peers_failed(key, failed);
                return;
            }
        }
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
    }

    // ... rest of success path (cache insert, notify waiters) ...
}
```

---

## Fix 7: No Change (Publish Ledger Hardcoding)

PD operates exclusively on the Publish ledger. This is a permanent design
constraint, now documented in CLAUDE.md. The hardcoded `DataLedger::Publish` in
`specs_to_keys` and `resolve_peers_for_chunk` is correct.

Add a code comment at both sites for clarity:

```rust
// PD is Publish-ledger-only by design — see CLAUDE.md
let publish_ledger_id: u32 = DataLedger::Publish.into();
```

---

## Fix 8: Downgrade `[PD_DEBUG]` Logs (Low)

### Fix

**File**: `crates/actors/src/pd_service.rs`

Bulk replace all `info!("[PD_DEBUG] ...")` calls with `debug!(...)` and remove
the `[PD_DEBUG]` prefix. These are development debugging logs that should not
appear at `info` level in production.

Affected lines (approximate): 152, 156, 194, 237, 277, 531-558, 643-646, 1042,
1053.

Pattern: `info!("[PD_DEBUG]` → `debug!("`

---

## Implementation Order

Execute in this order due to dependencies:

1. **Fix 8** — Log levels (trivial, no deps)
2. **Fix 7** — Add comments (trivial, no deps)
3. **Fix 5** — Implement `generation` properly (service-level counter)
4. **Fix 1+4** — Trait bridge + `excluded_peers` with `SocketAddr` + peer attribution (largest change, defines trait once with final types)
5. **Fix 6** — Wire `VerificationFailed` for retry-on-mismatch with peer exclusion (depends on Fix 1+4 for `serving_peer`)
6. **Fix 2** — Reject on derivation failure (simple, depends on Fix 6 so retry path exists for data_root mismatch)
7. **Fix 3** — Remove `#[ignore]` and run tests (validation gate — do last)

## Verification

After all fixes:

```bash
cargo check --workspace --tests
cargo clippy --workspace --tests --all-targets  # expect 0 warnings in pd_service
cargo nextest run -p irys-chain-tests test_pd_chunk_p2p  # all 4 tests pass
cargo xtask local-checks  # full CI-equivalent
```
