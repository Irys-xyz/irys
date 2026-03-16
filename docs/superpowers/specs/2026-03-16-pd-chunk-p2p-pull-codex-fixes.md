# PD Chunk P2P Pull — Codex Review Fixes

**Date**: 2026-03-16
**Source**: Codex (gpt-5.4) review of branch `rob/pd-chunk-gossiping-code`
**Status**: Implementation spec

## Fix 1: Wrong HTTP endpoint URL (High)

**Problem**: `fetch_chunk_from_peers` in `pd_service.rs:963` uses `http://{peer}/chunk/ledger/...` but the API server exposes `/v1/chunk/ledger/...`. All HTTP fetch attempts 404.

**Fix**: Change the URL format string from:
```rust
format!("http://{}/chunk/ledger/{}/{}", peer.api, key.ledger, key.offset)
```
to:
```rust
format!("http://{}/v1/chunk/ledger/{}/{}", peer.api, key.ledger, key.offset)
```

**Files**: `crates/actors/src/pd_service.rs` (one line)

## Fix 2: Cache reference leak on dropped oneshot (High)

**Problem**: In `on_fetch_success`, after all chunks arrive for a block, the code sends `Ok(())` on the oneshot and unconditionally inserts into `block_tracker`. If the oneshot receiver was already dropped (validation cancelled), the `send` fails silently, but refs stay pinned forever. The local-hit path at line ~805 properly rolls back on send failure.

**Fix**: Check the `send` result. If it fails, roll back cache references instead of inserting into `block_tracker`:

```rust
let pending = self.pending_blocks.remove(block_hash).unwrap();
if pending.response.send(Ok(())).is_ok() {
    self.block_tracker.insert(*block_hash, pending.all_keys);
} else {
    // Receiver dropped (validation cancelled) — roll back cache references
    for key in &pending.all_keys {
        self.cache.remove_reference(*key, *block_hash);
    }
    self.cache.try_shrink_to_fit();
}
```

**Files**: `crates/actors/src/pd_service.rs` (`on_fetch_success` method)

## Fix 3: CancellationToken masks real validation error (High)

**Problem**: After `tokio::join!`, the result scanning checks `shadow_tx_result` first, then walks other results with `first_invalid`. A real PoA failure triggers `cancel.cancel()`, but the shadow_tx wrapper returns `Err("cancelled: sibling validation task failed")` — which gets checked first and reported as the block's error, masking the real PoA failure.

**Fix**: In the result processing after `tokio::join!`, skip `ValidationCancelled` sentinels when scanning for the real error. The task that triggered cancellation completed with the real error *before* calling `cancel.cancel()`, so it's always in the results.

For `shadow_tx_result`: check if the error message is the cancellation sentinel. If so, don't return it as the primary error — fall through to check other results first.

For the `first_invalid` scan: add a two-pass approach:
1. First pass: find the first `Invalid` that is NOT `ValidationCancelled`
2. If none found (shouldn't happen): fall back to the first `ValidationCancelled`

```rust
// After tokio::join!, process shadow_tx_result:
let shadow_tx_cancelled = shadow_tx_result.as_ref()
    .is_err_and(|e| e.to_string().contains("cancelled: sibling"));

// Only return shadow_tx error if it's NOT a cancellation sentinel
if !shadow_tx_cancelled {
    if let Err(e) = shadow_tx_result {
        return ValidationResult::Invalid(ValidationError::ShadowTransactionInvalid(e.to_string()));
    }
}

// For ValidationResult tasks: skip ValidationCancelled in first_invalid scan
let results = [recall_result, poa_result, seeds_result, commitment_result, data_txs_result];
// First pass: find real errors (not cancellation sentinels)
if let Some(real_error) = results.iter().find(|r| matches!(r, ValidationResult::Invalid(e) if !matches!(e, ValidationError::ValidationCancelled { .. }))) {
    return real_error.clone();
}
// Second pass: if shadow was cancelled too, find any error
if shadow_tx_cancelled {
    if let Some(any_error) = results.iter().find(|r| matches!(r, ValidationResult::Invalid(_))) {
        return any_error.clone();
    }
}
```

**Files**: `crates/actors/src/validation_service/block_validation_task.rs` (result processing after `tokio::join!`)

## Fix 4: JoinError leaves chunks stuck in pending_fetches (Medium)

**Problem**: When a fetch task panics or is cancelled unexpectedly (not via `cancel_fetch_if_no_waiters`), the `JoinError` arm at line ~150 logs but doesn't clean up state. Waiters are stranded in `Fetching` forever.

**Fix**: Extract the `ChunkKey` from the JoinError context (if possible) or handle it generically. Since `JoinError` from a cancelled task doesn't carry the result, we need another approach: when `cancel_fetch_if_no_waiters` aborts a task, the state is already removed *before* the abort. So any `JoinError` reaching the handler means an unexpected panic. For panics, we should:

1. The `PdChunkFetchResult` is lost on panic. We can't know which key failed.
2. Add a wrapper that catches panics and returns them as `PdChunkFetchError`:

```rust
// In the JoinSet spawn, wrap to catch panics:
self.join_set.spawn(async move {
    match std::panic::AssertUnwindSafe(fetch_chunk_from_peers(key, peers, client))
        .catch_unwind()
        .await
    {
        Ok(result) => result,
        Err(_) => PdChunkFetchResult {
            key,
            result: Err(PdChunkFetchError::AllPeersFailed {
                excluded_peers: HashSet::new(),
            }),
        },
    }
});
```

This way, panics are converted to `AllPeersFailed` and handled by the normal retry path. The `JoinError::Cancelled` case (from `abort_handle.abort()`) is already handled because state is removed before abort.

For the `JoinError` arm itself, just log and continue — it should be unreachable after this change, but keep it as defense-in-depth.

**Files**: `crates/actors/src/pd_service.rs` (fetch spawn sites + `on_fetch_done` JoinError arm)

## Fix 5: Integration tests don't prove P2P fetch (Medium)

**Problem**: Tests assert against wrong surfaces and lack byte-level verification.

### Fix 5a: Strengthen precondition

Change from accepting `Err(_)` as "chunk absent" to asserting `Ok(None)` specifically:

```rust
// Before:
assert!(node_b.node_ctx.chunk_provider.get_chunk_by_ledger_offset(...).is_err()
    || node_b.node_ctx.chunk_provider.get_chunk_by_ledger_offset(...).unwrap().is_none());
// After:
let result = node_b.node_ctx.chunk_provider.get_chunk_by_ledger_offset(...);
assert!(matches!(result, Ok(None)), "Node B should have no local chunks, got: {:?}", result);
```

### Fix 5b: Assert against ChunkDataIndex, not chunk_provider

The P2P fetch path populates `ChunkDataIndex` (the `Arc<DashMap<(u32, u64), Arc<Bytes>>>` that the EVM precompile reads from), NOT storage modules. Tests should assert against this DashMap.

Check if `ChunkDataIndex` is accessible from `IrysNodeCtx`. If not, it needs to be exposed (it's created in `chain.rs` and passed to PdService).

```rust
// Assert chunk is in the PD cache (ChunkDataIndex DashMap):
let chunk_data = node_b.node_ctx.chunk_data_index.get(&(ledger_id, offset));
assert!(chunk_data.is_some(), "Chunk should be in PD cache after P2P fetch");
```

### Fix 5c: Add byte-level content verification

In the mempool path test (Test 4), verify the fetched chunk bytes match the original upload:

```rust
let fetched_bytes = chunk_data.unwrap().value().as_ref();
let expected_bytes = &ctx.data_bytes[chunk_start..chunk_end];
assert_eq!(fetched_bytes, expected_bytes, "Fetched chunk data should match original upload");
```

### Fix 5d: Assert ready_pd_txs for mempool tests

Tests 3 and 4 should assert that the PD tx hash is in `ready_pd_txs`:

```rust
assert!(node_b.node_ctx.ready_pd_txs.contains(&tx_hash), "PD tx should be ready after chunk fetch");
```

**Files**: `crates/chain-tests/src/programmable_data/pd_chunk_p2p_pull.rs` (all 4 tests), possibly `crates/chain/src/chain.rs` (expose `ChunkDataIndex` on `IrysNodeCtx` if not already)

## Fix 6: Duplicate block_hash guard tears down original request's refs (Medium)

**Problem**: When a second `ProvisionBlockChunks` arrives for the same `block_hash`, the rollback at line ~836 removes cache references using that `block_hash`. But those refs belong to the first (still in-flight) request.

**Fix**: On duplicate detection, respond `Err` on the second oneshot WITHOUT rolling back cache refs. The first request owns those refs and will clean them up when it completes.

```rust
if self.pending_blocks.contains_key(&block_hash) {
    // Duplicate — first request is still in-flight, don't touch its refs
    let _ = response.send(Err(vec![]));
    return;
}
```

Remove the cache reference rollback that currently follows the duplicate detection.

**Files**: `crates/actors/src/pd_service.rs` (`handle_provision_block_chunks` duplicate guard)
