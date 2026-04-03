# Multi-Miner Genesis Commitments: Unification Plan

**Date:** 2026-04-01
**Status:** Draft
**Branches:** `feat/genesis-cli` (foundation), `deployment/testnet` (hacks to replace)

## Problem Statement

The current genesis block creation flow (`get_genesis_commitments()` in `system_ledger.rs`) assumes a **single miner** — the genesis block producer — creates all stake and pledge commitments from its own signing key. For multi-miner networks (testnet, mainnet), all participating miners need their commitments included in the genesis block.

The `deployment/testnet` branch worked around this with 5 hacks:

1. **Hardcoded JSON blobs** — 106 pre-signed `CommitmentTransaction` objects from 9 miners pasted as string literals in `system_ledger.rs`
2. **Hardcoded commitment ordering** — duplicate `ordered_ids` arrays in both `system_ledger.rs` and `epoch_snapshot.rs` to force a specific pledge-to-partition assignment
3. **Hardcoded `storage_module_count = 8`** — overriding the `commitments.len() - 1` formula in `chain.rs` which breaks with multi-miner commitment counts
4. **Hardcoded peer balances** — 9 EVM addresses with 10M token overrides in `dump.rs`
5. **Debug logging** — `JESSEDEBUG` traces for extracting partition assignment data

These hacks required code changes and recompilation for every genesis configuration change.

## Existing Work

### `feat/genesis-cli` branch (base: `7619da858`, ~1 month behind master)

This branch already provides a solid foundation with:

| Component | File | Description |
|---|---|---|
| `GenesisMinerManifest` | `genesis_builder.rs` | TOML config (`genesis_miners.toml`) mapping mining keys to pledge counts |
| `build_signed_genesis_block()` | `genesis_builder.rs` | Core function: takes `Config` + `Vec<GenesisMinerEntry>`, produces signed genesis block with all commitments |
| `generate_multi_miner_commitments()` | `genesis_builder.rs` | Per-miner stake + N pledges, globally rotating anchors for unique IDs |
| `build-genesis` CLI subcommand | `cli/main.rs` | `irys-cli build-genesis --miners genesis_miners.toml --output .` |
| Commitment disk I/O | `genesis_utilities.rs` | `save_genesis_commitments_to_disk()` / `load_genesis_commitments_from_disk()` for `.irys_genesis_commitments.json` |
| `IrysNode` delegation | `chain.rs` | `create_new_genesis_block()` delegates to `build_signed_genesis_block()` with a single-miner entry |

### What master already has

| Feature | Location | Notes |
|---|---|---|
| `reth.alloc` config field | `ConsensusConfig::IrysRethConfig` | `BTreeMap<Address, GenesisAccount>` — peer account funding is configurable in TOML without code changes |
| `extend_genesis_accounts()` | `ConsensusConfig` | Runtime helper for adding accounts |
| Genesis block JSON persistence | `genesis_utilities.rs` | `.irys_genesis.json` save/load |
| Trusted peer genesis fetch | `peer_utilities.rs` | `fetch_genesis_block()` + `fetch_genesis_commitments()` |
| Deterministic pledge sorting | `epoch_snapshot.rs:1014` | `unassigned_pledges.sort_unstable_by(\|a, b\| a.id.cmp(&b.id))` |

## Gap Analysis

### Gaps addressed by `feat/genesis-cli`

| Testnet Hack | Solution |
|---|---|
| Hardcoded JSON commitment blobs | `GenesisMinerManifest` + `build_signed_genesis_block()` generates commitments from keys |
| Hardcoded SM count | `feat/genesis-cli` uses `total_pledges` — still incorrect (see "Initial difficulty" gap below) |
| No CLI tooling | `irys-cli build-genesis` subcommand |

### Remaining gaps

| Gap | Severity | Details |
|---|---|---|
| **Rebase needed** | High | Branch is ~1 month behind master. `build_unsigned_irys_genesis_block` return type changed from `IrysBlockHeader` to `eyre::Result<IrysBlockHeader>` on master. |
| **Non-producer node startup with external commitments** | Medium | `IrysNode` can generate genesis (single-miner) or fetch from peer, but cannot load pre-built multi-miner genesis from disk files. Nodes that aren't the genesis producer and can't reach a trusted peer have no path. |
| **Funded peer accounts workflow** | Low | Already solvable via `reth.alloc` in TOML config — just needs documentation. No code change required. |
| **Partition assignment determinism verification** | Medium | The testnet hack forced custom pledge ordering in `epoch_snapshot.rs`. Need to confirm that IDs from `build_signed_genesis_block()` produce correct partition assignments under the normal `sort_by(id)`. |
| **Initial difficulty based on wrong metric** | High | Both master (`commitments.len() - 1`) and `feat/genesis-cli` (`total_pledges`) calculate difficulty from pledge count. The correct input is **number of packed partitions** — at genesis, partitions are pledged but empty, so the actual hashrate depends on how much data is packed. This should be a configurable float in `GenesisConfig`, falling back to total pledge count if unset. |

## Implementation Plan

### Task 1: Rebase `feat/genesis-cli` onto master

**Goal:** Bring the branch up to date and resolve conflicts.

**Files affected:**
- `crates/chain/src/genesis_builder.rs` — update `build_unsigned_irys_genesis_block` call to handle `Result` return type
- `crates/chain/src/chain.rs` — resolve merge conflicts from recent refactors
- `Cargo.lock` — regenerate

**Key conflict:** `build_unsigned_irys_genesis_block` now returns `eyre::Result<IrysBlockHeader>` on master. The genesis_builder.rs call needs to propagate the `?`:

```rust
// Before (genesis-cli branch):
let mut genesis_block = build_unsigned_irys_genesis_block(
    &config.consensus.genesis,
    reth_chain_spec.genesis_hash(),
    number_of_ingress_proofs_total,
);

// After (reconciled with master):
let mut genesis_block = build_unsigned_irys_genesis_block(
    &config.consensus.genesis,
    reth_chain_spec.genesis_hash(),
    number_of_ingress_proofs_total,
)?;
```

**Acceptance criteria:**
- `cargo xtask local-checks` passes
- `cargo xtask test` passes (existing tests unbroken)
- `irys-cli build-genesis` produces valid output with a test manifest

---

### Task 2: Configurable initial packed partitions for difficulty

**Goal:** Replace the incorrect pledge-count-based difficulty calculation with a configurable "initial packed partitions" value.

**Problem:** `calculate_initial_difficulty()` takes `fully_packed_storage_modules: f64` — a float representing how many partitions are fully packed with data. At genesis, partitions are pledged but **empty** (unpacked). The actual mining hashrate is `num_chunks_in_recall_range * packed_partitions`, so using pledge count overstates the hashrate at launch. The correct input is the expected number of packed partitions, which depends on how much pre-seeded data exists or how quickly packing is expected to ramp up.

**Files:**
- Modify: `crates/types/src/config/consensus.rs` — add field to `GenesisConfig`
- Modify: `crates/chain/src/genesis_builder.rs` — use new config field
- Modify: `crates/chain/src/chain.rs` — use new config field in single-miner path (until delegated to genesis_builder)

**Config change:**
```rust
// In GenesisConfig:
/// Number of packed partitions to use for initial difficulty calculation.
/// This is a float to support partial-packing scenarios (e.g., 2.5 means
/// two fully packed partitions and one half-packed).
/// If not set, defaults to the total number of pledge commitments.
#[serde(default)]
pub initial_packed_partitions: Option<f64>,
```

**Usage in genesis_builder.rs:**
```rust
let packed_partitions = config.consensus.genesis.initial_packed_partitions
    .unwrap_or(total_pledges as f64);
let difficulty = calculate_initial_difficulty(&config.consensus, packed_partitions)?;
```

**Example configs:**
```toml
# Testnet with 80 pledged partitions but expecting ~8 packed at launch
[consensus.genesis]
initial_packed_partitions = 8.0

# Default behavior — omit the field, uses pledge count
[consensus.genesis]
# initial_packed_partitions not set
```

**Acceptance criteria:**
- When `initial_packed_partitions` is set, difficulty uses that value
- When omitted, difficulty falls back to total pledge count (backwards compatible)
- Existing tests pass without config changes (they don't set the field)

---

### Task 3: Support loading pre-built genesis on non-producer nodes

**Goal:** Allow nodes that aren't the genesis block producer to start from pre-built genesis files (`.irys_genesis.json` + `.irys_genesis_commitments.json`) without needing a trusted peer.

**Files:**
- Modify: `crates/chain/src/chain.rs` — `get_or_create_genesis_info()` / startup flow

**Current startup flow:**
```
NodeMode::Genesis → create_new_genesis_block()     [single-miner, local key]
NodeMode::Peer    → fetch_genesis_from_trusted_peer() [HTTP fetch]
```

**New startup flow:**
```
NodeMode::Genesis →
  if genesis files exist on disk →
    load_genesis_block_from_disk() + load_genesis_commitments_from_disk()
    validate signature + expected_genesis_hash (if configured)
    persist_genesis_block_and_commitments()  [REQUIRED — downstream reads from DB]
  else →
    create_new_genesis_block()        [single-miner, local key]

NodeMode::Peer →
  if genesis files exist on disk →
    load_genesis_block_from_disk() + load_genesis_commitments_from_disk()
    validate signature + expected_genesis_hash
    persist_genesis_block_and_commitments()  [REQUIRED — downstream reads from DB]
  else →
    fetch_genesis_from_trusted_peer()  [HTTP fetch]
```

**Critical implementation detail:** After loading from disk, `persist_genesis_block_and_commitments()` must still be called when `block_index.num_blocks() == 0`. All downstream code — `EpochReplayData`, block index, block tree initialization — reads from the DB, not from disk files.

This means the workflow for a multi-miner genesis becomes:
1. Genesis coordinator runs `irys-cli build-genesis --miners manifest.toml --output ./genesis-artifacts/`
2. Distribute `.irys_genesis.json` + `.irys_genesis_commitments.json` to all nodes (copy into each node's `base_directory`)
3. All nodes start and find the files on disk — no trusted peer needed, no code changes

**Validation:** When loading from disk, perform the same checks as peer-fetched blocks:
1. Validate block signature via `is_signature_valid()`
2. Validate block hash against `consensus.expected_genesis_hash` (mandatory for `Peer` mode; checked if configured for `Genesis` mode)
3. Validate that commitment-ledger txids match the loaded commitment transactions (count and membership)
4. Require both `.irys_genesis.json` and `.irys_genesis_commitments.json` to exist together — partial presence is a hard error

**Acceptance criteria:**
- A node configured as `Peer` with genesis files on disk starts successfully without a trusted peer
- A node configured as `Genesis` with genesis files on disk uses them instead of regenerating
- Signature and hash validation rejects tampered genesis files
- Partial file presence (only one of the two files) produces a clear error
- After disk load, `EpochReplayData` and block tree init work correctly (DB persistence verified)

---

### Task 4: Verify partition assignment determinism

**Goal:** Confirm that commitments generated by `build_signed_genesis_block()` produce the same partition assignments on every node.

**Files:**
- Possibly modify: `crates/domain/src/snapshots/epoch_snapshot/epoch_snapshot.rs` (only if verification fails)

**Analysis:**

The existing `epoch_snapshot.rs` sorts unassigned pledges by `id` (line 1014):
```rust
unassigned_pledges.sort_unstable_by(|a, b| a.id.cmp(&b.id));
```

Commitment IDs are derived from `Keccak256(signature)`. Since the signing key, anchor chain, and pledge data are all deterministic given the same `GenesisMinerManifest` + `Config`, the IDs will be identical on every node that processes the same genesis block.

The testnet hack needed a custom ordering because the commitments were **pre-signed externally** with potentially different anchor chains, producing IDs that didn't sort to the desired partition mapping. With `build_signed_genesis_block()` generating everything deterministically from keys, the normal `sort_by(id)` should produce consistent results.

**Verification approach:**
- Write a test that:
  1. Creates a `GenesisMinerManifest` with 3+ miners, varying pledge counts
  2. Calls `build_signed_genesis_block()` twice with the same inputs
  3. Asserts identical commitment IDs and ordering
  4. Replays the commitments through `EpochSnapshot::new(...)` / `compute_commitment_state()` + `assign_partition_hashes_to_pledges()`
  5. Asserts identical partition assignments

**Acceptance criteria:**
- Test passes confirming deterministic output
- No custom epoch_snapshot ordering hack needed

---

### Task 5: Document the genesis workflow

**Goal:** Provide clear operator documentation for multi-miner genesis setup.

**Files:**
- Create: `docs/40-mining/genesis-setup.md` (or similar)

**Content outline:**

1. **Create `genesis_miners.toml`:**
   ```toml
   [[miners]]
   mining_key = "hex-encoded-secp256k1-private-key"
   pledge_count = 8

   [[miners]]
   mining_key = "..."
   pledge_count = 3
   ```

2. **Fund miner accounts in consensus config** (`config.toml`):
   ```toml
   [consensus.reth.alloc."0xMinerEvmAddress1"]
   balance = "10000000000000000000000000"

   [consensus.reth.alloc."0xMinerEvmAddress2"]
   balance = "10000000000000000000000000"
   ```

3. **Build genesis:**
   ```bash
   irys-cli build-genesis --miners genesis_miners.toml --output ./genesis/
   ```

4. **Distribute files to all nodes:**
   - Copy `.irys_genesis.json` and `.irys_genesis_commitments.json` into each node's `base_directory`
   - Add `consensus.expected_genesis_hash = "<hash>"` to each node's config

5. **Start nodes** — all nodes will load genesis from disk

**Acceptance criteria:**
- A new operator can follow the doc end-to-end to set up a multi-miner network

---

### Task 6: Add `irys-cli generate-miner-info` helper (nice-to-have)

**Goal:** Help miners derive their EVM address from their mining key so they know what to put in `reth.alloc`.

**Files:**
- Modify: `crates/cli/src/main.rs`

**Behavior:**
```bash
$ irys-cli generate-miner-info --key f57554aff54acd4cfaa084f45a7062d5869c8dbb789f7d6a883fade660960303
Mining key:   f57554aff...
Irys address: 2Z7NNbu2hgdx9qzoYbLX8YTAAJtR
EVM address:  0x81c23e4bde4c7086400cdcbca2dfe9a96dbd0fad
```

This avoids miners needing to manually derive their address to populate `reth.alloc`.

**Acceptance criteria:**
- Correct address derivation from private key
- Output usable directly in config files

---

## Security Considerations

- **Key handling in `genesis_miners.toml`:** The manifest contains raw private keys. This is acceptable for testnet but not for mainnet. Future mainnet genesis would need a ceremony where miners sign their own commitments independently. The `build_signed_genesis_block()` architecture could be extended to accept pre-signed commitments as an alternative input mode.
- **Genesis file integrity:** The `expected_genesis_hash` check ensures nodes reject tampered genesis blocks. Disk-loaded genesis must also pass block signature validation (`is_signature_valid()`). Commitment consistency is guaranteed by the block's commitment ledger containing all txids — validate membership on load.
- **`reth.alloc` balance accuracy:** Miner account balances must be >= the sum of their stake + pledge commitment values, or the EVM state transitions for shadow transactions will fail. The documentation should include guidance on calculating required balances.
- **Manifest order is consensus-critical:** The anchor chain rotates globally across miners in manifest order. Two coordinators using the same miner set in different order produce different txids and a different block hash. Either document this clearly, or have `build_signed_genesis_block()` canonicalize miner order (e.g., sort by public key) before generating commitments. **Recommendation: canonicalize by sorting miners by their derived `IrysAddress`.**
- **Manifest validation:** `GenesisMinerManifest::into_entries()` should reject: (1) duplicate mining keys (since `compute_commitment_state()` stores stakes in a `BTreeMap<IrysAddress, StakeEntry>`, duplicates would silently overwrite), (2) zero-pledge miners (unless intentionally supported for stake-only participants).
- **Local storage-module compatibility:** A node can load valid genesis from disk but fail operationally if its `storage_submodules.toml` doesn't have enough paths for the pledges assigned to its mining key. This should be validated during startup with a clear error message.

## Migration Path

1. Rebase and merge `feat/genesis-cli` into master (Tasks 1-2)
2. Add disk-load support and verify determinism (Tasks 3-4)
3. Deploy testnet using the new workflow instead of hardcoded hacks
4. Remove the `deployment/testnet` branch hacks (they become dead code)
5. Document the workflow (Task 5)

## Appendix: Testnet Hack Inventory

For reference, these are the exact hacks on `deployment/testnet` that this plan eliminates:

| File | Hack | Lines |
|---|---|---|
| `crates/database/src/system_ledger.rs` | 106 hardcoded `CommitmentTransaction` JSON objects across two string literals (`json`, `json2`) | ~117-118 |
| `crates/database/src/system_ledger.rs` | `ordered_ids` array + `HashMap` sort for commitment ordering | ~120-220 |
| `crates/domain/src/snapshots/epoch_snapshot/epoch_snapshot.rs` | Duplicate `ordered_ids` array gated behind `if self.epoch_height == 0` overriding normal pledge sort | ~968-1100 |
| `crates/chain/src/chain.rs` | `let storage_module_count = 8;` replacing `commitments.len() - 1` | ~567 |
| `crates/chain/src/chain.rs` | `JESSEDEBUG` partition assignment logging | ~1364-1397 |
| `crates/reth-node-bridge/src/dump.rs` | 9 hardcoded peer addresses with 10M balance override | ~95-126 |
| `crates/types/src/config/consensus.rs` | Aurora hardfork activation timestamp change | ~863 |
