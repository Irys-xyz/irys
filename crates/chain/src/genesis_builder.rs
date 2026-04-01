//! Genesis block builder with multi-miner support.
//!
//! Provides the core [`build_signed_genesis_block`] function that constructs a
//! fully-signed genesis block from a [`Config`] and a list of
//! [`GenesisMinerEntry`] descriptors. Each miner gets one stake commitment plus
//! N pledge commitments (where N = `pledge_count`). Anchors rotate across ALL
//! commitments globally to produce unique transaction IDs.

use std::{
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use eyre::Context as _;
use irys_config::chain::chainspec::build_unsigned_irys_genesis_block;
use irys_types::{
    CommitmentTransaction, CommitmentTypeV2, Config, H256, H256List, IrysAddress, IrysBlockHeader,
    SystemLedger, SystemTransactionLedger, U256, UnixTimestamp, UnixTimestampMs,
    calculate_initial_difficulty, chainspec::irys_chain_spec, irys::IrysSigner,
};
use irys_vdf::vdf::run_vdf_for_genesis_block;
use k256::ecdsa::SigningKey;
use reth::chainspec::ChainSpec;
use tracing::info;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Describes a single miner that should be included in the genesis block.
pub struct GenesisMinerEntry {
    /// The miner's secp256k1 signing key.
    pub signing_key: SigningKey,
    /// How many pledge (storage partition) commitments this miner contributes.
    pub pledge_count: u64,
}

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenesisMinerManifestEntry {
    pub mining_key: String,
    pub pledge_count: u64,
}

impl GenesisMinerManifest {
    /// Load from a TOML file path.
    pub fn load(path: &Path) -> eyre::Result<Self> {
        let contents = std::fs::read_to_string(path)
            .map_err(|e| eyre::eyre!("Failed to read genesis miners file {:?}: {}", path, e))?;
        let manifest: Self = toml::from_str(&contents)
            .map_err(|e| eyre::eyre!("Failed to parse genesis miners file {:?}: {}", path, e))?;
        eyre::ensure!(
            !manifest.miners.is_empty(),
            "genesis_miners.toml must contain at least one [[miners]] entry"
        );
        Ok(manifest)
    }

    /// Convert parsed entries into [`GenesisMinerEntry`] values.
    ///
    /// Validates that no miner has `pledge_count == 0` and that there are no
    /// duplicate mining keys (by derived [`IrysAddress`]).  The returned entries
    /// are sorted by `IrysAddress` so the manifest ordering is canonical and
    /// does not affect the resulting block hash.
    pub fn into_entries(self) -> eyre::Result<Vec<GenesisMinerEntry>> {
        let mut entries: Vec<GenesisMinerEntry> = self
            .miners
            .into_iter()
            .enumerate()
            .map(|(i, entry)| {
                eyre::ensure!(
                    entry.pledge_count > 0,
                    "miner[{}] has pledge_count == 0; every miner must pledge at least one partition",
                    i,
                );
                let key_bytes = hex::decode(entry.mining_key.trim_start_matches("0x"))
                    .map_err(|e| eyre::eyre!("Invalid hex for miner[{}] mining_key: {}", i, e))?;
                let signing_key = SigningKey::from_slice(&key_bytes)
                    .map_err(|e| eyre::eyre!("Invalid signing key for miner[{}]: {}", i, e))?;
                Ok(GenesisMinerEntry {
                    signing_key,
                    pledge_count: entry.pledge_count,
                })
            })
            .collect::<eyre::Result<Vec<_>>>()?;

        // Sort by derived IrysAddress for canonical ordering.
        // Use sort_by_cached_key to avoid redundant EC derivations (O(n log n) calls).
        entries.sort_by_cached_key(|e| signer_from_key_address(&e.signing_key));

        // Detect duplicate keys by checking adjacent entries after sorting.
        // Compute addresses once rather than twice per window element.
        let addrs: Vec<IrysAddress> = entries
            .iter()
            .map(|e| signer_from_key_address(&e.signing_key))
            .collect();
        for pair in addrs.windows(2) {
            eyre::ensure!(
                pair[0] != pair[1],
                "duplicate mining key detected: two miners resolve to the same \
                 IrysAddress {}. Each miner must have a unique key.",
                pair[0],
            );
        }

        Ok(entries)
    }
}

/// The fully-assembled output of [`build_signed_genesis_block`].
pub struct GenesisOutput {
    /// The signed genesis block header.
    pub block: IrysBlockHeader,
    /// All commitment transactions (stakes + pledges) across every miner.
    pub commitments: Vec<CommitmentTransaction>,
    /// The Reth chain specification derived from the genesis timestamp.
    pub reth_chain_spec: Arc<ChainSpec>,
}

// ---------------------------------------------------------------------------
// Core builder
// ---------------------------------------------------------------------------

/// Build a fully-signed genesis block that includes commitments from multiple
/// miners.
///
/// The first miner in `miners` is treated as the block producer and signs the
/// block header. Every miner receives one stake commitment followed by
/// `pledge_count` pledge commitments. Anchors rotate globally across all
/// commitments so that every transaction has a unique ID.
///
/// # Errors
///
/// Returns an error if the reth chain spec cannot be built, difficulty
/// calculation fails, or block signing fails.
pub async fn build_signed_genesis_block(
    config: &Config,
    miners: &[GenesisMinerEntry],
) -> eyre::Result<GenesisOutput> {
    // 1. Determine timestamp (prefer configured value, else now())
    let configured_ts = config.consensus.genesis.timestamp_millis;
    let timestamp_millis = if configured_ts != 0 {
        configured_ts
    } else {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before UNIX epoch")
            .as_millis()
    };
    let timestamp_secs = Duration::from_millis(
        timestamp_millis
            .try_into()
            .wrap_err("timestamp_millis overflow")?,
    )
    .as_secs();

    // 2. Build reth chain spec
    let reth_chain_spec = irys_chain_spec(
        config.consensus.chain_id,
        &config.consensus.reth,
        &config.consensus.hardforks,
        timestamp_secs,
    )?;

    // 3. Build unsigned genesis block
    let number_of_ingress_proofs_total =
        config.number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(timestamp_secs));
    let mut genesis_block = build_unsigned_irys_genesis_block(
        &config.consensus.genesis,
        reth_chain_spec.genesis_hash(),
        number_of_ingress_proofs_total,
        config.consensus.hardforks.cascade.as_ref(),
    )?;

    // 4. Set timestamp fields
    if config.consensus.genesis.last_epoch_hash != H256::zero() {
        genesis_block.last_epoch_hash = config.consensus.genesis.last_epoch_hash;
    }
    genesis_block.timestamp = UnixTimestampMs::from_millis(timestamp_millis);
    genesis_block.last_diff_timestamp = UnixTimestampMs::from_millis(timestamp_millis);

    // 5. Generate multi-miner commitments
    let (commitments, initial_treasury) =
        generate_multi_miner_commitments(&mut genesis_block, config, miners).await;

    // 6. Calculate difficulty from total pledge count (total commitments minus
    //    one stake per miner).
    let total_pledges: u64 = miners.iter().map(|m| m.pledge_count).sum();
    let difficulty = calculate_initial_difficulty(&config.consensus, total_pledges as f64)
        .wrap_err("failed to calculate initial difficulty")?;
    genesis_block.diff = difficulty;
    genesis_block.treasury = initial_treasury;

    // 7. Run VDF for genesis
    run_vdf_for_genesis_block(&mut genesis_block, &config.vdf);

    // 8. Sign block with first miner's key
    let block_signer = signer_from_key(&miners[0].signing_key, config);
    block_signer
        .sign_block_header(&mut genesis_block)
        .wrap_err("failed to sign genesis block header")?;

    info!("=====================================");
    info!("GENESIS BLOCK CREATED (multi-miner)");
    info!("Hash: {}", genesis_block.block_hash);
    info!("Miners: {}", miners.len());
    info!("Total pledges: {}", total_pledges);
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

/// Build a signed genesis block from pre-existing commitment transactions.
///
/// Unlike [`build_signed_genesis_block`] which generates commitments from mining
/// keys, this function packages already-signed commitments into a genesis block.
/// The caller provides a signing key for the block header signature.
///
/// # Errors
///
/// Returns an error if difficulty calculation fails, VDF execution fails,
/// or block signing fails.
pub fn build_genesis_block_from_commitments(
    config: &Config,
    commitments: Vec<CommitmentTransaction>,
    block_signing_key: &SigningKey,
) -> eyre::Result<GenesisOutput> {
    // 1. Determine timestamp (prefer configured value, else now())
    let configured_ts = config.consensus.genesis.timestamp_millis;
    let timestamp_millis = if configured_ts != 0 {
        configured_ts
    } else {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before UNIX epoch")
            .as_millis()
    };
    let timestamp_secs = Duration::from_millis(
        timestamp_millis
            .try_into()
            .wrap_err("timestamp_millis overflow")?,
    )
    .as_secs();

    // 2. Build reth chain spec
    let reth_chain_spec = irys_chain_spec(
        config.consensus.chain_id,
        &config.consensus.reth,
        &config.consensus.hardforks,
        timestamp_secs,
    )?;

    // 3. Build unsigned genesis block
    let number_of_ingress_proofs_total =
        config.number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(timestamp_secs));
    let mut genesis_block = build_unsigned_irys_genesis_block(
        &config.consensus.genesis,
        reth_chain_spec.genesis_hash(),
        number_of_ingress_proofs_total,
    )?;

    // 4. Set timestamp fields
    if config.consensus.genesis.last_epoch_hash != H256::zero() {
        genesis_block.last_epoch_hash = config.consensus.genesis.last_epoch_hash;
    }
    genesis_block.timestamp = UnixTimestampMs::from_millis(timestamp_millis);
    genesis_block.last_diff_timestamp = UnixTimestampMs::from_millis(timestamp_millis);

    // 5. Register all commitment txids in the commitment ledger and sum values
    let ledger = get_or_create_commitment_ledger(&mut genesis_block);
    let mut initial_treasury = U256::zero();
    for commitment in &commitments {
        ledger.tx_ids.push(commitment.id());
        initial_treasury = initial_treasury.saturating_add(commitment.value());
    }

    // 6. Count pledges for difficulty
    let total_pledges = commitments
        .iter()
        .filter(|c| matches!(c.commitment_type(), CommitmentTypeV2::Pledge { .. }))
        .count() as u64;

    // 7. Calculate difficulty from pledge count
    let difficulty = calculate_initial_difficulty(&config.consensus, total_pledges as f64)
        .wrap_err("failed to calculate initial difficulty")?;
    genesis_block.diff = difficulty;
    genesis_block.treasury = initial_treasury;

    // 8. Run VDF for genesis
    run_vdf_for_genesis_block(&mut genesis_block, &config.vdf);

    // 9. Sign with provided key
    let block_signer = signer_from_key(block_signing_key, config);
    block_signer
        .sign_block_header(&mut genesis_block)
        .wrap_err("failed to sign genesis block header")?;

    info!("=====================================");
    info!("GENESIS BLOCK CREATED (from commitments)");
    info!("Hash: {}", genesis_block.block_hash);
    info!("Total commitments: {}", commitments.len());
    info!("Total pledges: {}", total_pledges);
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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create commitment transactions for every miner and register them in the
/// genesis block's commitment ledger.
///
/// For each miner: 1 stake + N pledges. Anchors rotate globally across ALL
/// commitments so that every txid is unique.
async fn generate_multi_miner_commitments(
    genesis_block: &mut IrysBlockHeader,
    config: &Config,
    miners: &[GenesisMinerEntry],
) -> (Vec<CommitmentTransaction>, U256) {
    let mut all_commitments: Vec<CommitmentTransaction> = Vec::new();
    // The very first stake uses H256::default() as anchor (matching existing
    // behaviour in `get_genesis_commitments`). Subsequent anchors rotate.
    let mut anchor = H256::default();

    for miner in miners {
        let signer = signer_from_key(&miner.signing_key, config);

        // -- Stake commitment --
        let mut stake = CommitmentTransaction::new_stake(&config.consensus, anchor);
        signer
            .sign_commitment(&mut stake)
            .expect("stake commitment should be signable");
        anchor = stake.id();
        all_commitments.push(stake);

        // -- Pledge commitments --
        for i in 0..miner.pledge_count {
            let mut pledge = CommitmentTransaction::new_pledge(
                &config.consensus,
                anchor,
                &i, // u64 implements PledgeDataProvider — returns itself as pledge count
                signer.address(),
            )
            .await;
            signer
                .sign_commitment(&mut pledge)
                .expect("pledge commitment should be signable");
            anchor = pledge.id();
            all_commitments.push(pledge);
        }
    }

    // Register all commitment txids in the genesis block's commitment ledger.
    let ledger = get_or_create_commitment_ledger(genesis_block);
    let mut total_value = U256::zero();
    for commitment in &all_commitments {
        ledger.tx_ids.push(commitment.id());
        total_value = total_value.saturating_add(commitment.value());
    }

    (all_commitments, total_value)
}

/// Find or create the `Commitment` system ledger on the genesis block.
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
        .expect("commitment ledger should exist after creation")
}

/// Construct an [`IrysSigner`] from a raw [`SigningKey`] and the node
/// [`Config`].
fn signer_from_key(key: &SigningKey, config: &Config) -> IrysSigner {
    IrysSigner {
        signer: key.clone(),
        chain_id: config.consensus.chain_id,
        chunk_size: config.consensus.chunk_size,
    }
}

/// Derive the [`IrysAddress`] from a [`SigningKey`] without requiring a full
/// [`Config`].
fn signer_from_key_address(key: &SigningKey) -> IrysAddress {
    use alloy_signer::utils::secret_key_to_address;
    secret_key_to_address(key).into()
}
