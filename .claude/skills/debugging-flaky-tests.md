---
name: debugging-flaky-tests
description: Use when investigating flaky CI test failures that can't be reproduced locally — guides systematic analysis of timeout, race condition, and resource contention issues
---

# Debugging Flaky Tests in Irys

## Step 1: Gather CI Evidence

Fetch logs and isolate the failing test output:

```sh
gh run view <run_id> --repo Irys-xyz/irys --log 2>&1 | grep -A 200 "test_name"
```

Check for:
- **TRY 1 vs TRY 2 timing** — if TRY 1 timed out (TMT) and TRY 2 passed quickly, suspect CPU starvation
- **stderr output** — were blocks actually produced, or do you only see startup/initialization logs?
- **Exit condition** — timeout (TMT), assertion failure, or panic?

## Step 2: Read the Test Source

Locate the test and identify:

1. **Mining approach**: manual (`mine_block()`, `mine_blocks()`) vs autonomous (`start_mining()`)
   - Manual mining is deterministic and does not depend on VDF throughput
   - Autonomous mining depends on VDF (CPU-bound sequential SHA256) and is susceptible to CPU starvation
2. **Waiting mechanism**: polling vs event-driven
   - `wait_until_height()` — polling with 1s sleep intervals, fragile under load
   - `wait_for_block_at_height()` — event-driven via `BlockStateUpdated` subscription, preferred
   - `block_quiescence()` — waits until no `BlockStateUpdated` events arrive for an idle duration, useful after gossip
3. **Event subscriptions**: check if `subscribe_block_state_updates()` / `subscribe_reorgs()` is called BEFORE the action that produces events

Key test utility file: `crates/chain/tests/utils.rs`

## Step 3: Check Nextest Configuration

Open `.config/nextest.toml` and determine what the test gets based on its name prefix:

| Prefix      | threads-required | slow-timeout (kill after) | priority |
|-------------|-----------------|---------------------------|----------|
| (none)      | 1               | 60s (30s x terminate-after=2) | default  |
| `heavy_`    | 2               | 60s                       | 90       |
| `heavy3_`   | 3               | 60s                       | 80       |
| `heavy4_`   | 4               | 60s                       | 70       |
| `slow_`     | 1               | 180s (90s x 2)            | 100      |
| `serial_`   | max-threads=1 (serialized) | 60s              | default  |

Prefixes are combinable. A test named `slow_heavy3_my_test` gets both 180s timeout AND 3 threads reserved.

## Step 4: Match Against Known Flaky Patterns

### Pattern 1: CPU Starvation Timeout

**Symptoms:**
- Test uses `start_mining()` (autonomous VDF-driven mining)
- TRY 1: timed out (TMT); TRY 2: passed in seconds
- CI stderr shows only startup logs, no "block produced" or "solution found" messages
- Test has low `threads-required` (1 or 2)

**Root cause:** VDF thread is CPU-bound (sequential SHA256 hashing). When CI runs many parallel tests, the VDF thread gets starved and never completes steps, so no blocks are produced and the test times out.

**Fix:**
- Increase CPU reservation by adding/changing the name prefix: `heavy3_` or `heavy4_`
- Add `slow_` prefix if the test legitimately needs more time even with enough CPUs
- Consider converting from autonomous mining (`start_mining()`) to manual mining (`mine_block()`) if the test doesn't specifically need VDF-driven block production

### Pattern 2: Event Subscription Race Condition

**Symptoms:**
- Test hangs or times out waiting for a specific event
- The action that triggers the event happens BEFORE the subscription is established
- Works locally because timing is faster, fails under CI load

**Root cause:** `tokio::sync::broadcast` channels only deliver messages to subscribers who are already subscribed when the message is sent. If you subscribe after the event fires, you miss it forever.

**Fix:**
- Subscribe BEFORE starting the action, then check current state as fallback
- Use `wait_for_block_at_height()` which subscribes first, then checks if the block already exists
- Use `block_quiescence()` which subscribes first, returns a future, and you await it after the action:
  ```rust
  let quiescent = node.block_quiescence(
      Duration::from_millis(500),
      Duration::from_secs(10),
  );
  node.gossip_block_to_peers(&block)?;
  quiescent.await;
  ```

### Pattern 3: Polling with Tight Timeouts

**Symptoms:**
- Test uses `wait_until_height(height, N)` with small `N`
- Passes locally, fails under CI load
- Each retry sleeps 1 second, so `N` retries = `N` seconds max

**Root cause:** Under CI load, blocks may take longer to produce. The per-iteration 1s sleep wastes time and the cumulative retries can exceed the test's kill timeout.

**Fix:**
- Replace `wait_until_height()` with `wait_for_block_at_height()` (event-driven, no polling waste)
- If polling is necessary, increase the `max_seconds` parameter generously
- Use a single overall deadline (`tokio::time::timeout`) wrapping the entire operation rather than per-iteration retries

### Pattern 4: Block Tree Pruning

**Symptoms:**
- Test mines many blocks (>50), then verifies an early block by hash
- `get_block_by_hash()` returns `None` for a block that was definitely produced
- Works with fewer blocks, fails when block count exceeds tree depth

**Root cause:** Block tree has a configurable depth (default 50 in tests via `ConsensusConfig::testing()`). Old blocks are pruned from the in-memory tree. `get_block_by_hash()` reads from the in-memory block tree only.

**Fix:**
- Use `get_block_by_hash_on_chain()` which reads from the database instead of the in-memory tree
- Or use `get_block_by_height_from_index()` / `wait_for_block_in_index()` which also read from persistent storage
- If test needs to verify many historical blocks, collect hashes during mining and verify from DB

## Step 5: Apply and Validate the Fix

1. **Rename the test** if the fix is resource allocation (e.g., add `heavy3_` prefix)
2. **Modify wait logic** if the fix is polling-to-event-driven conversion
3. **Fix subscription ordering** if the fix is a race condition
4. **Run the flakiness detector** to confirm stability:
   ```sh
   cargo xtask flaky -i 10 -- -E 'test(test_name)'
   ```
5. **Check that the test still passes under normal conditions:**
   ```sh
   cargo nextest run -p irys-chain test_name
   ```

## Quick Reference: Key Files

| Purpose | Path |
|---------|------|
| Test harness & wait methods | `crates/chain/tests/utils.rs` |
| Nextest config (timeouts, threads) | `.config/nextest.toml` |
| Block tree (pruning, canonical chain) | `crates/domain/src/models/block_tree.rs` |
| Service senders & broadcast events | `crates/actors/src/services.rs` |
| Block tree service & events | `crates/actors/src/block_tree_service.rs` |
| VDF state | `crates/vdf/src/state.rs` |
| Chain node entry point | `crates/chain/src/chain.rs` |

## Quick Reference: Wait Methods on `IrysNodeTest`

| Method | Mechanism | Use When |
|--------|-----------|----------|
| `wait_until_height(h, secs)` | Polling (1s sleep) | Simple cases, generous timeout |
| `wait_for_block_at_height(h, secs)` | Event-driven (`BlockStateUpdated`) | Preferred for autonomous mining |
| `wait_for_block_in_index(h, include_chunk, secs)` | Polling DB index (1s sleep) | Need block in persistent index |
| `wait_for_block(hash, secs)` | Polling block tree (20ms sleep) | Waiting for a specific known hash |
| `block_quiescence(idle, deadline)` | Event-driven (idle gap detection) | After gossip, wait for processing to settle |
| `wait_until_height_confirmed(h, secs)` | Polling (1s sleep) | Need block in confirmed/on-chain state |
