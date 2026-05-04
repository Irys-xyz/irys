# Hybrid `wait_for_active_peers` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the current "wait for any one active peer, forever" startup gate with a hybrid wait that proceeds when N peers are active OR when an X-second timeout fires (whichever comes first), with N and X configurable via `SyncConfig`.

**Architecture:** Modify `PeerList::wait_for_active_peers` to take `(min_count, timeout)` and return the final active-peer count. Subscribe to the existing `PeerEvent` broadcast and recount on every wakeup so peer offline transitions correctly decrement. Keep the genesis branch's "skip sync if 0 peers" escape hatch by inspecting the returned count. Add the two tunables (`min_active_peers`, `peer_wait_timeout_millis`) to the existing `SyncConfig` struct.

**Tech Stack:** Rust 1.93.0, tokio (broadcast channel + monotonic-time `tokio::time::timeout_at`), nextest. See spec: `docs/superpowers/specs/2026-04-29-hybrid-peer-wait-design.md`.

---

## File Structure

| File | Responsibility | Change |
|------|----------------|--------|
| `crates/types/src/config/node.rs` | Config types | Add 2 fields + defaults to `SyncConfig`; small TODO above `genesis_peer_discovery_timeout_millis` |
| `crates/domain/src/models/peer_list.rs` | `PeerList::wait_for_active_peers` + tests | Change signature/body; add 5 new tests; update 1 existing test |
| `crates/p2p/src/chain_sync.rs` | Startup sync orchestration | Update genesis-branch (lines 837-859) and peer-branch (lines 860-863) call sites |
| `crates/p2p/src/peer_network_service.rs` | Test-only callers | Update 2 test callers to new signature |

---

## Task 1: Add `min_active_peers` and `peer_wait_timeout_millis` to `SyncConfig`

**Files:**
- Modify: `crates/types/src/config/node.rs:709-722` (struct)
- Modify: `crates/types/src/config/node.rs:724-736` (Default impl)
- Modify: `crates/types/src/config/node.rs:149-150` (TODO comment above `genesis_peer_discovery_timeout_millis`)
- Modify: `crates/types/src/config/node.rs:1067` (testing builder — already has `sync: SyncConfig::default()` but we want testing override of `min_active_peers = 1`)

- [ ] **Step 1.1: Read the current `SyncConfig` struct and Default impl to confirm formatting style**

Run: `sed -n '707,736p' crates/types/src/config/node.rs`
Expected: see existing struct with `block_batch_size`, `periodic_sync_check_interval_secs`, etc., and matching `Default`.

- [ ] **Step 1.2: Add the two new fields to `SyncConfig`**

In `crates/types/src/config/node.rs`, locate `pub struct SyncConfig` (line 709). Add the two fields at the end of the struct, just before the closing brace:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SyncConfig {
    /// How many blocks to fetch in parallel per batch during the sync
    pub block_batch_size: usize,
    /// How often to check if we're behind and need to sync
    pub periodic_sync_check_interval_secs: u64,
    /// Timeout for retry block pull/process
    pub retry_block_request_timeout_secs: u64,
    /// Whether to enable periodic sync checks
    pub enable_periodic_sync_check: bool,
    /// Timeout per attempt when waiting for a queue slot
    pub wait_queue_slot_timeout_secs: u64,
    /// Maximum consecutive timeout attempts when waiting for a queue slot with no active validations
    pub wait_queue_slot_max_attempts: usize,
    /// Minimum number of active+online peers required before chain sync proceeds.
    /// If unmet within `peer_wait_timeout_millis`, sync proceeds best-effort
    /// with however many peers are available.
    pub min_active_peers: usize,
    /// How long to wait for `min_active_peers` peers before proceeding
    /// best-effort. Does not apply to the genesis branch, which uses
    /// `NodeConfig::genesis_peer_discovery_timeout_millis`.
    pub peer_wait_timeout_millis: u64,
}
```

- [ ] **Step 1.3: Update `Default` impl for `SyncConfig` with production defaults**

Replace the existing `impl Default for SyncConfig` body (line 724-736) with the same fields plus the two new ones:

```rust
impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            block_batch_size: 50,
            // Check every 30 seconds if we're behind
            periodic_sync_check_interval_secs: 30,
            retry_block_request_timeout_secs: 30,
            enable_periodic_sync_check: true,
            wait_queue_slot_timeout_secs: 30,
            wait_queue_slot_max_attempts: 3,
            // Production: require 3 active+online peers before chain sync proceeds.
            min_active_peers: 3,
            peer_wait_timeout_millis: 20_000,
        }
    }
}
```

- [ ] **Step 1.4: Add testing override for `min_active_peers` in `NodeConfig::testing()`**

`NodeConfig::testing()` at line 1084 currently sets `sync: SyncConfig::default()` (line 1067). We need it to use `min_active_peers: 1` for tests. Find the `sync: SyncConfig::default(),` line in `NodeConfig::testing()` (around line 1067) and replace with:

```rust
            sync: SyncConfig {
                min_active_peers: 1,
                ..SyncConfig::default()
            },
```

Leave the production builder (`NodeConfig::production_test`-style or whichever uses `SyncConfig::default()` at line 1228) unchanged — production uses the default of 3.

- [ ] **Step 1.5: Add a small TODO above `genesis_peer_discovery_timeout_millis`**

In `crates/types/src/config/node.rs` at line 149, replace:

```rust
    #[serde(default = "default_genesis_peer_discovery_timeout_millis")]
    pub genesis_peer_discovery_timeout_millis: u64,
```

with:

```rust
    // TODO: move into SyncConfig so all sync-related tunables live together.
    #[serde(default = "default_genesis_peer_discovery_timeout_millis")]
    pub genesis_peer_discovery_timeout_millis: u64,
```

- [ ] **Step 1.6: Add a unit test that asserts the testing override**

At the end of `crates/types/src/config/node.rs`, find the existing `#[cfg(test)] mod tests` (line 1330) and add the following test inside it:

```rust
#[test]
fn testing_node_config_overrides_sync_min_active_peers() {
    let cfg = NodeConfig::testing();
    assert_eq!(cfg.sync.min_active_peers, 1, "testing override expected");
    assert_eq!(cfg.sync.peer_wait_timeout_millis, 20_000);
}

#[test]
fn sync_config_defaults_match_spec() {
    let cfg = SyncConfig::default();
    assert_eq!(cfg.min_active_peers, 3, "production default expected");
    assert_eq!(cfg.peer_wait_timeout_millis, 20_000);
}
```

- [ ] **Step 1.7: Compile-check the workspace**

Run: `cargo check --workspace`
Expected: clean (the new fields are unused at this point, but no consumers reference them yet so no errors).

- [ ] **Step 1.8: Run the new tests**

Run: `cargo nextest run -p irys-types testing_node_config_overrides_sync_min_active_peers sync_config_defaults_match_spec`
Expected: both tests PASS.

- [ ] **Step 1.9: Commit**

```bash
git add crates/types/src/config/node.rs
git commit -m "feat(config): add sync.min_active_peers and sync.peer_wait_timeout_millis"
```

---

## Task 2: Rewrite `wait_for_active_peers` with hybrid behavior + first test + update all callers

This task is one atomic compile-safe change because the signature change breaks compile until all callers are updated. The end of the task leaves the workspace fully compiling with the new behavior wired in.

**Files:**
- Modify: `crates/domain/src/models/peer_list.rs:322-354` (function body)
- Modify: `crates/domain/src/models/peer_list.rs:1869` (existing test `test_wait_for_active_peers_includes_both_staked_and_unstaked`)
- Modify: `crates/p2p/src/peer_network_service.rs:1535-1583` (two test callers)
- Modify: `crates/p2p/src/chain_sync.rs:837-863` (genesis + peer branches)
- Test: `crates/domain/src/models/peer_list.rs` (new test in existing `#[cfg(test)] mod tests`)

- [ ] **Step 2.1: Write the first failing test (immediate satisfaction)**

In `crates/domain/src/models/peer_list.rs`, locate the existing `#[cfg(test)] mod tests` block (line 1287). Add this test inside it, near the other `wait_for_active_peers`-related tests:

```rust
#[tokio::test]
async fn wait_for_n_peers_returns_immediately_when_already_satisfied() {
    let peer_list = create_test_peer_list(Config::new_with_random_peer_id(NodeConfig::testing()));

    let (_addr_a, _id_a, peer_a) = create_test_peer(1);
    let (_addr_b, _id_b, peer_b) = create_test_peer(2);
    peer_list.add_or_update_peer(peer_a, true);
    peer_list.add_or_update_peer(peer_b, true);

    // Both peers start with PeerScore::INITIAL = 50 which is >= ACTIVE_THRESHOLD = 10
    // and is_online = true, so both are active+online.

    let start = std::time::Instant::now();
    let count = peer_list
        .wait_for_active_peers(2, Duration::from_secs(5))
        .await;
    let elapsed = start.elapsed();

    assert!(count >= 2, "expected at least 2 active peers, got {count}");
    assert!(
        elapsed < Duration::from_millis(100),
        "fast path should return ~immediately, took {:?}",
        elapsed
    );
}
```

- [ ] **Step 2.2: Run the test to verify it fails to compile**

Run: `cargo nextest run -p irys-domain wait_for_n_peers_returns_immediately_when_already_satisfied 2>&1 | head -40`
Expected: compile error like `wait_for_active_peers takes 1 argument but 3 arguments were supplied` (the existing signature is `(&self)`, the test passes `(2, Duration)`).

- [ ] **Step 2.3: Replace `wait_for_active_peers` with the hybrid implementation**

In `crates/domain/src/models/peer_list.rs`, replace the existing function (lines 322-354):

```rust
pub async fn wait_for_active_peers(&self) {
    // Fast path: return immediately if any active peers exist
    {
        let bindings = self.read();
        let persistent_active = bindings
            .persistent_peers_cache
            .values()
            .any(|peer| peer.reputation_score.is_active() && peer.is_online);
        let purgatory_active = bindings
            .unstaked_peer_purgatory
            .iter()
            .map(|(_, v)| v)
            .any(|peer| peer.reputation_score.is_active() && peer.is_online);
        if persistent_active || purgatory_active {
            return;
        }
    }

    // Slow path: subscribe and wait for the next BecameActive event
    let mut rx = self.subscribe_to_peer_events();
    loop {
        match rx.recv().await {
            Ok(PeerEvent::BecameActive { .. }) => return,
            Ok(_) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                warn!("peer events channel closed while waiting for active peers");
                tokio::time::sleep(Duration::from_millis(200)).await;
                rx = self.subscribe_to_peer_events();
            }
        }
    }
}
```

with:

```rust
/// Wait until at least `min_count` peers are active+online, or `timeout` elapses.
///
/// Returns the count of active+online peers when the wait ends. If the count is
/// `>= min_count` the wait was satisfied; if it is less, the timeout fired and
/// the caller is expected to proceed best-effort.
pub async fn wait_for_active_peers(&self, min_count: usize, timeout: Duration) -> usize {
    let count_active = || -> usize {
        let bindings = self.read();
        let persistent = bindings
            .persistent_peers_cache
            .values()
            .filter(|peer| peer.reputation_score.is_active() && peer.is_online)
            .count();
        let purgatory = bindings
            .unstaked_peer_purgatory
            .iter()
            .map(|(_, v)| v)
            .filter(|peer| peer.reputation_score.is_active() && peer.is_online)
            .count();
        persistent + purgatory
    };

    // Fast path: already satisfied
    let initial = count_active();
    if initial >= min_count {
        return initial;
    }

    // Slow path: subscribe and recount on every event wakeup. Recounting (rather
    // than tracking deltas) ensures BecameInactive transitions correctly drop
    // the count.
    let mut rx = self.subscribe_to_peer_events();
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let current = count_active();
        if current >= min_count {
            return current;
        }

        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(_)) => continue,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                warn!("peer events channel closed while waiting for active peers");
                tokio::time::sleep(Duration::from_millis(200)).await;
                rx = self.subscribe_to_peer_events();
            }
            // Deadline elapsed: return whatever the current count is.
            Err(_) => return count_active(),
        }
    }
}
```

- [ ] **Step 2.4: Update existing test `test_wait_for_active_peers_includes_both_staked_and_unstaked` to compile**

This test (line 1869) doesn't actually call `wait_for_active_peers` — it inlines its own count predicate (lines 1887-1901, 1911-…). It compiles fine without changes. Verify by reading lines 1860-1930; **no edit needed** unless the test directly invokes the function. (If you find a direct call, update it to `(1, Duration::from_secs(2))`.)

- [ ] **Step 2.5: Update test caller `test_wait_for_active_peer` in `peer_network_service.rs`**

In `crates/p2p/src/peer_network_service.rs`, locate `test_wait_for_active_peer` (line 1535). Replace:

```rust
        let wait_handle = tokio::spawn(async move {
            wait_list.wait_for_active_peers().await;
        });
```

with:

```rust
        let wait_handle = tokio::spawn(async move {
            wait_list
                .wait_for_active_peers(1, Duration::from_secs(2))
                .await
        });
```

Then update the join below (currently `wait_handle.await.expect("wait task");`) to capture the count and assert `>= 1`:

```rust
        let count = wait_handle.await.expect("wait task");
        assert!(count >= 1, "expected at least 1 active peer, got {count}");
```

- [ ] **Step 2.6: Update test caller `test_wait_for_active_peer_no_peers` in `peer_network_service.rs`**

In `crates/p2p/src/peer_network_service.rs`, locate `test_wait_for_active_peer_no_peers` (line 1565). Replace the body:

```rust
        let wait_list = peer_list.clone();
        let result = timeout(Duration::from_millis(200), async move {
            wait_list.wait_for_active_peers().await;
        })
        .await;
        assert!(result.is_err(), "wait should time out without peers");
```

with:

```rust
        let count = peer_list
            .wait_for_active_peers(1, Duration::from_millis(200))
            .await;
        assert_eq!(count, 0, "wait should return count=0 when no peers");
```

The outer `tokio::time::timeout` is no longer needed — the function self-bounds.

- [ ] **Step 2.7: Update genesis branch in `chain_sync.rs`**

In `crates/p2p/src/chain_sync.rs`, locate the genesis branch (lines 837-859, inside `initialize_sync_mode`). Replace:

```rust
    if params.is_a_genesis_node {
        warn!(
            "Sync task: Because the node is a genesis node, waiting for active peers for {}, and if no peers are added, then skipping the sync task",
            params.genesis_peer_discovery_timeout_millis
        );
        match timeout(
            Duration::from_millis(params.genesis_peer_discovery_timeout_millis),
            peer_list.wait_for_active_peers(),
        )
        .await
        {
            Ok(()) => {
                info!("Genesis node has active peers");
            }
            Err(elapsed) => {
                warn!(
                    "Sync task: Due to the node being in genesis mode, after waiting for active peers for {} and no peers showing up, skipping the sync task",
                    elapsed
                );
                sync_state.finish_sync();
                return Ok(false);
            }
        };
    } else {
        sync_state.set_is_syncing(true);
        peer_list.wait_for_active_peers().await;
    }
```

with:

```rust
    if params.is_a_genesis_node {
        warn!(
            "Sync task: Because the node is a genesis node, waiting for active peers for {}ms, and if no peers are added, then skipping the sync task",
            params.genesis_peer_discovery_timeout_millis
        );
        let count = peer_list
            .wait_for_active_peers(
                config.node_config.sync.min_active_peers,
                Duration::from_millis(params.genesis_peer_discovery_timeout_millis),
            )
            .await;
        if count == 0 {
            warn!(
                "Sync task: Due to the node being in genesis mode, after waiting for active peers for {}ms and no peers showing up, skipping the sync task",
                params.genesis_peer_discovery_timeout_millis
            );
            sync_state.finish_sync();
            return Ok(false);
        }
        info!(
            "Genesis node has {} active peer(s) (wanted {})",
            count, config.node_config.sync.min_active_peers
        );
    } else {
        sync_state.set_is_syncing(true);
        let min_count = config.node_config.sync.min_active_peers;
        let timeout_ms = config.node_config.sync.peer_wait_timeout_millis;
        let count = peer_list
            .wait_for_active_peers(min_count, Duration::from_millis(timeout_ms))
            .await;
        if count < min_count {
            warn!(
                "Sync task: proceeding with {} active peer(s) (wanted {}) after {}ms timeout",
                count, min_count, timeout_ms
            );
        }
    }
```

- [ ] **Step 2.8: Compile-check the workspace**

Run: `cargo check --workspace --tests`
Expected: clean compile across all crates.

- [ ] **Step 2.9: Run the first new test**

Run: `cargo nextest run -p irys-domain wait_for_n_peers_returns_immediately_when_already_satisfied`
Expected: PASS.

- [ ] **Step 2.10: Run the updated existing tests**

Run: `cargo nextest run -p irys-p2p test_wait_for_active_peer test_wait_for_active_peer_no_peers`
Expected: both PASS.

- [ ] **Step 2.11: Commit**

```bash
git add crates/domain/src/models/peer_list.rs \
        crates/p2p/src/peer_network_service.rs \
        crates/p2p/src/chain_sync.rs
git commit -m "refactor(p2p): hybrid wait_for_active_peers (N peers or timeout)"
```

---

## Task 3: Add the remaining four tests

**Files:**
- Test: `crates/domain/src/models/peer_list.rs` (extend `#[cfg(test)] mod tests`)

- [ ] **Step 3.1: Add test #2 — satisfied via events**

In the same `#[cfg(test)] mod tests` block, after the test from Step 2.1, add:

```rust
#[tokio::test]
async fn wait_for_n_peers_satisfied_via_events() {
    let peer_list = create_test_peer_list(Config::new_with_random_peer_id(NodeConfig::testing()));
    let waiter = peer_list.clone();

    let handle = tokio::spawn(async move {
        waiter.wait_for_active_peers(2, Duration::from_secs(2)).await
    });

    // Give the waiter time to enter the slow path
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (_addr_a, _id_a, peer_a) = create_test_peer(1);
    peer_list.add_or_update_peer(peer_a, true);

    tokio::time::sleep(Duration::from_millis(50)).await;

    let (_addr_b, _id_b, peer_b) = create_test_peer(2);
    peer_list.add_or_update_peer(peer_b, true);

    let count = handle.await.expect("waiter task");
    assert!(count >= 2, "expected at least 2 after second peer added, got {count}");
}
```

- [ ] **Step 3.2: Add test #3 — timeout with partial count**

```rust
#[tokio::test]
async fn wait_for_n_peers_timeout_partial() {
    let peer_list = create_test_peer_list(Config::new_with_random_peer_id(NodeConfig::testing()));

    let (_addr, _id, peer) = create_test_peer(1);
    peer_list.add_or_update_peer(peer, true);

    let start = std::time::Instant::now();
    let count = peer_list
        .wait_for_active_peers(3, Duration::from_millis(200))
        .await;
    let elapsed = start.elapsed();

    assert_eq!(count, 1, "only one peer was added");
    assert!(
        elapsed >= Duration::from_millis(190),
        "should have waited near the full timeout, elapsed={:?}",
        elapsed
    );
}
```

- [ ] **Step 3.3: Add test #4 — timeout with zero peers**

```rust
#[tokio::test]
async fn wait_for_n_peers_timeout_zero_peers() {
    let peer_list = create_test_peer_list(Config::new_with_random_peer_id(NodeConfig::testing()));

    let count = peer_list
        .wait_for_active_peers(1, Duration::from_millis(200))
        .await;

    assert_eq!(count, 0, "no peers were added");
}
```

- [ ] **Step 3.4: Add test #5 — recount when peer goes offline**

```rust
#[tokio::test]
async fn wait_for_n_peers_recounts_when_peer_goes_offline() {
    let peer_list = create_test_peer_list(Config::new_with_random_peer_id(NodeConfig::testing()));

    let (addr_a, _id_a, peer_a) = create_test_peer(1);
    let (_addr_b, _id_b, peer_b) = create_test_peer(2);
    peer_list.add_or_update_peer(peer_a, true);
    peer_list.add_or_update_peer(peer_b, true);

    let waiter = peer_list.clone();
    let handle = tokio::spawn(async move {
        // N=3 with only 2 peers online; we want the slow path so the offline
        // transition is observed via PeerEvent.
        waiter.wait_for_active_peers(3, Duration::from_millis(300)).await
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    // Flip peer A offline mid-wait. This emits PeerEvent::BecameInactive.
    peer_list.set_is_online(&addr_a, false);

    let count = handle.await.expect("waiter task");
    assert_eq!(
        count, 1,
        "BecameInactive should drop A from the count without satisfying the wait"
    );
}
```

- [ ] **Step 3.5: Run all five new tests**

Run: `cargo nextest run -p irys-domain wait_for_n_peers_`
Expected: all 5 tests PASS (the immediate-satisfaction test from Task 2 plus the 4 new ones).

- [ ] **Step 3.6: Commit**

```bash
git add crates/domain/src/models/peer_list.rs
git commit -m "test: cover hybrid wait_for_active_peers behavior"
```

---

## Task 4: Final verification

- [ ] **Step 4.1: Run formatter**

Run: `cargo fmt --all`
Expected: no output (or trivial whitespace fixes).

- [ ] **Step 4.2: Run clippy across workspace**

Run: `cargo clippy --workspace --tests --all-targets -- -D warnings`
Expected: clean. Common issues to watch for:
- `clippy::needless_pass_by_value` on the new function args — pass by value is correct for `usize` and `Duration` (both `Copy`).
- `clippy::redundant_closure` on `count_active` — fine to leave as a closure for readability.

- [ ] **Step 4.3: Run the full test suite for the affected crates**

Run: `cargo nextest run -p irys-types -p irys-domain -p irys-p2p`
Expected: all tests PASS (no regressions in existing tests; new tests all green).

- [ ] **Step 4.4: Run a broader smoke check**

Run: `cargo xtask test`
Expected: failure-tracked nextest run completes with no new failures attributable to this change. Pre-existing flaky tests should be re-run via `cargo xtask test --rerun-failures` and considered noise unless they involve `peer_list` / `chain_sync` / startup paths.

- [ ] **Step 4.5: If any fmt/clippy fixes were needed, commit them**

```bash
git add -A
git status   # confirm only formatting/clippy fixes are staged
git commit -m "chore: fmt/clippy after hybrid wait_for_active_peers"
```

(Skip this step if no changes were needed.)

---

## Done-ness checklist

- [ ] `SyncConfig` has `min_active_peers` (default 3, testing 1) and `peer_wait_timeout_millis` (default 20_000)
- [ ] `wait_for_active_peers(min_count, timeout) -> usize` is the new signature, with fast path + slow path + recount-on-event
- [ ] Genesis branch in `chain_sync.rs` skips sync iff returned count == 0
- [ ] Peer branch in `chain_sync.rs` warn-logs when returned count < min_count
- [ ] Five new tests cover: immediate satisfaction, satisfied-via-events, timeout-partial, timeout-zero, recount-on-offline
- [ ] Two existing test callers in `peer_network_service.rs` updated to new signature
- [ ] `cargo fmt`, `cargo clippy --workspace --tests --all-targets`, `cargo nextest run` all clean
