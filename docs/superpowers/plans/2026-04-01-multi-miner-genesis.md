# Multi-Miner Genesis Commitments Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Harden the `feat/genesis-cli` branch's multi-miner genesis builder with manifest validation, deterministic commitment ordering, a `generate-miner-info` CLI helper, and operator documentation.

**Architecture:** The genesis builder (`genesis_builder.rs`) already creates multi-miner commitment sets. This plan adds: (1) input validation and canonical miner ordering so the same miner set always produces the same block hash regardless of manifest order, (2) a test proving partition assignment determinism end-to-end, (3) a CLI subcommand to derive addresses from mining keys, and (4) operator docs.

**Tech Stack:** Rust 1.93, `k256` (secp256k1), `alloy-signer` (EVM address derivation), `clap` (CLI), `rstest`/`tokio::test` (testing)

**Backlog (out of scope):**
- **Disk-load genesis on non-producer nodes** — allowing `NodeMode::Peer` to load `.irys_genesis.json` + `.irys_genesis_commitments.json` from disk instead of requiring a trusted peer. Tracked separately.
- **Configurable initial packed partitions** — adding `initial_packed_partitions: Option<f64>` to `GenesisConfig`. Cherry-pick from `dmac/mainnet_consensus_configs` when needed; the current code uses `total_pledges as f64` which is correct for fully-packed launches.

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `crates/chain/src/genesis_builder.rs` | Modify | Add manifest validation (duplicates, zero-pledge) and canonical sort to `into_entries()` |
| `crates/chain/tests/genesis_builder_tests.rs` | Create | Tests for manifest validation, commitment determinism, partition assignment determinism |
| `crates/cli/src/main.rs` | Modify | Add `generate-miner-info` CLI subcommand |
| `docs/40-mining/genesis-setup.md` | Create | Operator documentation for multi-miner genesis workflow |

---

### Task 1: Manifest Validation — Reject Duplicate Keys and Zero-Pledge Miners

**Files:**
- Modify: `crates/chain/src/genesis_builder.rs:78-95` (`into_entries()` method)

Commitment IDs are derived from `Keccak256(signature)`. The anchor chain rotates globally across miners in manifest order, so different manifest orderings produce different txids and a different block hash. Also, `compute_commitment_state()` stores stakes in a `BTreeMap<IrysAddress, StakeEntry>` — duplicate keys would silently overwrite earlier entries.

- [ ] **Step 1: Write failing tests for validation**

Create the test file `crates/chain/tests/genesis_builder_tests.rs`:

```rust
use irys_chain::genesis_builder::{GenesisMinerManifest, GenesisMinerManifestEntry};

fn make_manifest(entries: Vec<(&str, u64)>) -> GenesisMinerManifest {
    GenesisMinerManifest {
        miners: entries
            .into_iter()
            .map(|(key, pledge_count)| GenesisMinerManifestEntry {
                mining_key: key.to_string(),
                pledge_count,
            })
            .collect(),
    }
}

// Two well-known test keys (deterministic, no randomness)
const KEY_A: &str = "db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0";
const KEY_B: &str = "f57554aff54acd4cfaa084f45a7062d5869c8dbb789f7d6a883fade660960303";

#[test]
fn into_entries_rejects_duplicate_mining_keys() {
    let manifest = make_manifest(vec![(KEY_A, 3), (KEY_A, 5)]);
    let result = manifest.into_entries();
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("duplicate"),
        "expected 'duplicate' in error: {msg}"
    );
}

#[test]
fn into_entries_rejects_zero_pledge_count() {
    let manifest = make_manifest(vec![(KEY_A, 0)]);
    let result = manifest.into_entries();
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("pledge_count"),
        "expected 'pledge_count' in error: {msg}"
    );
}

#[test]
fn into_entries_accepts_valid_manifest() {
    let manifest = make_manifest(vec![(KEY_A, 3), (KEY_B, 5)]);
    let entries = manifest.into_entries().expect("valid manifest");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].pledge_count, 3);
    assert_eq!(entries[1].pledge_count, 5);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run -p irys-chain --test genesis_builder_tests`
Expected: `into_entries_rejects_duplicate_mining_keys` and `into_entries_rejects_zero_pledge_count` FAIL (currently no validation). `into_entries_accepts_valid_manifest` should PASS.

- [ ] **Step 3: Implement validation in `into_entries()`**

In `crates/chain/src/genesis_builder.rs`, replace the `into_entries` method body:

```rust
/// Convert parsed entries into [`GenesisMinerEntry`] values.
///
/// Validates that:
/// - No two miners share the same signing key (duplicates would silently
///   overwrite in `BTreeMap<IrysAddress, StakeEntry>`)
/// - Every miner has `pledge_count >= 1` (a stake-only participant with
///   zero pledges is not currently supported)
///
/// Returns entries sorted by derived [`IrysAddress`] for deterministic
/// commitment ordering regardless of manifest file order.
pub fn into_entries(self) -> eyre::Result<Vec<GenesisMinerEntry>> {
    let mut entries: Vec<GenesisMinerEntry> = self
        .miners
        .into_iter()
        .enumerate()
        .map(|(i, entry)| {
            eyre::ensure!(
                entry.pledge_count > 0,
                "miner[{i}] has pledge_count = 0; every genesis miner must pledge \
                 at least one storage partition"
            );
            let key_bytes = hex::decode(entry.mining_key.trim_start_matches("0x"))
                .map_err(|e| eyre::eyre!("Invalid hex for miner[{i}] mining_key: {e}"))?;
            let signing_key = SigningKey::from_slice(&key_bytes)
                .map_err(|e| eyre::eyre!("Invalid signing key for miner[{i}]: {e}"))?;
            Ok(GenesisMinerEntry {
                signing_key,
                pledge_count: entry.pledge_count,
            })
        })
        .collect::<eyre::Result<Vec<_>>>()?;

    // Canonicalize order by IrysAddress so the same set of miners always
    // produces the same commitment chain regardless of TOML ordering.
    entries.sort_by_key(|e| signer_from_key_address(&e.signing_key));

    // Reject duplicate mining keys (easier to detect after sorting).
    for window in entries.windows(2) {
        let addr_a = signer_from_key_address(&window[0].signing_key);
        let addr_b = signer_from_key_address(&window[1].signing_key);
        eyre::ensure!(
            addr_a != addr_b,
            "duplicate mining key detected: two miners resolve to the same \
             IrysAddress {addr_a}. Each miner must have a unique key."
        );
    }

    Ok(entries)
}
```

Add this helper function at the bottom of the file (near `signer_from_key`):

```rust
/// Derive the [`IrysAddress`] from a raw [`SigningKey`] without needing a
/// full [`Config`].
fn signer_from_key_address(key: &SigningKey) -> IrysAddress {
    use alloy_signer::utils::secret_key_to_address;
    secret_key_to_address(key).into()
}
```

Add `use irys_types::IrysAddress;` to the imports at the top if not already present.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo nextest run -p irys-chain --test genesis_builder_tests`
Expected: All 3 tests PASS.

- [ ] **Step 5: Run `cargo xtask local-checks --fix`**

Run: `cargo xtask local-checks --fix`
Expected: No errors (fmt + clippy clean).

- [ ] **Step 6: Commit**

```bash
git add crates/chain/src/genesis_builder.rs crates/chain/tests/genesis_builder_tests.rs
git commit -m "feat: validate and canonicalize genesis miner manifest

Reject duplicate mining keys and zero-pledge miners in into_entries().
Sort miners by derived IrysAddress so manifest order does not affect
the resulting block hash."
```

---

### Task 2: Canonical Sort Verification Test

**Files:**
- Modify: `crates/chain/tests/genesis_builder_tests.rs`

Verify that `into_entries()` sorts miners by `IrysAddress` regardless of input order, so two manifests with the same miners in different order produce identical entry vectors.

- [ ] **Step 1: Write the test**

Append to `crates/chain/tests/genesis_builder_tests.rs`:

```rust
#[test]
fn into_entries_canonicalizes_order() {
    // Manifest with miners in A, B order
    let manifest_ab = make_manifest(vec![(KEY_A, 3), (KEY_B, 5)]);
    let entries_ab = manifest_ab.into_entries().expect("valid");

    // Manifest with miners in reversed (B, A) order
    let manifest_rev = make_manifest(vec![(KEY_B, 5), (KEY_A, 3)]);
    let entries_rev = manifest_rev.into_entries().expect("valid");

    // Both should produce the same canonical order
    assert_eq!(entries_ab.len(), entries_rev.len());
    for (a, b) in entries_ab.iter().zip(entries_rev.iter()) {
        assert_eq!(
            a.signing_key.to_bytes(),
            b.signing_key.to_bytes(),
            "canonical sort should produce identical key order"
        );
        assert_eq!(a.pledge_count, b.pledge_count);
    }
}
```

- [ ] **Step 2: Run test to verify it passes**

Run: `cargo nextest run -p irys-chain --test genesis_builder_tests into_entries_canonicalizes_order`
Expected: PASS (validation from Task 1 already sorts).

- [ ] **Step 3: Commit**

```bash
git add crates/chain/tests/genesis_builder_tests.rs
git commit -m "test: verify manifest canonicalization produces stable order"
```

---

### Task 3: Commitment Determinism Test

**Files:**
- Modify: `crates/chain/tests/genesis_builder_tests.rs`

Verify that `build_signed_genesis_block()` called twice with the same inputs produces identical commitment IDs, block hash, and ordering.

- [ ] **Step 1: Write the test**

Append to `crates/chain/tests/genesis_builder_tests.rs`:

```rust
use irys_chain::genesis_builder::{build_signed_genesis_block, GenesisMinerEntry};
use irys_types::{Config, NodeConfig};
use k256::ecdsa::SigningKey;

fn test_config() -> Config {
    let node_config = NodeConfig::testing();
    Config::new_with_random_peer_id(node_config)
}

fn test_miners() -> Vec<GenesisMinerEntry> {
    let key_a = SigningKey::from_slice(
        &hex::decode(KEY_A).unwrap()
    ).unwrap();
    let key_b = SigningKey::from_slice(
        &hex::decode(KEY_B).unwrap()
    ).unwrap();

    // Return in non-canonical order — build_signed_genesis_block receives
    // pre-sorted entries from into_entries(), but we construct directly here.
    // Sort manually to match what into_entries() would produce.
    let mut entries = vec![
        GenesisMinerEntry { signing_key: key_a, pledge_count: 3 },
        GenesisMinerEntry { signing_key: key_b, pledge_count: 2 },
    ];
    // Sort by IrysAddress to match canonical order
    entries.sort_by_key(|e| {
        use alloy_signer::utils::secret_key_to_address;
        irys_types::IrysAddress::from(secret_key_to_address(&e.signing_key))
    });
    entries
}

#[tokio::test]
async fn build_signed_genesis_block_is_deterministic() {
    let config = test_config();
    let miners = test_miners();

    let output_1 = build_signed_genesis_block(&config, &miners).await.unwrap();
    let output_2 = build_signed_genesis_block(&config, &miners).await.unwrap();

    // Block hashes must match
    assert_eq!(
        output_1.block.block_hash, output_2.block.block_hash,
        "genesis block hash must be deterministic"
    );

    // Commitment count must match
    assert_eq!(output_1.commitments.len(), output_2.commitments.len());

    // Every commitment ID must match in order
    for (c1, c2) in output_1.commitments.iter().zip(output_2.commitments.iter()) {
        assert_eq!(c1.id(), c2.id(), "commitment IDs must be identical and in the same order");
    }
}
```

**Important:** This test requires `NodeConfig::testing()` to have a non-zero `genesis.timestamp_millis` so the timestamp is deterministic and doesn't use `SystemTime::now()`. Check that `ConsensusConfig::testing()` sets `timestamp_millis` to a concrete value. If it does not, the test must set it explicitly:

```rust
fn test_config() -> Config {
    let mut node_config = NodeConfig::testing();
    // Ensure deterministic timestamp (if testing() defaults to 0, which means "use now()")
    if node_config.consensus.get().genesis.timestamp_millis == 0 {
        node_config.consensus.get_mut().genesis.timestamp_millis = 1_700_000_000_000;
    }
    Config::new_with_random_peer_id(node_config)
}
```

- [ ] **Step 2: Run the test**

Run: `cargo nextest run -p irys-chain --test genesis_builder_tests build_signed_genesis_block_is_deterministic`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/chain/tests/genesis_builder_tests.rs
git commit -m "test: verify build_signed_genesis_block produces deterministic output"
```

---

### Task 4: Partition Assignment Determinism Test

**Files:**
- Modify: `crates/chain/tests/genesis_builder_tests.rs`

Verify that commitments from `build_signed_genesis_block()` produce identical partition assignments when replayed through `EpochSnapshot`. This is the end-to-end test that confirms the testnet hack's custom ordering in `epoch_snapshot.rs` is unnecessary.

- [ ] **Step 1: Write the test**

Append to `crates/chain/tests/genesis_builder_tests.rs`:

```rust
use irys_config::submodules::StorageSubmodulesConfig;
use irys_domain::EpochSnapshot;
use std::path::PathBuf;

/// Build a StorageSubmodulesConfig with enough paths for the total pledge count.
fn test_storage_submodules(total_pledges: usize) -> StorageSubmodulesConfig {
    StorageSubmodulesConfig {
        is_using_hardcoded_paths: true,
        submodule_paths: (0..total_pledges)
            .map(|i| PathBuf::from(format!("/tmp/test-sm-{i}")))
            .collect(),
    }
}

#[tokio::test]
async fn partition_assignments_are_deterministic() {
    let config = test_config();
    let miners = test_miners();
    let total_pledges: usize = miners.iter().map(|m| m.pledge_count as usize).sum();

    let output = build_signed_genesis_block(&config, &miners).await.unwrap();

    let submodules = test_storage_submodules(total_pledges);

    // Create two EpochSnapshots from the same genesis data
    let snap_1 = EpochSnapshot::new(
        &submodules,
        output.block.clone(),
        output.commitments.clone(),
        &config,
    );
    let snap_2 = EpochSnapshot::new(
        &submodules,
        output.block.clone(),
        output.commitments.clone(),
        &config,
    );

    // Extract partition assignments from both snapshots
    let assignments_1 = &snap_1.partition_assignments.capacity_partitions;
    let assignments_2 = &snap_2.partition_assignments.capacity_partitions;

    assert_eq!(
        assignments_1.len(),
        assignments_2.len(),
        "partition assignment count must match"
    );

    // BTreeMap iteration is ordered — compare element by element
    for ((hash_1, assign_1), (hash_2, assign_2)) in assignments_1.iter().zip(assignments_2.iter())
    {
        assert_eq!(hash_1, hash_2, "partition hashes must match");
        assert_eq!(
            assign_1.miner_address, assign_2.miner_address,
            "miner assignments must match for partition {hash_1}"
        );
    }

    // Verify we actually assigned partitions (not a vacuous pass)
    assert_eq!(
        assignments_1.len(),
        total_pledges,
        "every pledge should have a partition assignment"
    );
}
```

- [ ] **Step 2: Run the test**

Run: `cargo nextest run -p irys-chain --test genesis_builder_tests partition_assignments_are_deterministic`
Expected: PASS. If it fails, the `epoch_snapshot.rs` pledge sort logic or the commitment generation has a non-determinism bug that must be investigated.

- [ ] **Step 3: Commit**

```bash
git add crates/chain/tests/genesis_builder_tests.rs
git commit -m "test: verify partition assignments are deterministic from genesis commitments"
```

---

### Task 5: `generate-miner-info` CLI Subcommand

**Files:**
- Modify: `crates/cli/src/main.rs` (add `GenerateMinerInfo` variant + handler)

Operators need to derive their EVM address and Irys address from their mining key to populate `reth.alloc` in their config. This avoids manual address derivation.

- [ ] **Step 1: Write failing CLI parse test**

Append to the existing `#[cfg(test)] mod tests` block in `crates/cli/src/main.rs`:

```rust
#[rstest]
#[case(
    &["irys-cli", "generate-miner-info", "--key", "db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0"],
    "db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0"
)]
fn test_generate_miner_info_parsing(#[case] args: &[&str], #[case] expected_key: &str) {
    let cli = IrysCli::try_parse_from(args).expect("valid CLI args");
    match cli.command {
        Commands::GenerateMinerInfo { key } => {
            assert_eq!(key, expected_key);
        }
        other => panic!("expected GenerateMinerInfo, got {:?}", other),
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p irys-cli test_generate_miner_info_parsing`
Expected: FAIL — `Commands::GenerateMinerInfo` doesn't exist yet.

- [ ] **Step 3: Add the CLI variant**

In `crates/cli/src/main.rs`, add to the `Commands` enum (after the `BuildGenesis` variant):

```rust
#[command(
    name = "generate-miner-info",
    about = "Derive Irys and EVM addresses from a mining key"
)]
GenerateMinerInfo {
    /// Hex-encoded secp256k1 private key (with or without 0x prefix)
    #[arg(long)]
    key: String,
},
```

- [ ] **Step 4: Add the handler**

In the `match args.command` block in `main()`, add before the `Commands::Tui` arm:

```rust
Commands::GenerateMinerInfo { key } => {
    use alloy_signer::utils::secret_key_to_address;
    use irys_types::IrysAddress;
    use k256::ecdsa::SigningKey;

    let key_bytes = hex::decode(key.trim_start_matches("0x"))
        .map_err(|e| eyre::eyre!("Invalid hex: {e}"))?;
    let signing_key = SigningKey::from_slice(&key_bytes)
        .map_err(|e| eyre::eyre!("Invalid secp256k1 key: {e}"))?;

    let evm_address = secret_key_to_address(&signing_key);
    let irys_address = IrysAddress::from(evm_address);

    println!("Mining key:   {}...", &key[..16]);
    println!("Irys address: {irys_address}");
    println!("EVM address:  {evm_address}");

    Ok(())
}
```

Add `hex` to the cli crate's `Cargo.toml` dependencies if not already present. Check with:
```bash
grep 'hex' crates/cli/Cargo.toml
```
If missing, add `hex = "0.4"` under `[dependencies]`.

- [ ] **Step 5: Run parse test**

Run: `cargo nextest run -p irys-cli test_generate_miner_info_parsing`
Expected: PASS.

- [ ] **Step 6: Write address derivation unit test**

Append to the test module in `crates/cli/src/main.rs`:

```rust
#[test]
fn test_generate_miner_info_address_derivation() {
    use alloy_signer::utils::secret_key_to_address;
    use irys_types::IrysAddress;
    use k256::ecdsa::SigningKey;

    let key_hex = "db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0";
    let key_bytes = hex::decode(key_hex).unwrap();
    let signing_key = SigningKey::from_slice(&key_bytes).unwrap();

    let evm_address = secret_key_to_address(&signing_key);
    let irys_address = IrysAddress::from(evm_address);

    // EVM address should be a valid 0x-prefixed checksum address
    let evm_str = format!("{evm_address}");
    assert!(evm_str.starts_with("0x"), "EVM address should be 0x-prefixed");
    assert_eq!(evm_str.len(), 42, "EVM address should be 42 chars (0x + 40 hex)");

    // Irys address should be non-empty base58
    let irys_str = format!("{irys_address}");
    assert!(!irys_str.is_empty(), "Irys address should not be empty");
}
```

- [ ] **Step 7: Run all CLI tests**

Run: `cargo nextest run -p irys-cli`
Expected: All tests PASS.

- [ ] **Step 8: Run `cargo xtask local-checks --fix`**

Run: `cargo xtask local-checks --fix`
Expected: Clean.

- [ ] **Step 9: Commit**

```bash
git add crates/cli/src/main.rs crates/cli/Cargo.toml
git commit -m "feat: add generate-miner-info CLI to derive addresses from mining key"
```

---

### Task 6: Genesis Workflow Documentation

**Files:**
- Create: `docs/40-mining/genesis-setup.md`

Write operator-facing documentation for multi-miner genesis setup.

- [ ] **Step 1: Write the documentation**

Create `docs/40-mining/genesis-setup.md`:

```markdown
# Multi-Miner Genesis Setup

This guide walks through creating a genesis block for a multi-miner Irys network.

## Prerequisites

- `irys-cli` binary built from the `irys-rs` repo
- A `config.toml` with the desired consensus configuration
- Each miner's hex-encoded secp256k1 private key

## Step 1: Collect Miner Keys

Each miner generates or provides their mining key. Use `generate-miner-info` to
derive their addresses:

```bash
irys-cli generate-miner-info --key <hex-private-key>
```

Output:
```
Mining key:   f57554aff54acd4...
Irys address: 2Z7NNbu2hgdx9qzoYbLX8YTAAJtR
EVM address:  0x81c23e4bde4c7086400cdcbca2dfe9a96dbd0fad
```

Record each miner's **EVM address** for Step 3 and their **mining key** for
Step 2.

## Step 2: Create `genesis_miners.toml`

```toml
[[miners]]
mining_key = "f57554aff54acd4cfaa084f45a7062d5869c8dbb789f7d6a883fade660960303"
pledge_count = 8

[[miners]]
mining_key = "db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0"
pledge_count = 3
```

**Notes:**
- `pledge_count` is the number of storage partitions this miner commits to store.
  Each miner also gets one implicit stake commitment.
- Miner order in the TOML does not affect the output -- miners are sorted by
  their derived Irys address before commitment generation.
- Every miner must have `pledge_count >= 1`.
- No two miners may share the same key.

## Step 3: Fund Miner Accounts

In the node's `config.toml`, add genesis account balances for each miner under
`[consensus.reth.alloc]`. Each miner needs enough balance to cover their stake
plus all pledge fees.

```toml
[consensus.reth.alloc."0x81c23e4bde4c7086400cdcbca2dfe9a96dbd0fad"]
balance = "10000000000000000000000000"

[consensus.reth.alloc."0xAnotherMinerEvmAddress"]
balance = "10000000000000000000000000"
```

The balance must be >= the sum of the miner's stake value + (pledge_count *
pledge_base_value), denominated in the chain's smallest token unit.

## Step 4: Build the Genesis Block

```bash
CONFIG=config.toml irys-cli build-genesis \
  --miners genesis_miners.toml \
  --output ./genesis-artifacts/
```

This produces two files in `./genesis-artifacts/`:
- `.irys_genesis.json` — the signed genesis block header
- `.irys_genesis_commitments.json` — all commitment transactions

The CLI prints the block hash. Record it for Step 5.

## Step 5: Configure Peer Nodes

Add the genesis block hash to every peer node's `config.toml`:

```toml
[consensus]
expected_genesis_hash = "<block-hash-from-step-4>"
```

## Step 6: Start the Network

1. The genesis block producer starts in `Genesis` mode with a trusted peer
   pointing to itself or with the genesis files on disk.
2. Peer nodes start in `Peer` mode and fetch genesis from the producer (or
   another peer that already has it).

## Security Notes

- **`genesis_miners.toml` contains raw private keys.** Treat it as a secret.
  Delete it after genesis block generation. For mainnet, a ceremony where
  miners sign their own commitments independently is recommended.
- **`expected_genesis_hash` is critical.** It prevents nodes from accepting a
  tampered genesis block. Every peer node must have this set.
- **Miner order is canonical.** The CLI sorts miners by Irys address before
  generating commitments, so two coordinators using the same miner set will
  always produce the same genesis block.
```

- [ ] **Step 2: Verify the doc renders correctly**

Open the file and scan for broken markdown formatting, orphaned code fences, or
incorrect command examples.

- [ ] **Step 3: Commit**

```bash
git add docs/40-mining/genesis-setup.md
git commit -m "docs: add multi-miner genesis setup guide"
```

---

## Verification Checklist

After all tasks are complete:

- [ ] `cargo xtask local-checks` passes (fmt, clippy, check, unused-deps, typos)
- [ ] `cargo nextest run -p irys-chain --test genesis_builder_tests` — all 5 tests pass
- [ ] `cargo nextest run -p irys-cli` — all CLI tests pass (including new `generate-miner-info` tests)
- [ ] `irys-cli build-genesis --miners <test-manifest> --output /tmp/test-genesis/` produces valid output (manual smoke test)
