# Standalone Genesis Block CLI Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Create a reusable core function for genesis block construction with multi-miner support, and a thin CLI subcommand that wraps it.

**Architecture:** Extract genesis block construction from `IrysNode::create_genesis_block()` into a standalone `build_signed_genesis_block()` function in `crates/chain/src/genesis_builder.rs`. This function accepts a `Config` and a list of `GenesisMinerEntry` structs (each with a signing key and pledge count). The CLI subcommand (`build-genesis`) reads a `genesis_miners.toml` file for multi-miner input, calls the core function, and writes both the genesis block and commitments to disk as JSON. Finally, refactor `IrysNode::create_genesis_block()` to delegate to the new core function.

**Tech Stack:** Rust, clap (CLI), serde/toml (config parsing), k256 (ECDSA signing), irys-types/irys-config/irys-vdf (domain crates)

---

## Task 1: Create `GenesisMinerEntry` and `GenesisOutput` types + core function skeleton

**Files:**
- Create: `crates/chain/src/genesis_builder.rs`
- Modify: `crates/chain/src/lib.rs` (add `pub mod genesis_builder;`)

**Step 1: Create `genesis_builder.rs` with types and core function**

```rust
// crates/chain/src/genesis_builder.rs

use eyre::ensure;
use irys_config::chain::chainspec::build_unsigned_irys_genesis_block;
use irys_types::{
    calculate_initial_difficulty, chainspec::irys_chain_spec, CommitmentTransaction, Config,
    ConsensusConfig, IrysBlockHeader, SystemLedger, SystemTransactionLedger, UnixTimestamp,
    UnixTimestampMs, H256, H256List, U256,
    irys::IrysSigner,
};
use irys_vdf::vdf::run_vdf_for_genesis_block;
use k256::ecdsa::SigningKey;
use reth::chainspec::ChainSpec;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::info;

/// A miner entry for genesis block construction.
/// Each miner gets one stake commitment + `pledge_count` pledge commitments.
#[derive(Debug, Clone)]
pub struct GenesisMinerEntry {
    pub signing_key: SigningKey,
    pub pledge_count: u64,
}

/// The output of genesis block construction.
pub struct GenesisOutput {
    pub block: IrysBlockHeader,
    pub commitments: Vec<CommitmentTransaction>,
    pub reth_chain_spec: Arc<ChainSpec>,
}

/// Build a fully signed genesis block with commitments from multiple miners.
///
/// The first miner in the `miners` slice signs the genesis block itself.
/// Each miner receives one stake commitment + their specified number of pledge commitments,
/// all signed with that miner's key.
///
/// This is the core function used by both the CLI and the node startup path.
pub async fn build_signed_genesis_block(
    config: &Config,
    miners: &[GenesisMinerEntry],
) -> eyre::Result<GenesisOutput> {
    ensure!(!miners.is_empty(), "At least one miner entry is required");

    // --- 1. Determine timestamp ---
    let configured_ts = config.consensus.genesis.timestamp_millis;
    let timestamp_millis = if configured_ts != 0 {
        configured_ts
    } else {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis()
    };
    let timestamp_secs = Duration::from_millis(timestamp_millis.try_into()?).as_secs();

    // --- 2. Build Reth chain spec (needed for evm_block_hash) ---
    let reth_chain_spec = irys_chain_spec(
        config.consensus.chain_id,
        &config.consensus.reth,
        &config.consensus.hardforks,
        timestamp_secs,
    )?;

    // --- 3. Build unsigned genesis block ---
    let number_of_ingress_proofs_total = config
        .number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(timestamp_secs));
    let mut genesis_block = build_unsigned_irys_genesis_block(
        &config.consensus.genesis,
        reth_chain_spec.genesis_hash(),
        number_of_ingress_proofs_total,
    );

    // Override timestamp fields
    if config.consensus.genesis.last_epoch_hash != H256::zero() {
        genesis_block.last_epoch_hash = config.consensus.genesis.last_epoch_hash;
    }
    genesis_block.timestamp = UnixTimestampMs::from_millis(timestamp_millis);
    genesis_block.last_diff_timestamp = UnixTimestampMs::from_millis(timestamp_millis);

    // --- 4. Generate multi-miner commitments ---
    let (commitments, initial_treasury) =
        generate_multi_miner_commitments(&config.consensus, miners).await;

    // Add commitment IDs to block header's system ledger
    let commitment_ledger = get_or_create_commitment_ledger(&mut genesis_block);
    for commitment in &commitments {
        commitment_ledger.tx_ids.push(commitment.id());
    }

    // --- 5. Calculate initial difficulty ---
    // Total pledge count across all miners = total storage modules
    let total_pledges: u64 = miners.iter().map(|m| m.pledge_count).sum();
    let difficulty = calculate_initial_difficulty(&config.consensus, total_pledges as f64)
        .expect("valid calculated initial difficulty");
    genesis_block.diff = difficulty;
    genesis_block.treasury = initial_treasury;

    // --- 6. Run VDF ---
    run_vdf_for_genesis_block(&mut genesis_block, &config.vdf);

    // --- 7. Sign the block with the first miner's key ---
    let block_signer = IrysSigner {
        signer: miners[0].signing_key.clone(),
        chain_id: config.consensus.chain_id,
        chunk_size: config.consensus.chunk_size,
    };
    block_signer
        .sign_block_header(&mut genesis_block)
        .expect("Failed to sign genesis block");

    info!("=====================================");
    info!("GENESIS BLOCK CREATED");
    info!("Hash: {}", genesis_block.block_hash);
    info!(
        "consensus.expected_genesis_hash = \"{}\"",
        genesis_block.block_hash
    );
    info!("=====================================");

    Ok(GenesisOutput {
        block: genesis_block,
        commitments,
        reth_chain_spec,
    })
}

/// Generate stake + pledge commitments for each miner.
///
/// For each miner: creates 1 stake commitment + N pledge commitments.
/// Anchors rotate across all commitments to ensure unique signatures/txids.
async fn generate_multi_miner_commitments(
    consensus: &ConsensusConfig,
    miners: &[GenesisMinerEntry],
) -> (Vec<CommitmentTransaction>, U256) {
    let mut all_commitments: Vec<CommitmentTransaction> = Vec::new();
    let mut total_value = U256::zero();
    // Start with a zero anchor; rotate through commitment IDs for uniqueness
    let mut anchor = H256::default();

    for miner in miners {
        let signer = IrysSigner {
            signer: miner.signing_key.clone(),
            chain_id: consensus.chain_id,
            chunk_size: consensus.chunk_size,
        };

        // Stake commitment
        let mut stake = CommitmentTransaction::new_stake(consensus, anchor);
        signer
            .sign_commitment(&mut stake)
            .expect("stake commitment signing failed");
        anchor = stake.id();
        total_value = total_value.saturating_add(stake.value());
        all_commitments.push(stake);

        // Pledge commitments
        // PledgeDataProvider is implemented for u64 — it returns the value itself as the
        // pledge count. We pass the running index within this miner's pledges.
        for i in 0..miner.pledge_count {
            let mut pledge =
                CommitmentTransaction::new_pledge(consensus, anchor, &i, signer.address()).await;
            signer
                .sign_commitment(&mut pledge)
                .expect("pledge commitment signing failed");
            anchor = pledge.id();
            total_value = total_value.saturating_add(pledge.value());
            all_commitments.push(pledge);
        }
    }

    (all_commitments, total_value)
}

/// Find or create the Commitment system ledger in a genesis block header.
fn get_or_create_commitment_ledger(
    genesis_block: &mut IrysBlockHeader,
) -> &mut SystemTransactionLedger {
    let exists = genesis_block
        .system_ledgers
        .iter()
        .any(|e| e.ledger_id == SystemLedger::Commitment);

    if !exists {
        genesis_block.system_ledgers.push(SystemTransactionLedger {
            ledger_id: SystemLedger::Commitment.into(),
            tx_ids: H256List::new(),
        });
    }

    genesis_block
        .system_ledgers
        .iter_mut()
        .find(|e| e.ledger_id == SystemLedger::Commitment)
        .expect("Commitment ledger should exist")
}
```

**Step 2: Register the module in `lib.rs`**

Add `pub mod genesis_builder;` to `crates/chain/src/lib.rs`.

**Step 3: Verify it compiles**

Run: `cargo check -p irys-chain`

**Step 4: Commit**

```
feat: add genesis_builder core function with multi-miner support
```

---

## Task 2: Add `GenesisMinerConfig` TOML parsing

**Files:**
- Modify: `crates/chain/src/genesis_builder.rs` (add config struct + parsing)

**Step 1: Add the TOML-parseable config struct and loader**

Add these types at the top of `genesis_builder.rs`:

```rust
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Configuration file format for genesis miners (`genesis_miners.toml`).
///
/// Example:
/// ```toml
/// [[miners]]
/// mining_key = "f57554aff54acd4cfaa084f45a7062d5869c8dbb789f7d6a883fade660960303"
/// pledge_count = 5
///
/// [[miners]]
/// mining_key = "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890"
/// pledge_count = 3
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenesisMinerManifest {
    pub miners: Vec<GenesisMinerManifestEntry>,
}

/// A single miner entry in the genesis miners manifest.
/// The `mining_key` is a hex-encoded secp256k1 private key (no 0x prefix).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenesisMinerManifestEntry {
    pub mining_key: String,
    pub pledge_count: u64,
}

impl GenesisMinerManifest {
    /// Load a genesis miners manifest from a TOML file.
    pub fn load(path: &Path) -> eyre::Result<Self> {
        let contents = std::fs::read_to_string(path)
            .map_err(|e| eyre::eyre!("Failed to read genesis miners file {:?}: {}", path, e))?;
        let manifest: Self = toml::from_str(&contents)
            .map_err(|e| eyre::eyre!("Failed to parse genesis miners file {:?}: {}", path, e))?;
        ensure!(
            !manifest.miners.is_empty(),
            "genesis_miners.toml must contain at least one [[miners]] entry"
        );
        Ok(manifest)
    }

    /// Convert manifest entries into `GenesisMinerEntry` values by parsing hex keys.
    pub fn into_entries(self) -> eyre::Result<Vec<GenesisMinerEntry>> {
        self.miners
            .into_iter()
            .enumerate()
            .map(|(i, entry)| {
                let key_bytes = hex::decode(entry.mining_key.trim_start_matches("0x"))
                    .map_err(|e| eyre::eyre!("Invalid hex for miner[{}] mining_key: {}", i, e))?;
                let signing_key = SigningKey::from_slice(&key_bytes)
                    .map_err(|e| eyre::eyre!("Invalid signing key for miner[{}]: {}", i, e))?;
                Ok(GenesisMinerEntry {
                    signing_key,
                    pledge_count: entry.pledge_count,
                })
            })
            .collect()
    }
}
```

**Step 2: Verify it compiles**

Run: `cargo check -p irys-chain`

Note: `hex` crate should already be available transitively. If not, add it to `crates/chain/Cargo.toml`.

**Step 3: Commit**

```
feat: add GenesisMinerManifest TOML config for multi-miner genesis
```

---

## Task 3: Add disk I/O for genesis commitments

**Files:**
- Modify: `crates/chain/src/genesis_utilities.rs` (add commitment save/load)

**Step 1: Add save/load functions for commitments**

Add to `genesis_utilities.rs`:

```rust
const GENESIS_COMMITMENTS_FILENAME: &str = ".irys_genesis_commitments.json";

/// Write genesis commitment transactions to disk as JSON.
pub fn save_genesis_commitments_to_disk(
    commitments: &[CommitmentTransaction],
    base_directory: &PathBuf,
) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(commitments)
        .expect("genesis commitments should convert to json string");
    if let Err(e) = create_dir_all(base_directory) {
        panic!(
            "unable to recursively read or create directory \"{:?}\" error {}",
            base_directory, e
        );
    }
    let mut file = File::create(Path::new(base_directory).join(GENESIS_COMMITMENTS_FILENAME))?;
    file.write_all(json.as_bytes())?;
    Ok(())
}

/// Read genesis commitment transactions from disk.
pub fn load_genesis_commitments_from_disk(
    base_directory: &PathBuf,
) -> std::io::Result<Vec<CommitmentTransaction>> {
    let file = File::open(Path::new(base_directory).join(GENESIS_COMMITMENTS_FILENAME))?;
    let reader = std::io::BufReader::new(file);
    let commitments: Vec<CommitmentTransaction> = serde_json::from_reader(reader)
        .expect("genesis_commitments.json should be valid JSON");
    Ok(commitments)
}
```

Add `use irys_types::CommitmentTransaction;` to the imports at the top of the file (alongside the existing `use irys_types::{IrysBlockHeader, IrysBlockHeaderV1};`).

Also add `use serde_json;` if not already present (it is used via `.expect` but the crate needs to be in scope for `serde_json::to_string_pretty`). The `serde_json` dep is already in `irys-chain`'s `Cargo.toml`.

**Step 2: Verify it compiles**

Run: `cargo check -p irys-chain`

**Step 3: Commit**

```
feat: add disk I/O for genesis commitment transactions
```

---

## Task 4: Add `build-genesis` CLI subcommand

**Files:**
- Modify: `crates/cli/src/main.rs` (add `BuildGenesis` command variant + handler)
- Modify: `crates/cli/Cargo.toml` (add `serde_json` dependency if not present)

**Step 1: Add the command variant to `Commands` enum**

In `crates/cli/src/main.rs`, add to the `Commands` enum:

```rust
#[command(
    name = "build-genesis",
    about = "Build a signed genesis block with multi-miner commitments and write to disk"
)]
BuildGenesis {
    /// Path to genesis_miners.toml containing miner keys and pledge counts
    #[arg(long)]
    miners: PathBuf,

    /// Output directory for genesis block and commitments JSON files.
    /// Defaults to current directory.
    #[arg(long, default_value = ".")]
    output: PathBuf,
},
```

**Step 2: Add the command handler in the `match` block**

```rust
Commands::BuildGenesis { miners, output } => {
    use irys_chain::genesis_builder::{GenesisMinerManifest, build_signed_genesis_block};
    use irys_chain::genesis_utilities::{
        save_genesis_block_to_disk, save_genesis_commitments_to_disk,
    };

    // Load node config (consensus params, chain_id, etc.)
    let node_config: NodeConfig = load_config()?;
    let config = Config::new_with_random_peer_id(node_config);

    // Load and parse miner manifest
    let manifest = GenesisMinerManifest::load(&miners)?;
    let miner_entries = manifest.into_entries()?;

    info!(
        "Building genesis block with {} miner(s), {} total pledges",
        miner_entries.len(),
        miner_entries.iter().map(|m| m.pledge_count).sum::<u64>()
    );

    // Build the genesis block
    let genesis_output = build_signed_genesis_block(&config, &miner_entries).await?;

    // Write genesis block to disk
    save_genesis_block_to_disk(
        Arc::new(genesis_output.block.clone()),
        &output,
    )
    .map_err(|e| eyre::eyre!("Failed to write genesis block: {}", e))?;

    // Write commitments to disk
    save_genesis_commitments_to_disk(
        &genesis_output.commitments,
        &output,
    )
    .map_err(|e| eyre::eyre!("Failed to write genesis commitments: {}", e))?;

    info!("Genesis block written to {:?}", &output);
    info!(
        "  Block hash: {}",
        genesis_output.block.block_hash
    );
    info!(
        "  Commitments: {} total",
        genesis_output.commitments.len()
    );
    info!(
        "  Add to peer configs: consensus.expected_genesis_hash = \"{}\"",
        genesis_output.block.block_hash
    );

    Ok(())
}
```

Add to imports at the top of `main.rs`:
```rust
use std::sync::Arc;
```

Also add `serde_json` to `crates/cli/Cargo.toml` if not already there (check first — it may be a transitive dep but we should be explicit).

**Step 3: Verify it compiles**

Run: `cargo check -p irys-cli`

**Step 4: Commit**

```
feat: add build-genesis CLI subcommand
```

---

## Task 5: Refactor `IrysNode::create_new_genesis_block()` to use core function

**Files:**
- Modify: `crates/chain/src/chain.rs` (~lines 579-655)

**Step 1: Replace the body of `create_new_genesis_block()` with delegation**

The existing method at `chain.rs:579` should delegate to the core function.
It needs to build a single `GenesisMinerEntry` from the node's config + storage_submodules.toml.

```rust
async fn create_new_genesis_block(
    &self,
) -> eyre::Result<(IrysBlockHeader, Vec<CommitmentTransaction>, Arc<ChainSpec>)> {
    use crate::genesis_builder::{GenesisMinerEntry, build_signed_genesis_block};
    use irys_config::submodules::StorageSubmodulesConfig;

    // Build a single-miner entry from the node's own config
    let storage_submodule_config =
        StorageSubmodulesConfig::load(
            self.config.node_config.base_directory.clone(),
            self.config.node_config.node_mode,
        )
            .expect("storage_submodules.toml must exist for genesis node");
    let pledge_count = storage_submodule_config.submodule_paths.len() as u64;

    let miner_entry = GenesisMinerEntry {
        signing_key: self.config.node_config.mining_key.clone(),
        pledge_count,
    };

    let output = build_signed_genesis_block(&self.config, &[miner_entry]).await?;

    Ok((output.block, output.commitments, output.reth_chain_spec))
}
```

Remove now-unused imports from `chain.rs` that were only used by the old implementation body:
- `irys_config::chain::chainspec::build_unsigned_irys_genesis_block` (if not used elsewhere in the file)
- `irys_database::add_genesis_commitments` (if not used elsewhere)

Check each removed import to make sure it's not referenced elsewhere in the file first. The `irys_config::submodules::StorageSubmodulesConfig` import can stay at the top level or be in the function — either is fine.

**Step 2: Verify it compiles**

Run: `cargo check -p irys-chain`

**Step 3: Run existing tests to verify no regression**

Run: `IRYS_CUSTOM_TMP_DIR=/tmpfs/irys cargo xtask test -p irys-chain`

If there are genesis-related integration tests in `crates/chain/tests/`, run those specifically.

**Step 4: Commit**

```
refactor: delegate IrysNode genesis creation to genesis_builder core function
```

---

## Task 6: Verify end-to-end with a test config

**Step 1: Create a test genesis_miners.toml**

Create a temporary file with known test keys:

```toml
[[miners]]
mining_key = "f57554aff54acd4cfaa084f45a7062d5869c8dbb789f7d6a883fade660960303"
pledge_count = 3

[[miners]]
mining_key = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
pledge_count = 2
```

**Step 2: Run the CLI with the test config**

```bash
cd /workspaces/irys-rs/.worktrees/genesis-cli
CONFIG=config.toml cargo run -p irys-cli -- build-genesis \
    --miners /tmp/test_genesis_miners.toml \
    --output /tmp/genesis_output
```

**Step 3: Verify output files exist and are valid JSON**

```bash
cat /tmp/genesis_output/.irys_genesis.json | python3 -m json.tool > /dev/null && echo "valid JSON"
cat /tmp/genesis_output/.irys_genesis_commitments.json | python3 -m json.tool > /dev/null && echo "valid JSON"
```

Check that the commitments file contains the expected number of entries:
- Miner 1: 1 stake + 3 pledges = 4
- Miner 2: 1 stake + 2 pledges = 3
- Total: 7 commitments

**Step 4: Verify the genesis block hash is non-zero and signature is present**

```bash
grep -o '"block_hash":"[^"]*"' /tmp/genesis_output/.irys_genesis.json
```

**Step 5: Final commit if any fixes needed**

```
test: verify build-genesis CLI end-to-end
```

---

## Summary of file changes

| File | Action | Description |
|------|--------|-------------|
| `crates/chain/src/genesis_builder.rs` | Create | Core function + types + config parsing |
| `crates/chain/src/lib.rs` | Modify | Add `pub mod genesis_builder` |
| `crates/chain/src/genesis_utilities.rs` | Modify | Add commitment save/load functions |
| `crates/chain/src/chain.rs` | Modify | Refactor `create_new_genesis_block()` to delegate |
| `crates/cli/src/main.rs` | Modify | Add `BuildGenesis` command + handler |
| `crates/cli/Cargo.toml` | Modify | Add deps if needed |

## Risks and considerations

- **Commitment anchor rotation**: The multi-miner commitment generator must rotate anchors across ALL commitments (not just per-miner) to ensure globally unique txids. The plan handles this by threading a single `anchor` through all miners sequentially.
- **Pledge cost calculation**: The `PledgeDataProvider` (implemented by `u64`) returns the pledge count. For multi-miner genesis, each miner's pledges start at index 0, matching the existing single-miner behavior where each miner's first pledge is at count 0.
- **Backwards compatibility**: The refactored `IrysNode::create_new_genesis_block()` should produce identical genesis blocks to the old implementation when given the same config. This is worth verifying with a test.
- **Config timestamp**: If `genesis.timestamp_millis` is 0 in config, `SystemTime::now()` is used. This means repeated runs produce different genesis blocks. This matches existing behavior.
