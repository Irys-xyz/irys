# Multiversion Test Harness

Integration test framework for verifying that Irys nodes running different binary versions can form clusters, produce blocks, and converge on a canonical chain. Catches protocol-breaking changes before they reach production.

## Quick Start

```sh
# Run all multiversion tests via xtask (recommended)
cargo xtask multiversion-test

# Run only upgrade tests
cargo xtask multiversion-test --test upgrade

# Run a specific test by name
cargo xtask multiversion-test --filter rolling_upgrade_all_nodes

# Combine: specific file + name filter
cargo xtask multiversion-test --test upgrade --filter rolling

# Run directly with cargo
cargo test -p irys-multiversion-tests --test '*' -- --ignored --test-threads=1 --nocapture
cargo test -p irys-multiversion-tests --test upgrade -- --ignored rolling_upgrade_all_nodes --nocapture
```

Tests are marked `#[ignore]` because they build full `irys` binaries from source, which takes several minutes on first run. The xtask handles `--ignored` automatically; when running directly via `cargo test` you must pass it yourself.

## Prerequisites

- Rust 1.93.0 (pinned in `rust-toolchain.toml`)
- System dependencies: `clang`, `libgmp-dev`, `pkg-config`, `libssl-dev`
- Full git history (needed for worktree-based old-version builds)
- For network fault injection: passwordless `sudo iptables`

## How It Works

The harness builds two versions of the `irys` binary, spins up a local multi-node cluster, and verifies consensus convergence across version boundaries.

### Binary Resolution

`BinaryResolver` produces two binaries — **new** and **old** — each of which can come from the current working tree (`CURRENT`) or a specific git ref.

| Ref value | Build source | Caching |
|-----------|-------------|---------|
| `CURRENT` (default) | Current working tree | No rev-keyed cache; cargo's own fingerprinting decides whether to rebuild |
| Any git ref (branch, tag, SHA) | Git worktree checkout | Cached by commit SHA in `target/multiversion/irys-{sha}` |

By default both **new** and **old** resolve to `CURRENT`, meaning they build from the working tree. This is useful for iterating on the test harness or smoke-testing that the current code can start, cluster, and upgrade. To test actual cross-version upgrades, pass `--old-ref master` (or any other ref) explicitly.

When building from a git ref, the resolver creates a detached worktree, builds inside it, and caches the result by commit SHA. An inter-process file lock prevents parallel test processes from racing on the same build. Worktree cleanup is deferred to test teardown — see [Artifact Preservation on Failure](#artifact-preservation).

Build order: tries `--profile debug-release` first, falls back to `--release` for older branches that lack the custom profile.

**Git ref overrides** control which versions are built:

```sh
# Default: both old and new build from the working tree
cargo xtask multiversion-test

# Cross-version upgrade: old from master, new from working tree
cargo xtask multiversion-test --old-ref master

# Compare two specific refs
cargo xtask multiversion-test --old-ref v0.5.0 --new-ref my-feature-branch

# Explicitly request working tree (same as default)
cargo xtask multiversion-test --old-ref CURRENT --new-ref CURRENT

# Environment variables work too (used by xtask under the hood)
IRYS_OLD_REF=master cargo test --test upgrade -- --ignored --nocapture
IRYS_NEW_REF=my-feature-branch cargo test --test upgrade -- --ignored --nocapture
```

**Binary path overrides** skip building entirely and use pre-built binaries:

```sh
IRYS_BINARY_NEW=/path/to/irys-new cargo test ...
IRYS_BINARY_OLD=/path/to/irys-old cargo test ...
```

Binary path overrides (`IRYS_BINARY_NEW`/`IRYS_BINARY_OLD`) take precedence over git ref overrides (`IRYS_NEW_REF`/`IRYS_OLD_REF`).

### Cluster Lifecycle

```text
1. Allocate ephemeral ports (API, gossip, Reth) per node
2. Generate TOML configs from fixtures/base-config.toml template
3. Spawn genesis node, wait for /v1/info to respond
4. Fetch genesis hash + consensus config from genesis
5. Patch peer configs with real consensus params + genesis hash
6. Spawn peer nodes, wait for all to be ready
7. Run test scenario (upgrade, rollback, fault injection, etc.)
8. Shutdown: SIGTERM → 30s grace → SIGKILL
```

All node data lives in a run directory under `target/multiversion/test-data/{run_id}/{test_name}/`. Artifacts are never cleaned up automatically — see [Artifact Preservation](#artifact-preservation).

### Artifact Preservation

Test artifacts (logs, configs, node data) and git worktrees (old-version source trees) are **never cleaned up automatically**. This ensures artifacts are always available for debugging, whether the test passed or failed.

**Relevant paths:**

```text
target/multiversion/test-data/{run_id}/    # test artifacts (logs, configs, data dirs)
target/multiversion/worktree-{sha}/        # old-version source tree
target/multiversion/irys-{sha}             # cached binaries
```

To clean up, use the `--clean` flag which removes the entire `target/multiversion/` directory before running:

```sh
cargo xtask multiversion-test --clean
```

**Environment variables:**

| Variable | Default | Effect |
|----------|---------|--------|
| `IRYS_OLD_REF` | `CURRENT` | Git ref for the old binary, or `CURRENT` for working tree |
| `IRYS_NEW_REF` | `CURRENT` | Git ref for the new binary, or `CURRENT` for working tree |
| `IRYS_BINARY_OLD` | unset | Path to pre-built old binary (skips building entirely) |
| `IRYS_BINARY_NEW` | unset | Path to pre-built new binary (skips building entirely) |
| `IRYS_BUILD_PROFILE` | unset | Cargo profile (`dev`, `release`, etc.) |

Precedence: `IRYS_BINARY_*` > `IRYS_*_REF` > default (`CURRENT`).

## Writing Tests

### Minimal Example: Single-Version Cluster

```rust
mod common;

use irys_multiversion_tests::binary::BinaryResolver;
use irys_multiversion_tests::cluster::Cluster;

#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires building irys binary from HEAD"]
async fn my_single_version_test() {
    let resolver = BinaryResolver::new(&common::repo_root());
    let binary = resolver.resolve_new().await.expect("build failed");

    let spec = common::cluster_spec("my_single_version_test", vec![
        common::genesis_spec("genesis", &binary, vec![]),
        common::peer_spec("peer-1", &binary, 0, vec!["genesis".to_owned()]),
    ]);
    let mut cluster = Cluster::start(spec).await.expect("cluster start failed");

    cluster
        .wait_for_convergence(common::CONVERGENCE_TIMEOUT)
        .await
        .expect("cluster did not converge");

    cluster.shutdown().await;
}
```

### Cross-Version Upgrade Test

`resolve_binaries()` in `src/tests/upgrade.rs` **panics** if `IRYS_OLD_REF` is unset or equals `CURRENT_REF` — upgrade tests always require a distinct old revision. Set `IRYS_OLD_REF` before running:

```sh
cargo xtask multiversion-test --old-ref master --test upgrade
# or: export IRYS_OLD_REF=master
```

```rust
use irys_multiversion_tests::binary::{BinaryResolver, CURRENT_REF};

#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires building irys binaries from HEAD and master"]
async fn my_upgrade_test() {
    // Panics if IRYS_OLD_REF is unset or == CURRENT_REF
    let old_ref = std::env::var("IRYS_OLD_REF").unwrap_or_else(|_| CURRENT_REF.to_owned());
    assert_ne!(old_ref, CURRENT_REF, "IRYS_OLD_REF must differ from CURRENT_REF");

    let resolver = BinaryResolver::new(&common::repo_root());
    let old = resolver.resolve_old(&old_ref).await.expect("old build failed");
    let new = resolver.resolve_new().await.expect("new build failed");

    // Start cluster on the old version
    let spec = common::cluster_spec("my_upgrade_test", vec![
        common::genesis_spec("genesis", &old, vec![]),
        common::peer_spec("peer-1", &old, 0, vec!["genesis".to_owned()]),
    ]);
    let mut cluster = Cluster::start(spec).await.expect("cluster start failed");

    cluster
        .wait_for_convergence(common::CONVERGENCE_TIMEOUT)
        .await
        .expect("initial convergence failed");

    // Upgrade a node: shuts down, swaps binary, respawns, waits for ready
    cluster.upgrade_node("peer-1", &new).await.expect("upgrade failed");

    // Verify mixed-version cluster still converges
    cluster
        .wait_for_convergence(common::CONVERGENCE_TIMEOUT)
        .await
        .expect("post-upgrade convergence failed");

    cluster.shutdown().await;
}
```

### Test Helpers (`src/tests/helpers.rs`)

| Helper | Purpose |
|--------|---------|
| `repo_root()` | Returns the irys repo root path |
| `genesis_spec(name, binary, peers)` | Create a genesis `NodeSpec` with pre-configured mining key |
| `peer_spec(name, binary, index, peers)` | Create a peer `NodeSpec` (0-based peer index; key pool index 0 is reserved for genesis) |
| `cluster_spec(test_name, nodes)` | Bundle `NodeSpec`s into a `ClusterSpec` with tolerance=2 and a per-test run directory |
| `HEIGHT_TIMEOUT` | 120s — max wait for a node to reach a target height |
| `CONVERGENCE_TIMEOUT` | 120s — max wait for all nodes to agree on chain height |

### Key Cluster APIs

```rust
// Access a node by name
let node = cluster.node_mut("peer-1")?;
assert!(node.is_running());
assert_eq!(node.version_label(), "new");

// Get all node API URLs (includes stopped nodes)
let urls = cluster.api_urls();

// Probe a specific node
let info = cluster.probe.get_info(&url).await?;
let height = info.height;

// Wait for a node to reach a specific height
cluster.probe.wait_for_height(&url, 10, timeout).await?;

// Kill a node (SIGKILL, no graceful shutdown)
cluster.node_mut("peer-1")?.kill().await?;

// Upgrade: shutdown → swap binary → respawn → wait ready
cluster.upgrade_node("peer-1", &new_binary).await?;
```

## Fault Injection

The `fault` module provides primitives for chaos testing.

### Process Faults

```rust
use irys_multiversion_tests::fault::crash::{ProcessFreezer, ProcessKiller};

// Freeze a node (SIGSTOP) — resumes on guard drop or explicit undo
let guard = ProcessFreezer.inject(&FaultTarget::Process { pid }).await?;
// ... test behavior with frozen node ...
guard.undo()?; // sends SIGCONT

// Kill a node (SIGKILL)
ProcessKiller.inject(&FaultTarget::Process { pid }).await?;
```

### Network Partitions

Requires passwordless `sudo iptables`.

```rust
use irys_multiversion_tests::fault::network::NetworkPartitioner;

let partitioner = NetworkPartitioner::new();
let guard = partitioner.inject(&FaultTarget::Network {
    source_port: node_a_gossip_port,
    dest_port: node_b_gossip_port,
}).await?;
// ... traffic between these ports is now dropped ...
guard.undo()?; // removes iptables rules
```

`FaultGuard` auto-cleans on drop, so partitions are removed even if the test panics. Each `NetworkPartitioner` instance gets a unique iptables chain (e.g. `IRYS_T_<pid>_<seq>`) so concurrent test processes don't interfere. The chain is fully torn down when all active injections on that instance are undone.

## Existing Test Suite

### End-to-End Tests (`tests/e2e.rs`)

| Test | Nodes | What it verifies |
|------|-------|-----------------|
| `single_genesis_produces_blocks` | 1 | Genesis node reaches height 3 |
| `same_version_two_node_convergence` | 2 | Two nodes on same version converge |
| `three_node_cluster_convergence` | 3 | Three-node cluster reaches consensus |

### Upgrade Tests (`tests/upgrade.rs`)

| Test | Nodes | What it verifies |
|------|-------|-----------------|
| `upgrade_one_node_in_running_cluster` | 3 | Upgrade one peer, cluster re-converges |
| `rolling_upgrade_all_nodes` | 3 | Sequentially upgrade all nodes one by one |
| `rollback_after_upgrade` | 2 | Upgrade then downgrade back to old version |
| `crash_during_upgrade` | 2 | Kill a node, then upgrade it from crashed state |

## CI

The multiversion tests run in GitHub Actions (`.github/workflows/multiversion.yml`):

- **Triggers**: Push to `master`, approved pull-request review, or manual `workflow_dispatch`
- **Runner**: `[self-hosted, misc-runner]`
- **Timeout**: 60 minutes
- Tests run with `--test-threads=1` (sequential) to avoid resource contention

## Architecture

```text
crates/tooling/multiversion-tests/
├── fixtures/
│   └── base-config.toml     # Node config template (ports, peers, consensus)
├── src/
│   ├── lib.rs                # URL helpers, module exports
│   ├── binary.rs             # BinaryResolver: build + cache versioned binaries
│   ├── cluster.rs            # Cluster: multi-node orchestration + upgrade
│   ├── config.rs             # TOML config generation + consensus patching
│   ├── ports.rs              # Ephemeral port allocation with guard pattern
│   ├── probe.rs              # HTTP polling (/v1/info, /v1/genesis, convergence)
│   ├── process.rs            # NodeProcess: spawn, shutdown, freeze, log capture
│   └── fault/
│       ├── mod.rs            # FaultInjector trait + FaultGuard (RAII cleanup)
│       ├── crash.rs          # ProcessFreezer (SIGSTOP) + ProcessKiller (SIGKILL)
│       └── network.rs        # NetworkPartitioner (iptables-based packet dropping)
└── tests/
    ├── common/mod.rs         # Shared helpers (specs, keys, timeouts)
    ├── e2e.rs                # Single-version cluster tests
    └── upgrade.rs            # Cross-version upgrade/rollback tests
```

This is a workspace crate at `crates/tooling/multiversion-tests/`, part of the main Cargo workspace.

## Timeouts Reference

| Constant | Value | Used for |
|----------|-------|----------|
| `READY_TIMEOUT` | 120s | Node startup (responds to `/v1/info`) |
| `SHUTDOWN_TIMEOUT` | 30s | Graceful shutdown before SIGKILL |
| `HEIGHT_TIMEOUT` | 120s | Waiting for a node to reach a target block height |
| `CONVERGENCE_TIMEOUT` | 120s | Waiting for all nodes to agree on chain height |
| `GIT_TIMEOUT` | 30s | Git operations (rev-parse, worktree add, worktree prune) |
| `CARGO_BUILD_TIMEOUT` | 1200s | Building a binary from source |
