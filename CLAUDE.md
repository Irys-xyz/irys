# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Development Commands

```sh
cargo xtask check              # Quick compile check
cargo xtask test               # Run tests via nextest with failure tracking
cargo xtask test --rerun-failures  # Re-run only previously failed tests
cargo xtask local-checks       # Run ~99% of CI checks (fmt, check, clippy, unused-deps, typos)
cargo xtask local-checks --fix # Auto-fix issues
cargo xtask flaky -i 5         # Run tests 5 times to detect flakiness
```

Run a single test:
```sh
cargo nextest run -p irys-actors test_name
cargo test -p irys-actors test_name  # alternative without nextest
```

### Before Pushing or Creating PRs

Always run these checks and fix any issues:

1. `cargo fmt --all`
2. `cargo clippy --workspace --tests --all-targets`

### Toolchain

Rust 1.93.0, edition 2024 (pinned in `rust-toolchain.toml`). Requires clang, gmp, pkg-config for OpenSSL build.

### Environment Notes

- macOS: Increase open file limit (default 256 is too low for some tests) — see README.md
- `IRYS_CUSTOM_TMP_DIR` env var overrides the `./.tmp` test temp directory

### Reading Dependency Source Code

When you need to understand a dependency's API or internals, read its source directly from the cargo registry cache at `~/.cargo/registry/src/`. Use glob/grep to find the crate directory (e.g. `~/.cargo/registry/src/*/nodit-0.9*`). You are free to do this at any time.

## Architecture Overview

Irys is a **decentralized data storage blockchain** combining VDF-paced Proof-of-Access consensus with EVM execution via a Reth fork. The main binary is `crates/chain` (`IrysNode`).

### Crate Dependency Layers

```
Foundation:  irys-types, irys-config, irys-database
Domain:      irys-domain (BlockTree, BlockIndex, StorageModule, SupplyState)
Services:    irys-actors (all services), irys-p2p, irys-api-server (actix-web)
Integration: irys-reth + irys-reth-node-bridge (EVM via Reth fork)
Subsystems:  irys-packing, irys-packing-worker, irys-storage, irys-vdf
Utilities:   irys-utils, irys-testing-utils, irys-cli, xtask
```

### Actor/Service System

**Not Actix actors** — uses a custom Tokio channel-based message-passing pattern:
- `ServiceSenders` — centralized hub of `UnboundedSender` channels (Arc-wrapped)
- `ServiceReceivers` — one receiver per service, each running in its own tokio task
- Broadcast events (`ReorgEvent`, `BlockMigratedEvent`, `MiningBroadcastEvent`) for system-wide notifications

Key services in `crates/actors/src/`:
- **BlockProducerService** — assembles blocks from mining solutions + mempool txs + shadow txs
- **BlockTreeService** — fork choice / canonical chain
- **ValidationService** — two-phase: structural validation then state validation (PoA + EVM)
- **PartitionMiningService** — per-partition PoA mining
- **PackingService** — entropy packing/unpacking via C/CUDA
- **MempoolService** — transaction pool
- **DataSyncService** — syncs chunk data from peers
- **RethService** — interfaces with the Reth EVM node

### Consensus: VDF + Proof of Access

1. **VDF** (`crates/vdf`) — SHA256-based sequential hashing providing network timing (~1 step/sec)
2. **PoA mining** (`partition_mining_service`) — miners prove data storage: `hash(packed_chunk + vdf_step + nonce) < difficulty`
3. **Block production** — solution found → collect mempool txs → generate shadow txs → build Reth payload → assemble block → propagate

### CL/EL Split (Consensus Layer / Execution Layer)

- **CL (Irys)**: block production, PoA consensus, storage commitments, block tree
- **EL (Reth fork)**: EVM state transitions, eth_* JSON-RPC
- **Shadow transactions**: protocol-level actions (rewards, storage fees, staking) encoded as EVM txs sent to `SHADOW_TX_DESTINATION_ADDR` and executed via `IRYS_SHADOW_EXEC` precompile
- Integration via payload building / fork choice update / payload validation

### Storage & Packing

Data is split into 256KB chunks, XOR-encrypted with entropy derived from `(miner_address, partition_hash, chunk_offset, chain_id)`. Packing makes mining data-independent. Unpacking reverses XOR for retrieval.

Storage modules (`crates/domain`) manage partition assignments. Lifecycle: Active → Expiring → Expired → Defunct.

### P2P (`crates/p2p`)

HTTP-based gossip protocol. Routes: `/gossip/block`, `/gossip/tx`, `/gossip/chunk`. Includes circuit breaker pattern and per-peer rate limiting.

## Git Conventions

Never add "Co-Authored-By" lines to commit messages.

## Testing Conventions

- `#[tokio::test]` for async, `#[test_log::test(tokio::test)]` for tracing output in tests
- `NodeConfig::testing()` / `ConsensusConfig::testing()` for test configurations
- `rstest` for parameterized tests, `proptest` for property-based tests
- Failure tracking: `cargo xtask test` records failures to `target/nextest-failure-tracking/failures.json`
- Crypto crates (`irys-vdf`, `irys-packing`, `irys-c`, `sha2`, `openssl`) are always compiled with `opt-level = 3`, even in dev builds

### Local changes

NOTE: all the above instructions can be overridden and added to as instructed in `./.claude/local/CLAUDE.md` - if it exists, read it and follow any listed instructions before continuing.