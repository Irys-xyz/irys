# Stale TxID Promotion Candidate Pruning

## Status
Accepted

## Context
`CachedDataRoot.txid_set` is an append-only collection: txids are added when a data tx enters the mempool, but individual txids are never removed. When a tx is pruned from the mempool (anchor expired or fee-evicted) without ever being included in a block, its txid becomes a dangling reference. The `IngressProof` and `CachedDataRoot` entries persist because the cache service's `prune_data_root_cache` exempts entries with locally-generated ingress proofs.

During block production, `get_publish_txs_and_proofs` iterates all `IngressProofs`, reads each `CachedDataRoot.txid_set`, and calls `get_data_tx_in_parallel_inner` to fetch the tx headers. That function queries both the mempool and `IrysDataTxHeaders` DB table. For a txid that exists in neither, it returned a hard error (`Err("Missing transactions: [...]")`), which propagated up and blocked block production entirely.

The root cause is a lifecycle mismatch: the mempool prunes txs by anchor depth, `CachedDataRoot` prunes by expiry height or block inclusion, and `IngressProof` prunes by proof anchor depth. These three lifecycles don't align, creating a window where a txid is gone from the mempool but still referenced in `CachedDataRoot.txid_set`.

## Decision
A two-layer fix was chosen over either layer alone:

**Layer A (resilience): Tolerate missing txids in `get_data_tx_in_parallel_inner`.**
The function now warns and skips txids found in neither the mempool nor the DB, instead of returning a hard error. A `debug_assert!` is placed on the missing-txid path so that test builds (which compile with debug assertions) panic if this safeguard fires. This makes the condition immediately visible during development while keeping release builds resilient.

The `debug_assert!` was chosen over a pure warn-and-skip because:
- The stale txid condition should never happen once Layer B is active.
- If it does happen, it indicates a new code path creating stale references — a regression that should fail tests, not pass silently.
- Release builds must not crash on stale data; the `debug_assert` compiles out in release mode.

**Layer B (root cause): Prune stale txids from `CachedDataRoot.txid_set` at the source.**
`prune_pending_txs` (the mempool's anchor-expiry pruner) now has a Phase 4 that removes pruned txids from the corresponding `CachedDataRoot.txid_set` entries in the database. This is done in the same function that removes the tx from the mempool, ensuring the cleanup is co-located with the deletion that causes the staleness.

Design choices for Layer B:
- **Prune at the mempool, not the cache service.** The cache service (`prune_ingress_proofs`, `prune_data_root_cache`) lacks mempool access, so it cannot distinguish "txid in mempool but not yet in DB" from "txid gone from both." The mempool service knows exactly which txids it is removing and already has DB access.
- **Group by `data_root` for batch efficiency.** Multiple txids may share the same `data_root`; expired txids are grouped by `data_root` in `prune_pending_txs` before being passed to `prune_cached_data_root_txids`, avoiding redundant DB reads/writes.
- **`retain` instead of delete-if-empty.** When `txid_set` becomes empty, the `CachedDataRoot` entry is left in place rather than deleted. The cache service's existing `prune_data_root_cache` handles full entry deletion on its own schedule, and an empty `txid_set` is harmless (no txids means no publish candidates means no lookup failure).
- **Non-fatal errors.** DB write failures during cleanup are logged as warnings but not propagated. Cache cleanup is best-effort; Layer A provides the safety net.

## Consequences
- Block production no longer crashes when stale txids exist in `CachedDataRoot.txid_set` (Layer A).
- Stale txid references are cleaned up at the source, preventing unbounded accumulation (Layer B).
- The `debug_assert` acts as a regression canary: any future code path that creates stale txid references will fail tests.
- Tests that intentionally inject stale state must catch the `debug_assert` panic (via `tokio::task::spawn` + `JoinError` check) rather than asserting on an `Err` return.
- The mempool service now has a secondary responsibility: maintaining `CachedDataRoot.txid_set` consistency. This is a minor responsibility expansion, but it's well-scoped (one method, called from one place) and co-located with the pruning that causes the inconsistency.

## Source
Branch `fix/promotion-candidate-pruning` — fix: stale txid in CachedDataRoot.txid_set blocks block production
