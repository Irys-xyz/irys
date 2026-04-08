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
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use eyre::Context as _;
use irys_config::chain::chainspec::build_unsigned_irys_genesis_block;
use irys_types::{
    CommitmentTransaction, CommitmentTypeV2, Config, H256, H256List, IrysAddress, IrysBlockHeader,
    IrysTransactionCommon as _, SystemLedger, SystemTransactionLedger, U256, UnixTimestamp,
    UnixTimestampMs, calculate_initial_difficulty, chainspec::irys_chain_spec, irys::IrysSigner,
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
///
/// **Security:** This file contains raw private keys. It must never be
/// committed to version control or shared over insecure channels.
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
            .wrap_err_with(|| format!("failed to read genesis miners file {:?}", path))?;
        let manifest: Self = toml::from_str(&contents)
            .wrap_err_with(|| format!("failed to parse genesis miners file {:?}", path))?;
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
        let entries: Vec<GenesisMinerEntry> = self
            .miners
            .into_iter()
            .enumerate()
            .map(|(i, entry)| {
                eyre::ensure!(
                    entry.pledge_count > 0,
                    "miner[{}] has pledge_count == 0; every miner must pledge at least one partition",
                    i,
                );
                // Hex→key parsing duplicated from commands::signing_key_from_hex for
                // per-miner error context (includes index `i`).
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

        // Compute addresses once for both canonical sorting and duplicate detection.
        let mut entries_with_addrs: Vec<(GenesisMinerEntry, IrysAddress)> = entries
            .into_iter()
            .map(|e| {
                let addr = signer_from_key_address(&e.signing_key);
                (e, addr)
            })
            .collect();
        entries_with_addrs.sort_by_key(|(_, addr)| *addr);

        // Detect duplicate keys by checking adjacent entries after sorting.
        for pair in entries_with_addrs.windows(2) {
            eyre::ensure!(
                pair[0].1 != pair[1].1,
                "duplicate mining key detected: two miners resolve to the same \
                 IrysAddress {}. Each miner must have a unique key.",
                pair[0].1,
            );
        }

        Ok(entries_with_addrs.into_iter().map(|(e, _)| e).collect())
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

/// Determine the initial packed partitions count for difficulty calculation.
///
/// Precedence (consensus-affecting for genesis difficulty):
/// 1. `genesis.initial_packed_partitions` — explicit override
/// 2. `epoch.num_capacity_partitions` — if set in the epoch config
/// 3. `total_pledges` — fallback matching pre-multi-miner behavior
fn initial_packed_partitions_from_config(config: &Config, total_pledges: u64) -> eyre::Result<f64> {
    if let Some(packed_partitions) = config.consensus.genesis.initial_packed_partitions {
        eyre::ensure!(
            packed_partitions.is_finite() && packed_partitions > 0.0,
            "consensus.genesis.initial_packed_partitions must be a finite value > 0"
        );
        return Ok(packed_partitions);
    }

    if let Some(capacity_partitions) = config.consensus.epoch.num_capacity_partitions {
        eyre::ensure!(
            capacity_partitions > 0,
            "consensus.epoch.num_capacity_partitions must be > 0 when used for genesis difficulty"
        );
        return Ok(capacity_partitions as f64);
    }

    // Fallback: use total pledges as packed partitions count, matching the
    // pre-multi-miner behavior where all pledged partitions were assumed packed.
    Ok(total_pledges as f64)
}

// ---------------------------------------------------------------------------
// Shared genesis block preparation
// ---------------------------------------------------------------------------

/// Intermediate state after building the unsigned genesis block and chain spec.
/// Shared between `build_signed_genesis_block` and `build_genesis_block_from_commitments`.
struct GenesisPrelude {
    genesis_block: IrysBlockHeader,
    reth_chain_spec: Arc<ChainSpec>,
}

/// Build the unsigned genesis block, reth chain spec, and set timestamp fields.
/// This is the shared prelude for both genesis builder paths.
fn prepare_unsigned_genesis(config: &Config) -> eyre::Result<GenesisPrelude> {
    // Determine timestamp (prefer configured value, else now())
    let configured_ts = config.consensus.genesis.timestamp_millis;
    // A configured timestamp of 0 is the sentinel for "use current time",
    // matching the convention in IrysNode::create_new_genesis_block.
    let timestamp_millis = if configured_ts != 0 {
        configured_ts
    } else {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before UNIX epoch")
            .as_millis()
    };
    let timestamp_secs = u64::try_from(timestamp_millis / 1000).map_err(|_| {
        eyre::eyre!(
            "timestamp seconds {} overflows u64",
            timestamp_millis / 1000
        )
    })?;

    // Build reth chain spec
    let reth_chain_spec = irys_chain_spec(
        config.consensus.chain_id,
        &config.consensus.reth,
        &config.consensus.hardforks,
        timestamp_secs,
    )?;

    // Build unsigned genesis block
    let number_of_ingress_proofs_total =
        config.number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(timestamp_secs));
    let mut genesis_block = build_unsigned_irys_genesis_block(
        &config.consensus.genesis,
        reth_chain_spec.genesis_hash(),
        number_of_ingress_proofs_total,
        config.consensus.hardforks.cascade.as_ref(),
    )?;

    // Set timestamp fields
    if config.consensus.genesis.last_epoch_hash != H256::zero() {
        genesis_block.last_epoch_hash = config.consensus.genesis.last_epoch_hash;
    }
    genesis_block.timestamp = UnixTimestampMs::from_millis(timestamp_millis);
    genesis_block.last_diff_timestamp = UnixTimestampMs::from_millis(timestamp_millis);

    Ok(GenesisPrelude {
        genesis_block,
        reth_chain_spec,
    })
}

/// Finalize a genesis block: set difficulty, run VDF, sign, and log.
fn finalize_genesis_block(
    genesis_block: &mut IrysBlockHeader,
    config: &Config,
    initial_treasury: U256,
    total_pledges: u64,
    signing_key: &SigningKey,
    label: &str,
) -> eyre::Result<()> {
    let packed_partitions = initial_packed_partitions_from_config(config, total_pledges)?;
    let difficulty = calculate_initial_difficulty(&config.consensus, packed_partitions)
        .wrap_err("failed to calculate initial difficulty")?;
    genesis_block.diff = difficulty;
    genesis_block.treasury = initial_treasury;

    run_vdf_for_genesis_block(genesis_block, &config.vdf);

    let block_signer = signer_from_key(signing_key, config);
    block_signer
        .sign_block_header(genesis_block)
        .wrap_err("failed to sign genesis block header")?;

    info!("=====================================");
    info!("GENESIS BLOCK CREATED ({label})");
    info!("Hash: {}", genesis_block.block_hash);
    info!("Total pledges: {}", total_pledges);
    info!("Packed partitions (from config): {}", packed_partitions);
    info!(
        "consensus.expected_genesis_hash = \"{}\"",
        genesis_block.block_hash
    );
    info!("=====================================");

    Ok(())
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
/// **Miner ordering is consensus-relevant.** The CLI manifest (`GenesisMinerManifest`)
/// canonicalizes order via [`GenesisMinerManifest::into_entries`]; direct callers
/// must ensure canonical ordering themselves.
///
/// **No minimum pledge count is enforced here.** The old single-miner path
/// required >= 3 storage submodules; this builder delegates that policy to the
/// caller (e.g. the CLI's manifest validation or the node's submodule config).
///
/// # Errors
///
/// Returns an error if `miners` is empty, the reth chain spec cannot be built,
/// difficulty calculation fails, or block signing fails.
pub async fn build_signed_genesis_block(
    config: &Config,
    miners: &[GenesisMinerEntry],
) -> eyre::Result<GenesisOutput> {
    eyre::ensure!(
        !miners.is_empty(),
        "at least one miner entry is required to build a genesis block"
    );

    // Verify miners are in canonical (IrysAddress-sorted) order.
    // The CLI's GenesisMinerManifest::into_entries() guarantees this, but direct
    // callers must also provide canonically-ordered entries for deterministic output.
    for pair in miners.windows(2) {
        let addr_a = signer_from_key_address(&pair[0].signing_key);
        let addr_b = signer_from_key_address(&pair[1].signing_key);
        eyre::ensure!(
            addr_a < addr_b,
            "miners must be sorted by IrysAddress for deterministic genesis. \
             Found {} (>= {}) out of order.",
            addr_a,
            addr_b,
        );
    }

    let GenesisPrelude {
        mut genesis_block,
        reth_chain_spec,
    } = prepare_unsigned_genesis(config)?;

    // Generate multi-miner commitments
    let (commitments, initial_treasury) =
        generate_multi_miner_commitments(&mut genesis_block, config, miners).await?;

    let total_pledges: u64 = miners.iter().map(|m| m.pledge_count).sum();
    finalize_genesis_block(
        &mut genesis_block,
        config,
        initial_treasury,
        total_pledges,
        &miners[0].signing_key, // First miner is the designated block producer / signer.
        "multi-miner",
    )?;

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
/// Returns an error if any commitment signature is invalid, difficulty
/// calculation fails, VDF execution fails, or block signing fails.
pub fn build_genesis_block_from_commitments(
    config: &Config,
    commitments: Vec<CommitmentTransaction>,
    block_signing_key: &SigningKey,
) -> eyre::Result<GenesisOutput> {
    let has_stake = commitments
        .iter()
        .any(|c| matches!(c.commitment_type(), CommitmentTypeV2::Stake));
    let has_pledge = commitments
        .iter()
        .any(|c| matches!(c.commitment_type(), CommitmentTypeV2::Pledge { .. }));
    eyre::ensure!(
        has_stake,
        "commitments must contain at least one stake (required to register a miner)"
    );
    eyre::ensure!(
        has_pledge,
        "commitments must contain at least one pledge (required for mining)"
    );

    let GenesisPrelude {
        mut genesis_block,
        reth_chain_spec,
    } = prepare_unsigned_genesis(config)?;

    // Validate all commitment signatures before building the block.
    // A corrupted/tampered JSON file should fail here rather than producing
    // a genesis block that peers will reject later.
    for (i, c) in commitments.iter().enumerate() {
        eyre::ensure!(
            c.is_signature_valid(),
            "commitment {} (txid={}) has an invalid signature",
            i,
            c.id(),
        );
    }

    // Validate that every miner with pledges also has a stake.
    // We check existence only, not ordering — `compute_commitment_state()`
    // categorizes commitments by type first, then processes all stakes before
    // all pledges, so input ordering is irrelevant.
    {
        use std::collections::BTreeSet;
        let staked: BTreeSet<IrysAddress> = commitments
            .iter()
            .filter(|c| matches!(c.commitment_type(), CommitmentTypeV2::Stake))
            .map(CommitmentTransaction::signer)
            .collect();
        for c in &commitments {
            if matches!(c.commitment_type(), CommitmentTypeV2::Pledge { .. }) {
                eyre::ensure!(
                    staked.contains(&c.signer()),
                    "miner {} has pledge commitments but no stake commitment. \
                     Every miner with pledges must also have a stake.",
                    c.signer(),
                );
            }
        }
    }

    // Reject duplicate commitment IDs — duplicates would inflate the treasury.
    {
        let mut seen = std::collections::BTreeSet::new();
        for (i, c) in commitments.iter().enumerate() {
            eyre::ensure!(
                seen.insert(c.id()),
                "duplicate commitment txid at index {i}: {}",
                c.id(),
            );
        }
    }

    // Register all commitment txids in the commitment ledger and sum values
    let ledger = get_or_create_commitment_ledger(&mut genesis_block);
    let mut initial_treasury = U256::zero();
    for (i, commitment) in commitments.iter().enumerate() {
        ledger.tx_ids.push(commitment.id());
        initial_treasury = initial_treasury
            .checked_add(commitment.value())
            .ok_or_else(|| eyre::eyre!("treasury overflow at commitment {i}"))?;
    }

    let total_pledges = commitments
        .iter()
        .filter(|c| matches!(c.commitment_type(), CommitmentTypeV2::Pledge { .. }))
        .count() as u64;

    finalize_genesis_block(
        &mut genesis_block,
        config,
        initial_treasury,
        total_pledges,
        block_signing_key,
        "from commitments",
    )?;

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
///
/// Pledge fees are calculated **per-miner**: the `&i` passed to `new_pledge` is
/// the miner-local pledge index (0, 1, 2, ...), NOT a global count across all
/// miners. This means each miner's pledge cost curve starts from their own first
/// pledge, matching the single-miner behavior from `get_genesis_commitments`.
async fn generate_multi_miner_commitments(
    genesis_block: &mut IrysBlockHeader,
    config: &Config,
    miners: &[GenesisMinerEntry],
) -> eyre::Result<(Vec<CommitmentTransaction>, U256)> {
    let mut all_commitments: Vec<CommitmentTransaction> = Vec::new();
    // Anchor chaining: each commitment's txid becomes the anchor for the next,
    // creating a dependency chain that ensures unique transaction IDs even when
    // multiple miners submit identical commitment parameters. The very first
    // stake uses H256::default() (matching `get_genesis_commitments`).
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
        // `i` is the per-miner pledge index used for fee calculation via
        // PledgeDataProvider. Each miner's fee curve starts independently.
        for i in 0..miner.pledge_count {
            let mut pledge =
                CommitmentTransaction::new_pledge(&config.consensus, anchor, &i, signer.address())
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
        total_value = total_value.checked_add(commitment.value()).ok_or_else(|| {
            eyre::eyre!("treasury overflow from config-derived commitment values")
        })?;
    }

    Ok((all_commitments, total_value))
}

/// Find or create the `Commitment` system ledger on the genesis block.
fn get_or_create_commitment_ledger(
    genesis_block: &mut IrysBlockHeader,
) -> &mut SystemTransactionLedger {
    let pos = genesis_block
        .system_ledgers
        .iter()
        .position(|e| e.ledger_id == SystemLedger::Commitment);
    match pos {
        Some(i) => &mut genesis_block.system_ledgers[i],
        None => {
            genesis_block.system_ledgers.push(SystemTransactionLedger {
                ledger_id: SystemLedger::Commitment.into(),
                tx_ids: H256List::new(),
            });
            genesis_block.system_ledgers.last_mut().unwrap()
        }
    }
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
