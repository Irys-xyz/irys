# Multiversion Test Harness

Integration test framework for verifying that Irys nodes running different binary versions can form clusters, produce blocks, propagate transactions, and converge on a canonical chain. Catches protocol-breaking and data-format-breaking changes before they reach production.

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
```

Tests are marked `#[ignore]` because they build full `irys` binaries from source, which takes several minutes on first run. The xtask handles `--ignored` automatically; when running directly via `cargo test` you must pass it yourself.

For cross-version runs you'll typically pass at least `--old-ref <ref>` and `--new-ref <ref>`. For wider version spans where the `NodeConfig` or wire-format schemas have drifted, see [Cross-Version Configuration](#cross-version-configuration).

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

## Cross-Version Configuration

The harness ships with two layers of optional, file-based configuration that let it drive arbitrary OLD↔NEW spans without baking any particular version pair's quirks into the harness code itself. Both are **optional** — for adjacent-release tests (e.g. `vN` → `vN+1`) you typically don't need either, and the bundled defaults work.

You'll reach for them when:

- The OLD ref's `NodeConfig` / `[consensus]` TOML schema has drifted enough that one bundled template can't satisfy both binaries' `deny_unknown_fields` parsers → use **base-config templates**.
- The OLD ref's wire format for `DataTransactionHeader` or `BlockHeader` has drifted (renames, retypes, added fields) → use a **run config** to teach the strict cross-version comparison about those changes.

### Per-Version Base-Config Templates

`--base-config-old <path>` and `--base-config-new <path>` point to TOML templates the harness uses when generating each node's `config.toml`. The cluster picks the template based on which binary kind a given node is running, so OLD nodes can run with one schema and NEW nodes with another.

For peer nodes, the template's `[consensus.Custom]` block (if any) is overlaid on top of whatever the genesis serves at `/v1/network/config`. This is the cross-version-schema escape hatch: bare `consensus = "Testing"` in the template means "fetch consensus from genesis verbatim"; a hand-authored `[consensus.Custom]` block means "overlay these fields on top of what the genesis serves." Use this when the new binary requires consensus fields the old genesis doesn't know about.

When omitted, both kinds fall back to `fixtures/base-config.toml`. Sample templates for the d071fc03 ↔ HEAD span live in `examples/base-config-old.toml` and `examples/base-config-new.toml`.

### Run Config (`--run-config`)

A small TOML file with knobs for **what to compare** and **how to construct test transactions**. Pointed to by `--run-config <path>`; surfaced to the test harness via the `IRYS_TEST_RUN_CONFIG` env var. Default (no flag) is the empty config: no aliases, no skips, every applicable header field gets exercised non-default. That's the right default for adjacent-release tests where renames are rare.

```toml
# Cross-version handling for /v1/tx/{id} responses, compared against
# the originally-signed tx via a strict full-header diff.
[tx_header]
# camelCase field name pairs that mean the same thing across the OLD
# and NEW schemas (e.g. after a rename). Field presence is verified on
# both sides; values are NOT compared since a rename can also change
# types.
aliases = [["bundleFormat", "metadataFormat"]]
# Field names to skip entirely from the strict diff. Use when a field's
# wire shape differs between OLD and NEW in a way `aliases` doesn't
# model — e.g. a field that only exists on one side.
skip = ["promotedHeight"]

# Same shape, applied to /v1/block/{hash} responses compared across
# every running node.
[block_header]
aliases = []
skip = []

# Tx-construction overrides applied before signing.
[tx_build]
# By default the harness pokes a few normally-default header fields
# (e.g. metadata_format=1, header_size=64) to non-default sentinels so
# the on-disk Compact encoding actually exercises non-zero payload
# bytes — without that, fields whose default-value encoding is
# zero-byte trivially survive any schema change. Naming a field here
# (Rust struct field name, not the camelCase wire name) keeps it at
# its default. Useful for spans that straddle a rename: a non-default
# value would change the canonical signature prehash on one side and
# the OLD side would (correctly) reject the tx as `Invalid Signature`.
keep_default = ["metadata_format"]
```

A worked example for the d071fc03 ↔ HEAD span lives in `examples/run-config-d071fc03.toml`.

### Putting It All Together

```sh
cargo xtask multiversion-test \
  --profile dev \
  --old-ref d071fc03af2721a6eb58ffcc20fd234162f49ecc \
  --new-ref CURRENT \
  --base-config-old crates/tooling/multiversion-tests/examples/base-config-old.toml \
  --base-config-new crates/tooling/multiversion-tests/examples/base-config-new.toml \
  --run-config     crates/tooling/multiversion-tests/examples/run-config-d071fc03.toml \
  -- --no-fail-fast
```

For adjacent releases you'd typically just need the refs:

```sh
cargo xtask multiversion-test --old-ref v1.5.0 --new-ref v1.6.0
```

### What the Strict Checks Actually Verify

Every test that submits transactions runs three layers of cross-version assertions:

1. **Visibility** (status-code only) — every node responds 2xx to `/v1/tx/{id}`. Cheap; catches missing data.
2. **Strict tx-header round-trip** — fetches `/v1/tx/{id}` from every running node and asserts the returned JSON header matches the originally-signed `DataTransaction` byte-for-byte across every shared field (modulo the run config's `aliases` and `skip`). Catches silent corruption from a binary mis-decoding `Compact`-encoded records that another version wrote.
3. **Block-header cluster consistency** — enumerates `/v1/block-index?height=0&limit=500` from genesis, then for each block hash fetches `/v1/block/{hash}` from every node and diffs the responses against the genesis's view. Catches drift in enum-tagged sub-fields (PoA, ledger metadata, signatures) that the tx-header check would miss.

The "tx the harness actually submits" carries non-default values for `metadata_format` and `header_size` by default, so the strict checks aren't vacuous on fields whose default-value `Compact` encoding is a zero-byte payload. Use `[tx_build] keep_default` to opt out per field when those values would invalidate the canonical signature on the older side.

## Artifact Preservation

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

The xtask sets these for you when you pass the matching CLI flag, but you can also set them directly when running `cargo test` outside the xtask.

| Variable | xtask flag | Default | Effect |
|----------|-----------|---------|--------|
| `IRYS_OLD_REF` | `--old-ref` | `CURRENT` | Git ref for the old binary, or `CURRENT` for working tree |
| `IRYS_NEW_REF` | `--new-ref` | `CURRENT` | Git ref for the new binary, or `CURRENT` for working tree |
| `IRYS_BINARY_OLD` | `--binary-old` | unset | Path to pre-built old binary (skips building entirely) |
| `IRYS_BINARY_NEW` | `--binary-new` | unset | Path to pre-built new binary (skips building entirely) |
| `IRYS_BUILD_PROFILE` | `--profile` | unset | Cargo profile (`dev`, `release`, etc.) |
| `IRYS_BASE_CONFIG_OLD` | `--base-config-old` | unset (uses bundled `fixtures/base-config.toml`) | Path to a TOML base-config template for nodes running the OLD binary |
| `IRYS_BASE_CONFIG_NEW` | `--base-config-new` | unset (uses bundled `fixtures/base-config.toml`) | Path to a TOML base-config template for nodes running the NEW binary |
| `IRYS_TEST_RUN_CONFIG` | `--run-config` | unset (empty default) | Path to a TOML run-config (cross-version comparison knobs) — see [Run Config](#run-config---run-config) |

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
| `cluster_spec(test_name, nodes)` | Bundle nodes into a `ClusterSpec` for **single-version** flows. Always uses the bundled fixture for both base configs and the empty `RunConfig` — env-var overrides are intentionally ignored here. |
| `cluster_spec_with_refs(test_name, nodes, old_ref, new_ref)` | Same, but for cross-version flows. Loads `IRYS_BASE_CONFIG_OLD` / `IRYS_BASE_CONFIG_NEW` (templates) and `IRYS_TEST_RUN_CONFIG` (run config) from env. |
| `base_config_old()` / `base_config_new()` | Return the OLD / NEW template TOML as a string, falling back to the bundled `fixtures/base-config.toml` when the env var is unset. |
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

// Read chain params from the running genesis (chain_id, chunk_size,
// block_migration_depth) — used to build signers + pace transitions.
let chain_params = cluster.fetch_chain_params().await?;

// Submit a Publish-ledger data tx through `peer-1`, then poll
// /v1/tx/{id} on every running node until it returns 2xx.
let tx = cluster.submit_and_verify_data(
    "peer-1",
    b"my payload".to_vec(),
    chain_params.chain_id,
    chain_params.chunk_size,
    common::CONVERGENCE_TIMEOUT,
).await?;

// Strict full-header round-trip: every running node must return
// JSON whose every field matches the originally-signed tx (modulo
// run-config aliases / skips). Catches silent corruption.
cluster.assert_tx_matches_on_all_nodes(&tx).await?;

// Cross-node block-header consistency: enumerates /v1/block-index
// from genesis, fetches each block from every running node, and
// asserts byte-identical responses. Catches drift in enum-tagged
// sub-fields (PoA, ledger metadata, signatures).
cluster.assert_block_index_consistent().await?;

// Wait for cluster height to advance past `target` — useful for
// forcing tx records past `block_migration_depth` so they actually
// land in the persistent IrysDataTxHeaders table before a binary swap.
cluster.wait_for_height_at_least(target_height, timeout).await?;
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

Every test below also runs the [data-tx submission + strict cross-version assertions](#what-the-strict-checks-actually-verify) described above.

### End-to-End Tests (`src/tests/e2e.rs`)

Single-version flows. The `IRYS_BASE_CONFIG_*` and `IRYS_TEST_RUN_CONFIG` env vars are intentionally ignored here — fresh clusters always boot from the bundled fixture.

| Test | Nodes | What it verifies |
|------|-------|-----------------|
| `single_genesis_produces_blocks` | 1 | Genesis reaches height 3 + serves a submitted data tx via `/v1/tx/{id}` |
| `same_version_two_node_convergence` | 2 | Two nodes converge + a tx posted to a peer reaches the genesis |
| `three_node_cluster_convergence` | 3 | Three-node cluster reaches consensus + cross-peer gossip works in both directions |

### Upgrade Tests (`src/tests/upgrade.rs`)

Cross-version flows. Each transition is bracketed by data-tx submissions so the strict checks have something to compare; the rollback test also waits past `block_migration_depth` to force on-disk persistence before the binary swap so post-migration records actually get exercised.

| Test | Nodes | What it verifies |
|------|-------|-----------------|
| `upgrade_one_node_in_running_cluster` | 3 | Upgrade one peer, cluster re-converges, NEW peer can read OLD-written tx + write fresh tx OLD peers accept |
| `rolling_upgrade_all_nodes` | 3 | Sequentially upgrade all nodes; each freshly-upgraded node submits + verifies full tx history |
| `rollback_after_upgrade` | 2 | Upgrade → wait past `block_migration_depth` → submit more under NEW → roll back → strict-verify pre-rollback writes survive + OLD accepts new writes |
| `crash_during_upgrade` | 2 | Kill a node, upgrade it from crashed state, verify it recovers prior history + accepts new writes |

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
│   └── base-config.toml          # Default node config template (used when --base-config-* is omitted)
├── examples/
│   ├── base-config-old.toml      # Sample template for the d071fc03 OLD ref
│   ├── base-config-new.toml      # Sample template for HEAD
│   └── run-config-d071fc03.toml  # Sample run config for the d071fc03 ↔ HEAD span
├── src/
│   ├── lib.rs                    # URL helpers, module exports
│   ├── binary.rs                 # BinaryResolver: build + cache versioned binaries
│   ├── cluster.rs                # Cluster: multi-node orchestration + upgrade + cross-version assertions
│   ├── config.rs                 # TOML config generation + consensus patching/overlay
│   ├── data_tx.rs                # /v1/tx submission, strict header diff, /v1/block-index sweep
│   ├── run_config.rs             # RunConfig (--run-config TOML): aliases, skip lists, tx-build knobs
│   ├── ports.rs                  # Ephemeral port allocation with guard pattern
│   ├── probe.rs                  # HTTP polling (/v1/info, /v1/network/config, convergence)
│   ├── process.rs                # NodeProcess: spawn, shutdown, freeze, log capture
│   ├── fault/
│   │   ├── mod.rs                # FaultInjector trait + FaultGuard (RAII cleanup)
│   │   ├── crash.rs              # ProcessFreezer (SIGSTOP) + ProcessKiller (SIGKILL)
│   │   └── network.rs            # NetworkPartitioner (iptables-based packet dropping)
│   └── tests/                    # Test files live in src/ so the harness modules can be private/pub-crate
│       ├── mod.rs                # Test module roots
│       ├── helpers.rs            # Shared helpers (specs, keys, timeouts, base-config + run-config loading)
│       ├── e2e.rs                # Single-version cluster tests
│       └── upgrade.rs            # Cross-version upgrade/rollback tests
└── README.md
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

## Future Improvements

The harness in its current form catches a meaningful slice of cross-version regressions, but there are several gaps where coverage is shallower than it could be. None of these are urgent — they're tracked here so future operators have a working list when (a) a real regression slips past the current checks, or (b) someone wants to tighten the rollback signal specifically.

### Coverage gaps

- **Chunk-level read-back.** We upload chunks via `/v1/chunk` so promotion can happen, but the strict post-transition checks never fetch them back. Storage-module schemas evolve independently of `IrysDataTxHeaders` (different MDBX tables, different `Compact` derivations), so a migration that breaks chunk decoding wouldn't surface in any current test. Adding a `cluster.assert_chunk_for_tx_consistent(tx)` that GETs `/v1/chunk/{ledger}/{data_root}/{offset}` from every node and diffs — analogous to the existing block-index sweep — would close this.
- **Non-default values for renamed fields.** `tx_build.keep_default` exists specifically so non-default `metadata_format` doesn't trip OLD signature validation across the d071fc03 ↔ HEAD rename. For adjacent-release tests where renames don't apply, dropping that entry exercises actual non-zero `Compact` payload bytes for that field. A nice follow-up would be a CI matrix entry that runs *without* `keep_default` against a recent enough OLD ref to prove the field is genuinely round-trip-safe.
- **Long-running chains under rollback.** The rollback test populates only a few txs and waits past `block_migration_depth` once. Persistent-corruption modes that only manifest after many migrated blocks (e.g. migration-state inconsistency between `IrysBlockHeaders` and `IrysDataTxMetadata`) wouldn't surface. A variant that runs the chain for several hundred blocks under NEW before rolling back, with periodic submissions throughout, would catch more.
- **Promotion-state preservation under true isolation.** Current promotion checks pass on the rolled-back peer in part because gossip catch-up rebuilds the peer's `IrysDataTxMetadata` table from blocks delivered after the binary swap. To prove peer-1's *own* on-disk state is sufficient post-rollback, we'd need to either (a) inspect the MDBX file directly with an OLD-schema reader, or (b) add a no-gossip mode (firewall the peer with `iptables` post-rollback). Earlier in this branch's history we tried (b) but hit OLD's startup requirement that a trusted peer be reachable; adding a `--bootstrap-from-disk` flag to the node binary would unblock it.
- **Network-partition + upgrade combinations.** The `fault` module already provides `NetworkPartitioner` with iptables-based packet dropping. No current test combines a partition with an upgrade. A scenario like "partition peer-2 from genesis, upgrade peer-1, heal partition, verify peer-2 catches up correctly" would exercise gossip recovery against version-mixed peers — a class of bug the current happy-path tests would miss.
- **Commitment txs (stake / pledge / unstake).** Every test today exercises only data txs. Commitment txs go through `IrysCommitments` — a different table with a different `Compact` derivation — and are how staking and pledges propagate. Migrations to that table would be invisible to the current suite.

### Harness ergonomics

- **End-to-end run time.** A full-suite run is ~4–6 min. Most of that is sequential `--test-threads=1` execution to avoid port + tmpfs contention. With per-test isolated tmpfs subdirs and a small bump to the port-allocation range, several tests could run in parallel.
- **Build cache reuse for `--new-ref CURRENT`.** The harness rebuilds the working-tree binary on every run because cargo's fingerprinting decides whether to rebuild. A persistent test-data dir keyed by working-tree hash (or a sentinel file checked against `git diff --stat`) would let consecutive same-tree runs skip the rebuild.
- **Better failure-time artifact summaries.** Today, when a test panics, the harness preserves the full `target/multiversion/test-data/{run_id}/` tree but doesn't produce a summary. A post-failure step that scans node logs for the first `ERROR` / first panic and prints a one-line cause per test would shorten the debug loop.
- **Parameterizing the test set.** Right now the test functions are hardcoded against the harness's specific specs. Reframing them as table-driven cases (e.g. `#[rstest]` with parametric `(node_count, upgrade_order, fault_pattern)`) would let CI run a wider matrix without proportional code growth.

### Comparison-strictness improvements

- **Per-tx commitment check on the genesis side.** The block-index sweep proves every node agrees on each block's full content, but it doesn't directly assert that the genesis's signed-block hash matches the block's serialized form (in case some field is included in the hash but excluded from the JSON envelope). A round-trip via `block.signature_hash()` would catch that.
- **Detect drift in the run config itself.** If a future schema rename happens and nobody updates the example run config, the d071fc03 ↔ HEAD test would silently start passing for the wrong reason. A separate test that fails when the run config carries an alias for a field that doesn't actually exist on either OLD or NEW (i.e. is over-permissive) would prevent silent rot.
- **`compare_full_object` could surface multiple field mismatches.** Today it short-circuits on the first mismatch. For investigating large drifts, it'd be more useful to collect every diff and print a unified report.

### Documentation

- **Walked-through examples for two more spans.** This README has a complete worked example for the d071fc03 ↔ HEAD span and a one-line example for adjacent releases. Adding a third intermediate case (e.g. one major-version step where only one of `--base-config-old` / `--run-config` is needed) would make the "how to set this up for a new span" decision easier for future operators.
- **Run-config field reference.** The TOML structure is documented in the inline `[tx_header]` / `[tx_build]` example, but a separate "every field, what it does" table would be easier to skim.
