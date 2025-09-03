use crate::block_tree_service::ValidationResult;
use crate::{
    block_discovery::{get_commitment_tx_in_parallel, get_data_tx_in_parallel},
    block_producer::ledger_expiry,
    mempool_service::MempoolServiceMessage,
    mining::hash_to_number,
    services::ServiceSenders,
    shadow_tx_generator::{PublishLedgerWithTxs, ShadowTxGenerator},
};
use alloy_consensus::Transaction as _;
use alloy_eips::eip7685::{Requests, RequestsOrHash};
use alloy_rpc_types_engine::ExecutionData;
use eyre::{ensure, OptionExt as _};
use irys_database::db::IrysDatabaseExt as _;
use irys_database::{block_header_by_hash, tx_header_by_txid, SystemLedger};
use irys_domain::{
    BlockIndex, BlockIndexReadGuard, BlockTreeReadGuard, EmaSnapshot, EpochSnapshot,
    ExecutionPayloadCache,
};
use irys_packing::{capacity_single::compute_entropy_chunk, xor_vec_u8_arrays_in_place};
use irys_primitives::CommitmentType;
use irys_reth::shadow_tx::{ShadowTransaction, IRYS_SHADOW_EXEC, SHADOW_TX_DESTINATION_ADDR};
use irys_reth_node_bridge::IrysRethNodeAdapter;
use irys_reward_curve::HalvingCurve;
use irys_storage::ii;
use irys_types::get_ingress_proofs;
use irys_types::storage_pricing::phantoms::{Irys, NetworkFee};
use irys_types::storage_pricing::{calculate_perm_fee_from_config, Amount};
use irys_types::BlockHash;
use irys_types::{
    app_state::DatabaseProvider,
    calculate_difficulty, next_cumulative_diff,
    transaction::fee_distribution::{PublishFeeCharges, TermFeeCharges},
    validate_path, Address, CommitmentTransaction, Config, ConsensusConfig, DataLedger,
    DataTransactionHeader, DataTransactionLedger, DifficultyAdjustmentConfig, IrysBlockHeader,
    PoaData, H256, U256,
};
use irys_vdf::last_step_checkpoints_is_valid;
use irys_vdf::state::VdfStateReadonly;
use itertools::*;
use openssl::sha;
use reth::rpc::api::EngineApiClient as _;
use reth::rpc::types::engine::ExecutionPayload;
use reth_db::Database as _;
use reth_ethereum_primitives::Block;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use tracing::{debug, error, info, warn, Instrument as _};

#[derive(Debug, PartialEq, Error)]
pub enum PreValidationError {
    #[error("Failed to get block bounds: {0}")]
    BlockBoundsLookupError(String),
    #[error("block signature is not valid")]
    BlockSignatureInvalid,
    #[error("Invalid cumulative_difficulty (expected {expected} got {got})")]
    CumulativeDifficultyMismatch { expected: U256, got: U256 },
    #[error("Invalid difficulty (expected {expected} got {got})")]
    DifficultyMismatch { expected: U256, got: U256 },
    #[error("Ema mismatch: recomputed EMA does not match Ema in block header")]
    EmaMismatch,
    #[error("EmaSnapshot creation error: {0}")]
    EmaSnapshotError(String),
    #[error("Ingress proofs missing")]
    IngressProofsMissing,
    #[error("Invalid ingress proof signature: {0}")]
    IngressProofSignatureInvalid(String),
    #[error("Invalid last_diff_timestamp (expected {expected} got {got})")]
    LastDiffTimestampMismatch { expected: u128, got: u128 },
    #[error("Invalid ledger id {ledger_id}")]
    LedgerIdInvalid { ledger_id: u32 },
    #[error("Invalid merkle proof: {0}")]
    MerkleProofInvalid(String),
    #[error("Oracle price invalid")]
    OraclePriceInvalid,
    #[error("PoA capacity chunk mismatch entropy_first={entropy_first:?} poa_first={poa_first:?}")]
    PoACapacityChunkMismatch {
        entropy_first: Option<u8>,
        poa_first: Option<u8>,
    },
    #[error("PoA chunk hash mismatch: expected {expected:?}, got {got:?}, ledger_id={ledger_id:?}, ledger_chunk_offset={ledger_chunk_offset:?}")]
    PoAChunkHashMismatch {
        expected: H256,
        got: H256,
        ledger_id: Option<u32>,
        ledger_chunk_offset: Option<u64>,
    },
    #[error("Missing PoA chunk to be pre validated")]
    PoAChunkMissing,
    #[error("PoA chunk offset out of tx's data chunks bounds")]
    PoAChunkOffsetOutOfDataChunksBounds,
    #[error("PoA chunk offset out of block bounds")]
    PoAChunkOffsetOutOfBlockBounds,
    #[error("PoA chunk offset out of tx bounds")]
    PoAChunkOffsetOutOfTxBounds,
    #[error("Missing partition assignment for partition hash {partition_hash}")]
    PartitionAssignmentMissing { partition_hash: H256 },
    #[error("Partition assignment for partition hash {partition_hash} is missing slot index")]
    PartitionAssignmentSlotIndexMissing { partition_hash: H256 },
    #[error("Partition assignment slot index too large for u64: {slot_index} (partition {partition_hash})")]
    PartitionAssignmentSlotIndexTooLarge {
        partition_hash: H256,
        slot_index: usize,
    },
    #[error(
        "Invalid data PoA, partition hash {partition_hash} is not a data partition, it may have expired"
    )]
    PoADataPartitionExpired { partition_hash: H256 },
    #[error("Invalid previous_cumulative_diff (expected {expected} got {got})")]
    PreviousCumulativeDifficultyMismatch { expected: U256, got: U256 },
    #[error("Invalid previous_solution_hash - expected {expected} got {got}")]
    PreviousSolutionHashMismatch { expected: H256, got: H256 },
    #[error("Reward curve error: {0}")]
    RewardCurveError(String),
    #[error("Reward mismatch: got {got}, expected {expected}")]
    RewardMismatch { got: U256, expected: U256 },
    #[error("Invalid solution_hash - expected difficulty >={expected} got {got}")]
    SolutionHashBelowDifficulty { expected: U256, got: U256 },
    #[error("Invalid solution_hash link - expected {expected} got {got}")]
    SolutionHashLinkInvalid { expected: H256, got: H256 },
    #[error("system time error: {0}")]
    SystemTimeError(String),
    #[error("block timestamp {current} is older than parent block {parent}")]
    TimestampOlderThanParent { current: u128, parent: u128 },
    #[error("block timestamp {current} too far in the future (now {now})")]
    TimestampTooFarInFuture { current: u128, now: u128 },
    #[error("Validation service unreachable")]
    ValidationServiceUnreachable,
    #[error("last_step_checkpoints validation failed: {0}")]
    VDFCheckpointsInvalid(String),
    #[error("vdf_limiter.prev_output ({got}) does not match previous blocks vdf_limiter.output ({expected})")]
    VDFPreviousOutputMismatch { got: H256, expected: H256 },
    #[error("Invalid block height (expected {expected} got {got})")]
    HeightInvalid { expected: u64, got: u64 },
    #[error("Invalid last_epoch_hash - expected {expected} got {got}")]
    LastEpochHashMismatch { expected: BlockHash, got: BlockHash },
    #[error("Transaction {tx_id} in Publish ledger must have a prior Submit ledger inclusion")]
    PublishTxMissingPriorSubmit { tx_id: H256 },

    #[error(
        "Transaction {tx_id} already included in previous Publish ledger in block {block_hash:?}"
    )]
    PublishTxAlreadyIncluded { tx_id: H256, block_hash: BlockHash },

    #[error("Transaction {tx_id} in Submit ledger was already included in past {ledger:?} ledger in block {block_hash:?}")]
    SubmitTxAlreadyIncluded {
        tx_id: H256,
        ledger: DataLedger,
        block_hash: BlockHash,
    },

    #[error("Transaction {tx_id} found in multiple previous blocks. First occurrence in {ledger:?} ledger at block {block_hash}")]
    TxFoundInMultipleBlocks {
        tx_id: H256,
        ledger: DataLedger,
        block_hash: BlockHash,
    },
    #[error("Publish transaction and ingress proof length mismatch, cannot validate publish ledger transaction proofs")]
    PublishTxProofLengthMismatch,
    #[error("Block EMA snapshot not found for block {block_hash}")]
    BlockEmaSnapshotNotFound { block_hash: BlockHash },
    #[error("Failed to extract data ledgers: {0}")]
    DataLedgerExtractionFailed(String),
    #[error("Failed to fetch transactions: {0}")]
    TransactionFetchFailed(String),
    #[error("Failed to get previous transaction inclusions: {0}")]
    PreviousTxInclusionsFailed(String),
    #[error("Transaction {tx_id} has invalid ledger_id. Expected: {expected}, Actual: {actual}")]
    InvalidLedgerId {
        tx_id: H256,
        expected: u32,
        actual: u32,
    },
    #[error("Failed to calculate fees: {0}")]
    FeeCalculationFailed(String),
    #[error("Transaction {tx_id} has insufficient perm_fee. Expected at least: {expected}, Actual: {actual}")]
    InsufficientPermFee {
        tx_id: H256,
        expected: U256,
        actual: U256,
    },
    #[error("Transaction {tx_id} has insufficient term_fee. Expected at least: {expected}, Actual: {actual}")]
    InsufficientTermFee {
        tx_id: H256,
        expected: U256,
        actual: U256,
    },
    #[error("Transaction {tx_id} has invalid term fee structure: {reason}")]
    InvalidTermFeeStructure { tx_id: H256, reason: String },
    #[error("Transaction {tx_id} has invalid perm fee structure: {reason}")]
    InvalidPermFeeStructure { tx_id: H256, reason: String },
    #[error(
        "Publish ledger proof count ({proof_count}) does not match transaction count ({tx_count})"
    )]
    PublishLedgerProofCountMismatch { proof_count: usize, tx_count: usize },
    #[error(
        "Incorrect Ingress proof count to publish a transaction. Expected: {expected}, Actual: {actual}"
    )]
    IngressProofCountMismatch { expected: usize, actual: usize },
    #[error("Transaction {tx_id} has invalid ingress proof: {reason}")]
    InvalidIngressProof { tx_id: H256, reason: String },
    #[error("Ingress proof mismatch for transaction {tx_id}")]
    IngressProofMismatch { tx_id: H256 },
    #[error("Duplicate ingress proof signer {signer} for transaction {tx_id}")]
    DuplicateIngressProofSigner { tx_id: H256, signer: Address },
    #[error("Database Error {error}")]
    DatabaseError { error: String },
}

/// Full pre-validation steps for a block
pub async fn prevalidate_block(
    block: IrysBlockHeader,
    previous_block: IrysBlockHeader,
    parent_epoch_snapshot: Arc<EpochSnapshot>,
    config: Config,
    reward_curve: Arc<HalvingCurve>,
    parent_ema_snapshot: &EmaSnapshot,
) -> Result<(), PreValidationError> {
    debug!(
        block_hash = ?block.block_hash,
        ?block.height,
        "Prevalidating block",
    );

    let poa_chunk: Vec<u8> = match &block.poa.chunk {
        Some(chunk) => chunk.clone().into(),
        None => return Err(PreValidationError::PoAChunkMissing),
    };

    let block_poa_hash: H256 = sha::sha256(&poa_chunk).into();
    if block.chunk_hash != block_poa_hash {
        return Err(PreValidationError::PoAChunkHashMismatch {
            expected: block.chunk_hash,
            got: block_poa_hash,
            ledger_id: None,
            ledger_chunk_offset: None,
        });
    }

    // Check prev_output (vdf)
    prev_output_is_valid(&block, &previous_block)?;
    debug!(
        block_hash = ?block.block_hash,
        ?block.height,
        "prev_output_is_valid",
    );

    // Check block height continuity
    height_is_valid(&block, &previous_block)?;
    debug!(
        block_hash = ?block.block_hash,
        ?block.height,
        "height_is_valid",
    );

    // Check block timestamp drift
    timestamp_is_valid(
        block.timestamp,
        previous_block.timestamp,
        config.consensus.max_future_timestamp_drift_millis,
    )?;

    // Check the difficulty
    difficulty_is_valid(
        &block,
        &previous_block,
        &config.consensus.difficulty_adjustment,
    )?;

    // Validate the last_diff_timestamp field
    last_diff_timestamp_is_valid(
        &block,
        &previous_block,
        &config.consensus.difficulty_adjustment,
    )?;

    debug!(
        block_hash = ?block.block_hash,
        ?block.height,
        "difficulty_is_valid",
    );

    // Validate previous_cumulative_diff points to parent's cumulative_diff
    previous_cumulative_difficulty_is_valid(&block, &previous_block)?;
    debug!(
        block_hash = ?block.block_hash,
        ?block.height,
        "previous_cumulative_difficulty_is_valid",
    );

    // Check the cumulative difficulty
    cumulative_difficulty_is_valid(&block, &previous_block)?;
    debug!(
        block_hash = ?block.block_hash,
        ?block.height,
        "cumulative_difficulty_is_valid",
    );

    check_poa_data_expiration(&block.poa, parent_epoch_snapshot.clone())?;
    debug!("poa data not expired");

    // Check the solution_hash
    solution_hash_is_valid(&block, &previous_block)?;
    debug!(
        block_hash = ?block.block_hash,
        ?block.height,
        "solution_hash_is_valid",
    );

    // Verify the solution_hash cryptographic link to PoA chunk, partition_chunk_offset and VDF seed
    solution_hash_link_is_valid(&block, &poa_chunk)?;
    debug!(
        block_hash = ?block.block_hash,
        ?block.height,
        "solution_hash_link_is_valid",
    );

    // Check the previous solution hash references the parent correctly
    previous_solution_hash_is_valid(&block, &previous_block)?;
    debug!(
        block_hash = ?block.block_hash,
        ?block.height,
        "previous_solution_hash_is_valid",
    );

    // Validate VDF seeds/next_seed against parent before any VDF-related processing
    let vdf_reset_frequency: u64 = config.vdf.reset_frequency as u64;
    if !matches!(
        is_seed_data_valid(&block, &previous_block, vdf_reset_frequency),
        ValidationResult::Valid
    ) {
        return Err(PreValidationError::VDFCheckpointsInvalid(
            "Seed data is invalid".to_string(),
        ));
    }

    // Ensure the last_epoch_hash field correctly references the most recent epoch block
    last_epoch_hash_is_valid(
        &block,
        &previous_block,
        config.consensus.epoch.num_blocks_in_epoch,
    )?;
    debug!(
        block_hash = ?block.block_hash,
        ?block.height,
        "last_epoch_hash_is_valid",
    );

    // We only check last_step_checkpoints during pre-validation
    last_step_checkpoints_is_valid(&block.vdf_limiter_info, &config.node_config.vdf())
        .await
        .map_err(|e| PreValidationError::VDFCheckpointsInvalid(e.to_string()))?;

    // Check that the oracle price does not exceed the EMA pricing parameters
    let oracle_price_valid = EmaSnapshot::oracle_price_is_valid(
        block.oracle_irys_price,
        previous_block.oracle_irys_price,
        config.consensus.token_price_safe_range,
    );
    if !oracle_price_valid {
        return Err(PreValidationError::OraclePriceInvalid);
    }

    // Check that the EMA has been correctly calculated
    let ema_valid = {
        let res = parent_ema_snapshot
            .calculate_ema_for_new_block(
                &previous_block,
                block.oracle_irys_price,
                config.consensus.token_price_safe_range,
                config.consensus.ema.price_adjustment_interval,
            )
            .ema;
        res == block.ema_irys_price
    };
    if !ema_valid {
        return Err(PreValidationError::EmaMismatch);
    }

    // Check valid curve price
    let reward = reward_curve
        .reward_between(
            // adjust ms to sec
            previous_block.timestamp.saturating_div(1000),
            block.timestamp.saturating_div(1000),
        )
        .map_err(|e| PreValidationError::RewardCurveError(e.to_string()))?;
    if reward.amount != block.reward_amount {
        return Err(PreValidationError::RewardMismatch {
            got: block.reward_amount,
            expected: reward.amount,
        });
    }

    // Validate ingress proof signer uniqueness
    validate_unique_ingress_proof_signers(&block)?;
    debug!(
        block_hash = ?block.block_hash,
        ?block.height,
        "ingress_proof_signers_unique",
    );

    // After pre-validating a bunch of quick checks we validate the signature
    // TODO: We may want to further check if the signer is a staked address
    // this is a little more advanced though as it requires knowing what the
    // commitment states looked like when this block was produced. For now
    // we just accept any valid signature.
    if !block.is_signature_valid() {
        return Err(PreValidationError::BlockSignatureInvalid);
    }

    // TODO: add validation for the term ledger 'expires' field,
    // ensuring it gets properly updated on epoch boundaries, and it's
    // consistent with the block's height and parent block's height

    Ok(())
}

pub fn prev_output_is_valid(
    block: &IrysBlockHeader,
    previous_block: &IrysBlockHeader,
) -> Result<(), PreValidationError> {
    if block.vdf_limiter_info.prev_output == previous_block.vdf_limiter_info.output {
        Ok(())
    } else {
        Err(PreValidationError::VDFPreviousOutputMismatch {
            got: block.vdf_limiter_info.prev_output,
            expected: previous_block.vdf_limiter_info.output,
        })
    }
}

// compares block timestamp against parent block
// errors if the block has a lower timestamp than the parent block
// compares timestamps of block against current system time
// errors on drift more than MAX_TIMESTAMP_DRIFT_SECS into future
pub fn timestamp_is_valid(
    current: u128,
    parent: u128,
    allowed_drift: u128,
) -> Result<(), PreValidationError> {
    // note: we have to make sure we don't overlap the parent's timestamp (even though it's very unlikely)
    if current <= parent {
        return Err(PreValidationError::TimestampOlderThanParent { current, parent });
    }

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| PreValidationError::SystemTimeError(e.to_string()))?
        .as_millis();

    let max_future = now_ms + allowed_drift;

    if current > max_future {
        return Err(PreValidationError::TimestampTooFarInFuture {
            current,
            now: now_ms,
        });
    }

    Ok(())
}

/// Validates if a block's difficulty matches the expected difficulty calculated
/// from previous block data.
/// Returns Ok if valid, Err if the difficulty doesn't match the calculated value.
pub fn difficulty_is_valid(
    block: &IrysBlockHeader,
    previous_block: &IrysBlockHeader,
    difficulty_config: &DifficultyAdjustmentConfig,
) -> Result<(), PreValidationError> {
    let block_height = block.height;
    let current_timestamp = block.timestamp;
    let last_diff_timestamp = previous_block.last_diff_timestamp;
    let current_difficulty = previous_block.diff;

    let (diff, _stats) = calculate_difficulty(
        block_height,
        last_diff_timestamp,
        current_timestamp,
        current_difficulty,
        difficulty_config,
    );

    if diff == block.diff {
        Ok(())
    } else {
        Err(PreValidationError::DifficultyMismatch {
            expected: diff,
            got: block.diff,
        })
    }
}

/// Validates the `last_diff_timestamp` field in the block.
///
/// The value should equal the previous block's `last_diff_timestamp` unless the
/// current block triggers a difficulty adjustment, in which case it must be set
/// to the block's own timestamp.
pub fn last_diff_timestamp_is_valid(
    block: &IrysBlockHeader,
    previous_block: &IrysBlockHeader,
    difficulty_config: &DifficultyAdjustmentConfig,
) -> Result<(), PreValidationError> {
    let blocks_between_adjustments = difficulty_config.difficulty_adjustment_interval;
    let expected = if block.height % blocks_between_adjustments == 0 {
        block.timestamp
    } else {
        previous_block.last_diff_timestamp
    };

    if block.last_diff_timestamp == expected {
        Ok(())
    } else {
        Err(PreValidationError::LastDiffTimestampMismatch {
            expected,
            got: block.last_diff_timestamp,
        })
    }
}

/// Checks PoA data chunk data solution partitions has not expired
pub fn check_poa_data_expiration(
    poa: &PoaData,
    epoch_snapshot: Arc<EpochSnapshot>,
) -> Result<(), PreValidationError> {
    let is_data_partition_assigned = epoch_snapshot
        .partition_assignments
        .data_partitions
        .contains_key(&poa.partition_hash);

    // if is a data chunk
    if poa.data_path.is_some()
        && poa.tx_path.is_some()
        && poa.ledger_id.is_some()
        && !is_data_partition_assigned
    {
        return Err(PreValidationError::PoADataPartitionExpired {
            partition_hash: poa.partition_hash,
        });
    };
    Ok(())
}

/// Validates if a block's cumulative difficulty equals the previous cumulative difficulty
/// plus the expected hashes from its new difficulty. Returns Ok if valid.
///
/// Note: Requires valid block difficulty - call `difficulty_is_valid()` first.
pub fn cumulative_difficulty_is_valid(
    block: &IrysBlockHeader,
    previous_block: &IrysBlockHeader,
) -> Result<(), PreValidationError> {
    let previous_cumulative_diff = previous_block.cumulative_diff;
    let new_diff = block.diff;

    let cumulative_diff = next_cumulative_diff(previous_cumulative_diff, new_diff);
    if cumulative_diff == block.cumulative_diff {
        Ok(())
    } else {
        Err(PreValidationError::CumulativeDifficultyMismatch {
            expected: cumulative_diff,
            got: block.cumulative_diff,
        })
    }
}

/// Validates that the block's previous_cumulative_diff equals the parent's cumulative_diff
pub fn previous_cumulative_difficulty_is_valid(
    block: &IrysBlockHeader,
    previous_block: &IrysBlockHeader,
) -> Result<(), PreValidationError> {
    if block.previous_cumulative_diff == previous_block.cumulative_diff {
        Ok(())
    } else {
        Err(PreValidationError::PreviousCumulativeDifficultyMismatch {
            expected: previous_block.cumulative_diff,
            got: block.previous_cumulative_diff,
        })
    }
}

/// Checks to see if the `solution_hash` exceeds the difficulty threshold
/// of the previous block
///
/// Note: Requires valid block difficulty - call `difficulty_is_valid()` first.
pub fn solution_hash_is_valid(
    block: &IrysBlockHeader,
    previous_block: &IrysBlockHeader,
) -> Result<(), PreValidationError> {
    let solution_hash = block.solution_hash;
    let solution_diff = hash_to_number(&solution_hash.0);

    if solution_diff >= previous_block.diff {
        Ok(())
    } else {
        Err(PreValidationError::SolutionHashBelowDifficulty {
            expected: previous_block.diff,
            got: solution_diff,
        })
    }
}

/// Validates the cryptographic link between solution_hash and its inputs:
/// PoA chunk bytes, partition_chunk_offset (little-endian), and the VDF seed (vdf_limiter_info.output)
pub fn solution_hash_link_is_valid(
    block: &IrysBlockHeader,
    poa_chunk: &[u8],
) -> Result<(), PreValidationError> {
    let expected = irys_types::compute_solution_hash(
        poa_chunk,
        block.poa.partition_chunk_offset,
        &block.vdf_limiter_info.output,
    );

    if block.solution_hash == expected {
        Ok(())
    } else {
        Err(PreValidationError::SolutionHashLinkInvalid {
            expected,
            got: block.solution_hash,
        })
    }
}

/// Checks if the `previous_solution_hash` equals the previous block's `solution_hash`
pub fn previous_solution_hash_is_valid(
    block: &IrysBlockHeader,
    previous_block: &IrysBlockHeader,
) -> Result<(), PreValidationError> {
    if block.previous_solution_hash == previous_block.solution_hash {
        Ok(())
    } else {
        Err(PreValidationError::PreviousSolutionHashMismatch {
            expected: previous_block.solution_hash,
            got: block.previous_solution_hash,
        })
    }
}

/// Validates the `last_epoch_hash` field against the previous block and epoch rules.
pub fn last_epoch_hash_is_valid(
    block: &IrysBlockHeader,
    previous_block: &IrysBlockHeader,
    blocks_in_epoch: u64,
) -> Result<(), PreValidationError> {
    // if First block after an epoch boundary
    let expected = if block.height > 0 && block.height % blocks_in_epoch == 1 {
        previous_block.block_hash
    } else {
        previous_block.last_epoch_hash
    };

    if block.last_epoch_hash == expected {
        Ok(())
    } else {
        Err(PreValidationError::LastEpochHashMismatch {
            expected,
            got: block.last_epoch_hash,
        })
    }
}

// Validates block height against previous block height + 1
pub fn height_is_valid(
    block: &IrysBlockHeader,
    previous_block: &IrysBlockHeader,
) -> Result<(), PreValidationError> {
    let expected = previous_block.height + 1;
    if block.height == expected {
        Ok(())
    } else {
        Err(PreValidationError::HeightInvalid {
            expected,
            got: block.height,
        })
    }
}

#[cfg(test)]
mod height_tests {
    use super::*;

    #[test]
    fn height_is_valid_ok() {
        let mut prev = IrysBlockHeader::new_mock_header();
        prev.height = 10;
        let mut block = IrysBlockHeader::new_mock_header();
        block.height = 11;
        assert!(height_is_valid(&block, &prev).is_ok());
    }

    #[test]
    fn height_is_invalid_fails() {
        let mut prev = IrysBlockHeader::new_mock_header();
        prev.height = 10;
        let mut block = IrysBlockHeader::new_mock_header();
        block.height = 12;
        assert!(height_is_valid(&block, &prev).is_err());
    }
}

/// Returns Ok if the vdf recall range in the block is valid
pub async fn recall_recall_range_is_valid(
    block: &IrysBlockHeader,
    config: &ConsensusConfig,
    steps_guard: &VdfStateReadonly,
) -> eyre::Result<()> {
    let num_recall_ranges_in_partition =
        irys_efficient_sampling::num_recall_ranges_in_partition(config);
    let reset_step_number = irys_efficient_sampling::reset_step_number(
        block.vdf_limiter_info.global_step_number,
        config,
    );
    info!(
        "Validating recall ranges steps from: {} to: {}",
        reset_step_number, block.vdf_limiter_info.global_step_number
    );
    let steps = steps_guard.read().get_steps(ii(
        reset_step_number,
        block.vdf_limiter_info.global_step_number,
    ))?;
    irys_efficient_sampling::recall_range_is_valid(
        (block.poa.partition_chunk_offset as u64 / config.num_chunks_in_recall_range) as usize,
        num_recall_ranges_in_partition as usize,
        &steps,
        &block.poa.partition_hash,
    )
}

pub fn get_recall_range(
    step_num: u64,
    config: &ConsensusConfig,
    steps_guard: &VdfStateReadonly,
    partition_hash: &H256,
) -> eyre::Result<usize> {
    let num_recall_ranges_in_partition =
        irys_efficient_sampling::num_recall_ranges_in_partition(config);
    let reset_step_number = irys_efficient_sampling::reset_step_number(step_num, config);
    let steps = steps_guard
        .read()
        .get_steps(ii(reset_step_number, step_num))?;
    irys_efficient_sampling::get_recall_range(
        num_recall_ranges_in_partition as usize,
        &steps,
        partition_hash,
    )
}

/// Returns Ok if the provided `PoA` is valid, Err otherwise
#[tracing::instrument(skip_all, fields(
    ?miner_address,
    chunk_offset = ?poa.partition_chunk_offset,
    partition_hash = ?poa.partition_hash,
    entropy_packing_iterations = ?config.entropy_packing_iterations,
    chunk_size = ?config.chunk_size
), err)]

pub fn poa_is_valid(
    poa: &PoaData,
    block_index_guard: &BlockIndexReadGuard,
    epoch_snapshot: &EpochSnapshot,
    config: &ConsensusConfig,
    miner_address: &Address,
) -> Result<(), PreValidationError> {
    debug!("PoA validating");
    let mut poa_chunk: Vec<u8> = match &poa.chunk {
        Some(chunk) => chunk.clone().into(),
        None => return Err(PreValidationError::PoAChunkMissing),
    };
    // data chunk
    if let (Some(data_path), Some(tx_path), Some(ledger_id)) =
        (poa.data_path.clone(), poa.tx_path.clone(), poa.ledger_id)
    {
        // partition data -> ledger data
        let partition_assignment = epoch_snapshot
            .get_data_partition_assignment(poa.partition_hash)
            .ok_or(PreValidationError::PartitionAssignmentMissing {
                partition_hash: poa.partition_hash,
            })?;

        let slot_index = partition_assignment.slot_index.ok_or(
            PreValidationError::PartitionAssignmentSlotIndexMissing {
                partition_hash: poa.partition_hash,
            },
        )?;
        let slot_index_u64 = u64::try_from(slot_index).map_err(|_| {
            PreValidationError::PartitionAssignmentSlotIndexTooLarge {
                partition_hash: poa.partition_hash,
                slot_index,
            }
        })?;
        let ledger_chunk_offset =
            slot_index_u64 * config.num_partitions_per_slot * config.num_chunks_in_partition
                + u64::from(poa.partition_chunk_offset);

        // ledger data -> block
        let ledger = DataLedger::try_from(ledger_id)
            .map_err(|_| PreValidationError::LedgerIdInvalid { ledger_id })?;

        let bb = block_index_guard
            .read()
            .get_block_bounds(ledger, ledger_chunk_offset)
            .map_err(|e| PreValidationError::BlockBoundsLookupError(e.to_string()))?;
        if !(bb.start_chunk_offset..bb.end_chunk_offset).contains(&ledger_chunk_offset) {
            return Err(PreValidationError::PoAChunkOffsetOutOfBlockBounds);
        };

        let block_chunk_offset = (ledger_chunk_offset - bb.start_chunk_offset) as u128;

        // tx_path validation
        let tx_path_result = validate_path(
            bb.tx_root.0,
            &tx_path,
            block_chunk_offset * (config.chunk_size as u128),
        )
        .map_err(|e| PreValidationError::MerkleProofInvalid(e.to_string()))?;

        if !(tx_path_result.left_bound..=tx_path_result.right_bound)
            .contains(&(block_chunk_offset * (config.chunk_size as u128)))
        {
            return Err(PreValidationError::PoAChunkOffsetOutOfTxBounds);
        }

        let tx_chunk_offset =
            block_chunk_offset * (config.chunk_size as u128) - tx_path_result.left_bound;

        // data_path validation
        let data_path_result = validate_path(tx_path_result.leaf_hash, &data_path, tx_chunk_offset)
            .map_err(|e| PreValidationError::MerkleProofInvalid(e.to_string()))?;

        if !(data_path_result.left_bound..=data_path_result.right_bound).contains(&tx_chunk_offset)
        {
            return Err(PreValidationError::PoAChunkOffsetOutOfDataChunksBounds);
        }

        let mut entropy_chunk = Vec::<u8>::with_capacity(config.chunk_size as usize);
        compute_entropy_chunk(
            *miner_address,
            poa.partition_chunk_offset as u64,
            poa.partition_hash.into(),
            config.entropy_packing_iterations,
            config.chunk_size as usize,
            &mut entropy_chunk,
            config.chain_id,
        );

        xor_vec_u8_arrays_in_place(&mut poa_chunk, &entropy_chunk);

        // Because all chunks are packed as config.chunk_size, if the proof chunk is
        // smaller we need to trim off the excess padding introduced by packing ?
        let (poa_chunk_pad_trimmed, _) = poa_chunk.split_at(
            (config
                .chunk_size
                .min((data_path_result.right_bound - data_path_result.left_bound) as u64))
                as usize,
        );

        let poa_chunk_hash = sha::sha256(poa_chunk_pad_trimmed);

        if poa_chunk_hash != data_path_result.leaf_hash {
            return Err(PreValidationError::PoAChunkHashMismatch {
                expected: data_path_result.leaf_hash.into(),
                got: poa_chunk_hash.into(),
                ledger_id: Some(ledger_id),
                ledger_chunk_offset: Some(ledger_chunk_offset),
            });
        }
    } else {
        let mut entropy_chunk = Vec::<u8>::with_capacity(config.chunk_size as usize);
        compute_entropy_chunk(
            *miner_address,
            poa.partition_chunk_offset as u64,
            poa.partition_hash.into(),
            config.entropy_packing_iterations,
            config.chunk_size as usize,
            &mut entropy_chunk,
            config.chain_id,
        );
        if entropy_chunk != poa_chunk {
            if poa_chunk.len() <= 32 {
                debug!("Chunk PoA:{:?}", poa_chunk);
                debug!("Entropy  :{:?}", entropy_chunk);
            }
            return Err(PreValidationError::PoACapacityChunkMismatch {
                entropy_first: entropy_chunk.first().copied(),
                poa_first: poa_chunk.first().copied(),
            });
        }
    }
    Ok(())
}

/// Validates that the shadow transactions in the EVM block match the expected shadow transactions
/// generated from the Irys block data.
pub async fn shadow_transactions_are_valid(
    config: &Config,
    service_senders: &ServiceSenders,
    block: &IrysBlockHeader,
    reth_adapter: &IrysRethNodeAdapter,
    db: &DatabaseProvider,
    payload_provider: ExecutionPayloadCache,
    parent_epoch_snapshot: Arc<EpochSnapshot>,
    block_index: Arc<std::sync::RwLock<BlockIndex>>,
) -> eyre::Result<()> {
    // 1. Validate that the evm block is valid
    let execution_data = payload_provider
        .wait_for_payload(&block.evm_block_hash)
        .await
        .ok_or_eyre("reth execution payload never arrived")?;

    let engine_api_client = reth_adapter.inner.engine_http_client();
    let ExecutionData { payload, sidecar } = execution_data;

    let ExecutionPayload::V3(payload_v3) = payload else {
        eyre::bail!("irys-reth expects that all payloads are of v3 type");
    };
    ensure!(
        payload_v3.withdrawals().is_empty(),
        "withdrawals must always be empty"
    );

    // ensure the execution payload timestamp matches the block timestamp
    // truncated to full seconds
    let payload_timestamp: u128 = payload_v3.timestamp().into();
    let block_timestamp_sec = block.timestamp / 1000;
    ensure!(
            payload_timestamp == block_timestamp_sec,
            "EVM payload timestamp {payload_timestamp} does not match block timestamp {block_timestamp_sec}"
        );

    let versioned_hashes = sidecar
        .versioned_hashes()
        .ok_or_eyre("version hashes must be present")?
        .clone();
    loop {
        let payload_status = engine_api_client
            .new_payload_v4(
                payload_v3.clone(),
                versioned_hashes.clone(),
                block.previous_block_hash.into(),
                RequestsOrHash::Requests(Requests::new(vec![])),
            )
            .await?;
        match payload_status.status {
            alloy_rpc_types_engine::PayloadStatusEnum::Invalid { validation_error } => {
                return Err(eyre::Report::msg(validation_error));
            }
            alloy_rpc_types_engine::PayloadStatusEnum::Syncing => {
                tracing::debug!(
                    "syncing extra blocks to validate payload {:?}",
                    payload_v3.payload_inner.payload_inner.block_num_hash()
                );
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            alloy_rpc_types_engine::PayloadStatusEnum::Valid => {
                tracing::info!("reth payload already known & is valid");
                break;
            }
            alloy_rpc_types_engine::PayloadStatusEnum::Accepted => {
                tracing::info!("accepted a side-chain (fork) payload");
                break;
            }
        }
    }
    let evm_block: Block = payload_v3.try_into_block()?;

    // 2. Extract shadow transactions from the beginning of the block
    let mut expect_shadow_txs = true;
    let actual_shadow_txs = evm_block
        .body
        .transactions
        .into_iter()
        .map(|tx| {
            if expect_shadow_txs {
                if Some(*SHADOW_TX_DESTINATION_ADDR) != tx.to() {
                    // after reaching first non-shadow tx, we scan the rest of the
                    // txs to check if we don't have any stray shadow txs in there
                    expect_shadow_txs = false;
                    ensure!(
                        !tx.input().starts_with(IRYS_SHADOW_EXEC),
                        "shadow tx injected in the middle of the block",
                    );
                    return Ok(None);
                }
                let input = tx.input();
                if input.strip_prefix(IRYS_SHADOW_EXEC).is_none() {
                    // after reaching first non-shadow tx, we scan the rest of the
                    // txs to check if we don't have any stray shadow txs in there
                    expect_shadow_txs = false;
                    return Ok(None);
                };
                let shadow_tx = ShadowTransaction::decode(&mut &input[..])
                    .map_err(|e| eyre::eyre!("failed to decode shadow tx: {e}"))?;
                let tx_signer = tx.into_signed().recover_signer()?;

                ensure!(
                    block.miner_address == tx_signer,
                    "Shadow tx signer is not the miner"
                );
                Ok(Some(shadow_tx))
            } else {
                // ensure that no other shadow txs are present in the block
                let input = tx.input();
                ensure!(
                    !(input.starts_with(IRYS_SHADOW_EXEC)),
                    "shadow tx injected in the middle of the block",
                );
                Ok(None)
            }
        })
        .filter_map(std::result::Result::transpose);

    // 3. Generate expected shadow transactions
    let expected_txs = generate_expected_shadow_transactions_from_db(
        config,
        service_senders,
        block,
        db,
        parent_epoch_snapshot,
        block_index,
    )
    .await?;

    // 4. Validate they match
    validate_shadow_transactions_match(actual_shadow_txs, expected_txs.into_iter())
}

/// Generates expected shadow transactions by looking up required data from the mempool or database
#[tracing::instrument(skip_all, err)]
async fn generate_expected_shadow_transactions_from_db<'a>(
    config: &Config,
    service_senders: &ServiceSenders,
    block: &'a IrysBlockHeader,
    db: &DatabaseProvider,
    parent_epoch_snapshot: Arc<EpochSnapshot>,
    block_index: Arc<std::sync::RwLock<BlockIndex>>,
) -> eyre::Result<Vec<ShadowTransaction>> {
    // Look up previous block to get EVM hash
    let prev_block = {
        let (tx_prev, rx_prev) = tokio::sync::oneshot::channel();
        service_senders
            .mempool
            .send(MempoolServiceMessage::GetBlockHeader(
                block.previous_block_hash,
                false,
                tx_prev,
            ))?;
        match rx_prev.await? {
            Some(h) => h,
            None => db
                .view_eyre(|tx| block_header_by_hash(tx, &block.previous_block_hash, false))?
                .ok_or_eyre("Previous block not found")?,
        }
    };

    // Look up commitment txs
    let commitment_txs = extract_commitment_txs(config, service_senders, block, db).await?;

    // Lookup data txs
    let data_txs = extract_submit_ledger_txs(service_senders, block, db).await?;

    // Lookup publish ledger for term fee rewards
    let mut publish_ledger_with_txs =
        extract_publish_ledger_with_txs(service_senders, block, db).await?;

    // Get treasury balance from previous block
    let initial_treasury_balance = prev_block.treasury;

    // Calculate expired ledger fees for epoch blocks
    let is_epoch_block = block.height % config.consensus.epoch.num_blocks_in_epoch == 0;
    let expired_ledger_fees = if is_epoch_block {
        ledger_expiry::calculate_expired_ledger_fees(
            &parent_epoch_snapshot,
            block.height,
            DataLedger::Submit, // Currently only Submit ledgers expire
            config,
            block_index,
            service_senders.mempool.clone(),
            db.clone(),
        )
        .in_current_span()
        .await?
    } else {
        BTreeMap::new()
    };

    let mut shadow_tx_generator = ShadowTxGenerator::new(
        &block.height,
        &block.reward_address,
        &block.reward_amount,
        &prev_block,
        &config.consensus,
        &commitment_txs,
        &data_txs,
        &mut publish_ledger_with_txs,
        initial_treasury_balance,
        &expired_ledger_fees,
    );

    let mut shadow_txs_vec = Vec::new();
    for result in shadow_tx_generator.by_ref() {
        let metadata = result?;
        shadow_txs_vec.push(metadata.shadow_tx);
    }

    // Get final treasury balance after processing all transactions
    let expected_treasury = shadow_tx_generator.treasury_balance();

    // Validate that the block's treasury matches the expected value
    ensure!(
        block.treasury == expected_treasury,
        "Treasury mismatch: expected {} but found {} at block height {}",
        expected_treasury,
        block.treasury,
        block.height
    );

    Ok(shadow_txs_vec)
}

async fn extract_commitment_txs(
    config: &Config,
    service_senders: &ServiceSenders,
    block: &IrysBlockHeader,
    db: &DatabaseProvider,
) -> Result<Vec<CommitmentTransaction>, eyre::Error> {
    let is_epoch_block = block.height % config.consensus.epoch.num_blocks_in_epoch == 0;
    let commitment_txs = if is_epoch_block {
        // IMPORTANT: on epoch blocks we don't generate shadow txs for commitment txs
        vec![]
    } else {
        match &block.system_ledgers[..] {
            [ledger] => {
                ensure!(
                    ledger.ledger_id == SystemLedger::Commitment,
                    "only commitment ledger supported"
                );

                get_commitment_tx_in_parallel(&ledger.tx_ids.0, &service_senders.mempool, db)
                    .await?
            }
            [] => {
                // this is valid as we can have a block that contains 0 system ledgers
                vec![]
            }
            // this is to ensure that we don't skip system ledgers and forget to add them to validation in the future
            [..] => eyre::bail!("Currently we support at most 1 system ledger per block"),
        }
    };
    Ok(commitment_txs)
}

async fn extract_submit_ledger_txs(
    service_senders: &ServiceSenders,
    block: &IrysBlockHeader,
    db: &DatabaseProvider,
) -> Result<Vec<DataTransactionHeader>, eyre::Error> {
    let (_publish_ledger, submit_ledger) = extract_data_ledgers(block)?;
    // we only access the submit ledger data. Publish ledger does not require billing the user extra
    let txs = get_data_tx_in_parallel(submit_ledger.tx_ids.0.clone(), &service_senders.mempool, db)
        .await?;
    Ok(txs)
}

/// Extracts publish ledger with transactions and ingress proofs for term fee reward distribution
async fn extract_publish_ledger_with_txs(
    service_senders: &ServiceSenders,
    block: &IrysBlockHeader,
    db: &DatabaseProvider,
) -> Result<PublishLedgerWithTxs, eyre::Error> {
    let (publish_ledger, _submit_ledger) = extract_data_ledgers(block)?;

    // Fetch the actual transactions for the publish ledger
    let txs = get_data_tx_in_parallel(
        publish_ledger.tx_ids.0.clone(),
        &service_senders.mempool,
        db,
    )
    .await?;
    Ok(PublishLedgerWithTxs {
        txs,
        proofs: publish_ledger.proofs.clone(),
    })
}

/// Validates  the actual shadow transactions match the expected ones
#[tracing::instrument(skip_all, err)]
fn validate_shadow_transactions_match(
    actual: impl Iterator<Item = eyre::Result<ShadowTransaction>>,
    expected: impl Iterator<Item = ShadowTransaction>,
) -> eyre::Result<()> {
    // Validate each expected shadow transaction
    for (idx, data) in actual.zip_longest(expected).enumerate() {
        let EitherOrBoth::Both(actual, expected) = data else {
            // If either of the shadow txs is not present, it means it was not generated as `expected`
            // or it was not included in the block. either way - an error
            tracing::warn!(?data, "shadow tx len mismatch");
            eyre::bail!("actual and expected shadow txs lens differ");
        };
        let actual = actual?;
        ensure!(
            actual == expected,
            "Shadow transaction mismatch at idx {}. expected {:?}, got {:?}",
            idx,
            expected,
            actual
        );
    }

    Ok(())
}

pub fn is_seed_data_valid(
    block_header: &IrysBlockHeader,
    previous_block_header: &IrysBlockHeader,
    reset_frequency: u64,
) -> ValidationResult {
    let vdf_info = &block_header.vdf_limiter_info;
    let expected_seed_data = vdf_info.calculate_seeds(reset_frequency, previous_block_header);

    // TODO: difficulty validation adjustment is likely needs to be done here too,
    //  but difficulty is not yet implemented
    let are_seeds_valid =
        expected_seed_data.0 == vdf_info.next_seed && expected_seed_data.1 == vdf_info.seed;
    if are_seeds_valid {
        ValidationResult::Valid
    } else {
        error!(
            "Seed data is invalid. Expected: {:?}, got: {:?}",
            expected_seed_data, vdf_info
        );
        ValidationResult::Invalid
    }
}

/// Validates that commitment transactions in a block are ordered correctly
/// according to the same priority rules used by the mempool:
/// 1. Stakes first (sorted by fee, highest first)
/// 2. Then pledges (sorted by pledge_count_before_executing ascending, then by fee descending)
#[tracing::instrument(skip_all, err, fields(block_hash = %block.block_hash, block_height = %block.height))]
pub async fn commitment_txs_are_valid(
    config: &Config,
    service_senders: &ServiceSenders,
    block: &IrysBlockHeader,
    db: &DatabaseProvider,
    block_tree_guard: &BlockTreeReadGuard,
) -> eyre::Result<()> {
    // Extract commitment transaction IDs from the block
    let block_tx_ids = block
        .system_ledgers
        .iter()
        .find(|ledger| ledger.ledger_id == SystemLedger::Commitment as u32)
        .map(|ledger| ledger.tx_ids.0.as_slice())
        .unwrap_or_else(|| &[]);

    // Fetch all actual commitment transactions from the block
    let actual_commitments =
        get_commitment_tx_in_parallel(block_tx_ids, &service_senders.mempool, db).await?;

    // Validate that all commitment transactions have correct values
    for (idx, tx) in actual_commitments.iter().enumerate() {
        tx.validate_value(&config.consensus).map_err(|e| {
            error!(
                "Commitment transaction {} at position {} has invalid value: {}",
                tx.id, idx, e
            );
            eyre::eyre!("Invalid commitment transaction value: {}", e)
        })?;
    }

    let is_epoch_block = block.height % config.consensus.epoch.num_blocks_in_epoch == 0;

    if is_epoch_block {
        debug!(
            "Validating commitment order for epoch block at height {}",
            block.height
        );

        // Get expected commitments from parent's snapshot
        let parent_commitment_snapshot = block_tree_guard
            .read()
            .get_commitment_snapshot(&block.previous_block_hash)?;
        let expected_commitments = parent_commitment_snapshot.get_epoch_commitments();

        // Use zip_longest to compare actual vs expected directly
        for (idx, pair) in actual_commitments
            .iter()
            .zip_longest(expected_commitments.iter())
            .enumerate()
        {
            match pair {
                EitherOrBoth::Both(actual, expected) => {
                    ensure!(
                        actual == expected,
                        "Epoch block commitment mismatch at position {}. Expected: {:?}, Got: {:?}",
                        idx,
                        expected,
                        actual
                    );
                }
                EitherOrBoth::Left(actual) => {
                    error!(
                        "Extra commitment in epoch block at position {}: {:?}",
                        idx, actual
                    );
                    eyre::bail!("Epoch block contains extra commitment transaction");
                }
                EitherOrBoth::Right(expected) => {
                    error!(
                        "Missing commitment in epoch block at position {}: {:?}",
                        idx, expected
                    );
                    eyre::bail!("Epoch block missing expected commitment transaction");
                }
            }
        }

        debug!("Epoch block commitment transaction validation successful");
        return Ok(());
    }

    // Regular block validation: check priority ordering for stake and pledge commitments
    let stake_and_pledge_txs: Vec<&CommitmentTransaction> = actual_commitments
        .iter()
        .filter(|tx| {
            matches!(
                tx.commitment_type,
                CommitmentType::Stake | CommitmentType::Pledge { .. }
            )
        })
        .collect();

    if stake_and_pledge_txs.is_empty() {
        return Ok(());
    }

    // Sort to get expected order
    let mut expected_order = stake_and_pledge_txs.clone();
    expected_order.sort();

    // Compare actual order vs expected order
    for (idx, pair) in stake_and_pledge_txs
        .iter()
        .zip_longest(expected_order.iter())
        .enumerate()
    {
        match pair {
            EitherOrBoth::Both(actual, expected) => {
                ensure!(
                    actual.id == expected.id,
                    "Commitment transaction at position {} in wrong order. Expected: {}, Got: {}",
                    idx,
                    expected.id,
                    actual.id
                );
            }
            _ => {
                // This should never happen since we're comparing the same filtered set
                eyre::bail!("Internal error: commitment ordering validation mismatch");
            }
        }
    }

    debug!("Commitment transaction ordering is valid");
    Ok(())
}

/// Helper function to calculate permanent storage fee using a specific EMA snapshot
/// This includes base network fee + ingress proof rewards
pub fn calculate_perm_storage_total_fee(
    bytes_to_store: u64,
    term_fee: U256,
    ema_snapshot: &EmaSnapshot,
    config: &Config,
) -> eyre::Result<Amount<(NetworkFee, Irys)>> {
    calculate_perm_fee_from_config(
        bytes_to_store,
        &config.consensus,
        ema_snapshot.ema_for_public_pricing(),
        term_fee,
    )
}

/// Helper function to calculate term storage fee using a specific EMA snapshot
/// Uses the same replica count as permanent storage but for the specified number of epochs
pub fn calculate_term_storage_base_network_fee(
    bytes_to_store: u64,
    epochs_for_storage: u64,
    ema_snapshot: &EmaSnapshot,
    config: &Config,
) -> eyre::Result<U256> {
    irys_types::storage_pricing::calculate_term_fee(
        bytes_to_store,
        epochs_for_storage,
        &config.consensus,
        ema_snapshot.ema_for_public_pricing(),
    )
}

/// Validates that data transactions in a block are correctly placed and have valid properties
/// based on their ledger placement (Submit or Publish) and ingress proof availability.
/// - Transactions in Publish ledger must have prior inclusion in Submit ledger
/// - Transactions should not appear in multiple blocks (duplicate inclusions)
/// - Submit ledger transactions must not have ingress proofs
/// - Publish ledger transactions must have valid ingress proofs
/// - All transactions must meet minimum fee requirements
/// - Fee structures must be valid for proper reward distribution
#[tracing::instrument(skip_all, err)]
pub async fn data_txs_are_valid(
    config: &Config,
    service_senders: &ServiceSenders,
    block: &IrysBlockHeader,
    db: &DatabaseProvider,
    block_tree_guard: &BlockTreeReadGuard,
) -> Result<(), PreValidationError> {
    // Get the block's EMA snapshot for fee calculations
    let block_ema = block_tree_guard
        .read()
        .get_ema_snapshot(&block.block_hash)
        .ok_or(PreValidationError::BlockEmaSnapshotNotFound {
            block_hash: block.block_hash,
        })?;

    // Extract data transactions from both ledgers
    let (publish_ledger, submit_ledger) = extract_data_ledgers(block)
        .map_err(|e| PreValidationError::DataLedgerExtractionFailed(e.to_string()))?;

    // Get transactions from both ledgers
    let publish_txs = get_data_tx_in_parallel(
        publish_ledger.tx_ids.0.clone(),
        &service_senders.mempool,
        db,
    )
    .await
    .map_err(|e| PreValidationError::TransactionFetchFailed(e.to_string()))?;

    let submit_txs =
        get_data_tx_in_parallel(submit_ledger.tx_ids.0.clone(), &service_senders.mempool, db)
            .await
            .map_err(|e| PreValidationError::TransactionFetchFailed(e.to_string()))?;

    // Step 1: Identify same-block promotions (txs appearing in both ledgers of current block)
    let submit_ids: HashSet<H256> = submit_txs.iter().map(|tx| tx.id).collect();
    let publish_ids: HashSet<H256> = publish_txs.iter().map(|tx| tx.id).collect();
    let same_block_promotions = submit_ids
        .intersection(&publish_ids)
        .copied()
        .collect::<HashSet<_>>();

    // Log same-block promotions for debugging
    for tx_id in &same_block_promotions {
        debug!(
            "Transaction {} promoted from Submit to Publish in same block",
            tx_id
        );
    }
    // Collect all tx_ids we need to check for previous inclusions
    let mut txs_to_check = publish_txs
        .iter()
        .map(|x| (x, DataLedger::Publish))
        .chain(submit_txs.iter().map(|x| (x, DataLedger::Submit)))
        .map(|(tx, ledger_current)| {
            let state = if same_block_promotions.contains(&tx.id) {
                TxInclusionState::Found {
                    ledger_current: DataLedger::Publish,
                    ledger_historical: DataLedger::Submit,
                    block_hash: block.block_hash,
                }
            } else {
                TxInclusionState::Searching { ledger_current }
            };
            (tx.id, (tx, state))
        })
        .collect::<HashMap<_, _>>();

    // Step 3: Check past inclusions only for non-promoted txs
    get_previous_tx_inclusions(
        &mut txs_to_check,
        block,
        config.consensus.mempool.anchor_expiry_depth as u64,
        service_senders,
        db,
    )
    .await
    .map_err(|e| PreValidationError::PreviousTxInclusionsFailed(e.to_string()))?;

    let ro_tx = db.tx().map_err(|e| PreValidationError::DatabaseError {
        error: e.to_string(),
    })?;

    // Step 4: Validate based on ledger rules
    for (tx, past_inclusion) in txs_to_check.values() {
        match past_inclusion {
            TxInclusionState::Searching { ledger_current } => {
                match ledger_current {
                    DataLedger::Publish => {
                        // check the db - if we can fetch it, we have a previous inclusion
                        if let Ok(Some(_header)) = tx_header_by_txid(&ro_tx, &tx.id) {
                            warn!("had to fetch header {:#?} from DB for {}, (exp: {:#?}) as submit inclusion wasn't within anchor depth", &_header, &tx.id, &tx);
                        } else {
                            // Publish tx with no past inclusion - INVALID
                            return Err(PreValidationError::PublishTxMissingPriorSubmit {
                                tx_id: tx.id,
                            });
                        }
                    }
                    DataLedger::Submit => {
                        // Submit tx with no past inclusion - VALID (new transaction)
                        debug!("Transaction {} is new in Submit ledger", tx.id);
                    }
                }
            }
            TxInclusionState::Found {
                ledger_current,
                ledger_historical,
                block_hash,
            } => {
                match (ledger_current, ledger_historical) {
                    (DataLedger::Publish, DataLedger::Submit) => {
                        // OK: Transaction promoted from past Submit to current Publish
                        debug!(
                            "Transaction {} promoted from past Submit to current Publish ledger",
                            tx.id
                        );
                    }
                    (DataLedger::Publish, DataLedger::Publish) => {
                        return Err(PreValidationError::PublishTxAlreadyIncluded {
                            tx_id: tx.id,
                            block_hash: *block_hash,
                        });
                    }
                    (DataLedger::Submit, _) => {
                        // Submit tx should not have any past inclusion
                        return Err(PreValidationError::SubmitTxAlreadyIncluded {
                            tx_id: tx.id,
                            ledger: *ledger_historical,
                            block_hash: *block_hash,
                        });
                    }
                }
            }
            TxInclusionState::Duplicate { ledger_historical } => {
                // Transaction found in multiple past blocks - this is always invalid
                return Err(PreValidationError::TxFoundInMultipleBlocks {
                    tx_id: tx.id,
                    ledger: ledger_historical.0,
                    block_hash: ledger_historical.1,
                });
            }
        }
    }

    // Step 5: Validate all transactions (including same-block promotions)
    let all_txs = publish_txs
        .iter()
        .map(|tx| (tx, DataLedger::Publish))
        .chain(submit_txs.iter().map(|tx| (tx, DataLedger::Submit)));

    for (tx, current_ledger) in all_txs {
        // All data transactions must have ledger_id set to Publish
        // TODO: support other term ledgers here
        if tx.ledger_id != DataLedger::Publish as u32 {
            return Err(PreValidationError::InvalidLedgerId {
                tx_id: tx.id,
                expected: DataLedger::Publish as u32,
                actual: tx.ledger_id,
            });
        }

        // Calculate expected fees based on data size using block's EMA
        // Calculate term fee first as it's needed for perm fee calculation
        // Calculate epochs for storage using the same method as mempool
        let epochs_for_storage = irys_types::ledger_expiry::calculate_submit_ledger_expiry(
            block.height,
            config.consensus.epoch.num_blocks_in_epoch,
            config.consensus.epoch.submit_ledger_epoch_length,
        );

        let expected_term_fee = calculate_term_storage_base_network_fee(
            tx.data_size,
            epochs_for_storage,
            &block_ema,
            config,
        )
        .map_err(|e| PreValidationError::FeeCalculationFailed(e.to_string()))?;
        let expected_perm_fee =
            calculate_perm_storage_total_fee(tx.data_size, expected_term_fee, &block_ema, config)
                .map_err(|e| PreValidationError::FeeCalculationFailed(e.to_string()))?;

        // Validate perm_fee is at least the expected amount
        let actual_perm_fee = tx.perm_fee.unwrap_or(U256::zero());
        if actual_perm_fee < expected_perm_fee.amount {
            return Err(PreValidationError::InsufficientPermFee {
                tx_id: tx.id,
                expected: expected_perm_fee.amount,
                actual: actual_perm_fee,
            });
        }

        // Validate term_fee is at least the expected amount
        let actual_term_fee = tx.term_fee;
        if actual_term_fee < expected_term_fee {
            return Err(PreValidationError::InsufficientTermFee {
                tx_id: tx.id,
                expected: expected_term_fee,
                actual: actual_term_fee,
            });
        }

        // Validate fee distribution structures can be created successfully
        // This ensures fees can be properly distributed to block producers, ingress proof providers, etc.
        TermFeeCharges::new(actual_term_fee, &config.consensus).map_err(|e| {
            PreValidationError::InvalidTermFeeStructure {
                tx_id: tx.id,
                reason: e.to_string(),
            }
        })?;

        PublishFeeCharges::new(actual_perm_fee, actual_term_fee, &config.consensus).map_err(
            |e| PreValidationError::InvalidPermFeeStructure {
                tx_id: tx.id,
                reason: e.to_string(),
            },
        )?;

        match current_ledger {
            DataLedger::Publish => {
                // no special publish-ledger-only asserts here
            }
            DataLedger::Submit => {
                // Submit ledger transactions should not have ingress proofs, that's why they are in the submit ledger
                // (they're waiting for proofs to arrive)
                if tx.promoted_height.is_some() {
                    // TODO: This should be a hard error, but the test infrastructure currently
                    // creates transactions with ingress proofs that get placed in Submit ledger.
                    // This needs to be fixed in the block production logic to properly place
                    // transactions with proofs in the Publish ledger.
                    tracing::warn!(
                        "Transaction {} in Submit ledger should not have a promoted_height",
                        tx.id
                    );
                }
            }
        }
    }

    if publish_txs.is_empty() && publish_ledger.proofs.is_some() {
        let proof_count = publish_ledger.proofs.as_ref().unwrap().len();
        return Err(PreValidationError::PublishLedgerProofCountMismatch {
            proof_count,
            tx_count: publish_txs.len(),
        });
    }

    if let Some(proofs_list) = &publish_ledger.proofs {
        let expected_proof_count =
            publish_txs.len() * (config.consensus.number_of_ingress_proofs_total as usize);

        if proofs_list.len() != expected_proof_count {
            return Err(PreValidationError::PublishLedgerProofCountMismatch {
                proof_count: proofs_list.len(),
                tx_count: publish_txs.len(),
            });
        }

        // Validate each proof corresponds to the correct transaction
        for tx_header in publish_txs {
            let tx_proofs = get_ingress_proofs(publish_ledger, &tx_header.id).map_err(|e| {
                PreValidationError::InvalidIngressProof {
                    tx_id: tx_header.id,
                    reason: e.to_string(),
                }
            })?;

            // Loop though all the ingress proofs for the published transaction and pre-validate them
            for ingress_proof in tx_proofs.iter() {
                // Validate ingress proof signature and data_root match the transaction
                let _ = ingress_proof
                    .pre_validate(&tx_header.data_root)
                    .map_err(|e| PreValidationError::InvalidIngressProof {
                        tx_id: tx_header.id,
                        reason: e.to_string(),
                    })?;
            }

            if tx_proofs.len() != config.consensus.number_of_ingress_proofs_total as usize {
                return Err(PreValidationError::IngressProofCountMismatch {
                    expected: config.consensus.number_of_ingress_proofs_total as usize,
                    actual: tx_proofs.len(),
                });
            }
        }
    }

    // TODO: validate that block.treasury is correctly updated

    debug!("Data transaction validation successful");
    Ok(())
}

fn extract_data_ledgers(
    block: &IrysBlockHeader,
) -> eyre::Result<(&DataTransactionLedger, &DataTransactionLedger)> {
    let (publish_ledger, submit_ledger) = match &block.data_ledgers[..] {
        [publish_ledger, submit_ledger] => {
            ensure!(
                publish_ledger.ledger_id == DataLedger::Publish,
                "Publish ledger must be the first ledger in the data ledgers"
            );
            ensure!(
                submit_ledger.ledger_id == DataLedger::Submit,
                "Submit ledger must be the second ledger in the data ledgers"
            );
            (publish_ledger, submit_ledger)
        }
        [..] => eyre::bail!("Expect exactly 2 data ledgers to be present on the block"),
    };
    Ok((publish_ledger, submit_ledger))
}

/// Validates that all ingress proof signers are unique for each transaction in the Publish ledger
fn validate_unique_ingress_proof_signers(
    block: &IrysBlockHeader,
) -> Result<(), PreValidationError> {
    // Extract publish ledger
    let publish_ledger = block
        .data_ledgers
        .iter()
        .find(|ledger| ledger.ledger_id == DataLedger::Publish as u32)
        .ok_or_else(|| {
            PreValidationError::DataLedgerExtractionFailed("Publish ledger not found".to_string())
        })?;

    // Early return if no proofs
    let Some(proofs_list) = &publish_ledger.proofs else {
        return Ok(());
    };

    // If we have proofs but no transactions, that's an error
    if publish_ledger.tx_ids.is_empty() && !proofs_list.is_empty() {
        return Err(PreValidationError::PublishLedgerProofCountMismatch {
            proof_count: proofs_list.len(),
            tx_count: 0,
        });
    }

    // For each transaction in the publish ledger, validate unique signers
    for tx_id in &publish_ledger.tx_ids.0 {
        let tx_proofs = get_ingress_proofs(publish_ledger, tx_id).map_err(|e| {
            PreValidationError::InvalidIngressProof {
                tx_id: *tx_id,
                reason: e.to_string(),
            }
        })?;

        // Track signer counts for this transaction to ensure each appears exactly once
        let mut signer_counts = std::collections::HashMap::new();

        for ingress_proof in tx_proofs.iter() {
            // Recover the signer address
            let signer = ingress_proof.recover_signer().map_err(|e| {
                PreValidationError::InvalidIngressProof {
                    tx_id: *tx_id,
                    reason: e.to_string(),
                }
            })?;

            // Increment the count for this signer
            *signer_counts.entry(signer).or_insert(0) += 1;
        }

        // Check that each signer appears exactly once
        for (signer, count) in signer_counts.iter() {
            if *count != 1 {
                return Err(PreValidationError::DuplicateIngressProofSigner {
                    tx_id: *tx_id,
                    signer: *signer,
                });
            }
        }
    }

    Ok(())
}

/// State for tracking transaction inclusion search
#[derive(Clone, Copy, Debug)]
enum TxInclusionState {
    Searching {
        ledger_current: DataLedger,
    },
    Found {
        ledger_current: DataLedger,
        ledger_historical: DataLedger,
        block_hash: BlockHash,
    },
    Duplicate {
        ledger_historical: (DataLedger, BlockHash),
    },
}

#[tracing::instrument(skip_all, fields(block_under_validation = ?block_under_validation.block_hash))]
async fn get_previous_tx_inclusions(
    tx_ids: &mut HashMap<H256, (&DataTransactionHeader, TxInclusionState)>,
    block_under_validation: &IrysBlockHeader,
    anchor_expiry_depth: u64,
    service_senders: &ServiceSenders,
    db: &DatabaseProvider,
) -> eyre::Result<()> {
    // Early return for empty input
    if tx_ids.is_empty() {
        return Ok(());
    }

    // Get mempool data and release lock quickly
    let (tx, rx) = tokio::sync::oneshot::channel();
    service_senders
        .mempool
        .send(MempoolServiceMessage::GetState(tx))?;
    let mempool_state = rx.await?;
    let mempool_guard = mempool_state.read().await;

    let min_anchor_height = block_under_validation
        .height
        .saturating_sub(anchor_expiry_depth);

    let mut block = (
        block_under_validation.block_hash,
        block_under_validation.height,
    );
    while block.1 >= min_anchor_height {
        // Stop if we've reached the genesis block
        if block.1 == 0 {
            break;
        }

        let mut update_states = |header: &IrysBlockHeader| {
            if header.block_hash == block_under_validation.block_hash {
                // don't process the states for a block we're putting under full validation
                return Ok(());
            }
            process_block_ledgers_with_states(&header.data_ledgers, header.block_hash, tx_ids)
        };
        // Move to the parent block and continue the traversal backwards
        block = match mempool_guard.prevalidated_blocks.get(&block.0) {
            Some(header) => {
                update_states(header)?;
                (header.previous_block_hash, header.height.saturating_sub(1))
            }
            None => {
                let header = db
                    .view(|tx| irys_database::block_header_by_hash(tx, &block.0, false))
                    .unwrap_or_else(|_| {
                        panic!(
                            "database returned error fetching parent block header {}",
                            &block.0
                        )
                    })
                    .unwrap_or_else(|_| {
                        panic!(
                            "db view error while fetching parent block header {}",
                            &block.0
                        )
                    })
                    .unwrap_or_else(|| {
                        panic!(
                            "to find the parent block header {} in the database",
                            &block.0
                        )
                    });
                update_states(&header)?;
                (header.previous_block_hash, header.height.saturating_sub(1))
            }
        };
    }

    Ok(())
}

/// Process ledgers and update transaction states
/// Returns true if all transactions have been found
fn process_block_ledgers_with_states(
    ledgers: &[DataTransactionLedger],
    block_hash: BlockHash,
    tx_states: &mut HashMap<H256, (&DataTransactionHeader, TxInclusionState)>,
) -> eyre::Result<()> {
    for ledger in ledgers {
        let ledger_type = DataLedger::try_from(ledger.ledger_id)?;

        // Check each transaction in this ledger
        for tx_id in &ledger.tx_ids.0 {
            if let Some((_, state)) = tx_states.get_mut(tx_id) {
                match state {
                    TxInclusionState::Searching { ledger_current } => {
                        // First time finding this transaction
                        *state = TxInclusionState::Found {
                            ledger_current: *ledger_current,
                            ledger_historical: ledger_type,
                            block_hash,
                        };
                    }
                    TxInclusionState::Found { .. } => {
                        // Transaction already found once, this is a duplicate
                        *state = TxInclusionState::Duplicate {
                            ledger_historical: (ledger_type, block_hash),
                        };
                    }
                    TxInclusionState::Duplicate { .. } => {
                        // Already marked as duplicate, no need to update
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        block_index_service::{BlockIndexService, GetBlockIndexGuardMessage},
        BlockMigrationMessage,
    };
    use actix::{prelude::*, SystemRegistry};
    use irys_config::StorageSubmodulesConfig;
    use irys_database::add_genesis_commitments;
    use irys_domain::{BlockIndex, EpochSnapshot};
    use irys_testing_utils::utils::temporary_directory;
    use irys_types::{
        hash_sha256, irys::IrysSigner, partition::PartitionAssignment, Address, Base64, BlockHash,
        DataTransaction, DataTransactionHeader, DataTransactionLedger, H256List, NodeConfig,
        Signature, H256, U256,
    };
    use std::sync::{Arc, RwLock};
    use tempfile::TempDir;
    use tracing::{debug, info};

    pub(super) struct TestContext {
        pub block_index: Arc<RwLock<BlockIndex>>,
        pub block_index_actor: Addr<BlockIndexService>,
        pub miner_address: Address,
        pub epoch_snapshot: EpochSnapshot,
        pub partition_hash: H256,
        pub partition_assignment: PartitionAssignment,
        pub consensus_config: ConsensusConfig,
        #[expect(dead_code)]
        pub node_config: NodeConfig,
    }

    async fn init() -> (TempDir, TestContext) {
        let data_dir = temporary_directory(Some("block_validation_tests"), false);
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 100,
                ..ConsensusConfig::testing()
            }),
            base_directory: data_dir.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new(node_config);

        let mut genesis_block = IrysBlockHeader::new_mock_header();
        genesis_block.height = 0;
        let chunk_size = 32;
        let mut node_config = NodeConfig::testing();
        node_config.storage.num_writes_before_sync = 1;
        node_config.base_directory = data_dir.path().to_path_buf();
        let consensus_config = ConsensusConfig {
            chunk_size,
            num_chunks_in_partition: 10,
            num_chunks_in_recall_range: 2,
            num_partitions_per_slot: 1,
            entropy_packing_iterations: 1_000,
            block_migration_depth: 1,
            ..node_config.consensus_config()
        };

        let (commitments, initial_treasury) =
            add_genesis_commitments(&mut genesis_block, &config).await;
        genesis_block.treasury = initial_treasury;

        let arc_genesis = Arc::new(genesis_block.clone());
        let signer = config.irys_signer();
        let miner_address = signer.address();

        // Create epoch service with random miner address
        let block_index = Arc::new(RwLock::new(
            BlockIndex::new(&node_config)
                .await
                .expect("Expected to create block index"),
        ));

        let block_index_actor =
            BlockIndexService::new(block_index.clone(), &consensus_config).start();
        SystemRegistry::set(block_index_actor.clone());

        let storage_submodules_config =
            StorageSubmodulesConfig::load(config.node_config.base_directory.clone())
                .expect("Expected to load storage submodules config");

        // Create an epoch snapshot for the genesis block
        let epoch_snapshot = EpochSnapshot::new(
            &storage_submodules_config,
            genesis_block,
            commitments.clone(),
            &config,
        );
        info!("Genesis Epoch tasks complete.");

        let partition_hash = epoch_snapshot.ledgers.get_slots(DataLedger::Submit)[0].partitions[0];

        let msg = BlockMigrationMessage {
            block_header: arc_genesis.clone(),
            all_txs: Arc::new(vec![]),
        };

        let block_index_actor = BlockIndexService::from_registry();
        match block_index_actor.send(msg).await {
            Ok(_) => info!("Genesis block indexed"),
            Err(_) => panic!("Failed to index genesis block"),
        }

        let partition_assignment = epoch_snapshot
            .get_data_partition_assignment(partition_hash)
            .expect("Expected to get partition assignment");

        debug!("Partition assignment {:?}", partition_assignment);

        (
            data_dir,
            TestContext {
                block_index,
                block_index_actor,
                miner_address,
                epoch_snapshot,
                partition_hash,
                partition_assignment,
                consensus_config,
                node_config,
            },
        )
    }

    #[actix::test]
    async fn poa_test_3_complete_txs() {
        let (_tmp, context) = init().await;
        // Create a bunch of TX chunks
        let data_chunks = vec![
            vec![[0; 32], [1; 32], [2; 32]], // tx0
            vec![[3; 32], [4; 32], [5; 32]], // tx1
            vec![[6; 32], [7; 32], [8; 32]], // tx2
        ];

        // Create a bunch of signed TX from the chunks
        // Loop though all the data_chunks and create wrapper tx for them
        let signer = IrysSigner::random_signer(&context.consensus_config);
        let mut txs: Vec<DataTransaction> = Vec::new();

        for chunks in &data_chunks {
            let mut data: Vec<u8> = Vec::new();
            for chunk in chunks {
                data.extend_from_slice(chunk);
            }
            let tx = signer
                .create_transaction(data, H256::zero())
                .expect("Expected to create a transaction");
            let tx = signer
                .sign_transaction(tx)
                .expect("Expected to sign the transaction");
            txs.push(tx);
        }

        for poa_tx_num in 0..3 {
            for poa_chunk_num in 0..3 {
                let mut poa_chunk: Vec<u8> = data_chunks[poa_tx_num][poa_chunk_num].into();
                poa_test(
                    &context,
                    &txs,
                    &mut poa_chunk,
                    poa_tx_num,
                    poa_chunk_num,
                    9,
                    context.consensus_config.chunk_size as usize,
                )
                .await;
            }
        }
    }

    #[actix::test]
    async fn poa_not_complete_last_chunk_test() {
        let (_tmp, context) = init().await;

        // Create a signed TX from the chunks
        let signer = IrysSigner::random_signer(&context.consensus_config);
        let mut txs: Vec<DataTransaction> = Vec::new();

        let data = vec![3; 40]; //32 + 8 last incomplete chunk
        let tx = signer
            .create_transaction(data.clone(), H256::zero())
            .expect("Expected to create a transaction");
        let tx = signer
            .sign_transaction(tx)
            .expect("Expected to sign the transaction");
        txs.push(tx);

        let poa_tx_num = 0;
        let chunk_size = context.consensus_config.chunk_size as usize;
        for poa_chunk_num in 0..2 {
            let mut poa_chunk: Vec<u8> = data[poa_chunk_num * (chunk_size)
                ..std::cmp::min((poa_chunk_num + 1) * chunk_size, data.len())]
                .to_vec();
            poa_test(
                &context,
                &txs,
                &mut poa_chunk,
                poa_tx_num,
                poa_chunk_num,
                2,
                chunk_size,
            )
            .await;
        }
    }

    #[actix::test]
    async fn is_seed_data_valid_should_validate_seeds() {
        let reset_frequency = 2;

        let mut parent_header = IrysBlockHeader::new_mock_header();
        let parent_seed = BlockHash::from_slice(&[2; 32]);
        let parent_next_seed = BlockHash::from_slice(&[3; 32]);
        parent_header.block_hash = BlockHash::from_slice(&[4; 32]);
        parent_header.vdf_limiter_info.seed = parent_seed;
        parent_header.vdf_limiter_info.next_seed = parent_next_seed;

        let mut header_2 = IrysBlockHeader::new_mock_header();
        // Reset frequency is 2, so setting global_step_number to 3 and adding 2 steps
        //  should result in the seeds being rotated
        header_2.vdf_limiter_info.global_step_number = 3;
        header_2.vdf_limiter_info.steps = H256List(vec![H256::zero(); 2]);
        header_2
            .vdf_limiter_info
            .set_seeds(reset_frequency, &parent_header);
        let is_valid = is_seed_data_valid(&header_2, &parent_header, reset_frequency);

        assert_eq!(
            header_2.vdf_limiter_info.next_seed,
            parent_header.block_hash
        );
        assert_eq!(header_2.vdf_limiter_info.seed, parent_next_seed);
        assert!(
            matches!(is_valid, ValidationResult::Valid),
            "Seed data should be valid"
        );

        // Now let's try to rotate the seeds when no rotation is needed by increasing the
        // reset frequency
        let large_reset_frequency = 100;
        let is_valid = is_seed_data_valid(&header_2, &parent_header, large_reset_frequency);
        assert!(
            matches!(is_valid, ValidationResult::Invalid),
            "Seed data should still be valid"
        );

        // Now let's try to set some random seeds that are not valid
        header_2.vdf_limiter_info.seed = BlockHash::from_slice(&[5; 32]);
        header_2.vdf_limiter_info.next_seed = BlockHash::from_slice(&[6; 32]);
        let is_valid = is_seed_data_valid(&header_2, &parent_header, reset_frequency);

        assert!(
            matches!(is_valid, ValidationResult::Invalid),
            "Seed data should be invalid"
        );
    }

    async fn poa_test(
        context: &TestContext,
        txs: &[DataTransaction],
        #[expect(
            clippy::ptr_arg,
            reason = "we need to clone this so it needs to be a Vec"
        )]
        poa_chunk: &mut Vec<u8>,
        poa_tx_num: usize,
        poa_chunk_num: usize,
        total_chunks_in_tx: usize,
        chunk_size: usize,
    ) {
        // Initialize genesis block at height 0
        let height: u64;
        {
            height = context
                .block_index
                .read()
                .expect("Expected to be able to read block index")
                .num_blocks();
        }

        let mut entropy_chunk = Vec::<u8>::with_capacity(chunk_size);
        compute_entropy_chunk(
            context.miner_address,
            (poa_tx_num * 3 /* tx's size in chunks */  + poa_chunk_num) as u64,
            context.partition_hash.into(),
            context.consensus_config.entropy_packing_iterations,
            chunk_size,
            &mut entropy_chunk,
            context.consensus_config.chain_id,
        );

        xor_vec_u8_arrays_in_place(poa_chunk, &entropy_chunk);

        // Create vectors of tx headers and txids
        let tx_headers: Vec<DataTransactionHeader> =
            txs.iter().map(|tx| tx.header.clone()).collect();

        let data_tx_ids = tx_headers.iter().map(|h| h.id).collect::<Vec<H256>>();

        let (tx_root, tx_path) = DataTransactionLedger::merklize_tx_root(&tx_headers);

        let poa = PoaData {
            tx_path: Some(Base64(tx_path[poa_tx_num].proof.clone())),
            data_path: Some(Base64(txs[poa_tx_num].proofs[poa_chunk_num].proof.clone())),
            chunk: Some(Base64(poa_chunk.clone())),
            ledger_id: Some(1),
            partition_chunk_offset: (poa_tx_num * 3 /* 3 chunks in each tx */ + poa_chunk_num)
                .try_into()
                .expect("Value exceeds u32::MAX"),

            partition_hash: context.partition_hash,
        };

        // Create a block from the tx
        let irys_block = IrysBlockHeader {
            height,
            reward_address: context.miner_address,
            poa: poa.clone(),
            block_hash: H256::zero(),
            previous_block_hash: H256::zero(),
            previous_cumulative_diff: U256::from(4000),
            miner_address: context.miner_address,
            signature: Signature::test_signature().into(),
            timestamp: 1000,
            data_ledgers: vec![
                // Permanent Publish Ledger
                DataTransactionLedger {
                    ledger_id: DataLedger::Publish.into(),
                    tx_root: H256::zero(),
                    tx_ids: H256List(Vec::new()),
                    max_chunk_offset: 0,
                    expires: None,
                    proofs: None,
                    required_proof_count: Some(1),
                },
                // Term Submit Ledger
                DataTransactionLedger {
                    ledger_id: DataLedger::Submit.into(),
                    tx_root,
                    tx_ids: H256List(data_tx_ids.clone()),
                    max_chunk_offset: 9,
                    expires: Some(1622543200),
                    proofs: None,
                    required_proof_count: None,
                },
            ],
            ..IrysBlockHeader::default()
        };

        // Send the block confirmed message
        let block = Arc::new(irys_block);
        let txs = Arc::new(tx_headers);
        let block_finalized_message = BlockMigrationMessage {
            block_header: block.clone(),
            all_txs: Arc::clone(&txs),
        };

        match context
            .block_index_actor
            .send(block_finalized_message.clone())
            .await
        {
            Ok(_) => info!("Second block indexed"),
            Err(_) => panic!("Failed to index second block"),
        };

        let block_index_guard = context
            .block_index_actor
            .send(GetBlockIndexGuardMessage)
            .await
            .expect("Failed to get block index guard");

        let ledger_chunk_offset = context
            .partition_assignment
            .slot_index
            .expect("Expected to have a slot index in the assignment")
            as u64
            * context.consensus_config.num_partitions_per_slot
            * context.consensus_config.num_chunks_in_partition
            + (poa_tx_num * 3 /* 3 chunks in each tx */ + poa_chunk_num) as u64;

        assert_eq!(
            ledger_chunk_offset,
            (poa_tx_num * 3 /* 3 chunks in each tx */ + poa_chunk_num) as u64,
            "ledger_chunk_offset mismatch"
        );

        // ledger data -> block
        let bb = block_index_guard
            .read()
            .get_block_bounds(DataLedger::Submit, ledger_chunk_offset)
            .expect("expected valid block bounds");
        info!("block bounds: {:?}", bb);

        assert_eq!(bb.start_chunk_offset, 0, "start_chunk_offset should be 0");
        assert_eq!(
            bb.end_chunk_offset, total_chunks_in_tx as u64,
            "end_chunk_offset should be 9, tx has 9 chunks"
        );

        let poa_valid = poa_is_valid(
            &poa,
            &block_index_guard,
            &context.epoch_snapshot,
            &context.consensus_config,
            &context.miner_address,
        );

        debug!("PoA validation result: {:?}", poa_valid);
        assert!(poa_valid.is_ok(), "PoA should be valid");
    }

    #[actix::test]
    async fn poa_does_not_allow_modified_leaves() {
        let (_tmp, context) = init().await;
        // Create a bunch of TX chunks
        let data_chunks = vec![
            vec![[0; 32], [1; 32], [2; 32]], // tx0
            vec![[3; 32], [4; 32], [5; 32]], // tx1
            vec![[6; 32], [7; 32], [8; 32]], // tx2
        ];

        // Create a bunch of signed TX from the chunks
        // Loop though all the data_chunks and create wrapper tx for them
        let signer = IrysSigner::random_signer(&context.consensus_config);
        let mut txs: Vec<DataTransaction> = Vec::new();

        for chunks in &data_chunks {
            let mut data: Vec<u8> = Vec::new();
            for chunk in chunks {
                data.extend_from_slice(chunk);
            }
            let tx = signer
                .create_transaction(data, H256::zero())
                .expect("Expected to create a transaction");
            let tx = signer
                .sign_transaction(tx)
                .expect("Expected to sign the transaction");
            txs.push(tx);
        }

        for poa_tx_num in 0..3 {
            for poa_chunk_num in 0..3 {
                let mut poa_chunk: Vec<u8> = data_chunks[poa_tx_num][poa_chunk_num].into();
                test_poa_with_malicious_merkle_data(
                    &context,
                    &txs,
                    &mut poa_chunk,
                    poa_tx_num,
                    poa_chunk_num,
                    9,
                    context.consensus_config.chunk_size as usize,
                )
                .await;
            }
        }
    }

    async fn test_poa_with_malicious_merkle_data(
        context: &TestContext,
        txs: &[DataTransaction],
        #[expect(
            clippy::ptr_arg,
            reason = "we need to clone this so it needs to be a Vec"
        )]
        poa_chunk: &mut Vec<u8>,
        poa_tx_num: usize,
        poa_chunk_num: usize,
        total_chunks_in_tx: usize,
        chunk_size: usize,
    ) {
        // Initialize genesis block at height 0
        let height: u64;
        {
            height = context
                .block_index
                .read()
                .expect("To read block index")
                .num_blocks();
        }

        let mut entropy_chunk = Vec::<u8>::with_capacity(chunk_size);
        compute_entropy_chunk(
            context.miner_address,
            (poa_tx_num * 3 /* tx's size in chunks */  + poa_chunk_num) as u64,
            context.partition_hash.into(),
            context.consensus_config.entropy_packing_iterations,
            chunk_size,
            &mut entropy_chunk,
            context.consensus_config.chain_id,
        );

        xor_vec_u8_arrays_in_place(poa_chunk, &entropy_chunk);

        // Create vectors of tx headers and txids
        let tx_headers: Vec<DataTransactionHeader> =
            txs.iter().map(|tx| tx.header.clone()).collect();

        let data_tx_ids = tx_headers.iter().map(|h| h.id).collect::<Vec<H256>>();

        let (tx_root, tx_path) = DataTransactionLedger::merklize_tx_root(&tx_headers);

        // Hacked data: DEADBEEF (but padded to chunk_size for proper entropy packing)
        let mut hacked_data = vec![0xde, 0xad, 0xbe, 0xef];
        hacked_data.resize(chunk_size, 0); // Pad to chunk_size like normal chunks

        // Calculate what the hash SHOULD BE after entropy packing
        let mut entropy_chunk = Vec::<u8>::with_capacity(chunk_size);
        compute_entropy_chunk(
            context.miner_address,
            (poa_tx_num * 3 + poa_chunk_num) as u64,
            context.partition_hash.into(),
            context.consensus_config.entropy_packing_iterations,
            chunk_size,
            &mut entropy_chunk,
            context.consensus_config.chain_id,
        );

        // Apply entropy packing to our hacked data to see what it becomes
        let mut entropy_packed_hacked = hacked_data.clone();
        xor_vec_u8_arrays_in_place(&mut entropy_packed_hacked, &entropy_chunk);

        // Trim to actual data size for hash calculation (chunk_size might be larger)
        let trimmed_hacked = &entropy_packed_hacked[0..hacked_data.len().min(chunk_size)];
        let entropy_packed_hash = hash_sha256(trimmed_hacked);

        // Calculate the correct offset for this chunk position
        let chunk_start_offset = poa_tx_num * 3 * 32 + poa_chunk_num * 32; // Each chunk is 32 bytes
        let chunk_end_offset = chunk_start_offset + hacked_data.len().min(32); // This chunk's end

        // Create fake leaf proof with the entropy-packed hash and correct offset
        let mut hacked_data_path = txs[poa_tx_num].proofs[poa_chunk_num].proof.clone();
        let hacked_data_path_len = hacked_data_path.len();
        if hacked_data_path_len < 64 {
            hacked_data_path.resize(64, 0);
        }

        // Overwrite last 64 bytes (LeafProof structure)
        let start = hacked_data_path.len() - 64;

        // 32 bytes: entropy-packed hash (what PoA validation expects to see)
        hacked_data_path[start..start + 32].copy_from_slice(&entropy_packed_hash);

        // 24 bytes: notepad (NOTE_SIZE - 8 = 32 - 8 = 24)
        for i in 0..24 {
            hacked_data_path[start + 32 + i] = 0;
        }

        // 8 bytes: offset as big-endian u64
        let offset_bytes = (chunk_end_offset as u64).to_be_bytes();
        hacked_data_path[start + 56..start + 64].copy_from_slice(&offset_bytes);

        debug!("Hacked attack:");
        debug!("  Original data: {:?}", &hacked_data[0..4]);
        debug!("  Entropy-packed hash: {:?}", &entropy_packed_hash[..4]);
        debug!("  Chunk offset: {}", chunk_end_offset);

        let poa = PoaData {
            tx_path: Some(Base64(tx_path[poa_tx_num].proof.clone())),
            data_path: Some(Base64(hacked_data_path.clone())),
            chunk: Some(Base64(hacked_data.clone())), // Use RAW data, PoA validation will entropy-pack it
            ledger_id: Some(1),
            partition_chunk_offset: (poa_tx_num * 3 /* 3 chunks in each tx */ + poa_chunk_num)
                .try_into()
                .expect("Value exceeds u32::MAX"),

            partition_hash: context.partition_hash,
        };

        // Create a block from the tx
        let irys_block = IrysBlockHeader {
            height,
            reward_address: context.miner_address,
            poa: poa.clone(),
            block_hash: H256::zero(),
            previous_block_hash: H256::zero(),
            previous_cumulative_diff: U256::from(4000),
            miner_address: context.miner_address,
            signature: Signature::test_signature().into(),
            timestamp: 1000,
            data_ledgers: vec![
                // Permanent Publish Ledger
                DataTransactionLedger {
                    ledger_id: DataLedger::Publish.into(),
                    tx_root: H256::zero(),
                    tx_ids: H256List(Vec::new()),
                    max_chunk_offset: 0,
                    expires: None,
                    proofs: None,
                    required_proof_count: Some(1),
                },
                // Term Submit Ledger
                DataTransactionLedger {
                    ledger_id: DataLedger::Submit.into(),
                    tx_root,
                    tx_ids: H256List(data_tx_ids.clone()),
                    max_chunk_offset: 9,
                    expires: Some(1622543200),
                    proofs: None,
                    required_proof_count: None,
                },
            ],
            ..IrysBlockHeader::default()
        };

        // Send the block confirmed message
        let block = Arc::new(irys_block);
        let txs = Arc::new(tx_headers);
        let block_finalized_message = BlockMigrationMessage {
            block_header: block.clone(),
            all_txs: Arc::clone(&txs),
        };

        match context
            .block_index_actor
            .send(block_finalized_message.clone())
            .await
        {
            Ok(_) => info!("Second block indexed"),
            Err(_) => panic!("Failed to index second block"),
        };

        let block_index_guard = context
            .block_index_actor
            .send(GetBlockIndexGuardMessage)
            .await
            .expect("Expected to get block index guard");

        let ledger_chunk_offset = context
            .partition_assignment
            .slot_index
            .expect("Expected to get slot index") as u64
            * context.consensus_config.num_partitions_per_slot
            * context.consensus_config.num_chunks_in_partition
            + (poa_tx_num * 3 /* 3 chunks in each tx */ + poa_chunk_num) as u64;

        assert_eq!(
            ledger_chunk_offset,
            (poa_tx_num * 3 /* 3 chunks in each tx */ + poa_chunk_num) as u64,
            "ledger_chunk_offset mismatch"
        );

        // ledger data -> block
        let bb = block_index_guard
            .read()
            .get_block_bounds(DataLedger::Submit, ledger_chunk_offset)
            .expect("expected valid block bounds");
        info!("block bounds: {:?}", bb);

        assert_eq!(bb.start_chunk_offset, 0, "start_chunk_offset should be 0");
        assert_eq!(
            bb.end_chunk_offset, total_chunks_in_tx as u64,
            "end_chunk_offset should be 9, tx has 9 chunks"
        );

        let poa_valid = poa_is_valid(
            &poa,
            &block_index_guard,
            &context.epoch_snapshot,
            &context.consensus_config,
            &context.miner_address,
        );

        match poa_valid {
            Err(PreValidationError::PoAChunkHashMismatch {
                ledger_id,
                ledger_chunk_offset,
                expected,
                got,
            }) => {
                assert!(ledger_id.is_some(), "expected ledger_id context");
                assert!(
                    ledger_chunk_offset.is_some(),
                    "expected ledger_chunk_offset context"
                );
                assert_ne!(expected, got, "expected and got hashes should differ");
            }
            Err(PreValidationError::MerkleProofInvalid(msg)) => {
                assert!(
                    msg.contains("hash mismatch"),
                    "expected hash mismatch merkle proof error, got: {}",
                    msg
                );
            }
            Err(other) => panic!(
                "expected PoAChunkHashMismatch or MerkleProofInvalid, got {:?}",
                other
            ),
            Ok(_) => panic!("expected invalid PoA, but validation succeeded"),
        }
    }

    #[test]
    /// unit test for acceptable block clock drift into future
    fn test_timestamp_is_valid_future() {
        let consensus_config = ConsensusConfig::testing();
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let future_ts = now_ms + consensus_config.max_future_timestamp_drift_millis - 1_000; // MAX DRIFT - 1 seconds in the future
        let previous_ts = now_ms - 10_000;
        let result = timestamp_is_valid(
            future_ts,
            previous_ts,
            consensus_config.max_future_timestamp_drift_millis,
        );
        // Expect an error due to block timestamp being too far in the future
        assert!(
            result.is_ok(),
            "Expected acceptable for future timestamp drift"
        );
    }

    #[test]
    /// unit test for block clock drift into past
    fn test_timestamp_is_valid_past() {
        let consensus_config = ConsensusConfig::testing();
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let block_ts = now_ms - consensus_config.max_future_timestamp_drift_millis - 1_000; // MAX DRIFT + 1 seconds in the past
        let previous_ts = now_ms - 60_000;
        let result = timestamp_is_valid(
            block_ts,
            previous_ts,
            consensus_config.max_future_timestamp_drift_millis,
        );
        // Expect an no error when block timestamp being too far in the past
        assert!(
            result.is_ok(),
            "Expected no error due to past timestamp drift"
        );
    }

    #[test]
    /// unit test for unacceptable block clock drift into future
    fn test_timestamp_is_invalid_future() {
        let consensus_config = ConsensusConfig::testing();
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let block_ts = now_ms + consensus_config.max_future_timestamp_drift_millis + 1_000; // MAX DRIFT + 1 seconds in the future
        let previous_ts = now_ms - 10_000;
        let result = timestamp_is_valid(
            block_ts,
            previous_ts,
            consensus_config.max_future_timestamp_drift_millis,
        );
        match result {
            Err(super::PreValidationError::TimestampTooFarInFuture { current, now }) => {
                assert!(
                    current > now,
                    "current should be greater than now for future drift"
                );
            }
            other => panic!("expected TimestampTooFarInFuture, got {:?}", other),
        }
    }

    #[test]
    fn timestamp_older_than_parent_is_invalid() {
        let consensus_config = ConsensusConfig::testing();
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let parent_ts = now_ms;
        let current_ts = parent_ts.saturating_sub(1);
        let result = timestamp_is_valid(
            current_ts,
            parent_ts,
            consensus_config.max_future_timestamp_drift_millis,
        );
        match result {
            Err(super::PreValidationError::TimestampOlderThanParent { current, parent }) => {
                assert_eq!(current, current_ts);
                assert_eq!(parent, parent_ts);
            }
            other => panic!("expected TimestampOlderThanParent, got {:?}", other),
        }
    }

    #[test]
    fn last_diff_timestamp_no_adjustment_ok() {
        let mut prev = IrysBlockHeader::new_mock_header();
        prev.height = 1;
        prev.last_diff_timestamp = 1000;

        let mut block = IrysBlockHeader::new_mock_header();
        block.height = 2;
        block.timestamp = 1500;
        block.last_diff_timestamp = prev.last_diff_timestamp;

        let mut config = ConsensusConfig::testing();
        config.difficulty_adjustment.difficulty_adjustment_interval = 10;

        assert!(
            last_diff_timestamp_is_valid(&block, &prev, &config.difficulty_adjustment,).is_ok()
        );
    }

    #[test]
    fn last_diff_timestamp_adjustment_ok() {
        let mut prev = IrysBlockHeader::new_mock_header();
        prev.height = 9;
        prev.last_diff_timestamp = 1000;

        let mut block = IrysBlockHeader::new_mock_header();
        block.height = 10;
        block.timestamp = 2000;
        block.last_diff_timestamp = block.timestamp;

        let mut config = ConsensusConfig::testing();
        config.difficulty_adjustment.difficulty_adjustment_interval = 10;

        assert!(
            last_diff_timestamp_is_valid(&block, &prev, &config.difficulty_adjustment,).is_ok()
        );
    }

    #[test]
    fn last_diff_timestamp_incorrect_fails() {
        let mut prev = IrysBlockHeader::new_mock_header();
        prev.height = 1;
        prev.last_diff_timestamp = 1000;

        let mut block = IrysBlockHeader::new_mock_header();
        block.height = 2;
        block.timestamp = 1500;
        block.last_diff_timestamp = 999;

        let mut config = ConsensusConfig::testing();
        config.difficulty_adjustment.difficulty_adjustment_interval = 10;

        assert!(
            last_diff_timestamp_is_valid(&block, &prev, &config.difficulty_adjustment,).is_err()
        );
    }

    #[test]
    fn previous_cumulative_difficulty_validates_match() {
        let mut prev = IrysBlockHeader::new_mock_header();
        prev.cumulative_diff = U256::from(12345);

        let mut block = IrysBlockHeader::new_mock_header();
        block.previous_cumulative_diff = prev.cumulative_diff;

        assert!(
            previous_cumulative_difficulty_is_valid(&block, &prev).is_ok(),
            "expected previous_cumulative_diff to match parent's cumulative_diff"
        );
    }

    #[test]
    fn previous_cumulative_difficulty_detects_mismatch() {
        let mut prev = IrysBlockHeader::new_mock_header();
        prev.cumulative_diff = U256::from(12345);

        let mut block = IrysBlockHeader::new_mock_header();
        block.previous_cumulative_diff = U256::from(9999);

        if let Err(PreValidationError::PreviousCumulativeDifficultyMismatch { expected, got }) =
            previous_cumulative_difficulty_is_valid(&block, &prev)
        {
            assert_eq!(expected, prev.cumulative_diff);
            assert_eq!(got, block.previous_cumulative_diff);
        } else {
            panic!("expected PreValidationError::PreviousCumulativeDifficultyMismatch");
        }
    }
}
