# Hybrid `wait_for_active_peers`: N-peers-or-timeout

**Status:** Design approved, pending implementation plan
**Date:** 2026-04-29
**Author:** jesse@irys.xyz (with Claude)

## Problem

`PeerList::wait_for_active_peers` (`crates/domain/src/models/peer_list.rs:322`)
returns the moment a single peer satisfies `reputation_score.is_active() &&
is_online`. It is the gate the chain-sync task waits on at startup before
fetching block index, switch-height, and history.

Two problems with the current single-peer gate:

1. **Weak startup guarantee.** A peer node that finds one randomly-reachable
   neighbor proceeds to sync. Block-index fetch, switch-height detection, and
   the genesis-vs-first-block decisions downstream are all "first responder
   wins" — so a single misbehaving or stale peer can shape the syncing node's
   view of the chain at startup.
2. **No bounded wait on the peer branch.** `chain_sync.rs:862`
   (non-genesis peer branch) awaits `wait_for_active_peers().await` with no
   timeout. A peer with no reachable neighbors blocks startup forever.

We want a hybrid wait: **proceed when N peers are active, OR when an X-second
timeout fires** (whichever comes first). On timeout, proceed best-effort with
whatever count we have, log a warning. Both N and X are configurable.

## Non-goals

- This change does not add cross-peer agreement / quorum at startup; downstream
  fetches (`get_block_index`, `check_and_update_full_validation_switch_height`)
  remain "first successful peer wins". Tightening those is a separate effort.
- This change does not relocate the existing top-level
  `genesis_peer_discovery_timeout_millis` into `SyncConfig`; that is left as
  a small follow-up TODO.

## Design

### Config

Two new fields on the existing `SyncConfig` struct
(`crates/types/src/config/node.rs:709`), accessed as
`node_config.sync.{min_active_peers, peer_wait_timeout_millis}`:

```rust
pub struct SyncConfig {
    // ... existing fields ...
    /// Minimum number of active+online peers required before chain sync proceeds.
    /// If unmet within `peer_wait_timeout_millis`, sync proceeds best-effort
    /// with however many peers are available.
    pub min_active_peers: usize,
    /// How long to wait for `min_active_peers` peers before proceeding
    /// best-effort. Does not apply to the genesis branch, which uses
    /// `genesis_peer_discovery_timeout_millis`.
    pub peer_wait_timeout_millis: u64,
}
```

| field | production default | testing override |
|---|---|---|
| `sync.min_active_peers` | 3 | 1 |
| `sync.peer_wait_timeout_millis` | 20_000 | 20_000 |

`SyncConfig::default()` returns the production defaults. Tests using
`NodeConfig::testing()` get the override via the existing testing-config
construction path. The production builder explicitly matches the default.

`genesis_peer_discovery_timeout_millis` remains top-level on `NodeConfig`
as a pre-existing outlier — it is the genesis-only escape window, not a
general sync tunable. A small `// TODO: move into SyncConfig` comment is
left above it so a follow-up cleanup is easy to find.

### API

`PeerList::wait_for_active_peers` signature changes:

```rust
pub async fn wait_for_active_peers(
    &self,
    min_count: usize,
    timeout: Duration,
) -> usize  // count of active+online peers when wait ended
```

Returning `usize` (rather than an enum / Result) keeps the surface minimal:
the caller knows `min_count`, so `count >= min_count` is unambiguous
"satisfied" and `count < min_count` is "timed out best-effort".

### Behavior

1. **Fast path.** Recount `active+online` peers across `persistent_peers_cache`
   and `unstaked_peer_purgatory`. If `count >= min_count`, return immediately.
2. **Slow path.** Subscribe to `PeerEvent` via the existing broadcast channel.
   Loop with a single overall deadline (`tokio::time::timeout_at`):
   - On every event wakeup, **recount** rather than track deltas. The fast-path
     predicate is the source of truth.
   - If `count >= min_count`, return.
   - On `Lagged`, continue.
   - On `Closed`, sleep briefly and re-subscribe (matches existing logic).
   - On deadline, return the current count (may be 0).

Recounting on every wakeup correctly handles `BecameInactive` events
(emitted at `peer_list.rs:225, 265, 918`) — peers transitioning offline
decrement the count just as `BecameActive` increments it.

`tokio::time` is monotonic under the hood, so the deadline behaves correctly
across wall-clock jumps. 

### Call site updates

Three production call sites in `crates/p2p/src/chain_sync.rs`:

**Genesis branch (`chain_sync.rs:842-859`).** Outer `timeout(...)` wrapper
goes away (the function self-bounds). Genesis branch keeps its skip-sync
escape hatch by inspecting the returned count:

```rust
let count = peer_list
    .wait_for_active_peers(
        config.node_config.sync.min_active_peers,
        Duration::from_millis(config.node_config.genesis_peer_discovery_timeout_millis),
    )
    .await;
if count == 0 {
    warn!("Sync task: genesis node found no peers within timeout, skipping sync");
    sync_state.finish_sync();
    return Ok(false);
}
```

Note: genesis branch keeps using `genesis_peer_discovery_timeout_millis`
(10s default), not the new `peer_wait_timeout_millis`. They are conceptually
distinct windows: genesis is "how long do I wait before assuming I am
cold-starting alone"; peer is "how long do I wait for N healthy neighbors".

**Peer branch (`chain_sync.rs:862`).** Replace bare await with the new call,
ignore returned count beyond a warning log:

```rust
sync_state.set_is_syncing(true);
let count = peer_list
    .wait_for_active_peers(
        config.node_config.sync.min_active_peers,
        Duration::from_millis(config.node_config.sync.peer_wait_timeout_millis),
    )
    .await;
if count < config.node_config.sync.min_active_peers {
    warn!(
        "Sync task: proceeding with {} active peers (wanted {}) after {}ms timeout",
        count,
        config.node_config.sync.min_active_peers,
        config.node_config.sync.peer_wait_timeout_millis,
    );
}
```

**Test callers** (`peer_network_service.rs:1549, 1580` and any others) — pass
explicit `(1, Duration::from_millis(...))`. Existing outer `tokio::time::timeout`
wrappers are no longer needed since the wait self-bounds.

### Tests

In `crates/domain/src/models/peer_list.rs`:

| # | name | scenario | asserts |
|---|------|----------|---------|
| 1 | `wait_for_n_peers_returns_immediately_when_already_satisfied` | 2 online, call N=2 | fast return, `count >= 2` |
| 2 | `wait_for_n_peers_satisfied_via_events` | 0 online, add 2 mid-wait | returns after 2nd add, `count >= 2` |
| 3 | `wait_for_n_peers_timeout_partial` | 1 online, N=3, 200ms | timeout fires, `count == 1` |
| 4 | `wait_for_n_peers_timeout_zero_peers` | 0 online, N=1, 200ms | timeout fires, `count == 0` |
| 5 | `wait_for_n_peers_recounts_when_peer_goes_offline` | 2 online, N=3, 300ms; flip 1 offline mid-wait | `BecameInactive` does not satisfy predicate; timeout returns `count == 1` |

Test #5 is the load-bearing one for the recount-on-event design — it proves
that `BecameInactive` events drop the count and don't accidentally satisfy
the wait.

Existing tests updated to pass `(1, Duration::from_…)`:

- `test_wait_for_active_peer` (`peer_network_service.rs:1535`) — pass
  `(1, Duration::from_secs(2))`, assert returned count `>= 1`.
- `test_wait_for_active_peer_no_peers` (`peer_network_service.rs:1565`) —
  pass `(1, Duration::from_millis(200))`, assert returned count is `0`.
  Drop the outer `tokio::time::timeout` wrapper.
- `test_wait_for_active_peers_includes_both_staked_and_unstaked`
  (`peer_list.rs:1869`) — pass `(1, Duration::from_…)`.

No new chain_sync.rs integration tests; the unit-level guarantees of
`wait_for_active_peers` cover the contract its callers depend on.

## Security considerations

- **Liveness vs safety tradeoff.** Best-effort proceed on timeout is an
  explicit liveness choice — a peer with no neighbors after X seconds will
  start up rather than wedge. Combined with the production default of N=3,
  this raises the bar from "any one peer" to "three peers preferred, fewer
  acceptable on timeout". Operators who want stricter behavior can raise N
  and/or X without code changes.
- **Trusted-mode interaction unchanged.** The trusted-peers check at
  `chain_sync.rs:867-878` still runs after the wait returns and is
  independent of the count semantics here. Counting any active peer (vs
  trusted-only) for N is intentional — adds liveness without weakening
  the existing trusted-mode gate.
- **Genesis branch escape preserved.** `count == 0` after the genesis-
  specific timeout window still skips sync via `finish_sync()`, matching
  current behavior.

## Follow-ups

- Move `genesis_peer_discovery_timeout_millis` from top-level `NodeConfig`
  into `SyncConfig` so all sync-related tunables live together. Tracked via
  a `// TODO` comment at the field site.
- Consider tightening "first responder wins" in `get_block_index` and
  `check_and_update_full_validation_switch_height` to require agreement
  across multiple trusted peers at startup. Out of scope for this change.
