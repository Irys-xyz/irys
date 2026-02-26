use crate::block_tree_service::{BlockTreeServiceMessage, ValidationResult};
use crate::{
    block_producer::ledger_expiry,
    mempool_guard::MempoolReadGuard,
    mempool_service::MempoolServiceMessage,
    services::ServiceSenders,
    shadow_tx_generator::{PublishLedgerWithTxs, ShadowTxGenerator},
};
use alloy_eips::eip7685::{Requests, RequestsOrHash};
use alloy_rpc_types_engine::ExecutionData;
use eyre::{ensure, eyre, OptionExt as _};
use irys_database::db::IrysDatabaseExt as _;
use irys_database::{block_header_by_hash, cached_data_root_by_data_root, tx_header_by_txid};
use irys_domain::{
    BlockIndex, BlockIndexReadGuard, BlockTreeReadGuard, CommitmentSnapshot,
    CommitmentSnapshotStatus, EmaSnapshot, EpochSnapshot, ExecutionPayloadCache,
    HardforkConfigExt as _,
};
use irys_packing::{capacity_single::compute_entropy_chunk, xor_vec_u8_arrays_in_place};
use irys_reth::shadow_tx::{detect_and_decode, ShadowTransaction, ShadowTxError};
use irys_reth_node_bridge::IrysRethNodeAdapter;
use irys_reward_curve::HalvingCurve;
use irys_storage::{ie, ii};
use irys_types::storage_pricing::phantoms::{Irys, NetworkFee};
use irys_types::storage_pricing::{calculate_perm_fee_from_config, Amount};
use irys_types::{
    app_state::DatabaseProvider,
    calculate_difficulty, next_cumulative_diff,
    transaction::fee_distribution::{PublishFeeCharges, TermFeeCharges},
    validate_path, BoundedFee, CommitmentTransaction, Config, ConsensusConfig, DataLedger,
    DataTransactionHeader, DataTransactionLedger, DifficultyAdjustmentConfig, IrysAddress,
    IrysBlockHeader, PoaData, SealedBlock, SendTraced as _, SystemLedger, UnixTimestamp, H256,
    U256,
};
use irys_types::{get_ingress_proofs, IngressProof, LedgerChunkOffset};
use irys_types::{u256_from_le_bytes as hash_to_number, IrysTransactionId};
use irys_types::{BlockHash, LedgerChunkRange};
use irys_types::{BlockTransactions, UnixTimestampMs};
use irys_types::{CommitmentTypeV2, IrysTransactionCommon, VersionDiscriminant as _};
use irys_vdf::last_step_checkpoints_is_valid;
use irys_vdf::state::VdfStateReadonly;
use itertools::*;
use nodit::InclusiveInterval as _;
use openssl::sha;
use reth::revm::primitives::{Address, FixedBytes};
use reth::rpc::api::EngineApiClient as _;
use reth::rpc::types::engine::ExecutionPayload;
use reth_db::Database as _;
use reth_ethereum_primitives::Block;
use std::future::Future;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use tracing::{debug, error, info, warn, Instrument as _};

#[derive(Debug, Clone, PartialEq, Eq, Error)]
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
    #[error("Invalid promotion, transaction {txid:?} data size {got:?} does not match confirmed data root size {expected:?}")]
    InvalidPromotionDataSizeMismatch { txid: H256, expected: u64, got: u64 },
    #[error("Invalid last_diff_timestamp (expected {expected} got {got})")]
    LastDiffTimestampMismatch {
        expected: UnixTimestampMs,
        got: UnixTimestampMs,
    },
    #[error("Invalid ledger id {ledger_id}")]
    LedgerIdInvalid { ledger_id: u32 },
    #[error("Invalid merkle proof: {0}")]
    MerkleProofInvalid(String),
    #[error("Oracle price invalid")]
    OraclePriceInvalid,
    #[error("Unable to update cache for scheduled validation at block_hash: {0}")]
    UpdateCacheForScheduledValidationError(H256),
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
    #[error("Transaction {tx_id} cannot be promoted from {from:?} to {to:?}")]
    InvalidPromotionPath {
        tx_id: H256,
        from: DataLedger,
        to: DataLedger,
    },

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
    InvalidLedgerIdForTx {
        tx_id: H256,
        expected: u32,
        actual: u32,
    },
    #[error("Ledger id :{ledger_id} is invalid at block height: {block_height}")]
    InvalidLedgerId { ledger_id: u32, block_height: u64 },
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
    #[error("Incorrect number of ingress proofs from assigned owners. Expected {expected}, Actual: {actual}")]
    AssignedProofCountMismatch { expected: usize, actual: usize },
    #[error("Transaction {tx_id} has invalid ingress proof: {reason}")]
    InvalidIngressProof { tx_id: H256, reason: String },
    #[error("Ingress proof mismatch for transaction {tx_id}")]
    IngressProofMismatch { tx_id: H256 },
    #[error("Duplicate ingress proof signer {signer} for transaction {tx_id}")]
    DuplicateIngressProofSigner { tx_id: H256, signer: IrysAddress },
    #[error("Ingress proof signer {signer} is not staked for transaction {tx_id}")]
    UnstakedIngressProofSigner { tx_id: H256, signer: IrysAddress },
    #[error("Database Error {error}")]
    DatabaseError { error: String },
    #[error("Invalid Epoch snapshot {error}")]
    InvalidEpochSnapshot { error: String },

    /// Too many data transactions in submit ledger
    #[error("Too many data transactions in submit ledger: max {max}, got {got}")]
    TooManyDataTxs { max: u64, got: usize },

    /// Too many commitment transactions
    #[error("Too many commitment transactions: max {max}, got {got}")]
    TooManyCommitmentTxs { max: u64, got: usize },

    /// Missing transactions that were expected in block header
    #[error("Missing transactions: {0:?}")]
    MissingTransactions(Vec<IrysTransactionId>),

    /// Transaction ID mismatch between provided tx and block header
    #[error("Transaction ID mismatch: expected {expected}, got {actual}")]
    TransactionIdMismatch {
        expected: IrysTransactionId,
        actual: IrysTransactionId,
    },

    /// Invalid transaction signature
    #[error("Invalid signature for transaction {0}")]
    InvalidTransactionSignature(IrysTransactionId),

    /// Commitment transaction version is below minimum required after hardfork activation
    #[error("Commitment {tx_id} at position {position} has version {version}, minimum required is {minimum}")]
    CommitmentVersionInvalid {
        tx_id: H256,
        position: usize,
        version: u8,
        minimum: u8,
    },

    /// Commitment transactions provided but no commitment ledger in block header
    #[error("Commitment transactions provided but no commitment ledger in block header")]
    UnexpectedCommitmentTransactions,

    /// Invalid data ledgers length
    #[error("Invalid data ledgers length: expected {expected} ledgers, got {got}")]
    InvalidDataLedgersLength { expected: u32, got: usize },

    /// Failed to add block to block tree
    #[error("Failed to add block {block_hash} to block tree: {reason}")]
    AddBlockFailed { block_hash: H256, reason: String },
}

/// Validation error type that covers all block validation failures.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ValidationError {
    /// Pre-validation error (consensus parameter validation)
    #[error("Pre-validation failed: {0}")]
    PreValidation(#[from] PreValidationError),

    /// Validation was cancelled due to block tree state changes
    #[error("Validation cancelled: {reason}")]
    ValidationCancelled { reason: String },

    /// A validation task panicked unexpectedly
    #[error("Validation task panicked: {task}: {details}")]
    TaskPanicked { task: String, details: String },

    /// VDF validation failed
    #[error("VDF validation failed: {0}")]
    VdfValidationFailed(String),

    /// Seed data validation failed
    #[error("Seed data invalid: {0}")]
    SeedDataInvalid(String),

    /// Execution layer (Reth) validation failed
    #[error("Execution layer validation failed: {0}")]
    ExecutionLayerFailed(String),

    /// Recall range validation failed
    #[error("Recall range validation failed: {0}")]
    RecallRangeInvalid(String),

    /// Shadow transaction validation failed
    #[error("Shadow transaction validation failed: {0}")]
    ShadowTransactionInvalid(String),

    /// Failed to fetch commitment transactions from mempool or database
    #[error("Failed to fetch commitment transactions: {0}")]
    CommitmentTransactionFetchFailed(String),

    /// Commitment transaction has invalid value (stake/pledge/unpledge amount)
    #[error("Commitment transaction {tx_id} at position {position} has invalid value: {reason}")]
    CommitmentValueInvalid {
        tx_id: H256,
        position: usize,
        reason: String,
    },

    /// Commitment transaction version is below minimum required after hardfork activation
    #[error("Commitment {tx_id} at position {position} has version {version}, minimum required is {minimum}")]
    CommitmentVersionInvalid {
        tx_id: H256,
        position: usize,
        version: u8,
        minimum: u8,
    },

    /// Commitment type not allowed before hardfork activation (e.g., UpdateRewardAddress before Borealis)
    #[error("Commitment {tx_id} at position {position} uses type {commitment_type} not allowed before hardfork activation")]
    CommitmentTypeNotAllowed {
        tx_id: H256,
        position: usize,
        commitment_type: String,
    },

    /// Commitment ordering validation failed
    #[error("Commitment ordering validation failed: {0}")]
    CommitmentOrderingFailed(String),

    /// Commitment snapshot validation rejected the commitment
    #[error("Commitment {tx_id} rejected by snapshot validation with status {status:?}")]
    CommitmentSnapshotRejected {
        tx_id: H256,
        status: CommitmentSnapshotStatus,
    },

    /// Unpledge commitment targets partition not owned by signer
    #[error("Unpledge commitment {tx_id} targets partition {partition_hash} not owned by signer {signer}")]
    UnpledgePartitionNotOwned {
        tx_id: H256,
        partition_hash: H256,
        signer: IrysAddress,
    },

    /// Parent commitment snapshot not found
    #[error("Parent commitment snapshot missing for block {block_hash}")]
    ParentCommitmentSnapshotMissing { block_hash: H256 },

    /// Parent epoch snapshot not found
    #[error("Parent epoch snapshot missing for block {block_hash}")]
    ParentEpochSnapshotMissing { block_hash: H256 },

    /// Parent block not found in block tree
    #[error("Parent block {block_hash} not found in block tree")]
    ParentBlockMissing { block_hash: H256 },

    /// Epoch block commitment mismatch
    #[error("Epoch block commitment mismatch at position {position}")]
    EpochCommitmentMismatch { position: usize },

    /// Epoch block contains extra commitment
    #[error("Epoch block contains extra commitment at position {position}")]
    EpochExtraCommitment { position: usize },

    /// Epoch block missing expected commitment
    #[error("Epoch block missing expected commitment at position {position}")]
    EpochMissingCommitment { position: usize },

    /// Commitment transaction in wrong order
    #[error("Commitment transaction at position {position} in wrong order")]
    CommitmentWrongOrder { position: usize },

    /// Generic validation error for edge cases
    #[error("Validation failed: {0}")]
    Other(String),
}

/// Full pre-validation steps for a block
#[tracing::instrument(level = "trace", skip_all, fields(block.hash = %sealed_block.header().block_hash, block.height = sealed_block.header().height))]
pub async fn prevalidate_block(
    sealed_block: &SealedBlock,
    previous_block: &IrysBlockHeader,
    parent_epoch_snapshot: Arc<EpochSnapshot>,
    config: Config,
    reward_curve: Arc<HalvingCurve>,
    parent_ema_snapshot: &EmaSnapshot,
) -> Result<(), PreValidationError> {
    let block = sealed_block.header();
    let transactions = sealed_block.transactions();

    debug!(
        block.hash = ?block.block_hash,
        block.height = ?block.height,
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
    prev_output_is_valid(block, previous_block)?;
    debug!(
        block.hash = ?block.block_hash,
        block.height = ?block.height,
        "prev_output_is_valid",
    );

    // Check block height continuity
    height_is_valid(block, previous_block)?;
    debug!(
        block.hash = ?block.block_hash,
        block.height = ?block.height,
        "height_is_valid",
    );

    // Check block timestamp drift
    timestamp_is_valid(
        block.timestamp.as_millis(),
        previous_block.timestamp.as_millis(),
        config.consensus.max_future_timestamp_drift_millis,
    )?;

    // Check the difficulty
    difficulty_is_valid(
        block,
        previous_block,
        &config.consensus.difficulty_adjustment,
    )?;

    // Validate the last_diff_timestamp field
    last_diff_timestamp_is_valid(
        block,
        previous_block,
        &config.consensus.difficulty_adjustment,
    )?;

    debug!(
        block.hash = ?block.block_hash,
        block.height = ?block.height,
        "difficulty_is_valid",
    );

    // Validate previous_cumulative_diff points to parent's cumulative_diff
    previous_cumulative_difficulty_is_valid(block, previous_block)?;
    debug!(
        block.hash = ?block.block_hash,
        block.height = ?block.height,
        "previous_cumulative_difficulty_is_valid",
    );

    // Check the cumulative difficulty
    cumulative_difficulty_is_valid(block, previous_block)?;
    debug!(
        block.hash = ?block.block_hash,
        block.height = ?block.height,
        "cumulative_difficulty_is_valid",
    );

    check_poa_data_expiration(&block.poa, parent_epoch_snapshot.clone())?;
    debug!("poa data not expired");

    // Check the solution_hash
    solution_hash_is_valid(block, previous_block)?;
    debug!(
        block.hash = ?block.block_hash,
        block.height = ?block.height,
        "solution_hash_is_valid",
    );

    // Verify the solution_hash cryptographic link to PoA chunk, partition_chunk_offset and VDF seed
    solution_hash_link_is_valid(block, &poa_chunk)?;
    debug!(
        block.hash = ?block.block_hash,
        block.height = ?block.height,
        "solution_hash_link_is_valid",
    );

    // Check the previous solution hash references the parent correctly
    previous_solution_hash_is_valid(block, previous_block)?;
    debug!(
        block.hash = ?block.block_hash,
        block.height = ?block.height,
        "previous_solution_hash_is_valid",
    );

    // Validate VDF seeds/next_seed against parent before any VDF-related processing
    let vdf_reset_frequency: u64 = config.vdf.reset_frequency as u64;
    if !matches!(
        is_seed_data_valid(block, previous_block, vdf_reset_frequency),
        ValidationResult::Valid
    ) {
        return Err(PreValidationError::VDFCheckpointsInvalid(
            "Seed data is invalid".to_string(),
        ));
    }

    // Ensure the last_epoch_hash field correctly references the most recent epoch block
    last_epoch_hash_is_valid(
        block,
        previous_block,
        config.consensus.epoch.num_blocks_in_epoch,
    )?;
    debug!(
        block.hash = ?block.block_hash,
        block.height = ?block.height,
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
                previous_block,
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

    let target_block_time_seconds = config.consensus.difficulty_adjustment.block_time;

    // Use height * target_block_time for consistent reward curve positioning
    let previous_block_seconds = (previous_block.height() * target_block_time_seconds) as u128;
    let current_block_seconds = previous_block_seconds + target_block_time_seconds as u128;

    let reward = reward_curve
        .reward_between(previous_block_seconds, current_block_seconds)
        .map_err(|e| PreValidationError::RewardCurveError(e.to_string()))?;

    // Check valid curve price
    if reward.amount != block.reward_amount {
        return Err(PreValidationError::RewardMismatch {
            got: block.reward_amount,
            expected: reward.amount,
        });
    }

    // Validate ingress proof signers are unique and staked
    validate_ingress_proof_signers(block, &parent_epoch_snapshot)?;
    debug!(
        block.hash = ?block.block_hash,
        block.height = ?block.height,
        "ingress_proof_signers_valid",
    );

    // After pre-validating a bunch of quick checks we validate the signature
    if !block.is_signature_valid() {
        return Err(PreValidationError::BlockSignatureInvalid);
    }

    // TODO: add validation for the term ledger 'expires' field,
    // ensuring it gets properly updated on epoch boundaries, and it's
    // consistent with the block's height and parent block's height

    // ========================================
    // Data Ledger Validation
    // ========================================
    // Ensure only active data ledgers are present in the block
    for ledger in &block.data_ledgers {
        match DataLedger::try_from(ledger.ledger_id) {
            Ok(DataLedger::Publish | DataLedger::Submit) => {
                // Valid ledgers - continue
            }
            _ => {
                return Err(PreValidationError::InvalidLedgerId {
                    ledger_id: ledger.ledger_id,
                    block_height: block.height,
                })
            }
        }
    }

    // ========================================
    // Transaction validation
    // ========================================

    // Validate submit ledger transactions
    let submit_ledger = block
        .data_ledgers
        .get(DataLedger::Submit as usize)
        .ok_or_else(|| PreValidationError::InvalidDataLedgersLength {
            expected: DataLedger::Submit.into(),
            got: block.data_ledgers.len(),
        })?;

    let submit_txs = transactions.get_ledger_txs(DataLedger::Submit);

    // Enforce max_data_txs_per_block on submit ledger
    let max_data_txs = config
        .node_config
        .consensus_config()
        .mempool
        .max_data_txs_per_block;
    if submit_txs.len() > max_data_txs as usize {
        return Err(PreValidationError::TooManyDataTxs {
            max: max_data_txs,
            got: submit_txs.len(),
        });
    }

    // Validate submit transactions: count, IDs, and signatures
    validate_transactions(submit_txs, &submit_ledger.tx_ids.0)?;
    debug!(
        block.hash = ?block.block_hash,
        block.height = ?block.height,
        "submit_transactions_valid",
    );

    // Validate publish ledger transactions
    let publish_ledger = block
        .data_ledgers
        .get(DataLedger::Publish as usize)
        .ok_or_else(|| PreValidationError::InvalidDataLedgersLength {
            expected: DataLedger::Publish.into(),
            got: block.data_ledgers.len(),
        })?;

    let publish_txs = transactions.get_ledger_txs(DataLedger::Publish);

    // Validate publish transactions: count, IDs, and signatures
    validate_transactions(publish_txs, &publish_ledger.tx_ids.0)?;
    debug!(
        block.hash = ?block.block_hash,
        block.height = ?block.height,
        "publish_transactions_valid",
    );

    // Validate ingress proofs for published transactions
    for tx_header in publish_txs.iter() {
        let tx_proofs = get_ingress_proofs(publish_ledger, &tx_header.id)
            .map_err(|_| PreValidationError::IngressProofsMissing)?;
        for proof in tx_proofs.iter() {
            proof
                .pre_validate(&tx_header.data_root)
                .map_err(|e| PreValidationError::IngressProofSignatureInvalid(e.to_string()))?;
        }
    }
    debug!(
        block.hash = ?block.block_hash,
        block.height = ?block.height,
        "ingress_proofs_valid",
    );

    // Validate commitment ledger transactions
    let commitment_ledger = block
        .system_ledgers
        .iter()
        .find(|b| b.ledger_id == SystemLedger::Commitment);

    let commitment_txs = transactions.get_ledger_system_txs(SystemLedger::Commitment);

    if let Some(commitment_ledger) = commitment_ledger {
        // Check commitment tx count limit (skip for epoch blocks which contain rollup of all epoch txs)
        let is_epoch_block = block.height.is_multiple_of(
            config
                .node_config
                .consensus_config()
                .epoch
                .num_blocks_in_epoch,
        );
        if !is_epoch_block {
            let max_commitment_txs = config
                .node_config
                .consensus_config()
                .mempool
                .max_commitment_txs_per_block;
            if commitment_txs.len() > max_commitment_txs as usize {
                return Err(PreValidationError::TooManyCommitmentTxs {
                    max: max_commitment_txs,
                    got: commitment_txs.len(),
                });
            }

            // Validate commitment transaction versions against hardfork rules (cheap check before signatures)
            if let Some((tx_id, position, version, minimum)) = find_invalid_commitment_version(
                &config.consensus,
                commitment_txs,
                block.timestamp_secs(),
            ) {
                return Err(PreValidationError::CommitmentVersionInvalid {
                    tx_id,
                    position,
                    version,
                    minimum,
                });
            }
        }

        // Validate commitment transactions: count, IDs, and signatures
        validate_transactions(commitment_txs, &commitment_ledger.tx_ids.0)?;
        debug!(
            block.hash = ?block.block_hash,
            block.height = ?block.height,
            "commitment_transactions_valid",
        );
    } else if !commitment_txs.is_empty() {
        // Commitment transactions provided but no commitment ledger in block header
        return Err(PreValidationError::UnexpectedCommitmentTransactions);
    }

    Ok(())
}

/// Finds the first commitment transaction with an invalid version according to hardfork rules.
/// Returns `Some((tx_id, position, version, minimum))` if an invalid tx is found, `None` if all are valid.
fn find_invalid_commitment_version(
    config: &ConsensusConfig,
    commitment_txs: &[CommitmentTransaction],
    timestamp: UnixTimestamp,
) -> Option<(H256, usize, u8, u8)> {
    let min_version = config.hardforks.minimum_commitment_version_at(timestamp)?;
    for (idx, tx) in commitment_txs.iter().enumerate() {
        if tx.version() < min_version {
            return Some((tx.id(), idx, tx.version(), min_version));
        }
    }
    None
}

/// Validate transactions against expected IDs from the block header.
/// Checks: count matches, IDs match in order, signatures are valid.
fn validate_transactions<T: IrysTransactionCommon>(
    txs: &[T],
    expected_ids: &[IrysTransactionId],
) -> Result<(), PreValidationError> {
    // Check count matches
    if txs.len() != expected_ids.len() {
        let provided_ids: std::collections::HashSet<_> =
            txs.iter().map(IrysTransactionCommon::id).collect();
        let missing: Vec<_> = expected_ids
            .iter()
            .filter(|id| !provided_ids.contains(id))
            .copied()
            .collect();
        return Err(PreValidationError::MissingTransactions(missing));
    }

    // Check IDs match in order and signatures are valid
    for (tx, expected_id) in txs.iter().zip(expected_ids.iter()) {
        let actual_id = tx.id();
        if actual_id != *expected_id {
            return Err(PreValidationError::TransactionIdMismatch {
                expected: *expected_id,
                actual: actual_id,
            });
        }
        if !tx.is_signature_valid() {
            return Err(PreValidationError::InvalidTransactionSignature(actual_id));
        }
    }

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
    let expected = if block.height.is_multiple_of(blocks_between_adjustments) {
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
#[tracing::instrument(level = "trace", skip_all, fields(
    block.miner_address = ?miner_address,
    poa.chunk_offset = ?poa.partition_chunk_offset,
    poa.partition_hash = ?poa.partition_hash,
    config.entropy_packing_iterations = ?config.entropy_packing_iterations,
    config.chunk_size = ?config.chunk_size
), err)]

pub fn poa_is_valid(
    poa: &PoaData,
    block_index_guard: &BlockIndexReadGuard,
    epoch_snapshot: &EpochSnapshot,
    config: &ConsensusConfig,
    miner_address: &IrysAddress,
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
            slot_index_u64 * config.num_chunks_in_partition + u64::from(poa.partition_chunk_offset);

        // ledger data -> block
        let ledger = DataLedger::try_from(ledger_id)
            .map_err(|_| PreValidationError::LedgerIdInvalid { ledger_id })?;

        let bb = block_index_guard
            .read()
            .get_block_bounds(ledger, LedgerChunkOffset::from(ledger_chunk_offset))
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

        if !(tx_path_result.min_byte_range..=tx_path_result.max_byte_range)
            .contains(&(block_chunk_offset * (config.chunk_size as u128)))
        {
            return Err(PreValidationError::PoAChunkOffsetOutOfTxBounds);
        }

        let tx_chunk_offset =
            block_chunk_offset * (config.chunk_size as u128) - tx_path_result.min_byte_range;

        // data_path validation
        let data_path_result = validate_path(tx_path_result.leaf_hash, &data_path, tx_chunk_offset)
            .map_err(|e| PreValidationError::MerkleProofInvalid(e.to_string()))?;

        if !(data_path_result.min_byte_range..=data_path_result.max_byte_range)
            .contains(&tx_chunk_offset)
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
                .min((data_path_result.max_byte_range - data_path_result.min_byte_range) as u64))
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
/// generated from the Irys block data. This is a pure validation function with no side effects.
/// Returns the ExecutionData on success to avoid re-fetching it for reth submission.
#[tracing::instrument(level = "trace", skip_all, fields(block = ?block.block_hash))]
pub async fn shadow_transactions_are_valid(
    config: &Config,
    service_senders: &ServiceSenders,
    block_tree_guard: &BlockTreeReadGuard,
    mempool_guard: &MempoolReadGuard,
    block: &IrysBlockHeader,
    db: &DatabaseProvider,
    payload_provider: ExecutionPayloadCache,
    parent_epoch_snapshot: Arc<EpochSnapshot>,
    parent_commitment_snapshot: Arc<CommitmentSnapshot>,
    block_index: BlockIndex,
    transactions: &BlockTransactions,
) -> eyre::Result<ExecutionData> {
    // 1. Get the execution payload for validation
    let execution_data = payload_provider
        .wait_for_payload(&block.evm_block_hash)
        .in_current_span()
        .await
        .ok_or_eyre("reth execution payload never arrived")?;

    let ExecutionData { payload, sidecar } = execution_data.clone();

    let ExecutionPayload::V3(payload_v3) = payload else {
        eyre::bail!("irys-reth expects that all payloads are of v3 type");
    };
    ensure!(
        payload_v3.withdrawals().is_empty(),
        "withdrawals must always be empty"
    );

    // Reject any blob gas usage in the payload
    if payload_v3.blob_gas_used != 0 {
        tracing::debug!(
            block.hash = %block.block_hash,
            block.evm_block_hash = %block.evm_block_hash,
            payload.blob_gas_used = payload_v3.blob_gas_used,
            "Rejecting block: blob_gas_used must be zero",
        );
        eyre::bail!("block has non-zero blob_gas_used which is disabled");
    }
    if payload_v3.excess_blob_gas != 0 {
        tracing::debug!(
            block.block_hash = %block.block_hash,
            block.evm_block_hash = %block.evm_block_hash,
            payload.excess_blob_gas = payload_v3.excess_blob_gas,
            "Rejecting block: excess_blob_gas must be zero",
        );
        eyre::bail!("block has non-zero excess_blob_gas which is disabled");
    }

    // Reject any block that carries blob sidecars (EIP-4844).
    // We keep Cancun active but disable blobs/sidecars entirely.
    if let Some(versioned_hashes) = sidecar.versioned_hashes() {
        if !versioned_hashes.is_empty() {
            tracing::debug!(
                block.block_hash = %block.block_hash,
                block.evm_block_hash = %block.evm_block_hash,
                block.versioned_hashes_len = versioned_hashes.len(),
                "Rejecting block: EIP-4844 blobs/sidecars are not supported",
            );
            eyre::bail!("block contains EIP-4844 blobs/sidecars which are disabled");
        }
    }
    // Requests are disabled: reject if any present or if header-level requests hash is set.
    if let Some(requests) = sidecar.requests() {
        if !requests.is_empty() {
            tracing::debug!(
                block.block_hash = %block.block_hash,
                block.evm_block_hash = %block.evm_block_hash,
                block.versioned_hashes_len = requests.len(),
                "Rejecting block: EIP-7685 requests which are disabled",
            );
            eyre::bail!("block contains EIP-7685 requests which are disabled");
        }
    }
    // Note: `requests_hash` may be present even when the requests list is empty.
    // Do not reject on presence of the hash alone; only non-empty requests are disallowed.

    // ensure the execution payload timestamp matches the block timestamp
    // truncated to full seconds
    let payload_timestamp: u128 = payload_v3.timestamp().into();
    let block_timestamp_sec: u128 = block.timestamp_secs().as_secs().into();
    ensure!(
        payload_timestamp == block_timestamp_sec,
        "EVM payload timestamp {payload_timestamp} does not match block timestamp {block_timestamp_sec}"
    );

    let evm_block: Block = payload_v3.try_into_block()?;

    // Reject presence of EIP-7685 requests via header-level requests_hash as we disable requests.
    if evm_block.header.requests_hash.is_some() {
        tracing::debug!(
            block.block_hash = %block.block_hash,
            block.evm_block_hash = %block.evm_block_hash,
            "Rejecting block: EIP-7685 requests_hash present which is disabled",
        );
        eyre::bail!("block contains EIP-7685 requests_hash which is disabled");
    }

    // 2. Enforce that no EIP-4844 (blob) transactions are present in the block
    for tx in evm_block.body.transactions.iter() {
        if tx.is_eip4844() {
            tracing::debug!(
                block.block_hash = %block.block_hash,
                block.evm_block_hash = %block.evm_block_hash,
                "Rejecting block: contains EIP-4844 transaction which is disabled",
            );
            eyre::bail!("block contains EIP-4844 transaction which is disabled");
        }
    }

    // 3. Extract shadow transactions from the beginning of the block lazily
    let txs_slice = &evm_block.body.transactions;
    let block_miner_address: Address = block.miner_address.into();
    let actual_shadow_txs = extract_leading_shadow_txs(txs_slice).map(|res| {
        // Verify signer for each yielded shadow tx (must be the miner)
        let (stx, tx_ref) = res?;
        let tx_signer = tx_ref.clone().into_signed().recover_signer()?;
        ensure!(
            block_miner_address == tx_signer,
            "Shadow tx signer is not the miner"
        );
        Ok(stx)
    });

    // 3. Generate expected shadow transactions
    let expected_txs = generate_expected_shadow_transactions(
        config,
        service_senders,
        block_tree_guard,
        mempool_guard,
        block,
        db,
        parent_epoch_snapshot,
        parent_commitment_snapshot,
        block_index,
        transactions,
    )
    .await?;

    // 4. Validate they match
    validate_shadow_transactions_match(actual_shadow_txs, expected_txs.into_iter(), block)?;

    // 5. Return the execution data for reuse
    Ok(execution_data)
}

/// Lazily extract all leading shadow transactions from a block's transactions using a streaming iterator.
///
/// - Yields shadow transactions at the front of the list.
/// - If any shadow transaction appears after the first non-shadow, yields a single error and ends.
fn extract_leading_shadow_txs(
    txs: &[reth_ethereum_primitives::TransactionSigned],
) -> impl Iterator<
    Item = eyre::Result<(
        ShadowTransaction,
        &reth_ethereum_primitives::TransactionSigned,
    )>,
> + '_ {
    let mut it = txs.iter();
    let mut seen_non_shadow = false;
    let mut reported_error = false;
    std::iter::from_fn(move || {
        if reported_error {
            return None;
        }
        for tx in it.by_ref() {
            match detect_and_decode(tx) {
                Ok(Some(stx)) => {
                    if seen_non_shadow {
                        reported_error = true;
                        return Some(Err(ShadowTxError::ShadowTxAfterNonShadow.into()));
                    }
                    return Some(Ok((stx, tx)));
                }
                Ok(None) => {
                    seen_non_shadow = true;
                    continue;
                }
                Err(e) => {
                    reported_error = true;
                    return Some(Err(e.into()));
                }
            }
        }
        None
    })
}

/// Submits the EVM payload to reth for execution layer validation.
/// This should only be called after all consensus layer validations have passed.
#[tracing::instrument(level = "trace", skip_all, err, fields(
    block.hash = %block.block_hash,
    block.height = %block.height,
    block.evm_block_hash = %block.evm_block_hash
))]
pub async fn submit_payload_to_reth(
    block: &IrysBlockHeader,
    reth_adapter: &IrysRethNodeAdapter,
    execution_data: ExecutionData,
) -> eyre::Result<()> {
    let ExecutionData { payload, sidecar } = execution_data;

    let ExecutionPayload::V3(payload_v3) = payload else {
        eyre::bail!("irys-reth expects that all payloads are of v3 type");
    };

    let versioned_hashes = sidecar
        .versioned_hashes()
        .ok_or_eyre("version hashes must be present")?
        .clone();

    // Submit to reth execution layer
    let engine_api_client = reth_adapter.inner.engine_http_client();
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

    Ok(())
}

/// Generates expected shadow transactions
#[tracing::instrument(level = "trace", skip_all, err)]
async fn generate_expected_shadow_transactions(
    config: &Config,
    service_senders: &ServiceSenders,
    block_tree_guard: &BlockTreeReadGuard,
    mempool_guard: &MempoolReadGuard,
    block: &IrysBlockHeader,
    db: &DatabaseProvider,
    parent_epoch_snapshot: Arc<EpochSnapshot>,
    parent_commitment_snapshot: Arc<CommitmentSnapshot>,
    block_index: BlockIndex,
    transactions: &BlockTransactions,
) -> eyre::Result<Vec<ShadowTransaction>> {
    // Look up previous block to get EVM hash
    let prev_block = {
        let (tx_prev, rx_prev) = tokio::sync::oneshot::channel();
        service_senders
            .mempool
            .send_traced(MempoolServiceMessage::GetBlockHeader(
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

    // Calculate is_epoch_block early since it's needed for multiple checks
    let is_epoch_block = block
        .height
        .is_multiple_of(config.consensus.epoch.num_blocks_in_epoch);

    // IMPORTANT: on epoch blocks we don't generate shadow txs for commitment txs
    let commitment_txs: &[CommitmentTransaction] = if is_epoch_block {
        &[]
    } else {
        transactions.get_ledger_system_txs(SystemLedger::Commitment)
    };

    // Use pre-fetched submit ledger transactions
    let data_txs = transactions.get_ledger_txs(DataLedger::Submit).to_vec();

    // Use pre-fetched publish ledger transactions with proofs from block header
    let (publish_ledger, _submit_ledger) = extract_data_ledgers(block)?;
    let publish_ledger_with_txs = PublishLedgerWithTxs {
        txs: transactions.get_ledger_txs(DataLedger::Publish).to_vec(),
        proofs: publish_ledger.proofs.clone(),
    };

    // Get treasury balance from previous block
    let initial_treasury_balance = prev_block.treasury;

    // Calculate expired ledger fees for epoch blocks
    let expired_ledger_fees = if is_epoch_block {
        ledger_expiry::calculate_expired_ledger_fees(
            &parent_epoch_snapshot,
            block.height,
            DataLedger::Submit, // Currently only Submit ledgers expire
            config,
            block_index,
            block_tree_guard,
            mempool_guard,
            db,
            true, // expect_txs_to_be_promoted: true - we expect txs to be promoted normally
        )
        .in_current_span()
        .await?
    } else {
        ledger_expiry::LedgerExpiryBalanceDelta::default()
    };

    // Compute commitment refund events for epoch blocks from parent's commitment snapshot
    let commitment_refund_events: Vec<crate::block_producer::UnpledgeRefundEvent> =
        if is_epoch_block {
            crate::commitment_refunds::derive_unpledge_refunds_from_snapshot(
                &parent_commitment_snapshot,
                &config.consensus,
            )?
        } else {
            Vec::new()
        };
    let unstake_refund_events: Vec<crate::block_producer::UnstakeRefundEvent> = if is_epoch_block {
        crate::commitment_refunds::derive_unstake_refunds_from_snapshot(
            &parent_commitment_snapshot,
            &config.consensus,
        )?
    } else {
        Vec::new()
    };

    let mut shadow_tx_generator = ShadowTxGenerator::new(
        &block.height,
        &block.reward_address,
        &block.reward_amount,
        &prev_block,
        &block.solution_hash,
        &config.consensus,
        commitment_txs,
        &data_txs,
        &publish_ledger_with_txs,
        initial_treasury_balance,
        &expired_ledger_fees,
        &commitment_refund_events,
        &unstake_refund_events,
        &parent_epoch_snapshot,
    )
    .map_err(|e| eyre!("Failed to create shadow tx generator: {}", e))?;

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

/// Validates  the actual shadow transactions match the expected ones
#[tracing::instrument(level = "trace", skip_all, err)]
fn validate_shadow_transactions_match(
    actual: impl Iterator<Item = eyre::Result<ShadowTransaction>>,
    expected: impl Iterator<Item = ShadowTransaction>,
    block_header: &IrysBlockHeader,
) -> eyre::Result<()> {
    // Verify solution hash matches the block
    let expected_hash: FixedBytes<32> = block_header.solution_hash.into();

    // Validate each expected shadow transaction
    for (idx, data) in actual.zip_longest(expected).enumerate() {
        let EitherOrBoth::Both(actual, expected) = data else {
            // If either of the shadow txs is not present, it means it was not generated as `expected`
            // or it was not included in the block. either way - an error
            tracing::warn!(?data, "shadow tx len mismatch");
            eyre::bail!("actual and expected shadow txs lens differ");
        };
        let actual = actual?;

        // Validate solution hash for all V1 transactions
        if let ShadowTransaction::V1 {
            packet: _,
            solution_hash,
        } = &actual
        {
            if *solution_hash != expected_hash {
                eyre::bail!(
                    "Invalid solution hash reference in shadow transaction at idx {}. Expected {:?}, got {:?}",
                    idx,
                    H256::from(*expected_hash),
                    H256::from(**solution_hash)
                );
            }
        }

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

#[tracing::instrument(level = "trace", skip_all)]
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
        ValidationResult::Invalid(ValidationError::SeedDataInvalid(format!(
            "Expected: {:?}, got: {:?}",
            expected_seed_data, vdf_info
        )))
    }
}

/// Validates that commitment transactions in a block are ordered correctly
/// according to the same priority rules used by the mempool:
/// 1. Stakes first (sorted by fee, highest first)
/// 2. Then pledges (sorted by pledge_count_before_executing ascending, then by fee descending)
#[tracing::instrument(level = "trace", skip_all, err, fields(block.hash = %block.block_hash, block.height = %block.height))]
pub async fn commitment_txs_are_valid(
    config: &Config,
    block: &IrysBlockHeader,
    block_tree_guard: &BlockTreeReadGuard,
    commitment_txs: &[CommitmentTransaction],
) -> Result<(), ValidationError> {
    // Validate commitment transaction versions against hardfork rules
    let block_timestamp = block.timestamp_secs();

    let is_epoch_block = block
        .height
        .is_multiple_of(config.consensus.epoch.num_blocks_in_epoch);

    if !is_epoch_block {
        // only filter txs for non-epoch blocks
        if let Some((tx_id, position, version, minimum)) =
            find_invalid_commitment_version(&config.consensus, commitment_txs, block_timestamp)
        {
            error!(
                "Commitment transaction {} at position {} has version {}, minimum is {}",
                tx_id, position, version, minimum
            );
            return Err(ValidationError::CommitmentVersionInvalid {
                tx_id,
                position,
                version,
                minimum,
            });
        }
    }

    // Get parent snapshots for Borealis activation check
    let (parent_commitment_snapshot, parent_epoch_snapshot) = {
        let read = block_tree_guard.read();
        let commitment_snapshot = read
            .get_commitment_snapshot(&block.previous_block_hash)
            .map_err(|_| ValidationError::ParentCommitmentSnapshotMissing {
                block_hash: block.previous_block_hash,
            })?;
        let epoch_snapshot = read
            .get_epoch_snapshot(&block.previous_block_hash)
            .ok_or_else(|| ValidationError::ParentEpochSnapshotMissing {
                block_hash: block.previous_block_hash,
            })?;
        (commitment_snapshot, epoch_snapshot)
    };

    // Validate Borealis: reject UpdateRewardAddress if not activated
    if !config
        .consensus
        .hardforks
        .is_update_reward_address_allowed_for_epoch(&parent_epoch_snapshot)
    {
        for (idx, tx) in commitment_txs.iter().enumerate() {
            if matches!(
                tx.commitment_type(),
                CommitmentTypeV2::UpdateRewardAddress { .. }
            ) {
                error!(
                        "Commitment transaction {} at position {} uses UpdateRewardAddress before Borealis activation",
                        tx.id(),
                        idx
                    );
                return Err(ValidationError::CommitmentTypeNotAllowed {
                    tx_id: tx.id(),
                    position: idx,
                    commitment_type: "UpdateRewardAddress".to_string(),
                });
            }
        }
    }

    // Validate that all commitment transactions have correct values
    for (idx, tx) in commitment_txs.iter().enumerate() {
        tx.validate_value(&config.consensus).map_err(|e| {
            error!(
                "Commitment transaction {} at position {} has invalid value: {}",
                tx.id(),
                idx,
                e
            );
            ValidationError::CommitmentValueInvalid {
                tx_id: tx.id(),
                position: idx,
                reason: e.to_string(),
            }
        })?;
    }

    if is_epoch_block {
        debug!(
            "Validating commitment order for epoch block at height {}",
            block.height
        );

        let expected_commitments = parent_commitment_snapshot.get_epoch_commitments();

        // Use zip_longest to compare actual vs expected directly
        for (idx, pair) in commitment_txs
            .iter()
            .zip_longest(expected_commitments.iter())
            .enumerate()
        {
            match pair {
                EitherOrBoth::Both(actual, expected) => {
                    if actual != expected {
                        error!(
                            "Epoch block commitment mismatch at position {}. Expected: {:?}, Got: {:?}",
                            idx, expected, actual
                        );
                        return Err(ValidationError::EpochCommitmentMismatch { position: idx });
                    }
                }
                EitherOrBoth::Left(actual) => {
                    error!(
                        "Extra commitment in epoch block at position {}: {:?}",
                        idx, actual
                    );
                    return Err(ValidationError::EpochExtraCommitment { position: idx });
                }
                EitherOrBoth::Right(expected) => {
                    error!(
                        "Missing commitment in epoch block at position {}: {:?}",
                        idx, expected
                    );
                    return Err(ValidationError::EpochMissingCommitment { position: idx });
                }
            }
        }

        debug!("Epoch block commitment transaction validation successful");
        return Ok(());
    }

    // Regular block validation: ensure commitments align with snapshot state
    let mut simulated_snapshot = CommitmentSnapshot {
        commitments: parent_commitment_snapshot.commitments.clone(),
    };

    for tx in commitment_txs {
        if let CommitmentTypeV2::Unpledge { partition_hash, .. } = tx.commitment_type() {
            let owner = parent_epoch_snapshot
                .partition_assignments
                .get_assignment(partition_hash)
                .map(|assignment| assignment.miner_address);
            if owner != Some(tx.signer()) {
                return Err(ValidationError::UnpledgePartitionNotOwned {
                    tx_id: tx.id(),
                    partition_hash,
                    signer: tx.signer(),
                });
            }
        }

        let status = simulated_snapshot.add_commitment(tx, &parent_epoch_snapshot);
        if status != CommitmentSnapshotStatus::Accepted {
            return Err(ValidationError::CommitmentSnapshotRejected {
                tx_id: tx.id(),
                status,
            });
        }
    }

    if commitment_txs.is_empty() {
        return Ok(());
    }

    // Sort to get expected order
    let mut expected_order = commitment_txs.to_vec();
    expected_order.sort();

    // Compare actual order vs expected order
    for (idx, pair) in commitment_txs
        .iter()
        .zip_longest(expected_order.iter())
        .enumerate()
    {
        match pair {
            EitherOrBoth::Both(actual, expected) => {
                if actual.id() != expected.id() {
                    error!(
                        "Commitment transaction at position {} in wrong order. Expected: {}, Got: {}",
                        idx, expected.id(), actual.id()
                    );
                    return Err(ValidationError::CommitmentWrongOrder { position: idx });
                }
            }
            _ => {
                // This should never happen since we're comparing the same set
                error!(
                    "Internal error: commitment ordering validation mismatch for block {} (height {})",
                    block.block_hash, block.height
                );
                return Err(ValidationError::CommitmentOrderingFailed(
                    format!("Internal error: commitment ordering validation mismatch for block {} (height {})", block.block_hash, block.height),
                ));
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
    timestamp_secs: UnixTimestamp,
) -> eyre::Result<Amount<(NetworkFee, Irys)>> {
    let number_of_ingress_proofs_total = config.number_of_ingress_proofs_total_at(timestamp_secs);
    calculate_perm_fee_from_config(
        bytes_to_store,
        &config.consensus,
        number_of_ingress_proofs_total,
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
    timestamp_secs: UnixTimestamp,
) -> eyre::Result<U256> {
    let number_of_ingress_proofs_total = config.number_of_ingress_proofs_total_at(timestamp_secs);
    irys_types::storage_pricing::calculate_term_fee(
        bytes_to_store,
        epochs_for_storage,
        &config.consensus,
        number_of_ingress_proofs_total,
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
#[tracing::instrument(level = "trace", skip_all, err)]
pub async fn data_txs_are_valid(
    config: &Config,
    service_senders: &ServiceSenders,
    block: &IrysBlockHeader,
    db: &DatabaseProvider,
    block_tree_guard: &BlockTreeReadGuard,
    submit_txs: &[DataTransactionHeader],
    publish_txs: &[DataTransactionHeader],
) -> Result<(), PreValidationError> {
    // Get the parent block's EMA snapshot for fee calculations
    let block_ema = block_tree_guard
        .read()
        .get_ema_snapshot(&block.previous_block_hash)
        .ok_or(PreValidationError::BlockEmaSnapshotNotFound {
            block_hash: block.previous_block_hash,
        })?;

    // Extract publish ledger for ingress proofs validation
    let (publish_ledger, _submit_ledger) = extract_data_ledgers(block)
        .map_err(|e| PreValidationError::DataLedgerExtractionFailed(e.to_string()))?;

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
                    // Same block promotion: both ledgers are in the current block
                    ledger_current: (DataLedger::Publish, block.block_hash),
                    ledger_historical: (DataLedger::Submit, block.block_hash),
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
        config.consensus.mempool.tx_anchor_expiry_depth as u64,
        service_senders,
        db,
    )
    .await
    .map_err(|e| PreValidationError::PreviousTxInclusionsFailed(e.to_string()))?;

    let ro_tx = db.tx().map_err(|e| PreValidationError::DatabaseError {
        error: e.to_string(),
    })?;

    let validate_price = |tx: &&DataTransactionHeader| {
        // Calculate expected fees based on data size using block's EMA
        // Calculate term fee first as it's needed for perm fee calculation
        // Calculate epochs for storage using the same method as mempool
        let epochs_for_storage = irys_types::ledger_expiry::calculate_submit_ledger_expiry(
            block.height,
            config.consensus.epoch.num_blocks_in_epoch,
            config.consensus.epoch.submit_ledger_epoch_length,
        );

        // Convert block timestamp from millis to seconds for hardfork params
        let timestamp_secs = block.timestamp_secs();
        let expected_term_fee = calculate_term_storage_base_network_fee(
            tx.data_size,
            epochs_for_storage,
            &block_ema,
            config,
            timestamp_secs,
        )
        .map_err(|e| PreValidationError::FeeCalculationFailed(e.to_string()))?;
        let expected_perm_fee = calculate_perm_storage_total_fee(
            tx.data_size,
            expected_term_fee,
            &block_ema,
            config,
            timestamp_secs,
        )
        .map_err(|e| PreValidationError::FeeCalculationFailed(e.to_string()))?;

        // Validate perm_fee is at least the expected amount
        let actual_perm_fee = tx.perm_fee.unwrap_or(BoundedFee::zero());
        if actual_perm_fee < expected_perm_fee.amount {
            return Err(PreValidationError::InsufficientPermFee {
                tx_id: tx.id,
                expected: expected_perm_fee.amount,
                actual: actual_perm_fee.get(),
            });
        }

        // Validate term_fee is at least the expected amount
        let actual_term_fee = tx.term_fee;
        if actual_term_fee < expected_term_fee {
            return Err(PreValidationError::InsufficientTermFee {
                tx_id: tx.id,
                expected: expected_term_fee,
                actual: actual_term_fee.get(),
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

        let number_of_ingress_proofs_total =
            config.number_of_ingress_proofs_total_at(timestamp_secs);
        PublishFeeCharges::new(
            actual_perm_fee,
            actual_term_fee,
            &config.consensus,
            number_of_ingress_proofs_total,
        )
        .map_err(|e| PreValidationError::InvalidPermFeeStructure {
            tx_id: tx.id,
            reason: e.to_string(),
        })?;
        Ok(())
    };

    // Step 4: Validate based on ledger rules
    for (tx, past_inclusion) in txs_to_check.values() {
        match past_inclusion {
            // no past inclusions
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
                        validate_price(tx)?;

                        // Submit ledger transactions should not have ingress proofs, that's why they are in the submit ledger
                        // (they're waiting for proofs to arrive)
                        if tx.promoted_height().is_some() {
                            // TODO: This should be a hard error, but the test infrastructure currently
                            // creates transactions with ingress proofs that get placed in Submit ledger.
                            // This needs to be fixed in the block production logic to properly place
                            // transactions with proofs in the Publish ledger.
                            tracing::error!(
                                "Transaction {} in Submit ledger should not have a promoted_height",
                                tx.id
                            );
                        }

                        debug!("Transaction {} is new in Submit ledger", tx.id);
                    }
                    DataLedger::OneYear => {
                        // TODO some validation
                    }
                    DataLedger::ThirtyDay => {
                        // TODO some validation
                    }
                }
            }
            TxInclusionState::Found {
                ledger_current: (ledger_current, current_block_hash),
                ledger_historical: (ledger_historical, historical_block_hash),
            } => {
                match ledger_current {
                    DataLedger::Publish => {
                        match ledger_historical {
                            DataLedger::Submit => {
                                if current_block_hash == historical_block_hash
                                    && current_block_hash == &block.block_hash
                                {
                                    // tx was included & promoted within the same block
                                    validate_price(tx)?;
                                }

                                // OK: Transaction promoted from past Submit to current Publish
                                debug!(
                        "Transaction {} promoted from past Submit to current Publish ledger",
                        tx.id
                    );
                            }
                            DataLedger::Publish => {
                                return Err(PreValidationError::PublishTxAlreadyIncluded {
                                    tx_id: tx.id,
                                    block_hash: *historical_block_hash,
                                });
                            }
                            _ => {
                                // Unexpected historical ledger for Publish
                                return Err(PreValidationError::InvalidPromotionPath {
                                    tx_id: tx.id,
                                    from: *ledger_historical,
                                    to: DataLedger::Publish,
                                });
                            }
                        }
                    }
                    DataLedger::Submit => {
                        // Submit tx should not have any past inclusion
                        return Err(PreValidationError::SubmitTxAlreadyIncluded {
                            tx_id: tx.id,
                            ledger: *ledger_historical,
                            block_hash: *historical_block_hash,
                        });
                    }
                    DataLedger::OneYear => {
                        // TODO: Validate OneYear term ledger data tx
                        // For now, accept any historical state
                    }
                    DataLedger::ThirtyDay => {
                        // TODO: Validate ThirtyDay term ledger data tx
                        // For now, accept any historical state
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

    // Step 5: Validate all SUBMIT tx (including same-block promotions)
    let all_txs = publish_txs.iter().map(|tx| (tx, DataLedger::Submit));

    // DO NOT INCLUDE ANY LOGIC OTHER THAN PRICING VALIDATION HERE
    // IT ONLY RUNS FOR NON-PROMOTED TXS
    for (tx, current_ledger) in all_txs {
        debug_assert_eq!(current_ledger, DataLedger::Submit);
        // All data transactions must have ledger_id set to Publish
        // TODO: support other term ledgers here
        if tx.ledger_id != DataLedger::Publish as u32 {
            return Err(PreValidationError::InvalidLedgerIdForTx {
                tx_id: tx.id,
                expected: DataLedger::Publish as u32,
                actual: tx.ledger_id,
            });
        }

        // Submit ledger transactions should not have ingress proofs, that's why they are in the submit ledger
        // (they're waiting for proofs to arrive)
        if tx.promoted_height().is_some() {
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

    if publish_txs.is_empty() && publish_ledger.proofs.is_some() {
        let proof_count = publish_ledger.proofs.as_ref().unwrap().len();
        return Err(PreValidationError::PublishLedgerProofCountMismatch {
            proof_count,
            tx_count: publish_txs.len(),
        });
    }

    if let Some(proofs_list) = &publish_ledger.proofs {
        // Compute the expected total number of proofs based on the number of
        // publish_tx and the number of proofs_per_tx
        let expected_proof_count = {
            let total_miners = block_tree_guard
                .read()
                .canonical_epoch_snapshot()
                .commitment_state
                .stake_commitments
                .len();

            // Take the smallest value, the configured proof count or the number
            // of staked miners that can produce a valid proof.
            let timestamp_secs = block.timestamp_secs();
            let number_of_ingress_proofs_total =
                config.number_of_ingress_proofs_total_at(timestamp_secs);
            let proofs_per_tx =
                std::cmp::min(number_of_ingress_proofs_total as usize, total_miners);
            publish_txs.len() * proofs_per_tx
        };

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

            // Validate assigned ingress proofs and get counts
            let (assigned_proofs, assigned_miners) = get_assigned_ingress_proofs(
                &tx_proofs,
                tx_header,
                |hash| mempool_block_retriever(hash, service_senders),
                block_tree_guard,
                db,
                config,
            )
            .await?;

            let timestamp_secs = block.timestamp_secs();
            let mut expected_assigned_proofs =
                config.number_of_ingress_proofs_from_assignees_at(timestamp_secs) as usize;

            // While the protocol can require X number of assigned proofs, if there
            // is less than that many assigned to the slot, it still needs to function.
            if assigned_miners < expected_assigned_proofs {
                warn!("Clamping expected_assigned_proofs from {} to {} to match number of assigned miners ", expected_assigned_proofs, assigned_miners);
                expected_assigned_proofs = assigned_miners;
            }

            if assigned_proofs.len() < expected_assigned_proofs {
                return Err(PreValidationError::AssignedProofCountMismatch {
                    expected: expected_assigned_proofs,
                    actual: assigned_proofs.len(),
                });
            }

            // Enforce data availability by verifying ingress proofs with the actual chunks
            // possible future improvements: refresh peer list on failure of all 5, try peers concurrently
            if config.consensus.enable_full_ingress_proof_validation {
                // Collect all chunks for this transaction from the DB (by tx-relative offset)
                let expected_chunk_count =
                    tx_header.data_size.div_ceil(config.consensus.chunk_size);

                let ro_tx = db.tx().map_err(|e| PreValidationError::DatabaseError {
                    error: e.to_string(),
                })?;

                let mut chunks: Vec<irys_types::ChunkBytes> =
                    Vec::with_capacity(expected_chunk_count as usize);

                let client = reqwest::Client::new();

                // Fetch active peers once outside the chunk loop (take up to 5)
                let api_addrs: Vec<_> = {
                    let (peers_tx, peers_rx) = tokio::sync::oneshot::channel();
                    let _ = service_senders
                        .data_sync
                        .send_traced(crate::DataSyncServiceMessage::GetActivePeersList(peers_tx));

                    match tokio::time::timeout(Duration::from_millis(1000), peers_rx).await {
                        Ok(Ok(active_peers)) => {
                            let guard = active_peers.read().unwrap();
                            guard
                                .iter()
                                .take(5)
                                .map(|(_addr, pbm)| pbm.peer_address.api)
                                .collect()
                        }
                        _ => Vec::new(),
                    }
                };

                for i in 0..expected_chunk_count {
                    let tx_offset_u32 =
                        u32::try_from(i).map_err(|_| PreValidationError::InvalidIngressProof {
                            tx_id: tx_header.id,
                            reason: format!("Tx chunk offset index {} exceeds u32::MAX", i),
                        })?;
                    let tx_chunk_offset = irys_types::TxChunkOffset::from(tx_offset_u32);

                    // Try local cache first
                    let mut maybe_chunk = irys_database::cached_chunk_by_chunk_offset(
                        &ro_tx,
                        tx_header.data_root,
                        tx_chunk_offset,
                    )
                    .map_err(|e| PreValidationError::DatabaseError {
                        error: e.to_string(),
                    })?;

                    // If missing locally, attempt fetch-on-miss from pre-selected peers and ingest
                    if maybe_chunk.is_none() {
                        for api_addr in api_addrs.iter() {
                            // Build data_root/offset fetch URL using peer API address
                            let url = format!(
                                "http://{}/v1/chunk/data-root/{}/{}/{}",
                                api_addr,
                                publish_ledger.ledger_id,
                                tx_header.data_root,
                                tx_offset_u32
                            );

                            // Fetch with short timeout
                            let resp = tokio::time::timeout(
                                Duration::from_millis(500),
                                client.get(&url).send(),
                            )
                            .await;

                            let Ok(Ok(resp)) = resp else {
                                continue;
                            };
                            if !resp.status().is_success() {
                                continue;
                            }

                            // Parse ChunkFormat and convert to UnpackedChunk
                            let Ok(chunk_format) = resp.json::<irys_types::ChunkFormat>().await
                            else {
                                continue;
                            };

                            let unpacked = match chunk_format {
                                irys_types::ChunkFormat::Unpacked(u) => u,
                                irys_types::ChunkFormat::Packed(p) => irys_packing::unpack(
                                    &p,
                                    config.consensus.entropy_packing_iterations,
                                    config.consensus.chunk_size as usize,
                                    config.consensus.chain_id,
                                ),
                            };

                            // Basic sanity checks before ingest
                            if unpacked.data_root != tx_header.data_root
                                || *unpacked.tx_offset != tx_offset_u32
                            {
                                continue;
                            }

                            // Ingest via chunk ingress service to persist and validate
                            let (ing_tx, ing_rx) = tokio::sync::oneshot::channel();
                            if service_senders
                                .chunk_ingress
                                .send_traced(
                                    crate::chunk_ingress_service::ChunkIngressMessage::IngestChunk(
                                        unpacked,
                                        Some(ing_tx),
                                    ),
                                )
                                .is_err()
                            {
                                return Err(PreValidationError::ValidationServiceUnreachable);
                            }

                            // Wait briefly for ingest to complete and log outcome
                            let recv_res =
                                tokio::time::timeout(std::time::Duration::from_millis(500), ing_rx)
                                    .await;
                            match recv_res {
                                Err(_elapsed) => {
                                    tracing::warn!(
                                        "Timed out waiting for chunk ingest completion for data_root {:?}, tx_offset {}",
                                        tx_header.data_root,
                                        tx_offset_u32
                                    );
                                }
                                Ok(Err(recv_err)) => {
                                    tracing::warn!(
                                        "IngestChunk oneshot channel error for data_root {:?}, tx_offset {}: {:?}",
                                        tx_header.data_root,
                                        tx_offset_u32,
                                        recv_err
                                    );
                                }
                                Ok(Ok(ingest_res)) => match ingest_res {
                                    Ok(()) => {
                                        tracing::debug!(
                                                "Chunk ingested successfully for data_root {:?}, tx_offset {}",
                                                tx_header.data_root,
                                                tx_offset_u32
                                            );
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                                "IngestChunk returned error for data_root {:?}, tx_offset {}: {:?}",
                                                tx_header.data_root,
                                                tx_offset_u32,
                                                e
                                            );
                                    }
                                },
                            }

                            // Re-open a fresh read tx to observe the write
                            let ro_tx2 =
                                db.tx().map_err(|e| PreValidationError::DatabaseError {
                                    error: e.to_string(),
                                })?;
                            maybe_chunk = irys_database::cached_chunk_by_chunk_offset(
                                &ro_tx2,
                                tx_header.data_root,
                                tx_chunk_offset,
                            )
                            .map_err(|e| {
                                PreValidationError::DatabaseError {
                                    error: format!(
                                        "Failed to fetch chunk for tx {} (data_root: {:?}, chunk_offset: {}): {}",
                                        tx_header.id, tx_header.data_root, tx_chunk_offset, e
                                    ),
                                }
                            })?;

                            if maybe_chunk.is_some() {
                                break;
                            }
                        }
                    }

                    let (_meta, cached_chunk) =
                        maybe_chunk.ok_or_else(|| PreValidationError::InvalidIngressProof {
                            tx_id: tx_header.id,
                            reason: format!(
                                "Data unavailable: missing chunk at offset {} for data_root {:?}",
                                i, tx_header.data_root
                            ),
                        })?;

                    let chunk_bytes = cached_chunk.chunk.ok_or_else(|| {
                        PreValidationError::InvalidIngressProof {
                            tx_id: tx_header.id,
                            reason: "Missing chunk body bytes".to_string(),
                        }
                    })?;
                    chunks.push(chunk_bytes.0);
                }

                // Verify each ingress proof against the actual chunks
                for proof in tx_proofs.iter() {
                    let ok = irys_types::ingress::verify_ingress_proof(
                        proof,
                        &chunks,
                        config.consensus.chain_id,
                    )
                    .map_err(|e| PreValidationError::InvalidIngressProof {
                        tx_id: tx_header.id,
                        reason: e.to_string(),
                    })?;

                    if !ok {
                        return Err(PreValidationError::IngressProofMismatch {
                            tx_id: tx_header.id,
                        });
                    }
                }
            }

            let timestamp_secs = block.timestamp_secs();
            let number_of_ingress_proofs_total =
                config.number_of_ingress_proofs_total_at(timestamp_secs);
            if tx_proofs.len() != number_of_ingress_proofs_total as usize {
                return Err(PreValidationError::IngressProofCountMismatch {
                    expected: number_of_ingress_proofs_total as usize,
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

/// Validates that all ingress proof signers are unique and staked for each transaction in the Publish ledger
fn validate_ingress_proof_signers(
    block: &IrysBlockHeader,
    parent_epoch_snapshot: &EpochSnapshot,
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

    // For each transaction in the publish ledger, validate signers are unique and staked
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

            // Validate that the signer is staked in the parent epoch snapshot
            if !parent_epoch_snapshot.is_staked(signer) {
                return Err(PreValidationError::UnstakedIngressProofSigner {
                    tx_id: *tx_id,
                    signer,
                });
            }

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
    // Track block hashes for both current and historical ledgers
    // so we can emit precise errors and diagnostics.
    // ledger_current.1 is the hash of the block under validation.
    // ledger_historical.1 is the hash of the block where the prior inclusion was found.
    Found {
        ledger_current: (DataLedger, BlockHash),
        ledger_historical: (DataLedger, BlockHash),
    },
    Duplicate {
        ledger_historical: (DataLedger, BlockHash),
    },
}

#[tracing::instrument(level = "trace", skip_all, fields(block.hash = ?block_under_validation.block_hash))]
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
        .block_tree
        .send_traced(BlockTreeServiceMessage::GetBlockTreeReadGuard { response: tx })?;
    let block_tree_guard = rx.await?;
    let block_tree_guard = block_tree_guard.read();

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
            process_block_ledgers_with_states(
                &header.data_ledgers,
                header.block_hash,
                block_under_validation.block_hash,
                tx_ids,
            )
        };
        // Move to the parent block and continue the traversal backwards
        block = match block_tree_guard.get_block(&block.0) {
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
    historical_block_hash: BlockHash,
    current_block_hash: BlockHash,
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
                            ledger_current: (*ledger_current, current_block_hash),
                            ledger_historical: (ledger_type, historical_block_hash),
                        };
                    }
                    TxInclusionState::Found { .. } => {
                        // Transaction already found once, this is a duplicate
                        *state = TxInclusionState::Duplicate {
                            ledger_historical: (ledger_type, historical_block_hash),
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

async fn mempool_block_retriever(
    hash: H256,
    service_senders: &ServiceSenders,
) -> Option<IrysBlockHeader> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    service_senders
        .mempool
        .send_traced(MempoolServiceMessage::GetBlockHeader(hash, false, tx))
        .expect("MempoolServiceMessage should be delivered");
    rx.await.expect("mempool service message should succeed")
}

pub async fn get_assigned_ingress_proofs<F, Fut>(
    tx_proofs: &[IngressProof],
    tx_header: &DataTransactionHeader,
    mempool_block_retriever: F,
    block_tree_guard: &BlockTreeReadGuard,
    db: &DatabaseProvider,
    config: &Config,
) -> Result<(Vec<IngressProof>, usize), PreValidationError>
where
    F: Fn(H256) -> Fut + Clone, // Changed to Fn and added Clone
    Fut: Future<Output = Option<IrysBlockHeader>>,
{
    // Returns (assigned_proofs, assigned_miners)
    let mut assigned_proofs = Vec::new();
    let mut assigned_miners = 0;

    // Loop through all the ingress proofs for the published transaction and pre-validate them
    for ingress_proof in tx_proofs.iter() {
        // Validate ingress proof signature and data_root match the transaction
        let proof_address = ingress_proof
            .pre_validate(&tx_header.data_root)
            .map_err(|e| PreValidationError::InvalidIngressProof {
                tx_id: tx_header.id,
                reason: e.to_string(),
            })?;

        // 1.) is the proof from a miner assigned to store the data in the submit ledger?

        //  a) Get the block hashes from the cached data_root
        let block_hashes = db
            .view(|tx| cached_data_root_by_data_root(tx, tx_header.data_root))
            .expect("creating a read tx should succeed")
            .expect("db query should succeed")
            .unwrap_or_else(|| {
                panic!(
                    "CachedDataRoot should be found for data_root {} (tx_id {})",
                    tx_header.data_root, tx_header.id
                )
            })
            .block_set;

        //  b) Get the submit ledger offset intervals for each of the blocks
        let mut block_ranges = Vec::new();
        for block_hash in block_hashes.iter() {
            if let Some(block_range) =
                get_ledger_range(block_hash, mempool_block_retriever.clone(), db).await
            {
                block_ranges.push(block_range);
            }
        }

        //  c) Get the slots the proof address is assigned to store
        let slot_indexes = get_submit_ledger_slot_assignments(&proof_address, block_tree_guard);

        // d) Get the ledger ranges of the slot indexes
        let slot_ranges: HashMap<usize, LedgerChunkRange> = slot_indexes
            .iter()
            .map(|index| {
                let num_chunks_in_partition = config.consensus.num_chunks_in_partition;
                let start = *index as u64 * num_chunks_in_partition;
                let end = start + num_chunks_in_partition;
                let range = LedgerChunkRange(ie(
                    LedgerChunkOffset::from(start),
                    LedgerChunkOffset::from(end),
                ));
                (*index, range)
            })
            .collect();

        // e) Get the number of unique addresses assigned to each slot
        let slot_address_counts = get_submit_ledger_slot_addresses(&slot_indexes, block_tree_guard);

        //  f) are there any intersections of block and slot ranges?
        let mut is_intersected = false;
        for block_range in &block_ranges {
            for (slot_index, slot_range) in &slot_ranges {
                if block_range.intersection(slot_range).is_some() {
                    is_intersected = true;
                    assigned_miners = *slot_address_counts.get(slot_index).unwrap();
                    break;
                }
            }
            if is_intersected {
                assigned_proofs.push(ingress_proof.clone());
                break;
            }
        }
    }

    Ok((assigned_proofs, assigned_miners))
}

async fn get_ledger_range<F, Fut>(
    hash: &H256,
    mempool_block_retriever: F,
    db: &DatabaseProvider,
) -> Option<LedgerChunkRange>
where
    F: Fn(H256) -> Fut + Clone, // Changed to Fn and added Clone
    Fut: Future<Output = Option<IrysBlockHeader>>,
{
    let block = get_block_by_hash(hash, mempool_block_retriever.clone(), db).await?;
    let prev_block_hash = block.previous_block_hash;

    if block.height == 0 {
        Some(LedgerChunkRange(ii(
            LedgerChunkOffset::from(0),
            LedgerChunkOffset::from(block.data_ledgers[DataLedger::Submit].total_chunks - 1),
        )))
    } else {
        let prev_block = get_block_by_hash(&prev_block_hash, mempool_block_retriever, db).await?;
        Some(LedgerChunkRange(ii(
            LedgerChunkOffset::from(prev_block.data_ledgers[DataLedger::Submit].total_chunks),
            LedgerChunkOffset::from(block.data_ledgers[DataLedger::Submit].total_chunks - 1),
        )))
    }
}

async fn get_block_by_hash<F, Fut>(
    hash: &H256,
    mempool_block_retriever: F,
    db: &DatabaseProvider,
) -> Option<IrysBlockHeader>
where
    F: FnOnce(H256) -> Fut, // This can stay FnOnce since it's only called once per invocation
    Fut: Future<Output = Option<IrysBlockHeader>>,
{
    let block = mempool_block_retriever(*hash).await;

    // Return the block if we found it in the mempool, otherwise get it from the db
    if let Some(block) = block {
        Some(block)
    } else {
        db.view(|tx| block_header_by_hash(tx, hash, false))
            .expect("creating a read tx should succeed")
            .expect("creating a read tx should succeed")
    }
}

fn get_submit_ledger_slot_assignments(
    address: &IrysAddress,
    block_tree_guard: &BlockTreeReadGuard,
) -> Vec<usize> {
    let epoch_snapshot = block_tree_guard.read().canonical_epoch_snapshot();
    let mut partition_assignments = epoch_snapshot.get_partition_assignments(*address);
    partition_assignments.retain(|pa| pa.ledger_id == Some(DataLedger::Submit.into()));
    partition_assignments
        .iter()
        .map(|pa| pa.slot_index.unwrap())
        .collect()
}

fn get_submit_ledger_slot_addresses(
    slot_indexes: &Vec<usize>,
    block_tree_guard: &BlockTreeReadGuard,
) -> HashMap<usize, usize> {
    let epoch_snapshot = block_tree_guard.read().canonical_epoch_snapshot();

    let mut num_addresses_per_slot: HashMap<usize, usize> = HashMap::new();

    for slot_index in slot_indexes {
        let num_addresses = epoch_snapshot
            .partition_assignments
            .data_partitions
            .iter()
            .filter(|(_hash, pa)| {
                pa.ledger_id == Some(DataLedger::Submit.into())
                    && pa.slot_index == Some(*slot_index)
            })
            .count();

        num_addresses_per_slot.insert(*slot_index, num_addresses);
    }

    num_addresses_per_slot
}

#[cfg(test)]
mod tests {
    use super::*;

    use irys_config::StorageSubmodulesConfig;
    use irys_database::add_genesis_commitments;
    use irys_domain::{block_index_guard::BlockIndexReadGuard, BlockIndex, EpochSnapshot};
    use irys_testing_utils::utils::temporary_directory;
    use irys_types::{
        hash_sha256, irys::IrysSigner, partition::PartitionAssignment, Base64, BlockHash,
        DataTransaction, DataTransactionHeader, DataTransactionLedger, H256List, IrysAddress,
        IrysBlockHeaderV1, NodeConfig, Signature, H256, U256,
    };
    use std::sync::Arc;
    use tempfile::TempDir;
    use tracing::{debug, info};

    pub(super) struct TestContext {
        pub block_index: BlockIndex,
        pub miner_address: IrysAddress,
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
        let config = Config::new_with_random_peer_id(node_config);

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
        let db_env = irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db(
            &data_dir.path().to_path_buf(),
        )
        .expect("to create DB");
        let db = irys_types::DatabaseProvider(Arc::new(db_env));
        let block_index = BlockIndex::new_for_testing(db);

        let storage_submodules_config =
            StorageSubmodulesConfig::load(config.node_config.base_directory.clone())
                .expect("Expected to load storage submodules config");

        // Create an epoch snapshot for the genesis block
        let epoch_snapshot = EpochSnapshot::new(
            &storage_submodules_config,
            genesis_block,
            commitments,
            &config,
        );
        info!("Genesis Epoch tasks complete.");

        let partition_hash = epoch_snapshot.ledgers.get_slots(DataLedger::Submit)[0].partitions[0];

        let genesis_sealed =
            SealedBlock::new_unchecked(Arc::clone(&arc_genesis), BlockTransactions::default());
        block_index
            .db()
            .update_eyre(|tx| BlockIndex::push_block(tx, &genesis_sealed, chunk_size))
            .expect("Failed to index genesis block");

        let partition_assignment = epoch_snapshot
            .get_data_partition_assignment(partition_hash)
            .expect("Expected to get partition assignment");

        debug!("Partition assignment {:?}", partition_assignment);

        (
            data_dir,
            TestContext {
                block_index,
                miner_address,
                epoch_snapshot,
                partition_hash,
                partition_assignment,
                consensus_config,
                node_config,
            },
        )
    }

    #[tokio::test]
    async fn poa_test_3_complete_txs() {
        let (_tmp, mut context) = init().await;
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

        let chunk_size = context.consensus_config.chunk_size as usize;
        for poa_tx_num in 0..3 {
            for poa_chunk_num in 0..3 {
                let mut poa_chunk: Vec<u8> = data_chunks[poa_tx_num][poa_chunk_num].into();
                poa_test(
                    &mut context,
                    &txs,
                    &mut poa_chunk,
                    poa_tx_num,
                    poa_chunk_num,
                    9,
                    chunk_size,
                );
            }
        }
    }

    #[tokio::test]
    async fn poa_not_complete_last_chunk_test() {
        let (_tmp, mut context) = init().await;

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
                &mut context,
                &txs,
                &mut poa_chunk,
                poa_tx_num,
                poa_chunk_num,
                2,
                chunk_size,
            );
        }
    }

    #[tokio::test]
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
        // reset frequency - this makes the previously calculated seeds invalid
        let large_reset_frequency = 100;
        let is_valid = is_seed_data_valid(&header_2, &parent_header, large_reset_frequency);
        assert!(
            matches!(
                is_valid,
                ValidationResult::Invalid(ValidationError::SeedDataInvalid(_))
            ),
            "Seed data should be invalid due to wrong reset frequency"
        );

        // Now let's try to set some random seeds that are not valid
        header_2.vdf_limiter_info.seed = BlockHash::from_slice(&[5; 32]);
        header_2.vdf_limiter_info.next_seed = BlockHash::from_slice(&[6; 32]);
        let is_valid = is_seed_data_valid(&header_2, &parent_header, reset_frequency);

        assert!(
            matches!(
                is_valid,
                ValidationResult::Invalid(ValidationError::SeedDataInvalid(_))
            ),
            "Seed data should be invalid with random seeds"
        );
    }

    fn poa_test(
        context: &mut TestContext,
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
        let height = context.block_index.num_blocks();

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
            ledger_id: Some(DataLedger::Submit.into()),
            partition_chunk_offset: (poa_tx_num * 3 /* 3 chunks in each tx */ + poa_chunk_num)
                .try_into()
                .expect("Value exceeds u32::MAX"),

            partition_hash: context.partition_hash,
        };

        // Create a block from the tx
        let irys_block = IrysBlockHeader::V1(IrysBlockHeaderV1 {
            height,
            reward_address: context.miner_address,
            poa: poa.clone(),
            block_hash: H256::zero(),
            previous_block_hash: H256::zero(),
            previous_cumulative_diff: U256::from(4000),
            miner_address: context.miner_address,
            signature: Signature::test_signature().into(),
            timestamp: UnixTimestampMs::from_millis(1000),
            data_ledgers: vec![
                // Permanent Publish Ledger
                DataTransactionLedger {
                    ledger_id: DataLedger::Publish.into(),
                    tx_root: H256::zero(),
                    tx_ids: H256List(Vec::new()),
                    total_chunks: 0,
                    expires: None,
                    proofs: None,
                    required_proof_count: Some(1),
                },
                // Term Submit Ledger
                DataTransactionLedger {
                    ledger_id: DataLedger::Submit.into(),
                    tx_root,
                    tx_ids: H256List(data_tx_ids),
                    total_chunks: 9,
                    expires: Some(1622543200),
                    proofs: None,
                    required_proof_count: None,
                },
            ],
            ..IrysBlockHeaderV1::default()
        });

        // Migrate block into the block index
        let block_txs = BlockTransactions {
            data_txs: HashMap::from([(DataLedger::Submit, tx_headers)]),
            ..Default::default()
        };
        let sealed = SealedBlock::new_unchecked(Arc::new(irys_block), block_txs);
        context
            .block_index
            .db()
            .update_eyre(|tx| {
                BlockIndex::push_block(tx, &sealed, context.consensus_config.chunk_size)
            })
            .expect("Failed to index second block");

        let block_index_guard = BlockIndexReadGuard::new(context.block_index.clone());

        let ledger_chunk_offset = context
            .partition_assignment
            .slot_index
            .expect("Expected to have a slot index in the assignment")
            as u64
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
            .get_block_bounds(
                DataLedger::Submit,
                LedgerChunkOffset::from(ledger_chunk_offset),
            )
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

    #[tokio::test]
    async fn poa_does_not_allow_modified_leaves() {
        let (_tmp, mut context) = init().await;
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

        let chunk_size = context.consensus_config.chunk_size as usize;
        for poa_tx_num in 0..3 {
            for poa_chunk_num in 0..3 {
                let mut poa_chunk: Vec<u8> = data_chunks[poa_tx_num][poa_chunk_num].into();
                test_poa_with_malicious_merkle_data(
                    &mut context,
                    &txs,
                    &mut poa_chunk,
                    poa_tx_num,
                    poa_chunk_num,
                    9,
                    chunk_size,
                );
            }
        }
    }

    fn test_poa_with_malicious_merkle_data(
        context: &mut TestContext,
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
        let height = context.block_index.num_blocks();

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
            ledger_id: Some(DataLedger::Submit.into()),
            partition_chunk_offset: (poa_tx_num * 3 /* 3 chunks in each tx */ + poa_chunk_num)
                .try_into()
                .expect("Value exceeds u32::MAX"),

            partition_hash: context.partition_hash,
        };

        // Create a block from the tx
        let irys_block = IrysBlockHeader::V1(IrysBlockHeaderV1 {
            height,
            reward_address: context.miner_address,
            poa: poa.clone(),
            block_hash: H256::zero(),
            previous_block_hash: H256::zero(),
            previous_cumulative_diff: U256::from(4000),
            miner_address: context.miner_address,
            signature: Signature::test_signature().into(),
            timestamp: UnixTimestampMs::from_millis(1000),
            data_ledgers: vec![
                // Permanent Publish Ledger
                DataTransactionLedger {
                    ledger_id: DataLedger::Publish.into(),
                    tx_root: H256::zero(),
                    tx_ids: H256List(Vec::new()),
                    total_chunks: 0,
                    expires: None,
                    proofs: None,
                    required_proof_count: Some(1),
                },
                // Term Submit Ledger
                DataTransactionLedger {
                    ledger_id: DataLedger::Submit.into(),
                    tx_root,
                    tx_ids: H256List(data_tx_ids),
                    total_chunks: 9,
                    expires: Some(1622543200),
                    proofs: None,
                    required_proof_count: None,
                },
            ],
            ..IrysBlockHeaderV1::default()
        });

        // Migrate block into the block index
        let block_txs = BlockTransactions {
            data_txs: HashMap::from([(DataLedger::Submit, tx_headers)]),
            ..Default::default()
        };
        let sealed = SealedBlock::new_unchecked(Arc::new(irys_block), block_txs);
        context
            .block_index
            .db()
            .update_eyre(|tx| {
                BlockIndex::push_block(tx, &sealed, context.consensus_config.chunk_size)
            })
            .expect("Failed to index second block");

        let block_index_guard = BlockIndexReadGuard::new(context.block_index.clone());

        let ledger_chunk_offset = context
            .partition_assignment
            .slot_index
            .expect("Expected to get slot index") as u64
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
            .get_block_bounds(
                DataLedger::Submit,
                LedgerChunkOffset::from(ledger_chunk_offset),
            )
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
        prev.last_diff_timestamp = UnixTimestampMs::from_millis(1000);

        let mut block = IrysBlockHeader::new_mock_header();
        block.height = 2;
        block.timestamp = UnixTimestampMs::from_millis(1500);
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
        prev.last_diff_timestamp = UnixTimestampMs::from_millis(1000);

        let mut block = IrysBlockHeader::new_mock_header();
        block.height = 10;
        block.timestamp = UnixTimestampMs::from_millis(2000);
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
        prev.last_diff_timestamp = UnixTimestampMs::from_millis(1000);

        let mut block = IrysBlockHeader::new_mock_header();
        block.height = 2;
        block.timestamp = UnixTimestampMs::from_millis(1500);
        block.last_diff_timestamp = UnixTimestampMs::from_millis(999);

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

#[cfg(test)]
mod commitment_version_tests {
    use super::*;
    use irys_types::{
        hardfork_config::{Aurora, FrontierParams, IrysHardforkConfig},
        CommitmentTransactionV1, CommitmentTransactionV2, CommitmentTypeV1, CommitmentTypeV2,
    };
    use proptest::prelude::*;
    use rstest::rstest;

    fn config_with_aurora(activation_secs: u64, min_version: u8) -> ConsensusConfig {
        ConsensusConfig {
            hardforks: IrysHardforkConfig {
                frontier: FrontierParams {
                    number_of_ingress_proofs_total: 1,
                    number_of_ingress_proofs_from_assignees: 0,
                },
                next_name_tbd: None,
                aurora: Some(Aurora {
                    activation_timestamp: UnixTimestamp::from_secs(activation_secs),
                    minimum_commitment_tx_version: min_version,
                }),
                borealis: None,
            },
            ..ConsensusConfig::testing()
        }
    }

    fn config_without_aurora() -> ConsensusConfig {
        ConsensusConfig {
            hardforks: IrysHardforkConfig {
                frontier: FrontierParams {
                    number_of_ingress_proofs_total: 1,
                    number_of_ingress_proofs_from_assignees: 0,
                },
                next_name_tbd: None,
                aurora: None,
                borealis: None,
            },
            ..ConsensusConfig::testing()
        }
    }

    fn make_v1_commitment(consensus: &ConsensusConfig) -> CommitmentTransaction {
        CommitmentTransaction::V1(irys_types::CommitmentV1WithMetadata {
            tx: CommitmentTransactionV1 {
                commitment_type: CommitmentTypeV1::Stake,
                ..CommitmentTransactionV1::new(consensus)
            },
            metadata: Default::default(),
        })
    }

    fn make_v2_commitment(consensus: &ConsensusConfig) -> CommitmentTransaction {
        CommitmentTransaction::V2(irys_types::CommitmentV2WithMetadata {
            tx: CommitmentTransactionV2 {
                commitment_type: CommitmentTypeV2::Stake,
                ..CommitmentTransactionV2::new(consensus)
            },
            metadata: Default::default(),
        })
    }

    #[rstest]
    #[case::no_hardfork_v1_valid(None, 999, 1, true)]
    #[case::no_hardfork_v2_valid(None, 999, 2, true)]
    #[case::before_activation_v1_valid(Some(1000), 999, 1, true)]
    #[case::before_activation_v2_valid(Some(1000), 999, 2, true)]
    #[case::at_activation_v1_invalid(Some(1000), 1000, 1, false)]
    #[case::at_activation_v2_valid(Some(1000), 1000, 2, true)]
    #[case::after_activation_v1_invalid(Some(1000), 1001, 1, false)]
    #[case::after_activation_v2_valid(Some(1000), 1001, 2, true)]
    fn version_validation_scenarios(
        #[case] activation_secs: Option<u64>,
        #[case] timestamp_secs: u64,
        #[case] tx_version: u8,
        #[case] expect_valid: bool,
    ) {
        let config = match activation_secs {
            Some(ts) => config_with_aurora(ts, 2),
            None => config_without_aurora(),
        };

        let tx = match tx_version {
            1 => make_v1_commitment(&config),
            _ => make_v2_commitment(&config),
        };
        let txs = vec![tx];

        let result = find_invalid_commitment_version(
            &config,
            &txs,
            UnixTimestamp::from_secs(timestamp_secs),
        );

        if expect_valid {
            assert!(result.is_none(), "Expected valid, got invalid");
        } else {
            assert!(result.is_some(), "Expected invalid, got valid");
            let (_, _, version, minimum) = result.unwrap();
            assert_eq!(version, tx_version);
            assert_eq!(minimum, 2);
        }
    }

    #[test]
    fn mixed_versions_returns_first_invalid() {
        let config = config_with_aurora(1000, 2);
        let v2_tx = make_v2_commitment(&config);
        let v1_tx = make_v1_commitment(&config);

        // V2 first, then V1 - should return position 1 (the V1)
        let txs = vec![v2_tx, v1_tx.clone()];
        let result = find_invalid_commitment_version(&config, &txs, UnixTimestamp::from_secs(1001));

        assert!(result.is_some());
        let (tx_id, position, version, _) = result.unwrap();
        assert_eq!(tx_id, v1_tx.id());
        assert_eq!(position, 1);
        assert_eq!(version, 1);
    }

    #[test]
    fn empty_list_returns_none() {
        let config = config_with_aurora(1000, 2);
        let txs: Vec<CommitmentTransaction> = vec![];

        let result = find_invalid_commitment_version(&config, &txs, UnixTimestamp::from_secs(1001));
        assert!(result.is_none());
    }

    proptest! {
        #[test]
        fn version_validation_property(
            activation_ts in 1000_u64..u64::MAX / 2,
            time_offset in 0_i64..2000_i64,
            tx_version in 1_u8..=3_u8,
        ) {
            let query_ts = if time_offset >= 0 {
                activation_ts.saturating_add(time_offset as u64)
            } else {
                activation_ts.saturating_sub(time_offset.unsigned_abs())
            };

            let config = config_with_aurora(activation_ts, 2);
            let tx = match tx_version {
                1 => make_v1_commitment(&config),
                _ => make_v2_commitment(&config),
            };
            let txs = vec![tx];

            let result = find_invalid_commitment_version(
                &config,
                &txs,
                UnixTimestamp::from_secs(query_ts),
            );

            let is_active = query_ts >= activation_ts;
            let should_be_valid = !is_active || tx_version >= 2;

            if should_be_valid {
                prop_assert!(result.is_none());
            } else {
                prop_assert!(result.is_some());
            }
        }
    }
}
