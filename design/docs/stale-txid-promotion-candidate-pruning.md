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
The function now returns a `TxLookupResult { found, missing }` instead of a hard error when txids are absent from both the mempool and the DB. The caller (the tx-selector path in `get_publish_txs_and_proofs`) owns the error policy: it warns on any non-empty `missing` set and places a `debug_assert!` on that path so that test builds (which compile with debug assertions) panic if this safeguard fires. This makes the condition immediately visible during development while keeping release builds resilient.

The `debug_assert!` was chosen over a pure warn-and-skip because:
- Layer B eliminates the known path that caused stale txids, reducing the likelihood of this condition; however, pre-existing stale rows or uncovered removal paths may still trigger the assert until a forthcoming cache-integrity validation pass lands.
- If it does happen, it indicates a new code path creating stale references — a regression that should fail tests, not pass silently.
- Release builds must not crash on stale data; the `debug_assert` compiles out in release mode.

**Layer B (root cause): Prune stale txids from `CachedDataRoot.txid_set` via message to the cache service.**
When `prune_pending_txs` (the mempool's anchor-expiry pruner) removes expired txs, it sends a `PruneTxidsFromCachedDataRoots` message to the cache service via the existing `CacheServiceAction` channel. The cache service — which owns `CachedDataRoots` — performs the actual DB writes, removing the specified txids from each entry's `txid_set`.

Design choices for Layer B:
- **Detect in the mempool, execute in the cache service.** The mempool knows exactly which txids are being pruned and their `data_root`s — information the cache service cannot derive on its own (it lacks mempool access and cannot distinguish "txid in mempool but not yet in DB" from "txid gone from both"). For this fix, `txid_set` pruning belongs to the cache service; the mempool sends a fire-and-forget message with the grouped txid-to-data_root mapping. Note: the mempool does perform one other controlled, limited write to `CachedDataRoots` — it sets `expiry_height` when a data tx is first accepted, which is a targeted update unrelated to `txid_set` ownership.
- **Group by `data_root` before sending.** Multiple txids may share the same `data_root`; `prune_pending_txs` groups them into a `HashMap<H256, Vec<H256>>` before sending, so the cache service can batch-update each `CachedDataRoot` entry once.
- **`retain` instead of delete-if-empty.** When `txid_set` becomes empty, the `CachedDataRoot` entry is left in place rather than deleted. The cache service's existing `prune_data_root_cache` handles full entry deletion on its own schedule, and an empty `txid_set` is harmless (no txids means no publish candidates means no lookup failure).
- **Non-fatal errors.** DB write failures during cleanup are logged as warnings but not propagated. Cache cleanup is best-effort; Layer A provides the safety net.

## Consequences

- Block production no longer crashes in release builds when stale txids exist in `CachedDataRoot.txid_set` (Layer A); debug/test builds still panic via `debug_assert!` as a regression canary.
- Stale txid references are cleaned up at the source, preventing unbounded accumulation (Layer B).
- The `debug_assert` acts as a regression canary: any future code path that creates stale txid references will fail tests.
- Tests that intentionally inject stale state must catch the `debug_assert` panic (via `tokio::task::spawn` + `JoinError` check) rather than asserting on an `Err` return.
- `txid_set` mutations in `CachedDataRoots` remain with the cache service. The mempool's role in this fix is limited to detecting which txids to prune and sending a message. The mempool does perform one separate, targeted write (`expiry_height`) when accepting a data tx, but does not perform general ownership of the table.

## Source

Branch `fix/promotion-candidate-pruning` — fix: stale txid in CachedDataRoot.txid_set blocks block production
