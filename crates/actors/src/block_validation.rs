use crate::block_tree_service::{BlockTreeServiceMessage, ValidationResult};
use crate::{
    block_producer::ledger_expiry,
    mempool_guard::MempoolReadGuard,
    services::ServiceSenders,
    shadow_tx_generator::{PublishLedgerWithTxs, ShadowTxGenerator},
};
use alloy_eips::eip7685::{Requests, RequestsOrHash};
use alloy_rpc_types_engine::ExecutionData;
use eyre::ensure;
use irys_database::{cached_data_root_by_data_root, tx_header_by_txid_canonical};
use irys_domain::{
    BlockBounds, BlockIndex, BlockIndexReadGuard, BlockTreeReadGuard, CommitmentSnapshot,
    CommitmentSnapshotStatus, EmaSnapshot, EpochSnapshot, HardforkConfigExt as _,
};
use irys_packing::{capacity_single::compute_entropy_chunk, xor_vec_u8_arrays_in_place};
use irys_reth::shadow_tx::{ShadowTransaction, ShadowTxError, detect_and_decode};
use irys_reth_node_bridge::IrysRethNodeAdapter;
use irys_reward_curve::HalvingCurve;
use irys_storage::{ie, ii};
use irys_types::storage_pricing::phantoms::{Irys, NetworkFee};
use irys_types::storage_pricing::{Amount, calculate_perm_fee_from_config};
use irys_types::{BlockHash, EvmBlockHash, LedgerChunkRange};
use irys_types::{BlockTransactions, UnixTimestampMs};
use irys_types::{
    BoundedFee, CommitmentTransaction, Config, ConsensusConfig, DataLedger, DataTransactionHeader,
    DataTransactionLedger, DifficultyAdjustmentConfig, H256, IrysAddress, IrysBlockHeader, PoaData,
    SealedBlock, SendTraced as _, SystemLedger, U256, UnixTimestamp,
    app_state::DatabaseProvider,
    calculate_difficulty, next_cumulative_diff,
    transaction::fee_distribution::{PublishFeeCharges, TermFeeCharges},
    validate_path,
};
use irys_types::{CommitmentTypeV2, IrysTransactionCommon, VersionDiscriminant as _};
use irys_types::{IngressProof, LedgerChunkOffset, get_ingress_proofs};
use irys_types::{IrysTransactionId, u256_from_le_bytes as hash_to_number};
use irys_vdf::last_step_checkpoints_is_valid;
use irys_vdf::state::VdfStateReadonly;
use itertools::*;
use nodit::InclusiveInterval as _;
use openssl::sha;
use rayon::prelude::*;
use reth::revm::primitives::{Address, FixedBytes};
use reth::rpc::api::EngineApiClient as _;
use reth::rpc::types::engine::ExecutionPayload;
use reth_db::Database as _;
use reth_ethereum_primitives::Block;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use tracing::{Instrument as _, debug, error, info, warn};

/// Classification of an error variant's failure mode. Drives whether the
/// block is removed from cache as a consensus rejection (`Consensus`),
/// parked as soft-internal pending passive prune+re-gossip recovery
/// (`SoftInternal`), or whether the node must abort + restart
/// (`NodeFault`).
///
/// SAFETY-CRITICAL: see the enum-level safety doc on `PreValidationError`
/// / `ValidationError`. Misclassification can attribute a local failure to
/// the peer (or vice versa) and corrupt consensus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// Strict subset of internal: local fault, retry will hit the same
    /// fault. Must abort + supervisor restart.
    NodeFault,
    /// Local condition (eviction race, missing snapshot), recoverable
    /// via passive depth-prune + fresh gossip re-entering the prevalidate
    /// path. Block stays in cache.
    SoftInternal,
    /// Peer's block is genuinely invalid. Discard the block and (where
    /// applicable) the peer.
    Consensus,
}

impl ErrorClass {
    pub fn is_node_fault(self) -> bool {
        matches!(self, Self::NodeFault)
    }
    pub fn is_internal_failure(self) -> bool {
        !matches!(self, Self::Consensus)
    }
}

/// SAFETY-CRITICAL: variants in this enum are returned from `prevalidate_block`
/// and used by callers to decide whether a block is consensus-invalid. Local
/// or runtime failures (I/O, task join errors, lock contention, transient
/// service unavailability, etc.) MUST NEVER be mapped to a variant that
/// describes a consensus-level rejection (`VDFCheckpointsInvalid`,
/// `BlockSignatureInvalid`, `InvalidTransactionSignature`, etc.). Marking a
/// valid block as invalid — or an invalid block as valid — by routing through
/// the wrong variant is catastrophic. When in doubt, add a distinct
/// internal/runtime variant (e.g. `InternalTaskJoin`) rather than reusing a
/// validation variant.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PreValidationError {
    /// Local lookup failure during the PoA-anchored `block_bounds` binary
    /// search. Construction sites in `get_block_bounds` /
    /// `get_block_bounds_at_height` pre-check peer-supplied offsets against
    /// the chain max and reject out-of-range / inactive-ledger cases first
    /// (`PoAChunkOffsetOutOfBlockBounds`, `PoALedgerInactive`); by the time
    /// this variant is constructed the failure is a local-index inconsistency
    /// (empty index, MDBX I/O, missing predecessor that should be present).
    /// Classified as `is_node_fault` — retry will hit the same broken state.
    ///
    /// MERGE-PAIR(C1): the `is_node_fault` classification is contingent on
    /// every construction site genuinely being a local-index inconsistency.
    /// The `get_block_bounds` walk-off-tree fallback (see the
    /// `MERGE-BLOCKER(C1)` marker in `get_block_bounds`) currently
    /// constructs this variant from a peer-controllable side-fork lookup —
    /// which means a remote peer can drive a `NodeFault` panic. When the
    /// `fix-cdr-block-set` merge replaces that fallback with a canonical
    /// lookup that returns `Ok(None)` for off-lineage, re-audit ALL
    /// remaining construction sites here: if any are still peer-
    /// controllable, demote the classification or split the variant.
    #[error("Failed to get block bounds: {0}")]
    BlockBoundsLookupError(String),

    /// Soft sibling of `BlockBoundsLookupError` for the
    /// `get_assigned_ingress_proofs` walk over `CachedDataRoots.block_set`.
    /// That set is explicitly fork-spanning (see the doc on
    /// `get_assigned_ingress_proofs`), accumulating every block hash —
    /// across all observed forks — that referenced a given `data_root`. If a
    /// side-fork block referenced in the set is later pruned from both
    /// `block_tree` and the database, `get_ledger_range` returns `Ok(None)`
    /// and this variant is surfaced. The peer's block is innocent — this is
    /// a fork-determinism gap in `block_set`, not a node fault. Classified
    /// as `is_internal_failure` (block parks in cache, retry plausible) and
    /// explicitly NOT a node fault.
    ///
    /// `Err(_)` from `get_ledger_range` is a local DB fault, NOT this
    /// variant — it routes through `BlockBoundsLookupError` (node fault).
    ///
    /// MERGE-NOTE(C2): this variant exists to soften the fork-spanning
    /// `CachedDataRoots.block_set` walk. Branch `fix-cdr-block-set`
    /// (4e21e25a) replaces that walk with
    /// `tx_inclusion::find_canonical_ledger_range`, which is
    /// parent-anchored + `ChainState::Onchain`-filtered and cannot
    /// surface a "pruned side-fork hash" outcome. Additional commits
    /// in that branch (7479e10e, ed27fada, c8e79e48, 9b64d7ab) demote
    /// `block_set` to a non-trusted hint with authoritative migration
    /// writes. When merging:
    ///   1. Verify this variant has no remaining construction site
    ///      (the only caller — `get_assigned_ingress_proofs` —
    ///      should now use `find_canonical_ledger_range` instead).
    ///   2. If no construction sites remain, DELETE this variant
    ///      along with its classification entry in
    ///      `PreValidationError::classify()` and the
    ///      `metric_label`/`metric_reason` tags.
    ///   3. Also re-evaluate the `H1` livelock concern (audit
    ///      2026-05-20): closed by the same merge because `block_set`
    ///      no longer drives consensus.
    #[error(
        "Assigned-proof block {block_hash} for tx {tx_id} no longer resolvable in block_tree or DB (likely pruned side fork)"
    )]
    AssignedProofBlockMissing { block_hash: H256, tx_id: H256 },
    #[error("block signature is not valid")]
    BlockSignatureInvalid,
    #[error("Cascade hardfork not configured but term ledger tx {tx_id} was found")]
    CascadeNotConfigured { tx_id: H256 },
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
    #[error(
        "Invalid promotion, transaction {txid:?} data size {got:?} does not match confirmed data root size {expected:?}"
    )]
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
    #[error(
        "PoA chunk hash mismatch: expected {expected:?}, got {got:?}, ledger_id={ledger_id:?}, ledger_chunk_offset={ledger_chunk_offset:?}"
    )]
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
    /// Peer-supplied PoA references a ledger id that the local chain has
    /// no committed chunks for. Consensus-invalid (the chain hasn't
    /// activated that ledger yet, or the peer is on a different fork).
    /// Distinct from `BlockBoundsLookupError` so the latter only carries
    /// genuine local lookup failures.
    #[error("PoA ledger {ledger_id} inactive in local chain")]
    PoALedgerInactive { ledger_id: u32 },
    #[error("PoA chunk offset out of tx bounds")]
    PoAChunkOffsetOutOfTxBounds,
    #[error("Missing partition assignment for partition hash {partition_hash}")]
    PartitionAssignmentMissing { partition_hash: H256 },
    #[error("Partition assignment for partition hash {partition_hash} is missing slot index")]
    PartitionAssignmentSlotIndexMissing { partition_hash: H256 },
    #[error(
        "Partition assignment slot index too large for u64: {slot_index} (partition {partition_hash})"
    )]
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
    #[error("Term ledger {ledger_id} expires mismatch: expected {expected:?}, got {actual:?}")]
    TermLedgerExpiryMismatch {
        ledger_id: u32,
        expected: Option<u64>,
        actual: Option<u64>,
    },
    #[error("block timestamp {current} is older than parent block {parent}")]
    TimestampOlderThanParent { current: u128, parent: u128 },
    #[error("block timestamp {current} too far in the future (now {now})")]
    TimestampTooFarInFuture { current: u128, now: u128 },
    #[error("Validation service unreachable")]
    ValidationServiceUnreachable,
    #[error("Internal prevalidation task failed (likely panic): {0}")]
    InternalTaskJoin(String),
    #[error("last_step_checkpoints validation failed: {0}")]
    VDFCheckpointsInvalid(String),
    #[error(
        "vdf_limiter.prev_output ({got}) does not match previous blocks vdf_limiter.output ({expected})"
    )]
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

    #[error(
        "Transaction {tx_id} in Submit ledger was already included in past {ledger:?} ledger in block {block_hash:?}"
    )]
    SubmitTxAlreadyIncluded {
        tx_id: H256,
        ledger: DataLedger,
        block_hash: BlockHash,
    },

    #[error(
        "Transaction {tx_id} found in multiple previous blocks. First occurrence in {ledger:?} ledger at block {block_hash}"
    )]
    TxFoundInMultipleBlocks {
        tx_id: H256,
        ledger: DataLedger,
        block_hash: BlockHash,
    },
    #[error("Transaction {tx_id} appears in multiple ledgers within the same block")]
    TxInMultipleLedgers { tx_id: H256 },
    #[error(
        "Publish transaction and ingress proof length mismatch, cannot validate publish ledger transaction proofs"
    )]
    PublishTxProofLengthMismatch,
    #[error("Failed to extract data ledgers: {0}")]
    DataLedgerExtractionFailed(String),
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
    #[error(
        "Transaction {tx_id} has insufficient perm_fee. Expected at least: {expected}, Actual: {actual}"
    )]
    InsufficientPermFee {
        tx_id: H256,
        expected: U256,
        actual: U256,
    },
    #[error(
        "Transaction {tx_id} has insufficient term_fee. Expected at least: {expected}, Actual: {actual}"
    )]
    InsufficientTermFee {
        tx_id: H256,
        expected: U256,
        actual: U256,
    },
    #[error("Transaction {tx_id} has invalid term fee structure: {reason}")]
    InvalidTermFeeStructure { tx_id: H256, reason: String },
    #[error("Transaction {tx_id} has invalid perm fee structure: {reason}")]
    InvalidPermFeeStructure { tx_id: H256, reason: String },
    #[error("Transaction {tx_id} in Submit ledger must not have a promoted_height")]
    SubmitTxHasPromotedHeight { tx_id: H256 },
    #[error("Transaction {tx_id} in term-only ledger must not have a perm_fee")]
    TermLedgerTxHasPermFee { tx_id: H256 },
    #[error(
        "Publish ledger proof count ({proof_count}) does not match transaction count ({tx_count})"
    )]
    PublishLedgerProofCountMismatch { proof_count: usize, tx_count: usize },
    #[error(
        "Incorrect Ingress proof count to publish a transaction. Expected: {expected}, Actual: {actual}"
    )]
    IngressProofCountMismatch { expected: usize, actual: usize },
    #[error(
        "Incorrect number of ingress proofs from assigned owners. Expected {expected}, Actual: {actual}"
    )]
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
    #[error(
        "Commitment {tx_id} at position {position} has version {version}, minimum required is {minimum}"
    )]
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

    /// The block tree cache RwLock was poisoned by a prior caller's panic.
    /// Surfaces here so the caller can decide whether to retry, drop, or
    /// escalate — instead of re-panicking at `expect("cache lock poisoned")`.
    #[error("block tree cache lock poisoned at: {at}")]
    CachePoisoned { at: &'static str },

    /// Parent block referenced by an incoming pre-validated block was not
    /// present in the cache. Reorg-driven cache prunes can race with this
    /// path; previously panicked.
    #[error("parent block {parent_hash} not in cache (expected at height {expected_height})")]
    ParentNotInCache {
        parent_hash: H256,
        expected_height: u64,
    },
}

impl PreValidationError {
    /// Single source of truth for failure-mode classification. Drives both
    /// `is_node_fault()` and `is_internal_failure()`.
    ///
    /// Node faults (`NodeFault`) are local failures whose retry will hit the
    /// same fault (verifier panic, MDBX I/O, local arithmetic bug, poisoned
    /// lock, internal channel dead, local cache mutation, OS clock).
    /// Consensus-safe response: abort + supervisor restart — running on with
    /// an undefined-state local failure risks forking off the network with
    /// silent data-level errors.
    ///
    /// Soft-internal failures (`SoftInternal`) are local but peer-innocent
    /// (pruning/eviction races); recovery is passive via depth-prune + fresh
    /// gossip re-entering the prevalidate path.
    ///
    /// SAFETY: every variant MUST appear explicitly in the match below — no
    /// `_` wildcard. Adding a new variant without classifying it here is a
    /// compile error by design: silently defaulting to `Consensus` would
    /// peer-attribute a local fault (or vice versa) and corrupt consensus.
    pub fn classify(&self) -> ErrorClass {
        match self {
            // === Node faults (must abort + restart) ===
            // Verifier-thread panic captured by spawn_blocking.
            Self::InternalTaskJoin(_)
            // Local MDBX I/O failure during prevalidation lookups.
            | Self::DatabaseError { .. }
            // Local channel/actor failure when querying historical inclusions.
            | Self::PreviousTxInclusionsFailed(_)
            // Local in-memory block-tree cache mutation failure.
            | Self::AddBlockFailed { .. }
            | Self::UpdateCacheForScheduledValidationError(_)
            // Local validation-service channel is dead.
            | Self::ValidationServiceUnreachable
            // OS clock failure.
            | Self::SystemTimeError(_)
            // Local arithmetic on locally-derived inputs (height * block_time);
            // the peer-supplied reward is checked separately via RewardMismatch.
            | Self::RewardCurveError(_)
            // Local arithmetic with local config inputs; per-tx fee comparisons
            // use the separate Insufficient*Fee variants.
            | Self::FeeCalculationFailed(_)
            // Local snapshot computation; has no real failure path today, but
            // if it ever fires it's a local bug, not a consensus failure.
            | Self::EmaSnapshotError(_)
            // PoA-anchored block-bounds lookup failure: caller pre-checks
            // peer-supplied offsets against the chain max
            // (`PoAChunkOffsetOutOfBlockBounds`) and peer-supplied ledger ids
            // against the parent's indexed item (`PoALedgerInactive`), so by
            // the time this variant is constructed the failure is a local
            // index inconsistency (empty index, DB I/O, missing predecessor).
            | Self::BlockBoundsLookupError(_)
            // Block-tree RwLock poisoned by a prior panic. Local corruption —
            // callers short-circuit this variant ahead of the generic
            // node-fault panic so it routes to the graceful shutdown path
            // (see `block_pool::process_block`'s dispatch table).
            | Self::CachePoisoned { .. } => ErrorClass::NodeFault,

            // === Soft internal (peer innocent, retry via passive prune+re-gossip) ===
            // Assigned-proof walk over `CachedDataRoots.block_set` hit a hash
            // no longer resolvable — `block_set` is fork-spanning, so a hash
            // for a now-pruned side-fork block is the expected cause.
            Self::AssignedProofBlockMissing { .. }
            // Parent missing from the in-memory block-tree cache due to a
            // local prune/reorg race. Peer is innocent.
            | Self::ParentNotInCache { .. } => ErrorClass::SoftInternal,

            // === Consensus rejections (peer's block is bad) ===
            Self::AssignedProofCountMismatch { .. }
            | Self::BlockSignatureInvalid
            | Self::CascadeNotConfigured { .. }
            | Self::CommitmentVersionInvalid { .. }
            | Self::CumulativeDifficultyMismatch { .. }
            | Self::DataLedgerExtractionFailed(_)
            | Self::DifficultyMismatch { .. }
            | Self::DuplicateIngressProofSigner { .. }
            | Self::EmaMismatch
            | Self::HeightInvalid { .. }
            | Self::IngressProofCountMismatch { .. }
            | Self::IngressProofMismatch { .. }
            | Self::IngressProofSignatureInvalid(_)
            | Self::IngressProofsMissing
            | Self::InsufficientPermFee { .. }
            | Self::InsufficientTermFee { .. }
            | Self::InvalidDataLedgersLength { .. }
            | Self::InvalidEpochSnapshot { .. }
            | Self::InvalidIngressProof { .. }
            | Self::InvalidLedgerId { .. }
            | Self::InvalidLedgerIdForTx { .. }
            | Self::InvalidPermFeeStructure { .. }
            | Self::InvalidPromotionDataSizeMismatch { .. }
            | Self::InvalidPromotionPath { .. }
            | Self::InvalidTermFeeStructure { .. }
            | Self::InvalidTransactionSignature(_)
            | Self::LastDiffTimestampMismatch { .. }
            | Self::LastEpochHashMismatch { .. }
            | Self::LedgerIdInvalid { .. }
            | Self::MerkleProofInvalid(_)
            | Self::MissingTransactions(_)
            | Self::OraclePriceInvalid
            | Self::PartitionAssignmentMissing { .. }
            | Self::PartitionAssignmentSlotIndexMissing { .. }
            | Self::PartitionAssignmentSlotIndexTooLarge { .. }
            | Self::PoACapacityChunkMismatch { .. }
            | Self::PoAChunkHashMismatch { .. }
            | Self::PoAChunkMissing
            | Self::PoAChunkOffsetOutOfBlockBounds
            | Self::PoAChunkOffsetOutOfDataChunksBounds
            | Self::PoAChunkOffsetOutOfTxBounds
            | Self::PoADataPartitionExpired { .. }
            | Self::PoALedgerInactive { .. }
            | Self::PreviousCumulativeDifficultyMismatch { .. }
            | Self::PreviousSolutionHashMismatch { .. }
            | Self::PublishLedgerProofCountMismatch { .. }
            | Self::PublishTxAlreadyIncluded { .. }
            | Self::PublishTxMissingPriorSubmit { .. }
            | Self::PublishTxProofLengthMismatch
            | Self::RewardMismatch { .. }
            | Self::SolutionHashBelowDifficulty { .. }
            | Self::SolutionHashLinkInvalid { .. }
            | Self::SubmitTxAlreadyIncluded { .. }
            | Self::SubmitTxHasPromotedHeight { .. }
            | Self::TermLedgerExpiryMismatch { .. }
            | Self::TermLedgerTxHasPermFee { .. }
            | Self::TimestampOlderThanParent { .. }
            | Self::TimestampTooFarInFuture { .. }
            | Self::TooManyCommitmentTxs { .. }
            | Self::TooManyDataTxs { .. }
            | Self::TransactionIdMismatch { .. }
            | Self::TxFoundInMultipleBlocks { .. }
            | Self::TxInMultipleLedgers { .. }
            | Self::UnexpectedCommitmentTransactions
            | Self::UnstakedIngressProofSigner { .. }
            | Self::VDFCheckpointsInvalid(_)
            | Self::VDFPreviousOutputMismatch { .. } => ErrorClass::Consensus,
        }
    }

    pub fn is_node_fault(&self) -> bool {
        self.classify().is_node_fault()
    }

    pub fn is_internal_failure(&self) -> bool {
        self.classify().is_internal_failure()
    }

    /// Per-variant snake_case label for the
    /// `irys.block.pre_validation_failed_total{reason}` counter and used by
    /// [`ValidationError::metric_label`] when delegating the `PreValidation`
    /// arm. Bounded-cardinality (capped at enum size).
    ///
    /// SAFETY: every variant MUST appear explicitly in the match below — no
    /// `_` wildcard. Adding a new variant without labelling it here is a
    /// compile error by design: silently routing through a generic
    /// `"invalid"` would mask per-stage node-fault rates (M1 audit
    /// 2026-05-20). Mirrors the SAFETY pattern documented on
    /// [`PreValidationError::classify`].
    ///
    /// MERGE-NOTE: tag values for variants shared with branch
    /// `fix-cdr-block-set` (commit 4e21e25a) match identically so the
    /// cross-branch merge is mechanically clean and dashboards see
    /// continuity. Tags for variants only in this branch
    /// (`AssignedProofBlockMissing`, `PoALedgerInactive`) follow the same
    /// snake_case style.
    pub fn metric_reason(&self) -> &'static str {
        match self {
            Self::BlockBoundsLookupError(_) => "block_bounds_lookup",
            Self::AssignedProofBlockMissing { .. } => "assigned_proof_block_missing",
            Self::BlockSignatureInvalid => "block_signature_invalid",
            Self::CascadeNotConfigured { .. } => "cascade_not_configured",
            Self::CumulativeDifficultyMismatch { .. } => "cumulative_difficulty_mismatch",
            Self::DifficultyMismatch { .. } => "difficulty_mismatch",
            Self::EmaMismatch => "ema_mismatch",
            Self::EmaSnapshotError(_) => "ema_snapshot_error",
            Self::IngressProofsMissing => "ingress_proofs_missing",
            Self::IngressProofSignatureInvalid(_) => "ingress_proof_signature_invalid",
            Self::InvalidPromotionDataSizeMismatch { .. } => "promotion_data_size_mismatch",
            Self::LastDiffTimestampMismatch { .. } => "last_diff_timestamp_mismatch",
            Self::LedgerIdInvalid { .. } => "ledger_id_invalid",
            Self::MerkleProofInvalid(_) => "merkle_proof_invalid",
            Self::OraclePriceInvalid => "oracle_price_invalid",
            Self::UpdateCacheForScheduledValidationError(_) => "update_cache_scheduled_validation",
            Self::PoACapacityChunkMismatch { .. } => "poa_capacity_chunk_mismatch",
            Self::PoAChunkHashMismatch { .. } => "poa_chunk_hash_mismatch",
            Self::PoAChunkMissing => "poa_chunk_missing",
            Self::PoAChunkOffsetOutOfDataChunksBounds => "poa_chunk_offset_out_of_data_bounds",
            Self::PoAChunkOffsetOutOfBlockBounds => "poa_chunk_offset_out_of_block_bounds",
            Self::PoALedgerInactive { .. } => "poa_ledger_inactive",
            Self::PoAChunkOffsetOutOfTxBounds => "poa_chunk_offset_out_of_tx_bounds",
            Self::PartitionAssignmentMissing { .. } => "partition_assignment_missing",
            Self::PartitionAssignmentSlotIndexMissing { .. } => "partition_slot_index_missing",
            Self::PartitionAssignmentSlotIndexTooLarge { .. } => "partition_slot_index_too_large",
            Self::PoADataPartitionExpired { .. } => "poa_data_partition_expired",
            Self::PreviousCumulativeDifficultyMismatch { .. } => "prev_cumulative_diff_mismatch",
            Self::PreviousSolutionHashMismatch { .. } => "prev_solution_hash_mismatch",
            Self::RewardCurveError(_) => "reward_curve_error",
            Self::RewardMismatch { .. } => "reward_mismatch",
            Self::SolutionHashBelowDifficulty { .. } => "solution_hash_below_difficulty",
            Self::SolutionHashLinkInvalid { .. } => "solution_hash_link_invalid",
            Self::SystemTimeError(_) => "system_time_error",
            Self::TermLedgerExpiryMismatch { .. } => "term_ledger_expiry_mismatch",
            Self::TimestampOlderThanParent { .. } => "timestamp_older_than_parent",
            Self::TimestampTooFarInFuture { .. } => "timestamp_too_far_in_future",
            Self::ValidationServiceUnreachable => "validation_service_unreachable",
            Self::InternalTaskJoin(_) => "internal_task_join",
            Self::VDFCheckpointsInvalid(_) => "vdf_checkpoints_invalid",
            Self::VDFPreviousOutputMismatch { .. } => "vdf_prev_output_mismatch",
            Self::HeightInvalid { .. } => "height_invalid",
            Self::LastEpochHashMismatch { .. } => "last_epoch_hash_mismatch",
            Self::PublishTxMissingPriorSubmit { .. } => "publish_tx_missing_prior_submit",
            Self::PublishTxAlreadyIncluded { .. } => "publish_tx_already_included",
            Self::InvalidPromotionPath { .. } => "invalid_promotion_path",
            Self::SubmitTxAlreadyIncluded { .. } => "submit_tx_already_included",
            Self::TxFoundInMultipleBlocks { .. } => "tx_in_multiple_blocks",
            Self::TxInMultipleLedgers { .. } => "tx_in_multiple_ledgers",
            Self::PublishTxProofLengthMismatch => "publish_tx_proof_length_mismatch",
            Self::DataLedgerExtractionFailed(_) => "data_ledger_extraction_failed",
            Self::PreviousTxInclusionsFailed(_) => "prev_tx_inclusions_failed",
            Self::InvalidLedgerIdForTx { .. } => "invalid_ledger_id_for_tx",
            Self::InvalidLedgerId { .. } => "invalid_ledger_id",
            Self::FeeCalculationFailed(_) => "fee_calculation_failed",
            Self::InsufficientPermFee { .. } => "insufficient_perm_fee",
            Self::InsufficientTermFee { .. } => "insufficient_term_fee",
            Self::InvalidTermFeeStructure { .. } => "invalid_term_fee_structure",
            Self::InvalidPermFeeStructure { .. } => "invalid_perm_fee_structure",
            Self::SubmitTxHasPromotedHeight { .. } => "submit_tx_has_promoted_height",
            Self::TermLedgerTxHasPermFee { .. } => "term_ledger_tx_has_perm_fee",
            Self::PublishLedgerProofCountMismatch { .. } => "publish_ledger_proof_count_mismatch",
            Self::IngressProofCountMismatch { .. } => "ingress_proof_count_mismatch",
            Self::AssignedProofCountMismatch { .. } => "assigned_proof_count_mismatch",
            Self::InvalidIngressProof { .. } => "invalid_ingress_proof",
            Self::IngressProofMismatch { .. } => "ingress_proof_mismatch",
            Self::DuplicateIngressProofSigner { .. } => "duplicate_ingress_proof_signer",
            Self::UnstakedIngressProofSigner { .. } => "unstaked_ingress_proof_signer",
            Self::DatabaseError { .. } => "database_error",
            Self::InvalidEpochSnapshot { .. } => "invalid_epoch_snapshot",
            Self::TooManyDataTxs { .. } => "too_many_data_txs",
            Self::TooManyCommitmentTxs { .. } => "too_many_commitment_txs",
            Self::MissingTransactions(_) => "missing_transactions",
            Self::TransactionIdMismatch { .. } => "tx_id_mismatch",
            Self::InvalidTransactionSignature(_) => "invalid_tx_signature",
            Self::CommitmentVersionInvalid { .. } => "commitment_version_invalid",
            Self::UnexpectedCommitmentTransactions => "unexpected_commitment_txs",
            Self::InvalidDataLedgersLength { .. } => "invalid_data_ledgers_length",
            Self::AddBlockFailed { .. } => "add_block_failed",
            Self::CachePoisoned { .. } => "cache_poisoned",
            Self::ParentNotInCache { .. } => "parent_not_in_cache",
        }
    }
}

/// Why a block validation task was cancelled before producing a verdict.
///
/// The [`ValidationCancelReason::IS_INTERNAL`] constant is `true` for all
/// variants; `ValidationError::ValidationCancelled` always routes through
/// `ValidationResult::InternalFailure` (validity unknown, retry plausible).
///
/// All reasons are local-side outcomes that say nothing about the
/// peer's block. Per the M2 audit (2026-05-20), every variant routes through
/// `IS_INTERNAL = true` → `InternalFailure` so the peer is never blamed
/// for a "we moved on" event. Historically `HeightDifference` and
/// `ChannelClosed` returned `false` here under the older rationale that
/// "the block can never become canonical, so discard rather than retry" —
/// that rationale conflated "block is bad" with "we're not pursuing it any
/// more". Discard still happens (the block is removed from cache), but it
/// is no longer recorded as a consensus rejection in metrics / logs / any
/// future peer-scoring code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationCancelReason {
    /// Canonical tip moved past this block by more than `block_tree_depth`.
    /// The block can never become canonical, but its validity is unknown —
    /// our tip simply advanced past it while validation was in flight.
    /// Routes through `IS_INTERNAL = true` so the block discards as a
    /// soft local outcome rather than peer-attributed Invalid.
    HeightDifference,
    /// Parent block absent from the in-memory block tree during the
    /// parent-wait stage. The parent's absence is never peer-attributable:
    /// either the block_pool failed to gate the child on its parent, the
    /// parent was depth-pruned past `block_tree_depth`, or the parent was
    /// proactively removed by the soft-`InternalFailure` handler. In every
    /// case the cause is local, so this routes through `IS_INTERNAL = true`
    /// → `ValidationResult::InternalFailure` (child removed on the soft path
    /// alongside its parent; fresh gossip can re-enter if/when the parent
    /// returns).
    ParentMissing,
    /// Block-state broadcast channel closed (typically during node shutdown).
    /// A shutdown is a local event, not a statement about the peer's block.
    /// Routes through `IS_INTERNAL = true` so shutdown-induced discards
    /// never poison consensus-rejection metrics.
    ChannelClosed,
    /// Concurrent-stage `JoinError::Cancelled` recurred for the same block
    /// past the per-block retry cap (`MAX_CONCURRENT_CANCEL_RETRIES`) in the
    /// validation-service result loop. A single cancel is a Tokio hiccup we
    /// transparently requeue; sustained recurrence for the same block means
    /// something local keeps tearing the task down (poisoned runtime, a
    /// sibling-worker panic loop, a watchdog edge case) and re-running it
    /// will likely just burn cycles. Routes through `IS_INTERNAL = true`
    /// so the block parks rather than being peer-attributed — the validity
    /// of the child block is still unknown, the cancellation said nothing
    /// about it. Recovery is fresh gossip re-entry (same lane as other
    /// SoftInternal parks) once the local condition clears.
    RepeatedCancellation,
}

impl ValidationCancelReason {
    /// All variants are local-side outcomes (M2 audit 2026-05-20, H4 fix).
    /// `HeightDifference`, `ParentMissing`, `ChannelClosed`,
    /// `RepeatedCancellation` — none are statements about the peer's block.
    ///
    /// SAFETY CRITICAL: adding a new variant that classifies as Consensus
    /// REQUIRES changing this constant to a method and re-auditing every
    /// caller. The compile-time tripwire below (`_exhaustive_check`) catches
    /// accidental new variants — a new variant must add an arm there, which
    /// forces the author to re-evaluate `IS_INTERNAL`.
    pub const IS_INTERNAL: bool = true;
}

// Compile-time tripwire: if anyone adds a new variant they must update
// this match (and re-evaluate IS_INTERNAL above).
const _: fn() = || {
    fn _exhaustive_check(reason: ValidationCancelReason) {
        match reason {
            ValidationCancelReason::HeightDifference => {}
            ValidationCancelReason::ParentMissing => {}
            ValidationCancelReason::ChannelClosed => {}
            ValidationCancelReason::RepeatedCancellation => {} // New variant? Update IS_INTERNAL above before adding an arm here.
        }
    }
};

impl std::fmt::Display for ValidationCancelReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HeightDifference => write!(f, "height difference"),
            Self::ParentMissing => write!(f, "parent missing"),
            Self::ChannelClosed => write!(f, "channel closed"),
            Self::RepeatedCancellation => write!(f, "repeated cancellation"),
        }
    }
}

/// Validation error type that covers all block validation failures.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ValidationError {
    /// Pre-validation error (consensus parameter validation)
    #[error("Pre-validation failed: {0}")]
    PreValidation(#[from] PreValidationError),

    /// Validation was cancelled before producing a verdict.
    ///
    /// The `reason` sub-variant determines whether the cancellation is
    /// treated as an internal/runtime failure (validity unknown, retry
    /// plausible) or as a "give up and discard" outcome — see
    /// [`ValidationCancelReason::IS_INTERNAL`].
    #[error("Validation cancelled: {reason}")]
    ValidationCancelled { reason: ValidationCancelReason },

    /// A validation task panicked unexpectedly
    #[error("Validation task panicked: {task}: {details}")]
    TaskPanicked { task: String, details: String },

    /// VDF validation failed
    #[error("VDF validation failed: {0}")]
    VdfValidationFailed(String),

    /// Seed data validation failed
    #[error("Seed data invalid: {0}")]
    SeedDataInvalid(String),

    /// Execution layer (Reth) rejected the payload — consensus rejection
    /// (the block's payload is genuinely invalid: bad state root, structure
    /// mismatch, etc., reported by reth as `PayloadStatusEnum::Invalid`).
    /// Distinct from [`Self::ExecutionLayerTransportFailed`], which is a
    /// local-EL transport hiccup and must NOT be peer-attributed.
    #[error("Execution layer validation failed: {0}")]
    ExecutionLayerFailed(String),

    /// Local reth engine RPC transport failure during payload submission
    /// (engine HTTP client unreachable, request error, etc.). This is a
    /// node-level fault — the EL is broken on this node, not the peer's
    /// block. Classified as `is_node_fault()` so the handler aborts and
    /// the supervisor restarts the node clean.
    #[error("Execution layer transport failure: {0}")]
    ExecutionLayerTransportFailed(String),

    /// Recall range validation failed (consensus mismatch — peer-attributable).
    #[error("Recall range validation failed: {0}")]
    RecallRangeInvalid(String),

    /// Local VDF state did not yet contain the steps required for recall-range
    /// validation. The block's validity is unknown — the node simply hasn't
    /// caught up to the block's VDF position yet. Classified as SoftInternal
    /// (block parks in cache; re-validate once VDF state advances).
    #[error("Recall range VDF steps unavailable: {0}")]
    RecallRangeStepsUnavailable(String),

    /// Shadow transaction validation failed (consensus rejection — the
    /// peer's payload doesn't match the expected shadow transactions, has
    /// invalid structure, or carries a treasury mismatch). Construction
    /// is reserved for genuine consensus mismatches — local DB/mempool/
    /// snapshot failures during shadow-tx *generation* route through
    /// `ShadowTxNodeFault` (hard faults) or early-return typed errors.
    #[error("Shadow transaction validation failed: {0}")]
    ShadowTransactionInvalid(String),

    /// A hard local I/O failure inside the shadow-tx generation pipeline
    /// (block-header DB read, MDBX corruption, lock poisoning, etc.).
    /// Retry cannot help — the DB itself is broken on this node, not the
    /// peer's block — so it is classified as `is_node_fault()` to abort
    /// + supervisor-restart rather than accumulating known-bad blocks
    /// in cache.
    #[error("Shadow transaction generation node fault: {0}")]
    ShadowTxNodeFault(String),

    /// The local `ExecutionPayloadCache` could not deliver the payload
    /// for this block's `evm_block_hash` before the wait completed.
    /// `ExecutionPayloadCache::wait_for_payload` collapses two distinct
    /// `ExecutionPayloadWaitError` variants into this single classified
    /// error (the diagnostic distinction lives in the variant, logged
    /// at the conversion site):
    ///   - `ReceiverDisrupted` — the `payload_senders` LRU evicted our
    ///     slot under heavy catch-up sync
    ///     (>`PAYLOAD_RECEIVERS_CAPACITY = 1000` concurrent waiters in
    ///     flight), or an explicit `remove_payload_from_cache` for the
    ///     same hash.
    ///   - `WaitTimeout` — the bounded
    ///     `sync.execution_payload_wait_timeout_millis` elapsed before
    ///     the payload arrived (peer advertised the header but never
    ///     served the EVM payload). Caps the previously LRU-bounded
    ///     (effectively unbounded under low load) wait.
    ///
    /// Both are local cache / wait disruption — the node is healthy,
    /// the EL is fine, the peer's block may simply be unreachable.
    /// Classified as a soft internal failure (block parks in cache,
    /// retry via fresh gossip re-entry). Specifically NOT a node
    /// fault: panicking here on every cache eviction or timeout would
    /// self-DoS healthy nodes during catch-up. Distinct from
    /// `ExecutionLayerTransportFailed`, which covers genuine local-EL
    /// RPC transport failures (those remain a node fault).
    #[error("Execution payload wait disrupted by local cache (evm_block_hash {evm_block_hash})")]
    ExecutionPayloadCacheEvicted { evm_block_hash: EvmBlockHash },

    /// Commitment transaction has invalid value (stake/pledge/unpledge amount)
    #[error("Commitment transaction {tx_id} at position {position} has invalid value: {reason}")]
    CommitmentValueInvalid {
        tx_id: H256,
        position: usize,
        reason: String,
    },

    /// Commitment transaction version is below minimum required after hardfork activation
    #[error(
        "Commitment {tx_id} at position {position} has version {version}, minimum required is {minimum}"
    )]
    CommitmentVersionInvalid {
        tx_id: H256,
        position: usize,
        version: u8,
        minimum: u8,
    },

    /// Commitment type not allowed before hardfork activation (e.g., UpdateRewardAddress before Borealis)
    #[error(
        "Commitment {tx_id} at position {position} uses type {commitment_type} not allowed before hardfork activation"
    )]
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
    #[error(
        "Unpledge commitment {tx_id} targets partition {partition_hash} not owned by signer {signer}"
    )]
    UnpledgePartitionNotOwned {
        tx_id: H256,
        partition_hash: H256,
        signer: IrysAddress,
    },

    /// Parent commitment snapshot not found at validation entry.
    ///
    /// Classified as internal: this is an eviction race against the in-memory
    /// snapshot window — the parent's snapshot may have been there moments
    /// before we looked. Validity is unknown; the block stays in cache. There
    /// is no automatic re-scheduler today — recovery is passive (depth-prune
    /// → fresh gossip re-enters the prevalidate path).
    #[error("Parent commitment snapshot missing for block {block_hash}")]
    ParentCommitmentSnapshotMissing { block_hash: H256 },

    /// Parent epoch snapshot not found at validation entry.
    ///
    /// Classified as internal for the same reason as
    /// [`Self::ParentCommitmentSnapshotMissing`].
    #[error("Parent epoch snapshot missing for block {block_hash}")]
    ParentEpochSnapshotMissing { block_hash: H256 },

    /// Parent EMA snapshot not found at validation entry.
    ///
    /// Classified as internal for the same reason as
    /// [`Self::ParentCommitmentSnapshotMissing`].
    #[error("Parent EMA snapshot missing for block {block_hash}")]
    ParentEmaSnapshotMissing { block_hash: H256 },

    /// Parent block not found in block tree at validation entry (looked up
    /// during seed-data validation, before the parent-wait stage).
    ///
    /// Classified as internal: the parent could have been present a moment
    /// ago and got evicted just before we looked. The block's validity is
    /// genuinely unknown. Recovery is passive (cache entry persists until
    /// depth-pruning evicts it, then fresh gossip re-enters the prevalidate
    /// path); no automatic re-scheduler today.
    ///
    /// Construction site is prevalidation-time. The cancellation-time analog
    /// is [`ValidationCancelReason::ParentMissing`], which fires from inside
    /// the parent-wait stage; both classify as internal because neither
    /// "parent absent from cache" condition is peer-attributable.
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
}

impl ValidationError {
    /// Single source of truth for failure-mode classification. Drives both
    /// `is_node_fault()` and `is_internal_failure()`. See
    /// [`PreValidationError::classify`] for the rationale.
    ///
    /// SAFETY: every variant MUST appear explicitly in the match below — no
    /// `_` wildcard. Adding a new variant without classifying it here is a
    /// compile error by design: silently defaulting would peer-attribute a
    /// local fault (or vice versa) and corrupt consensus.
    pub fn classify(&self) -> ErrorClass {
        match self {
            // Delegate to PreValidationError's classifier.
            Self::PreValidation(e) => e.classify(),
            // Task panic is always a node fault: a verifier thread crashed.
            Self::TaskPanicked { .. } => ErrorClass::NodeFault,
            // Local reth engine RPC transport failure — the EL is broken
            // on this node, not the peer's block. Abort + supervisor restart.
            Self::ExecutionLayerTransportFailed(_) => ErrorClass::NodeFault,
            // Hard local I/O failure inside shadow-tx generation (block-header
            // DB read, MDBX corruption, etc.) — retry cannot help, the DB is
            // broken on this node.
            Self::ShadowTxNodeFault(_) => ErrorClass::NodeFault,

            // Cancellation classification is sub-variant-dependent; delegate
            // so a future retry-plausible reason doesn't silently become a
            // node fault — that decision should be explicit at the reason
            // site. (Today, post-M2 audit 2026-05-20: all three reasons —
            // `ParentMissing`, `HeightDifference`, `ChannelClosed` — are
            // local-side outcomes and route to `SoftInternal`. No
            // cancellation reason ever peer-attributes the child block.)
            // All cancellation reasons are local-side outcomes (IS_INTERNAL
            // const documents + compile-time tripwire enforces). No
            // cancellation reason ever peer-attributes the child block.
            Self::ValidationCancelled { .. } => {
                if ValidationCancelReason::IS_INTERNAL {
                    ErrorClass::SoftInternal
                } else {
                    ErrorClass::Consensus
                }
            }
            // Prevalidation-time parent/snapshot lookups: eviction races, not
            // node faults. Recovery is passive via depth-prune + re-gossip.
            // The cancellation-time `ValidationCancelReason::ParentMissing`
            // routes via `ValidationCancelled` above — see the doc on
            // `Self::ParentBlockMissing` for the distinction.
            Self::ParentCommitmentSnapshotMissing { .. }
            | Self::ParentEpochSnapshotMissing { .. }
            | Self::ParentEmaSnapshotMissing { .. }
            | Self::ParentBlockMissing { .. } => ErrorClass::SoftInternal,
            // Local `ExecutionPayloadCache` tore down the wait receiver
            // (LRU eviction under sync load, or explicit cache removal).
            // The node is healthy and the EL is fine — cache is just
            // saturated. Block parks in cache for retry. See the variant
            // doc for the full rationale.
            Self::ExecutionPayloadCacheEvicted { .. } => ErrorClass::SoftInternal,
            // Local VDF state hasn't caught up to the block's step position.
            // Not peer-attributable — the block may be valid once VDF catches
            // up. Block parks in cache for passive retry.
            Self::RecallRangeStepsUnavailable(_) => ErrorClass::SoftInternal,

            // Consensus rejections — peer's block is genuinely bad.
            Self::VdfValidationFailed(_)
            | Self::SeedDataInvalid(_)
            | Self::ExecutionLayerFailed(_)
            | Self::RecallRangeInvalid(_)
            | Self::ShadowTransactionInvalid(_)
            | Self::CommitmentValueInvalid { .. }
            | Self::CommitmentVersionInvalid { .. }
            | Self::CommitmentTypeNotAllowed { .. }
            | Self::CommitmentOrderingFailed(_)
            | Self::CommitmentSnapshotRejected { .. }
            | Self::UnpledgePartitionNotOwned { .. }
            | Self::EpochCommitmentMismatch { .. }
            | Self::EpochExtraCommitment { .. }
            | Self::EpochMissingCommitment { .. }
            | Self::CommitmentWrongOrder { .. } => ErrorClass::Consensus,
        }
    }

    pub fn is_node_fault(&self) -> bool {
        self.classify().is_node_fault()
    }

    pub fn is_internal_failure(&self) -> bool {
        self.classify().is_internal_failure()
    }

    /// Most-specific label for per-stage metric recording. Distinguishes
    /// `node_fault` (local-state corruption / EL transport failure → abort
    /// + supervisor restart) from `internal_error` (soft-internal: block
    /// parks in cache for retry) from `invalid` (peer-attributable
    /// consensus rejection), so dashboards can isolate local-fault rates
    /// from peer-fault rates.
    ///
    /// The overall-result `label_for` closure at the dispatch site in
    /// `block_validation_task.rs` collapses `node_fault` and
    /// `internal_error` back to its default (`"invalid"` for
    /// `Invalid` / `"internal_error"` for `InternalFailure`) so the
    /// production-dashboard labels for the overall metric do not change.
    ///
    /// SAFETY: every variant MUST appear explicitly in the match below —
    /// no `_` wildcard. Adding a new variant without labelling it here is
    /// a compile error by design.
    pub fn metric_label(&self) -> &'static str {
        match self {
            Self::ValidationCancelled { .. } => "cancelled",
            Self::TaskPanicked { .. } => "panicked",
            // node-fault variants — distinct from generic "invalid" so
            // dashboards can isolate local-state corruption from
            // peer-attributable rejections.
            Self::ShadowTxNodeFault(_) | Self::ExecutionLayerTransportFailed(_) => "node_fault",
            // soft-internal — block parks for retry, not a peer fault.
            Self::ExecutionPayloadCacheEvicted { .. }
            | Self::ParentBlockMissing { .. }
            | Self::ParentCommitmentSnapshotMissing { .. }
            | Self::ParentEpochSnapshotMissing { .. }
            | Self::ParentEmaSnapshotMissing { .. }
            | Self::RecallRangeStepsUnavailable(_) => "internal_error",
            // Per-variant snake_case tag — pre-validation may surface
            // node-fault, soft-internal, or peer-attributable rejections;
            // the generic `"invalid"` would undercount local-state corruption
            // at this stage (M1 audit 2026-05-20).
            Self::PreValidation(e) => e.metric_reason(),
            // consensus rejections — all remaining variants.
            Self::VdfValidationFailed(_)
            | Self::SeedDataInvalid(_)
            | Self::ExecutionLayerFailed(_)
            | Self::RecallRangeInvalid(_)
            | Self::ShadowTransactionInvalid(_)
            | Self::CommitmentValueInvalid { .. }
            | Self::CommitmentVersionInvalid { .. }
            | Self::CommitmentTypeNotAllowed { .. }
            | Self::CommitmentOrderingFailed(_)
            | Self::CommitmentSnapshotRejected { .. }
            | Self::UnpledgePartitionNotOwned { .. }
            | Self::EpochCommitmentMismatch { .. }
            | Self::EpochExtraCommitment { .. }
            | Self::EpochMissingCommitment { .. }
            | Self::CommitmentWrongOrder { .. } => "invalid",
        }
    }
}

#[cfg(test)]
mod metric_label_tests {
    //! Regression tests for the granular per-stage
    //! [`ValidationError::metric_label`] partitioning. The overall metric
    //! collapses these back to `"invalid"` / `"internal_error"` via the
    //! dispatch-site `label_for` closure, but per-stage call sites surface
    //! the granular labels so dashboards can isolate local-fault rates from
    //! peer-attributable rejections.

    use super::*;
    use crate::shadow_tx_generator::ShadowTxGenError;

    /// Each category of `ValidationError` maps to a distinct per-stage
    /// label. This is the load-bearing test that adding a new variant must
    /// be classified (the match in `metric_label()` has no `_` wildcard).
    #[test]
    fn metric_label_distinguishes_node_fault_from_consensus() {
        // cancelled — `ValidationCancelled` regardless of sub-reason.
        for reason in [
            ValidationCancelReason::HeightDifference,
            ValidationCancelReason::ParentMissing,
            ValidationCancelReason::ChannelClosed,
            ValidationCancelReason::RepeatedCancellation,
        ] {
            let err = ValidationError::ValidationCancelled { reason };
            assert_eq!(
                err.metric_label(),
                "cancelled",
                "ValidationCancelled (reason {:?}) must label as 'cancelled'",
                reason,
            );
        }

        // panicked — verifier thread crashed.
        let err = ValidationError::TaskPanicked {
            task: "poa".into(),
            details: "boom".into(),
        };
        assert_eq!(err.metric_label(), "panicked");

        // node_fault — local-state corruption / EL transport failure.
        let err = ValidationError::ShadowTxNodeFault("db corrupted".into());
        assert_eq!(err.metric_label(), "node_fault");
        let err = ValidationError::ExecutionLayerTransportFailed("rpc down".into());
        assert_eq!(err.metric_label(), "node_fault");

        // internal_error — soft-internal: block parks for retry.
        let err = ValidationError::ParentBlockMissing {
            block_hash: H256::zero(),
        };
        assert_eq!(err.metric_label(), "internal_error");
        let err = ValidationError::ParentCommitmentSnapshotMissing {
            block_hash: H256::zero(),
        };
        assert_eq!(err.metric_label(), "internal_error");
        let err = ValidationError::ParentEpochSnapshotMissing {
            block_hash: H256::zero(),
        };
        assert_eq!(err.metric_label(), "internal_error");
        let err = ValidationError::ParentEmaSnapshotMissing {
            block_hash: H256::zero(),
        };
        assert_eq!(err.metric_label(), "internal_error");
        let err = ValidationError::ExecutionPayloadCacheEvicted {
            evm_block_hash: Default::default(),
        };
        assert_eq!(err.metric_label(), "internal_error");
        let err = ValidationError::RecallRangeStepsUnavailable("steps unavailable".into());
        assert_eq!(err.metric_label(), "internal_error");

        // invalid — consensus-rejection variants (peer-attributable).
        let err = ValidationError::ShadowTransactionInvalid("bad treasury".into());
        assert_eq!(err.metric_label(), "invalid");
        let err = ValidationError::ExecutionLayerFailed("state root mismatch".into());
        assert_eq!(err.metric_label(), "invalid");
        let err = ValidationError::VdfValidationFailed("step mismatch".into());
        assert_eq!(err.metric_label(), "invalid");
        let err = ValidationError::SeedDataInvalid("bad seed".into());
        assert_eq!(err.metric_label(), "invalid");
        let err = ValidationError::RecallRangeInvalid("out of range".into());
        assert_eq!(err.metric_label(), "invalid");

        // PreValidation — delegates to the inner variant's `metric_reason()`
        // so dashboards can isolate node-fault / soft-internal / consensus
        // failures at the pre-validation stage (M1 audit 2026-05-20).
        // Spot-check one variant from each `ErrorClass`:
        //   * NodeFault — `DatabaseError` (local MDBX I/O failure).
        let err = ValidationError::PreValidation(PreValidationError::DatabaseError {
            error: "mdbx i/o".into(),
        });
        assert_eq!(err.metric_label(), "database_error");
        //   * SoftInternal — `ParentNotInCache` (reorg/prune race; peer innocent).
        let err = ValidationError::PreValidation(PreValidationError::ParentNotInCache {
            parent_hash: H256::zero(),
            expected_height: 0,
        });
        assert_eq!(err.metric_label(), "parent_not_in_cache");
        //   * Consensus — `MerkleProofInvalid` (peer-attributable).
        let err =
            ValidationError::PreValidation(PreValidationError::MerkleProofInvalid("bad".into()));
        assert_eq!(err.metric_label(), "merkle_proof_invalid");
    }

    /// `PreValidationError::metric_reason` returns a distinct snake_case tag
    /// per variant; the match has no `_` wildcard so adding a new variant
    /// without tagging it is a compile error. Spot-check one representative
    /// from each `ErrorClass` (NodeFault / SoftInternal / Consensus) — the
    /// rest is covered by the compile-time exhaustiveness check.
    #[test]
    fn pre_validation_metric_reason_exhaustive_coverage() {
        // NodeFault — local MDBX I/O.
        let err = PreValidationError::DatabaseError {
            error: "mdbx i/o".into(),
        };
        assert_eq!(err.classify(), ErrorClass::NodeFault);
        assert_eq!(err.metric_reason(), "database_error");

        // NodeFault — block-bounds local-index inconsistency.
        let err = PreValidationError::BlockBoundsLookupError("empty index".into());
        assert_eq!(err.classify(), ErrorClass::NodeFault);
        assert_eq!(err.metric_reason(), "block_bounds_lookup");

        // NodeFault — verifier-thread panic captured by spawn_blocking.
        let err = PreValidationError::InternalTaskJoin("panicked".into());
        assert_eq!(err.classify(), ErrorClass::NodeFault);
        assert_eq!(err.metric_reason(), "internal_task_join");

        // SoftInternal — fork-spanning `block_set` walk surfaced a pruned hash.
        let err = PreValidationError::AssignedProofBlockMissing {
            block_hash: H256::zero(),
            tx_id: H256::zero(),
        };
        assert_eq!(err.classify(), ErrorClass::SoftInternal);
        assert_eq!(err.metric_reason(), "assigned_proof_block_missing");

        // SoftInternal — parent missing from in-memory cache due to prune/reorg race.
        let err = PreValidationError::ParentNotInCache {
            parent_hash: H256::zero(),
            expected_height: 0,
        };
        assert_eq!(err.classify(), ErrorClass::SoftInternal);
        assert_eq!(err.metric_reason(), "parent_not_in_cache");

        // Consensus — peer-attributable merkle proof failure.
        let err = PreValidationError::MerkleProofInvalid("bad".into());
        assert_eq!(err.classify(), ErrorClass::Consensus);
        assert_eq!(err.metric_reason(), "merkle_proof_invalid");

        // Consensus — peer-supplied ledger id inactive in the local chain.
        let err = PreValidationError::PoALedgerInactive { ledger_id: 99 };
        assert_eq!(err.classify(), ErrorClass::Consensus);
        assert_eq!(err.metric_reason(), "poa_ledger_inactive");
    }

    /// Regression coverage for the round-6 + R7 work: the
    /// `SnapshotTreasuryUnderflow` sub-variant of `ShadowTxGenError` routes
    /// through `ShadowTxNodeFault`, which must produce the `node_fault`
    /// per-stage label so dashboards see local-snapshot corruption
    /// separately from peer-attributable rejections.
    #[test]
    fn metric_label_for_snapshot_treasury_underflow_is_node_fault() {
        // Mirror the routing used inside
        // `shadow_tx_gen_error_dispatch_tests::classify_shadow_err` so the
        // assertion stays in lockstep with the production classifier.
        let err = match ShadowTxGenError::SnapshotTreasuryUnderflow(
            "cannot pay 10 from balance 5".into(),
        ) {
            ShadowTxGenError::SnapshotTreasuryUnderflow(s) => {
                ValidationError::ShadowTxNodeFault(format!("snapshot treasury underflow: {s}"))
            }
            other => panic!("unexpected ShadowTxGenError variant: {other:?}"),
        };
        assert!(matches!(err, ValidationError::ShadowTxNodeFault(_)));
        assert!(err.is_node_fault());
        assert_eq!(
            err.metric_label(),
            "node_fault",
            "SnapshotTreasuryUnderflow must surface as 'node_fault' per-stage label",
        );
    }
}

#[cfg(test)]
mod recall_range_error_tests {
    use super::*;

    #[test]
    fn recall_range_error_is_internal() {
        let steps_err = RecallRangeError::StepsUnavailable(eyre::eyre!("steps missing"));
        assert!(steps_err.is_internal(), "StepsUnavailable must be internal");

        let mismatch_err = RecallRangeError::Mismatch(eyre::eyre!("range mismatch"));
        assert!(!mismatch_err.is_internal(), "Mismatch must not be internal");
    }

    #[test]
    fn validation_error_recall_range_steps_unavailable_is_soft_internal() {
        let err = ValidationError::RecallRangeStepsUnavailable("no steps".into());
        assert_eq!(err.classify(), ErrorClass::SoftInternal);
        assert!(err.is_internal_failure());
        assert!(!err.is_node_fault());
        assert_eq!(err.metric_label(), "internal_error");
    }

    #[test]
    fn validation_error_recall_range_invalid_is_consensus() {
        let err = ValidationError::RecallRangeInvalid("mismatch".into());
        assert_eq!(err.classify(), ErrorClass::Consensus);
        assert!(!err.is_internal_failure());
        assert!(!err.is_node_fault());
        assert_eq!(err.metric_label(), "invalid");
    }
}

/// Full pre-validation steps for a block
#[tracing::instrument(level = "trace", skip_all, fields(block.hash = %sealed_block.header().block_hash, block.height = sealed_block.header().height))]
pub async fn prevalidate_block(
    sealed_block: &SealedBlock,
    previous_block: &IrysBlockHeader,
    parent_epoch_snapshot: Arc<EpochSnapshot>,
    config: Config,
    pool: Arc<rayon::ThreadPool>,
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

    let poa_chunk: &[u8] = match &block.poa.chunk {
        Some(chunk) => chunk.as_ref(),
        None => return Err(PreValidationError::PoAChunkMissing),
    };

    let block_poa_hash: H256 = sha::sha256(poa_chunk).into();
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
    solution_hash_link_is_valid(block, poa_chunk)?;
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

    // We only check last_step_checkpoints during pre-validation.
    // A spawn_blocking JoinError is a local panic in the verifier thread, not
    // a consensus failure — it must never be mapped to VDFCheckpointsInvalid.
    let pool_clone = Arc::clone(&pool);
    let vdf_info = block.vdf_limiter_info.clone();
    let vdf_config = config.vdf.clone();
    tokio::task::spawn_blocking(move || {
        last_step_checkpoints_is_valid(&pool_clone, &vdf_info, &vdf_config)
    })
    .await
    .map_err(|e| {
        error!(
            block.hash = ?block.block_hash,
            block.height = ?block.height,
            error = %e,
            "spawn_blocking for last_step_checkpoints_is_valid failed",
        );
        PreValidationError::InternalTaskJoin(format!("last_step_checkpoints_is_valid: {e}"))
    })?
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

    // ========================================
    // Data Ledger Validation
    // ========================================
    // Ensure only active data ledgers are present in the block
    let cascade_active = config
        .consensus
        .hardforks
        .is_cascade_active_for_epoch(&parent_epoch_snapshot);

    // Validate 'expires' field on term ledgers
    validate_term_ledger_expiry(block, &config.consensus, cascade_active)?;
    for ledger in &block.data_ledgers {
        match DataLedger::try_from(ledger.ledger_id) {
            Ok(DataLedger::Publish | DataLedger::Submit) => {
                // Always valid
            }
            Ok(DataLedger::OneYear | DataLedger::ThirtyDay) if cascade_active => {
                // Valid when Cascade hardfork is active
            }
            _ => {
                return Err(PreValidationError::InvalidLedgerId {
                    ledger_id: ledger.ledger_id,
                    block_height: block.height,
                });
            }
        }
    }

    // ========================================
    // Transaction validation
    // ========================================

    // Validate all data ledger transactions: count, IDs, and signatures
    let max_data_txs = config
        .node_config
        .consensus_config()
        .mempool
        .max_data_txs_per_block;
    let mut total_term_txs: usize = 0;

    for dl in &block.data_ledgers {
        let ledger = DataLedger::try_from(dl.ledger_id).map_err(|_| {
            PreValidationError::InvalidLedgerId {
                ledger_id: dl.ledger_id,
                block_height: block.height,
            }
        })?;

        let ledger_txs = transactions.get_ledger_txs(ledger);
        validate_transactions(&pool, ledger_txs, &dl.tx_ids.0)?;
        debug!(
            block.hash = ?block.block_hash,
            block.height = ?block.height,
            ledger = ?ledger,
            "{:?}_transactions_valid", ledger,
        );

        // Count term ledger txs for max_data_txs enforcement
        if ledger != DataLedger::Publish {
            total_term_txs += ledger_txs.len();
        }
    }

    // Enforce max_data_txs_per_block across all term ledgers (Submit + OneYear + ThirtyDay)
    if total_term_txs > max_data_txs as usize {
        return Err(PreValidationError::TooManyDataTxs {
            max: max_data_txs,
            got: total_term_txs,
        });
    }

    // Look up individual ledgers for ingress proof validation
    let publish_ledger = block
        .data_ledgers
        .iter()
        .find(|dl| dl.ledger_id == DataLedger::Publish as u32)
        .ok_or_else(|| PreValidationError::InvalidDataLedgersLength {
            expected: DataLedger::Publish.into(),
            got: block.data_ledgers.len(),
        })?;

    let publish_txs = transactions.get_ledger_txs(DataLedger::Publish);

    // Flatten (proof, data_root) pairs across publish txs so the parallel
    // verify below fans out across every proof rather than per-tx batches.
    let estimated_proofs = publish_ledger
        .required_proof_count
        .map_or(publish_txs.len(), |c| publish_txs.len() * c as usize);
    let mut ingress_pairs: Vec<(IngressProof, H256)> = Vec::with_capacity(estimated_proofs);
    for tx_header in publish_txs.iter() {
        let tx_proofs = get_ingress_proofs(publish_ledger, &tx_header.id)
            .map_err(|_| PreValidationError::IngressProofsMissing)?;
        for proof in tx_proofs.0 {
            ingress_pairs.push((proof, tx_header.data_root));
        }
    }
    pool.install(|| {
        ingress_pairs.par_iter().try_for_each(|(proof, data_root)| {
            proof
                .pre_validate(data_root)
                .map(|_| ())
                .map_err(|e| PreValidationError::IngressProofSignatureInvalid(e.to_string()))
        })
    })?;
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
        validate_transactions(&pool, commitment_txs, &commitment_ledger.tx_ids.0)?;
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
fn validate_transactions<T: IrysTransactionCommon + Sync>(
    pool: &rayon::ThreadPool,
    txs: &[T],
    expected_ids: &[IrysTransactionId],
) -> Result<(), PreValidationError> {
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

    pool.install(|| {
        txs.par_iter()
            .zip(expected_ids.par_iter())
            .try_for_each(|(tx, expected_id)| {
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
                Ok(())
            })
    })
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
mod prevalidation_error_classification_tests {
    use super::*;
    use rstest::rstest;

    /// `InternalTaskJoin` is a local runtime failure (verifier panicked); it
    /// must classify as internal so callers route it away from peer-attributed
    /// "block invalid" paths.
    #[test]
    fn internal_task_join_is_internal_failure() {
        let err = PreValidationError::InternalTaskJoin("panic".to_string());
        assert!(err.is_internal_failure());
    }

    /// Consensus-validation variants must NOT classify as internal.
    #[test]
    fn validation_errors_are_not_internal_failures() {
        assert!(!PreValidationError::BlockSignatureInvalid.is_internal_failure());
        assert!(
            !PreValidationError::VDFCheckpointsInvalid("bad".to_string()).is_internal_failure()
        );
    }

    /// Post-M2 audit (2026-05-20): every `ValidationCancelReason` is a
    /// local-side outcome and routes through `InternalFailure`. None are
    /// peer-attributable, so `IS_INTERNAL` is `true` and the wrapping
    /// `ValidationError::ValidationCancelled` is an `is_internal_failure()`.
    ///
    /// One case per `ValidationCancelReason` variant so per-variant failures
    /// surface as distinct nextest reports (matches the repo convention used
    /// by `is_parent_ready_chain_state_dispatch` / `block_status_returns_in_tree_pending_validation`).
    #[rstest]
    #[case::height_difference(ValidationCancelReason::HeightDifference)]
    #[case::channel_closed(ValidationCancelReason::ChannelClosed)]
    #[case::parent_missing(ValidationCancelReason::ParentMissing)]
    #[case::repeated_cancellation(ValidationCancelReason::RepeatedCancellation)]
    fn validation_cancel_reason_classifier_dispatch(#[case] reason: ValidationCancelReason) {
        // IS_INTERNAL is a const — verified at compile time by the tripwire below.
        // `ValidationError::is_internal_failure` must route through IS_INTERNAL.
        assert!(
            ValidationError::ValidationCancelled { reason }.is_internal_failure(),
            "ValidationError::ValidationCancelled {{ reason: {:?} }}.is_internal_failure()",
            reason,
        );
    }

    /// Expected `ValidationResult` shape for a given `ValidationCancelReason`
    /// after round-tripping through `From<ValidationError>`.
    ///
    /// Post-M2 audit (2026-05-20): every cancel reason routes to
    /// `InternalFailureSoft`. Retained as an enum (rather than a single
    /// assertion) to preserve the per-variant rstest case structure — adding
    /// a new variant that must land on a different shape stays mechanically
    /// expressible without a churn.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ExpectedRoundtripShape {
        /// Soft internal failure (not a node fault). The block discards from
        /// cache but is NOT peer-attributed as consensus-rejected.
        InternalFailureSoft,
    }

    /// Round-trip: every `ValidationCancelReason` must land on
    /// `ValidationResult::InternalFailure(_)` via the
    /// `From<ValidationError> for ValidationResult` dispatcher. Post-M2,
    /// no cancel reason peer-attributes the block as `Invalid`.
    ///
    /// One case per `ValidationCancelReason` variant so per-variant failures
    /// surface as distinct nextest reports.
    #[rstest]
    #[case::height_difference(
        ValidationCancelReason::HeightDifference,
        ExpectedRoundtripShape::InternalFailureSoft
    )]
    #[case::channel_closed(
        ValidationCancelReason::ChannelClosed,
        ExpectedRoundtripShape::InternalFailureSoft
    )]
    #[case::parent_missing(
        ValidationCancelReason::ParentMissing,
        ExpectedRoundtripShape::InternalFailureSoft
    )]
    #[case::repeated_cancellation(
        ValidationCancelReason::RepeatedCancellation,
        ExpectedRoundtripShape::InternalFailureSoft
    )]
    fn validation_cancel_reason_roundtrip_through_dispatcher(
        #[case] reason: ValidationCancelReason,
        #[case] expected: ExpectedRoundtripShape,
    ) {
        use crate::block_tree_service::ValidationResult;

        let result: ValidationResult = ValidationError::ValidationCancelled { reason }.into();
        match (expected, &result) {
            (
                ExpectedRoundtripShape::InternalFailureSoft,
                ValidationResult::InternalFailure(inner),
            ) => {
                assert!(
                    !inner.is_node_fault(),
                    "{:?} is a soft cancel — must NOT be a node fault",
                    reason,
                );
                assert!(
                    matches!(
                        inner.err(),
                        ValidationError::ValidationCancelled { reason: cancel_reason }
                            if *cancel_reason == reason
                    ),
                    "inner ValidationCancelled reason must round-trip identically; got {:?}",
                    inner.err(),
                );
            }
            (expected, actual) => panic!(
                "reason {:?} round-tripped to {:?}, expected shape {:?}",
                reason, actual, expected,
            ),
        }
    }

    /// Local/runtime variants (DB I/O, channel failure, OS clock, internal
    /// arithmetic, cache mutation) must classify as internal so block_pool
    /// does not peer-attribute or record the block as invalid.
    #[test]
    fn local_runtime_variants_are_internal_failures() {
        assert!(
            PreValidationError::DatabaseError {
                error: "mdbx".to_string()
            }
            .is_internal_failure()
        );
        assert!(
            PreValidationError::PreviousTxInclusionsFailed("ch".to_string()).is_internal_failure()
        );
        assert!(
            PreValidationError::AddBlockFailed {
                block_hash: H256::zero(),
                reason: "x".to_string(),
            }
            .is_internal_failure()
        );
        assert!(
            PreValidationError::UpdateCacheForScheduledValidationError(H256::zero())
                .is_internal_failure()
        );
        assert!(PreValidationError::ValidationServiceUnreachable.is_internal_failure());
        assert!(PreValidationError::SystemTimeError("clk".to_string()).is_internal_failure());
        assert!(PreValidationError::RewardCurveError("ovf".to_string()).is_internal_failure());
        assert!(PreValidationError::FeeCalculationFailed("ovf".to_string()).is_internal_failure());
        assert!(PreValidationError::EmaSnapshotError("ema".to_string()).is_internal_failure());
        assert!(
            PreValidationError::BlockBoundsLookupError("db gone".to_string()).is_internal_failure()
        );
        assert!(
            PreValidationError::CachePoisoned { at: "test" }.is_internal_failure(),
            "CachePoisoned is a local corruption, must not be peer-attributed"
        );
        assert!(
            PreValidationError::ParentNotInCache {
                parent_hash: H256::zero(),
                expected_height: 0,
            }
            .is_internal_failure(),
            "ParentNotInCache is a local prune/reorg race, must not be peer-attributed"
        );
    }

    /// Peer-supplied PoA referencing a ledger that the local chain has no
    /// committed chunks for is consensus-invalid, NOT internal. Guards against
    /// the regression where the PoA short-circuit's previous formulation fell
    /// through to BlockBoundsLookupError (now classified internal).
    #[test]
    fn poa_ledger_inactive_is_not_internal_failure() {
        assert!(!PreValidationError::PoALedgerInactive { ledger_id: 99 }.is_internal_failure());
    }

    /// Genuine node faults (panic, DB I/O, local arithmetic, channel dead,
    /// OS clock, local cache mutation, poisoned lock) must classify as
    /// `is_node_fault()` so handlers abort the node instead of leaving the
    /// block in cache for a retry that will hit the same fault.
    #[test]
    fn node_fault_variants_classify_as_node_fault() {
        let cases: &[PreValidationError] = &[
            PreValidationError::InternalTaskJoin("panic".to_string()),
            PreValidationError::DatabaseError {
                error: "mdbx".to_string(),
            },
            PreValidationError::PreviousTxInclusionsFailed("ch".to_string()),
            PreValidationError::AddBlockFailed {
                block_hash: H256::zero(),
                reason: "x".to_string(),
            },
            PreValidationError::UpdateCacheForScheduledValidationError(H256::zero()),
            PreValidationError::ValidationServiceUnreachable,
            PreValidationError::SystemTimeError("clk".to_string()),
            PreValidationError::RewardCurveError("ovf".to_string()),
            PreValidationError::FeeCalculationFailed("ovf".to_string()),
            PreValidationError::EmaSnapshotError("ema".to_string()),
            PreValidationError::BlockBoundsLookupError("db gone".to_string()),
            PreValidationError::CachePoisoned { at: "test" },
        ];
        for err in cases {
            assert!(err.is_node_fault(), "{:?} must classify as node fault", err);
            assert!(
                err.is_internal_failure(),
                "{:?} node-fault must also be internal-failure (strict subset)",
                err
            );
        }
    }

    /// Pruning/eviction races are `is_internal_failure()` but must NOT be
    /// node faults — they're recoverable via passive depth-prune + re-gossip,
    /// and aborting the node on every race would defeat the recovery path.
    #[test]
    fn eviction_race_variants_are_not_node_faults() {
        let cases: &[PreValidationError] = &[
            PreValidationError::ParentNotInCache {
                parent_hash: H256::zero(),
                expected_height: 0,
            },
            // `block_set` is fork-spanning — a hash that resolved at observe
            // time can be pruned with its side fork by validation time.
            // Innocent peer; not a node fault.
            PreValidationError::AssignedProofBlockMissing {
                block_hash: H256::zero(),
                tx_id: H256::zero(),
            },
        ];
        for err in cases {
            assert!(
                !err.is_node_fault(),
                "{:?} is an eviction race, must not classify as node fault",
                err
            );
            assert!(
                err.is_internal_failure(),
                "{:?} must still be internal-failure (peer is innocent)",
                err
            );
        }
    }

    /// The PoA-anchored `BlockBoundsLookupError` retains `is_node_fault =
    /// true` after the split: caller pre-checks at
    /// `get_data_poa_bounds_with_block_tree_fallback` rule out the
    /// peer-attributable cases (`PoAChunkOffsetOutOfBlockBounds`,
    /// `PoALedgerInactive`) before this variant is constructible, so the
    /// remaining failure modes are genuine local-index breakage. Guard
    /// against an accidental "soften everything" reclassification when the
    /// new soft sibling is added.
    #[test]
    fn block_bounds_lookup_error_retains_node_fault_classification() {
        let err = PreValidationError::BlockBoundsLookupError("local index broken".to_string());
        assert!(
            err.is_node_fault(),
            "PoA-anchored BlockBoundsLookupError must remain a node fault — \
             callers pre-filter all peer-attributable cases before it can fire",
        );
        assert!(
            err.is_internal_failure(),
            "BlockBoundsLookupError node-fault must also be internal-failure (strict subset)",
        );
    }

    /// `AssignedProofBlockMissing` is the soft sibling of
    /// `BlockBoundsLookupError`, emitted from the
    /// `CachedDataRoots.block_set` walk in `get_assigned_ingress_proofs`.
    /// That set is fork-spanning, so a missing hash there means a side fork
    /// was pruned — not a local-index inconsistency. Must classify as
    /// internal-failure (block parks in cache) but explicitly NOT a node
    /// fault (no restart; peer is innocent).
    #[test]
    fn assigned_proof_block_missing_is_internal_but_not_node_fault() {
        let err = PreValidationError::AssignedProofBlockMissing {
            block_hash: H256::zero(),
            tx_id: H256::zero(),
        };
        assert!(
            err.is_internal_failure(),
            "AssignedProofBlockMissing must classify as internal so the block parks in cache",
        );
        assert!(
            !err.is_node_fault(),
            "AssignedProofBlockMissing is a fork-determinism gap in block_set, NOT a node fault — \
             aborting the node here would self-DoS on every pruned side-fork data_root",
        );
    }

    /// Round-trip the new soft variant through the
    /// `From<PreValidationError> for ValidationResult` dispatcher: it must
    /// land on `ValidationResult::InternalFailure`, never `Invalid` or a
    /// panic. The wrapped `InternalFailureError::is_node_fault()` must also
    /// report `false` so the `send_validation_result` panic-guard does not
    /// fire on this path.
    #[test]
    fn assigned_proof_block_missing_routes_to_internal_failure() {
        use crate::block_tree_service::ValidationResult;

        let err = PreValidationError::AssignedProofBlockMissing {
            block_hash: H256::zero(),
            tx_id: H256::zero(),
        };
        let result: ValidationResult = err.into();
        match result {
            ValidationResult::InternalFailure(inner) => {
                assert!(
                    !inner.is_node_fault(),
                    "AssignedProofBlockMissing must round-trip as a non-node-fault InternalFailure",
                );
            }
            other => panic!(
                "AssignedProofBlockMissing must route to InternalFailure, got {:?}",
                other
            ),
        }
    }

    /// Consensus-validation variants must NOT classify as node faults — they
    /// indicate a bad block, not a node problem. Aborting on these would
    /// hand a DoS vector to any peer who sends a bad block.
    #[test]
    fn consensus_variants_are_not_node_faults() {
        assert!(!PreValidationError::BlockSignatureInvalid.is_node_fault());
        assert!(!PreValidationError::VDFCheckpointsInvalid("bad".to_string()).is_node_fault());
        assert!(!PreValidationError::PoALedgerInactive { ledger_id: 99 }.is_node_fault());
    }

    /// `ValidationError::TaskPanicked` is a node fault (verifier thread
    /// crashed); cancellations and parent-missing races are not.
    #[test]
    fn validation_error_node_fault_dispatch() {
        assert!(
            ValidationError::TaskPanicked {
                task: "poa".to_string(),
                details: "x".to_string(),
            }
            .is_node_fault()
        );

        for reason in [
            ValidationCancelReason::HeightDifference,
            ValidationCancelReason::ParentMissing,
            ValidationCancelReason::ChannelClosed,
            ValidationCancelReason::RepeatedCancellation,
        ] {
            assert!(
                !ValidationError::ValidationCancelled { reason }.is_node_fault(),
                "cancellation reason {:?} must not be a node fault",
                reason
            );
        }

        assert!(
            !ValidationError::ParentCommitmentSnapshotMissing {
                block_hash: H256::zero(),
            }
            .is_node_fault()
        );
        assert!(
            !ValidationError::ParentEpochSnapshotMissing {
                block_hash: H256::zero(),
            }
            .is_node_fault()
        );
        assert!(
            !ValidationError::ParentEmaSnapshotMissing {
                block_hash: H256::zero(),
            }
            .is_node_fault()
        );
        assert!(
            !ValidationError::ParentBlockMissing {
                block_hash: H256::zero(),
            }
            .is_node_fault()
        );

        // PreValidation delegation.
        assert!(
            ValidationError::PreValidation(PreValidationError::DatabaseError {
                error: "mdbx".to_string(),
            })
            .is_node_fault()
        );
        assert!(
            !ValidationError::PreValidation(PreValidationError::BlockSignatureInvalid)
                .is_node_fault()
        );

        // Local cache evicted the payload-wait receiver under sync load
        // — soft internal failure, NOT a node fault. The earlier
        // `ExecutionPayloadUnavailable` variant misclassified this as a
        // fault and self-DoS'd healthy nodes during catch-up; the
        // replacement variant must route to `InternalFailure` so the
        // block parks in cache for retry instead of panicking the node.
        let cache_evicted = ValidationError::ExecutionPayloadCacheEvicted {
            evm_block_hash: Default::default(),
        };
        assert!(
            !cache_evicted.is_node_fault(),
            "ExecutionPayloadCacheEvicted is local saturation, must NOT be a node fault — \
             panicking here would self-DoS healthy nodes under catch-up sync load",
        );
        assert!(
            cache_evicted.is_internal_failure(),
            "ExecutionPayloadCacheEvicted is a soft local failure, must be internal",
        );
        // Round-trip through the From dispatcher: must land on
        // InternalFailure with the wrapped `is_node_fault() == false`
        // (so `send_validation_result`'s panic-guard does NOT fire).
        let result: ValidationResult = cache_evicted.into();
        match result {
            ValidationResult::InternalFailure(inner) => {
                assert!(
                    !inner.is_node_fault(),
                    "ExecutionPayloadCacheEvicted wrapped in InternalFailure must NOT \
                     report is_node_fault() = true — that would re-introduce the panic loop",
                );
            }
            other => panic!(
                "ExecutionPayloadCacheEvicted must round-trip to InternalFailure, got {:?}",
                other,
            ),
        }

        // Hard local I/O failure inside shadow-tx generation — node
        // fault. Routing as `is_node_fault()` is what makes a corrupt
        // MDBX / failed parent-header DB read abort+restart rather
        // than letting unprovable-but-not-bad blocks accumulate in the
        // validation cache.
        let shadow_node_fault = ValidationError::ShadowTxNodeFault("mdbx I/O".to_string());
        assert!(shadow_node_fault.is_node_fault());
        assert!(
            shadow_node_fault.is_internal_failure(),
            "node-fault must also be internal-failure (strict subset)",
        );
        let result: ValidationResult = shadow_node_fault.into();
        assert!(matches!(result, ValidationResult::InternalFailure(_)));
    }

    /// `ShadowTxNodeFault` routes to `InternalFailure` (validity unknown) and
    /// must preserve `is_node_fault() == true` on the wrapped inner error
    /// so the `InternalFailureError::is_node_fault()` accessor — used by the
    /// validation-result handler to trigger panic+restart — fires for genuine
    /// local DB corruption.
    #[test]
    fn shadow_tx_node_fault_roundtrip_preserves_classification() {
        // Hard local fault: round-trip must land in InternalFailure AND the
        // wrapped error must still classify as a node fault.
        let hard = ValidationError::ShadowTxNodeFault("mdbx".to_string());
        assert!(
            hard.is_node_fault(),
            "ShadowTxNodeFault must be a node fault"
        );
        assert!(
            hard.is_internal_failure(),
            "node-fault must also be internal-failure (strict subset)",
        );
        let hard_result: ValidationResult = hard.into();
        match hard_result {
            ValidationResult::InternalFailure(inner) => {
                assert!(
                    inner.is_node_fault(),
                    "inner InternalFailureError must preserve is_node_fault() = true \
                     so the handler triggers panic+restart on a corrupt MDBX read",
                );
                assert!(
                    matches!(inner.err(), ValidationError::ShadowTxNodeFault(_)),
                    "wrapped variant must remain ShadowTxNodeFault, got {:?}",
                    inner.err(),
                );
            }
            other => panic!(
                "ShadowTxNodeFault must round-trip to InternalFailure, got {:?}",
                other
            ),
        }
    }
}

/// Tests for the typed-error → `ValidationError` dispatch used inside
/// `generate_expected_shadow_transactions`. These tests exercise the
/// production `classify_shadow_tx_gen_err` and `classify_commitment_refund_err`
/// functions directly — they are NOT a copy of the mapping. If the
/// production mapping drifts (e.g. a future variant is added and
/// misclassified), these assertions fail at test time, not in production.
///
/// SAFETY: the mappings are the single point where producer-side typed
/// failures are translated to validator-side `ValidationResult` semantics.
/// Misclassifying e.g. `TreasuryArithmetic` as a node fault would cause a
/// validator to panic+restart on every peer block with bad fees — a DoS
/// vector. Misclassifying `SnapshotInvariant` as consensus would
/// peer-attribute a local corruption.
#[cfg(test)]
mod shadow_tx_gen_error_dispatch_tests {
    use super::*;
    use crate::block_tree_service::ValidationResult;
    use crate::commitment_refunds::CommitmentRefundError;
    use crate::shadow_tx_generator::ShadowTxGenError;

    /// `SnapshotInvariant` → `ShadowTxNodeFault` → `InternalFailure` with
    /// `is_node_fault() == true`. Triggers panic+restart so a corrupted
    /// local snapshot can't silently keep producing/validating bad blocks.
    #[test]
    fn snapshot_invariant_maps_to_node_fault() {
        let err =
            classify_shadow_tx_gen_err(ShadowTxGenError::SnapshotInvariant("bad iter type".into()));
        assert!(matches!(err, ValidationError::ShadowTxNodeFault(_)));
        assert!(err.is_node_fault());
        let result: ValidationResult = err.into();
        match result {
            ValidationResult::InternalFailure(inner) => {
                assert!(
                    inner.is_node_fault(),
                    "must preserve node-fault classification"
                );
            }
            other => panic!("SnapshotInvariant must route to InternalFailure, got {other:?}"),
        }
    }

    /// `SnapshotTreasuryUnderflow` → `ShadowTxNodeFault` → node-fault
    /// `InternalFailure`. The deduction amount in the payout helper is
    /// derived from a local snapshot (expired-ledger payout or
    /// commitment-refund); an underflow means our snapshot disagrees with
    /// the inherited treasury, which two honest nodes cannot reach. Must
    /// route to node fault (loud restart) — never to consensus rejection
    /// (silent canonical-fork).
    #[test]
    fn snapshot_treasury_underflow_maps_to_node_fault() {
        let err = classify_shadow_tx_gen_err(ShadowTxGenError::SnapshotTreasuryUnderflow(
            "cannot pay 10 from balance 5".into(),
        ));
        assert!(matches!(err, ValidationError::ShadowTxNodeFault(_)));
        assert!(err.is_node_fault());
        // The wrapping prefix must be present so logs distinguish
        // snapshot-treasury underflows from other snapshot invariants.
        assert!(
            err.to_string().contains("snapshot treasury underflow"),
            "must carry the snapshot-treasury-underflow prefix for log disambiguation: {err}",
        );
        let result: ValidationResult = err.into();
        match result {
            ValidationResult::InternalFailure(inner) => {
                assert!(
                    inner.is_node_fault(),
                    "must preserve node-fault classification"
                );
            }
            other => panic!(
                "SnapshotTreasuryUnderflow must route to InternalFailure(node-fault), got {other:?}",
            ),
        }
    }

    /// `TreasuryArithmetic` → `ShadowTransactionInvalid` → `Invalid`. The
    /// peer's block carries fees whose arithmetic over/underflows; this is
    /// a structural defect of the peer's block, not a local fault. Routes
    /// as consensus rejection — block gets peer-attributed.
    #[test]
    fn treasury_arithmetic_maps_to_consensus() {
        let err = classify_shadow_tx_gen_err(ShadowTxGenError::TreasuryArithmetic(
            "overflow adding term_fee_treasury".into(),
        ));
        assert!(matches!(err, ValidationError::ShadowTransactionInvalid(_)));
        assert!(
            !err.is_node_fault(),
            "consensus rejection must NOT be a node fault"
        );
        let result: ValidationResult = err.into();
        assert!(
            matches!(result, ValidationResult::Invalid(_)),
            "TreasuryArithmetic must route to Invalid (consensus rejection)",
        );
    }

    /// `Structural` (e.g. publish-ledger tx missing perm_fee, or a fee
    /// constructor rejecting peer-supplied values) → `ShadowTransactionInvalid`
    /// → `Invalid`. Peer-attributable.
    #[test]
    fn structural_maps_to_consensus() {
        let err = classify_shadow_tx_gen_err(ShadowTxGenError::Structural(
            "publish ledger tx missing perm_fee".into(),
        ));
        assert!(matches!(err, ValidationError::ShadowTransactionInvalid(_)));
        assert!(!err.is_node_fault());
        let result: ValidationResult = err.into();
        assert!(matches!(result, ValidationResult::Invalid(_)));
    }

    /// `CommitmentRefundError::SnapshotInvariant` → `ShadowTxNodeFault` →
    /// node-fault `InternalFailure`. A snapshot whose unpledge has
    /// `pledge_count_before_executing == 0` is internally inconsistent;
    /// retry can't heal it.
    #[test]
    fn commitment_refund_snapshot_invariant_maps_to_node_fault() {
        let err = classify_commitment_refund_err(CommitmentRefundError::SnapshotInvariant(
            "pledge_count_before_executing = 0".into(),
        ));
        assert!(matches!(err, ValidationError::ShadowTxNodeFault(_)));
        assert!(err.is_node_fault());
        // The wrapping prefix "commitment refund invariant:" must be present
        // so logs distinguish refund-derived faults from shadow-tx-generator
        // faults.
        assert!(
            err.to_string().contains("commitment refund invariant"),
            "must carry the refund-invariant prefix for log disambiguation: {err}",
        );
        let result: ValidationResult = err.into();
        match result {
            ValidationResult::InternalFailure(inner) => {
                assert!(inner.is_node_fault());
            }
            other => panic!(
                "CommitmentRefundError must route to InternalFailure(node-fault), got {other:?}",
            ),
        }
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

/// MERGE-BLOCKER(C1) REGRESSION TEST MODULE
///
/// Captures the C1 vulnerability documented at `MERGE-BLOCKER(C1)` inside
/// `get_data_poa_bounds_with_block_tree_fallback`: for a non-canonical
/// lineage whose ancestry descends below `block_tree`'s window, the
/// height-only canonical-index fallback either returns wrong-fork bounds
/// or escalates to `BlockBoundsLookupError` → `NodeFault` → remote-triggerable
/// panic. The test below is `#[ignore]`d until the merge agent fixes C1
/// per the production marker — at which point the `#[ignore]` is removed
/// and the test must pass.
#[cfg(test)]
mod c1_side_fork_regression_tests {
    use super::*;
    use irys_domain::{
        BlockIndex, BlockMetadata, BlockState, BlockTree, ChainState, EmaSnapshot,
        dummy_epoch_snapshot,
    };
    use irys_testing_utils::IrysBlockHeaderTestExt as _;
    use irys_testing_utils::new_mock_signed_header;
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::{
        BlockBody, BlockHash, BlockIndexItem, DataLedger, DataTransactionLedger, DbSyncMode, H256,
        H256List, IrysBlockHeader, LedgerIndexItem, SealedBlock,
    };
    use std::sync::{Arc, RwLock};
    use std::time::SystemTime;

    /// Build a `DataTransactionLedger` entry with the given `total_chunks`
    /// and a freshly-randomised `tx_root`. The `tx_root` randomisation is
    /// load-bearing: under C1, the buggy fallback returns the *canonical*
    /// block's `tx_root` at the anchor height. If the side-fork's `tx_root`
    /// equalled canonical's, we wouldn't catch the wrong-fork output.
    fn ledger_with_total(ledger: DataLedger, total_chunks: u64) -> DataTransactionLedger {
        DataTransactionLedger {
            ledger_id: ledger.into(),
            tx_root: H256::random(),
            tx_ids: H256List::new(),
            total_chunks,
            expires: None,
            proofs: None,
            required_proof_count: None,
        }
    }

    /// Build a synthetic side-fork header at the given height with the
    /// given `previous_block_hash` and Submit total. The header is signed
    /// so `SealedBlock::new` accepts it.
    fn side_fork_header(
        height: u64,
        previous_block_hash: BlockHash,
        submit_total_chunks: u64,
    ) -> IrysBlockHeader {
        let mut h = IrysBlockHeader::new_mock_header();
        h.height = height;
        h.previous_block_hash = previous_block_hash;
        // Replace the default ledgers with explicit Submit/Publish totals so
        // the side fork's data ledger is distinguishable from canonical.
        h.data_ledgers = vec![
            ledger_with_total(DataLedger::Publish, 0),
            ledger_with_total(DataLedger::Submit, submit_total_chunks),
        ];
        h.test_sign(); // re-signs after mutation
        h
    }

    /// Directly inject `header` into `block_tree.blocks` (the field is
    /// `pub`), bypassing `add_common`'s parent-presence check. This is the
    /// only way to construct the C1 scenario in a unit test: a side-fork
    /// block whose `previous_block_hash` points outside `block_tree`'s
    /// window. In production this state arises when migration prunes the
    /// side fork's ancestors out of `block_tree`.
    fn inject_block(tree: &mut BlockTree, header: IrysBlockHeader) {
        let block_hash = header.block_hash;
        let body = BlockBody {
            block_hash,
            ..Default::default()
        };
        let sealed = Arc::new(
            SealedBlock::new(header.clone(), body)
                .expect("synthetic side-fork block must seal successfully"),
        );
        let entry = BlockMetadata {
            block: sealed,
            chain_state: ChainState::NotOnchain(BlockState::ValidBlock),
            timestamp: SystemTime::now(),
            children: std::collections::HashSet::new(),
            epoch_snapshot: dummy_epoch_snapshot(),
            commitment_snapshot: Arc::new(CommitmentSnapshot::default()),
            ema_snapshot: EmaSnapshot::genesis(&header),
        };
        tree.blocks.insert(block_hash, entry);
    }

    /// MERGE-BLOCKER(C1) REGRESSION TEST
    ///
    /// This test must pass before the `fix-cdr-block-set` merge ships. It
    /// is currently ignored because the C1 vulnerability is unfixed — the
    /// production code at `get_data_poa_bounds_with_block_tree_fallback`'s
    /// `None =>` arm still uses a height-only canonical-index fallback
    /// that produces wrong-fork bounds or escalates to NodeFault on
    /// out-of-range.
    ///
    /// When the merge agent fixes C1 (per the `MERGE-BLOCKER(C1)` comment
    /// in `block_validation.rs`), REMOVE the `#[ignore]` and verify this
    /// test passes. The expected outcome is documented in the assertions
    /// below — the side-fork PoA must NEVER produce `BlockBoundsLookupError`
    /// (NodeFault) and must NEVER produce a successful return rooted on
    /// canonical-chain bounds at the anchor height.
    ///
    /// Failure mode under current (buggy) code: returns
    /// `Ok(BlockBounds { start_chunk_offset: 20, end_chunk_offset: 30,
    /// tx_root: <side_fork's> })` — the height-only fallback at
    /// `curr_height - 1 = 1` (canonical height 1) sources `prev_total = 20`
    /// from the *canonical* chain, then combines that canonical-derived
    /// start with the side fork's own (curr_total, tx_root) from S to
    /// produce silently wrong-fork bounds. The PoA then validates against
    /// the side-fork's tx_root for an offset whose start is anchored on
    /// canonical history — a fork-deterministic violation. The other
    /// failure shape (when the chosen offset exceeds the canonical
    /// anchor's `anchor_max`) is `BlockBoundsLookupError` → NodeFault →
    /// remote-triggerable panic.
    #[ignore = "C1 not yet fixed — see MERGE-BLOCKER(C1) in block_validation.rs"]
    #[test]
    fn c1_side_fork_below_block_tree_window_must_not_node_fault() {
        // --- Build a 2-block canonical chain in block_index ----------------
        // Heights 0, 1. Genesis Submit total = 10. Canonical height 1
        // Submit total = 20. The canonical block at height 1 has a hash
        // DIFFERENT from any side-fork block.
        let tmp_dir = TempDirBuilder::new()
            .prefix("c1_side_fork_regression")
            .build();
        let db_env = irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db(
            tmp_dir.path(),
            DbSyncMode::UtterlyNoSync,
        )
        .expect("open db");
        let db = irys_types::DatabaseProvider(Arc::new(db_env));
        let block_index = BlockIndex::new_for_testing(db);

        let canonical_genesis_hash = BlockHash::random();
        let canonical_h1_hash = BlockHash::random();
        block_index
            .push_item(
                &BlockIndexItem {
                    block_hash: canonical_genesis_hash,
                    num_ledgers: 2,
                    ledgers: vec![
                        LedgerIndexItem {
                            total_chunks: 0,
                            tx_root: H256::random(),
                            ledger: DataLedger::Publish,
                        },
                        LedgerIndexItem {
                            total_chunks: 10,
                            tx_root: H256::random(),
                            ledger: DataLedger::Submit,
                        },
                    ],
                },
                0,
            )
            .expect("push genesis index item");
        block_index
            .push_item(
                &BlockIndexItem {
                    block_hash: canonical_h1_hash,
                    num_ledgers: 2,
                    ledgers: vec![
                        LedgerIndexItem {
                            total_chunks: 0,
                            tx_root: H256::random(),
                            ledger: DataLedger::Publish,
                        },
                        LedgerIndexItem {
                            total_chunks: 20,
                            tx_root: H256::random(),
                            ledger: DataLedger::Submit,
                        },
                    ],
                },
                1,
            )
            .expect("push canonical h1 index item");

        // --- Build a block_tree containing the side-fork block S -----------
        // S is at height 2, its `previous_block_hash` is a phantom hash
        // (NOT in block_tree, NOT in block_index) — simulating an ancestor
        // that has been pruned out of block_tree's window. S claims a
        // Submit total_chunks = 30 (the side fork's actual chain has more
        // submit chunks than canonical at height 1).
        let consensus_config = ConsensusConfig::testing();
        let genesis_for_tree = new_mock_signed_header();
        let mut tree = BlockTree::new(&genesis_for_tree, consensus_config);

        let phantom_parent_hash = BlockHash::random();
        let side_fork_block = side_fork_header(2, phantom_parent_hash, 30);
        let side_fork_hash = side_fork_block.block_hash;
        let side_fork_submit_tx_root =
            side_fork_block.data_ledgers[DataLedger::Submit as usize].tx_root;
        inject_block(&mut tree, side_fork_block);

        // --- Call the helper with the side-fork's parent identity ----------
        let block_index_guard = BlockIndexReadGuard::new(block_index);
        let block_tree_guard = BlockTreeReadGuard::new(Arc::new(RwLock::new(tree)));

        // Pick an offset in the gap between canonical height-1's Submit
        // total (20) and the side fork's claimed total (30). The walk-back
        // from S → phantom_parent_hash is None, so the fallback fires at
        // `curr_height - 1 = 1`. At canonical height 1 the Submit total
        // is 20, so offset 25 fails `ensure!(25 < 20)` →
        // BlockBoundsLookupError (NodeFault → panic).
        let ledger_chunk_offset: u64 = 25;
        let result = get_data_poa_bounds_with_block_tree_fallback(
            &block_index_guard,
            &block_tree_guard,
            side_fork_hash,
            2,
            DataLedger::Submit,
            ledger_chunk_offset,
        );

        // --- Assertions ----------------------------------------------------
        // After C1 fix: result must be EITHER (a) a clean consensus-class
        // rejection (`PoAChunkOffsetOutOfBlockBounds` / similar peer-
        // attributable variant), OR (b) an explicit "off canonical lineage"
        // outcome. It must NEVER be `BlockBoundsLookupError` (NodeFault),
        // and it must NEVER be `Ok(_)` with bounds rooted on canonical
        // (silent wrong-fork validation).
        match result {
            Err(PreValidationError::BlockBoundsLookupError(msg)) => {
                panic!(
                    "C1: side-fork PoA escalated to BlockBoundsLookupError (NodeFault → panic). \
                     A peer can remote-trigger node restart by gossiping a side-fork with \
                     out-of-range PoA offset. Error message: {msg}",
                );
            }
            Ok(bb) => {
                // The side-fork's predecessor at `curr_height - 1 = 1` is
                // `phantom_parent_hash` — NOT `canonical_h1_hash`. Any
                // `Ok(_)` here is wrong-fork: the production walk sourced
                // `prev_total` from canonical block_index at height 1
                // (whose hash is `canonical_h1_hash`), then combined that
                // canonical-derived start with the side fork's own
                // `curr_total` / `tx_root` from S. Two honest peers with
                // different local views would disagree about whether
                // `ledger_chunk_offset` is in-range, breaking fork
                // determinism. The side-fork's tx_root in the result
                // confirms the Frankenstein nature of the bounds —
                // canonical start + side-fork end + side-fork tx_root.
                let _ = side_fork_submit_tx_root;
                let _ = canonical_h1_hash;
                panic!(
                    "C1: side-fork PoA silently produced wrong-fork bounds. The walk-back from \
                     S → phantom_parent_hash hit the C1 fallback at curr_height - 1 = 1 and \
                     sourced prev_total from the CANONICAL block_index entry at height 1 — \
                     even though the side fork's ancestor at that height is phantom_parent_hash, \
                     not canonical_h1_hash. Returned BlockBounds \
                     {{ height: {}, start: {}, end: {}, tx_root: {} }}. After the C1 fix this \
                     must return an `Err` (off-lineage or out-of-bounds), never Ok.",
                    bb.height, bb.start_chunk_offset, bb.end_chunk_offset, bb.tx_root,
                );
            }
            Err(other) => {
                // Acceptable outcomes after the C1 fix: any non-NodeFault
                // error. PoAChunkOffsetOutOfBlockBounds is the cleanest,
                // but a new "off canonical lineage" variant is also OK.
                assert!(
                    !other.is_node_fault(),
                    "C1: side-fork PoA produced a node-fault error {other:?}. \
                     After the fix this must be a non-node-fault outcome.",
                );
            }
        }
    }
}

#[cfg(test)]
mod perm_fee_threshold_tests {
    use super::*;
    use irys_types::U256;
    use rstest::rstest;

    fn tx_id() -> H256 {
        H256::from_slice(&[0xab; 32])
    }

    /// `perm_fee` well above the expected threshold is accepted.
    #[test]
    fn above_threshold_is_ok() {
        let expected = U256::from(1_000_u64);
        let actual = Some(BoundedFee::new(U256::from(2_000_u64)));
        assert!(check_perm_fee_sufficient(tx_id(), actual, expected).is_ok());
    }

    /// Boundary: `actual == expected` is accepted. The comparison uses strict
    /// less-than, mirroring the original inlined check in `validate_publish_price`.
    #[test]
    fn at_threshold_is_ok() {
        let expected = U256::from(1_000_u64);
        let actual = Some(BoundedFee::new(U256::from(1_000_u64)));
        assert!(check_perm_fee_sufficient(tx_id(), actual, expected).is_ok());
    }

    /// `actual = expected - 1` is rejected and carries the expected/actual
    /// values verbatim.
    #[test]
    fn just_below_threshold_is_err_with_fields() {
        let expected = U256::from(1_000_u64);
        let actual_amount = U256::from(999_u64);
        let actual = Some(BoundedFee::new(actual_amount));
        let id = tx_id();
        let err = check_perm_fee_sufficient(id, actual, expected)
            .expect_err("below-threshold perm_fee must be rejected");
        match err {
            PreValidationError::InsufficientPermFee {
                tx_id: got_id,
                expected: got_expected,
                actual: got_actual,
            } => {
                assert_eq!(got_id, id);
                assert_eq!(got_expected, expected);
                assert_eq!(got_actual, actual_amount);
            }
            other => panic!("expected InsufficientPermFee, got {other:?}"),
        }
    }

    /// `perm_fee == 0` (explicit) against a non-zero expected is rejected.
    #[test]
    fn zero_perm_fee_is_err() {
        let expected = U256::from(1_u64);
        let actual = Some(BoundedFee::zero());
        let err = check_perm_fee_sufficient(tx_id(), actual, expected)
            .expect_err("zero perm_fee must be rejected against non-zero expected");
        assert!(matches!(
            err,
            PreValidationError::InsufficientPermFee { .. }
        ));
    }

    /// Missing `perm_fee` (`None`) is treated as zero — rejected against a
    /// non-zero expected. Preserves the `unwrap_or(BoundedFee::zero())`
    /// behaviour of the inlined check.
    #[test]
    fn missing_perm_fee_is_err() {
        let expected = U256::from(1_u64);
        let err = check_perm_fee_sufficient(tx_id(), None, expected)
            .expect_err("missing perm_fee must be rejected against non-zero expected");
        match err {
            PreValidationError::InsufficientPermFee { actual, .. } => {
                assert_eq!(actual, U256::zero());
            }
            other => panic!("expected InsufficientPermFee, got {other:?}"),
        }
    }

    /// Expected threshold of zero accepts anything, including missing perm_fee.
    #[rstest]
    #[case::missing(None)]
    #[case::zero(Some(BoundedFee::zero()))]
    #[case::large(Some(BoundedFee::new(U256::MAX)))]
    fn zero_expected_accepts_anything(#[case] actual: Option<BoundedFee>) {
        assert!(check_perm_fee_sufficient(tx_id(), actual, U256::zero()).is_ok());
    }

    /// Extreme low (1) against a high expected is rejected.
    #[test]
    fn extreme_low_perm_fee_is_err() {
        let expected = U256::from(u128::MAX);
        let actual = Some(BoundedFee::new(U256::from(1_u64)));
        assert!(check_perm_fee_sufficient(tx_id(), actual, expected).is_err());
    }
}

/// Inner error type for [`recall_recall_range_is_valid`], distinguishing a
/// local VDF-state gap from a genuine consensus mismatch.
#[derive(Debug, Error)]
pub enum RecallRangeError {
    /// Local VDF state doesn't yet contain the requested steps.
    /// Local-state issue — re-validate after VDF catches up.
    #[error("local VDF steps unavailable: {0}")]
    StepsUnavailable(eyre::Report),
    /// Recall range in the block does not match the computed range.
    /// Consensus mismatch — peer-attributable.
    #[error("recall range mismatch: {0}")]
    Mismatch(eyre::Report),
}

impl RecallRangeError {
    /// Returns `true` when the error reflects a local node-side condition
    /// (VDF state hasn't caught up) rather than a peer-attributable defect.
    pub fn is_internal(&self) -> bool {
        matches!(self, Self::StepsUnavailable(_))
    }
}

/// Returns Ok if the vdf recall range in the block is valid
pub async fn recall_recall_range_is_valid(
    block: &IrysBlockHeader,
    config: &ConsensusConfig,
    steps_guard: &VdfStateReadonly,
) -> Result<(), RecallRangeError> {
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
    let steps = steps_guard
        .read()
        .get_steps(ii(
            reset_step_number,
            block.vdf_limiter_info.global_step_number,
        ))
        .map_err(RecallRangeError::StepsUnavailable)?;
    irys_efficient_sampling::recall_range_is_valid(
        (block.poa.partition_chunk_offset as u64 / config.num_chunks_in_recall_range) as usize,
        num_recall_ranges_in_partition as usize,
        &steps,
        &block.poa.partition_hash,
    )
    .map_err(RecallRangeError::Mismatch)
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

/// Resolve PoA chunk bounds for a data-PoA at `parent_height`, falling back
/// to `block_tree` when `block_index` doesn't yet have the parent (the
/// un-migrated window between canonical tip and `tip - block_migration_depth`).
///
/// Returns `BlockBounds` for the block that introduced the chunk at
/// `ledger_chunk_offset`: `start_chunk_offset` is the predecessor's total
/// chunks at this ledger, `end_chunk_offset` is the introducing block's total,
/// and `tx_root` is the introducing block's tx_root for the ledger.
///
/// ## Lock ordering
/// Acquires `block_tree.read()` before `block_index.read()` when both are
/// needed mid-fallback. Mirrors the writer order in
/// `block_tree_service::on_block_validation_finished` and the reader order in
/// `p2p::block_pool::fcu_markers`, so this helper cannot deadlock against either.
///
/// ## Invariants the fallback relies on
/// - `block_tree_depth > block_migration_depth` (config-enforced in
///   `Config::validate`). Walking backwards from `parent_block_hash`, we
///   are guaranteed to reach the migrated portion before running out of
///   block_tree entries.
fn ledger_entry_in(header: &IrysBlockHeader, ledger: DataLedger) -> Option<(u64, H256)> {
    header
        .data_ledgers
        .iter()
        .find(|l| l.ledger_id == ledger as u32)
        .map(|l| (l.total_chunks, l.tx_root))
}

fn get_data_poa_bounds_with_block_tree_fallback(
    block_index_guard: &BlockIndexReadGuard,
    block_tree_guard: &BlockTreeReadGuard,
    parent_block_hash: BlockHash,
    parent_height: u64,
    ledger: DataLedger,
    ledger_chunk_offset: u64,
) -> Result<BlockBounds, PreValidationError> {
    // Fast path: parent is migrated. Identical disambiguation to the prior
    // implementation — offsets past chain-max and ledger-not-active are
    // consensus-invalid, lookup failures past those checks are local.
    //
    // The hash-mismatch fall-through below is load-bearing in release builds:
    // `block_index.get_item(parent_height)` returns the *canonical* indexed
    // item, which may not be the peer's claimed parent. A peer-submitted block
    // whose `parent_block_hash` is a non-canonical sibling at a migrated
    // `parent_height` would otherwise have its PoA bounds computed against
    // the canonical anchor — wrong fork. `block_tree`'s reorg-abort backstop
    // fires later, but the PoA verdict would already have been derived from
    // a fork-mismatched view. Fall through to the block_tree walk on
    // mismatch so the bounds are anchored on the supplied parent.
    {
        let index = block_index_guard.read();
        if let Some(parent_item) = index.get_item(parent_height)
            && parent_item.block_hash == parent_block_hash
        {
            match parent_item.ledgers.iter().find(|l| l.ledger == ledger) {
                Some(entry) if ledger_chunk_offset >= entry.total_chunks => {
                    return Err(PreValidationError::PoAChunkOffsetOutOfBlockBounds);
                }
                None => {
                    return Err(PreValidationError::PoALedgerInactive {
                        ledger_id: ledger as u32,
                    });
                }
                Some(_) => {}
            }
            return index
                .get_block_bounds_at_height(
                    ledger,
                    LedgerChunkOffset::from(ledger_chunk_offset),
                    parent_height,
                )
                .map_err(|e| PreValidationError::BlockBoundsLookupError(e.to_string()));
        }
        // Either the index doesn't have parent_height yet (un-migrated window)
        // or the indexed canonical at parent_height differs from the supplied
        // parent (non-canonical fork crossing the migration boundary). Either
        // way, fall through to the block_tree walk which anchors on the
        // supplied `parent_block_hash`. Drop the index lock first to keep
        // the tree-then-index ordering when we re-enter for older history.
    }

    // Fallback path: walk the parent chain in block_tree until we find the
    // block whose [prev_total, curr_total) range contains ledger_chunk_offset.
    // If the walk falls off the bottom of block_tree before finding the
    // chunk, delegate the remainder to block_index's binary search (older
    // history is always indexed).
    let tree = block_tree_guard.read();

    // Anchor: parent's data_ledgers[ledger].total_chunks defines the upper
    // bound on which offsets are in-range for the chain ending at this parent.
    let parent_header =
        tree.get_block(&parent_block_hash)
            .ok_or(PreValidationError::ParentNotInCache {
                parent_hash: parent_block_hash,
                expected_height: parent_height,
            })?;
    let (parent_total, parent_tx_root) =
        ledger_entry_in(parent_header, ledger).ok_or(PreValidationError::PoALedgerInactive {
            ledger_id: ledger as u32,
        })?;
    if ledger_chunk_offset >= parent_total {
        return Err(PreValidationError::PoAChunkOffsetOutOfBlockBounds);
    }

    // Walk backwards: at each step `curr` is the candidate introducing block.
    // We accept it when `prev_total <= offset < curr_total`.
    let mut curr: &IrysBlockHeader = parent_header;
    let mut curr_total: u64 = parent_total;
    let mut curr_tx_root: H256 = parent_tx_root;

    loop {
        let prev_hash = curr.previous_block_hash;
        let curr_height = curr.height;

        // prev_total for the predecessor of `curr`:
        //  - genesis (curr_height == 0): no predecessor, prev_total = 0
        //  - predecessor in block_tree: read its data_ledgers entry
        //    (missing entry ⇒ ledger introduced at `curr`, prev_total = 0)
        //  - predecessor not in block_tree: read from block_index
        //    (block_tree_depth > block_migration_depth guarantees this is
        //    always indexed; missing entry for this ledger ⇒ ledger not yet
        //    introduced at that height, prev_total = 0)
        let prev_total: u64 = if curr_height == 0 {
            0
        } else if let Some(prev_header) = tree.get_block(&prev_hash) {
            ledger_entry_in(prev_header, ledger)
                .map(|(total, _)| total)
                .unwrap_or(0)
        } else {
            // Inline `Result` block so `None` from `get_item` propagates as
            // `BlockBoundsLookupError` (node fault) rather than silently
            // collapsing to `prev_total = 0` like a missing ledger entry.
            let lookup: Result<u64, PreValidationError> = {
                let index = block_index_guard.read();
                match index.get_item(curr_height - 1) {
                    // `None` here violates the invariant documented above:
                    // `block_tree_depth > block_migration_depth` guarantees
                    // predecessors below `block_tree`'s window are always
                    // indexed. Surface as a node fault instead of producing
                    // a consensus-valid `BlockBounds` rooted at offset 0 on
                    // a corrupted node.
                    None => Err(PreValidationError::BlockBoundsLookupError(format!(
                        "block_index missing item at height {} (invariant violated: \
                         block_tree_depth > block_migration_depth guarantees this is \
                         always indexed; predecessor of block at height {} unreachable)",
                        curr_height - 1,
                        curr_height
                    ))),
                    // `Some(item)` but no matching ledger entry is legitimate:
                    // the ledger had not yet been introduced at that height.
                    Some(item) => Ok(item
                        .ledgers
                        .iter()
                        .find(|l| l.ledger == ledger)
                        .map(|l| l.total_chunks)
                        .unwrap_or(0)),
                }
            };
            lookup?
        };

        if ledger_chunk_offset >= prev_total {
            // Found: chunk falls in [prev_total, curr_total) for `curr`.
            // `curr_tx_root` was set when we descended into `curr` (or
            // initially from the parent header) — it always reflects the
            // tx_root of the block we're returning.
            return Ok(BlockBounds {
                height: curr_height,
                ledger,
                start_chunk_offset: prev_total,
                end_chunk_offset: curr_total,
                tx_root: curr_tx_root,
            });
        }

        // Descend to predecessor. We already proved `ledger_chunk_offset <
        // parent_total` above, so genesis is unreachable here in well-formed
        // data — but guard against logic errors anyway.
        if curr_height == 0 {
            return Err(PreValidationError::BlockBoundsLookupError(format!(
                "chunk offset {} not located in chain ending at parent {} (height {})",
                ledger_chunk_offset, parent_block_hash, parent_height
            )));
        }

        match tree.get_block(&prev_hash) {
            Some(prev_header) => {
                // We only reach here when `prev_total > 0`. Since `prev_total`
                // was sourced from `ledger_entry_in(prev_header, ledger)` in
                // the same loop iteration, the entry must exist with a real
                // tx_root. Re-look it up and trust it — no defensive defaults
                // that could silently surface H256::zero() as
                // `MerkleProofInvalid` (peer attribution) for a local-state
                // inconsistency.
                let (prev_total_check, prev_tx_root) = ledger_entry_in(prev_header, ledger).expect(
                    "predecessor must have ledger entry: prev_total > 0 was sourced from it",
                );
                debug_assert_eq!(
                    prev_total, prev_total_check,
                    "ledger_entry_in must be stable across re-lookups within a single loop iteration"
                );
                curr_total = prev_total_check;
                curr_tx_root = prev_tx_root;
                curr = prev_header;
            }
            None => {
                // MERGE-BLOCKER(C1): this fallback is NOT fork-deterministic.
                // The 2026-05-20 multi-reviewer audit (CodeRabbit + Codex,
                // both Critical, independent agreement) confirmed:
                //   - For a non-canonical lineage whose ancestry descends
                //     below block_tree's window, this height-only canonical-
                //     index lookup returns bounds for the WRONG block
                //     (canonical hash at that height ≠ walk's `prev_hash`).
                //   - The out-of-range case escalates to
                //     `BlockBoundsLookupError` → `NodeFault` → panic, i.e.
                //     a peer can craft a side-fork block that remote-
                //     triggers a node restart.
                //
                // When merging with branch `fix-cdr-block-set` (4e21e25a),
                // REPLACE this `None =>` arm with a canonical lookup
                // mirroring `tx_inclusion::find_canonical_ledger_range`:
                //   1. Use `curr_height - 1` as the `max_height` anchor.
                //   2. Walk `block_tree` filtered to
                //      `ChainState::Onchain`, fall through to
                //      `MigratedBlockHashes` + `IrysBlockHeaders` for
                //      older history.
                //   3. Return `Ok(None)` on "off canonical lineage" —
                //      caller routes this as a `Consensus` outcome,
                //      NEVER as `BlockBoundsLookupError` / `NodeFault`.
                //   4. Re-evaluate whether `BlockBoundsLookupError`'s
                //      `NodeFault` classification still applies once this
                //      site is the only construction path (paired marker
                //      lives on the variant doc).
                //
                // Until that replacement lands, this arm is the C1
                // vulnerability — DO NOT remove this comment without the
                // replacement.
                //
                // Predecessor is below block_tree's window — delegate the
                // remainder of the search to block_index's binary search,
                // anchored at (curr_height - 1).
                drop(tree);
                let index = block_index_guard.read();
                return index
                    .get_block_bounds_at_height(
                        ledger,
                        LedgerChunkOffset::from(ledger_chunk_offset),
                        curr_height - 1,
                    )
                    .map_err(|e| PreValidationError::BlockBoundsLookupError(e.to_string()));
            }
        }
    }
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
    block_tree_guard: &BlockTreeReadGuard,
    parent_block_hash: BlockHash,
    parent_height: u64,
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

        // Disambiguate at source so the BlockBoundsLookupError variant below
        // only carries genuine local failures (empty index, DB I/O):
        //  - offset past chain-max for this ledger at parent's height
        //                                                  → consensus-invalid
        //  - ledger absent from the parent's indexed item → consensus-invalid
        //    (the chain has no committed data for it yet, or the peer is on
        //    a different fork)
        //
        // Anchored on the parent (`parent_height` / `parent_block_hash`)
        // rather than the local tip so that two honest peers on the same
        // fork produce identical pre-validation outcomes regardless of how
        // far their local indices have advanced past `parent_height`. The
        // bounds lookup is likewise anchored on the parent so the search
        // never consults blocks beyond the parent — if we instead fell
        // through to a latest-tip lookup, the local-tip dependency would
        // re-enter through the back door.
        //
        // When the parent is too recent to be in the block_index yet (the
        // un-migrated window between canonical tip and
        // tip - block_migration_depth), we fall back to walking the parent
        // chain in `block_tree`. The config invariant
        // `block_tree_depth > block_migration_depth` guarantees that the
        // un-migrated window always lives in `block_tree`; older history
        // is always in `block_index`.
        let bb = get_data_poa_bounds_with_block_tree_fallback(
            block_index_guard,
            block_tree_guard,
            parent_block_hash,
            parent_height,
            ledger,
            ledger_chunk_offset,
        )?;
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
///
/// The caller is responsible for fetching `execution_data` (via
/// `ExecutionPayloadCache::wait_for_payload`) so that a local cache
/// disruption can be surfaced as the typed soft variant
/// (`ValidationError::ExecutionPayloadCacheEvicted`) instead of being
/// stringified into `ShadowTransactionInvalid`.
///
/// Returns a typed [`ValidationError`] on failure so the caller can
/// dispatch local/runtime failures to `InternalFailure` and genuine
/// consensus mismatches (`ShadowTransactionInvalid`) to `Invalid`.
/// Payload-structure rejections and the actual-vs-expected match are
/// consensus; downstream lookups inside
/// [`generate_expected_shadow_transactions`] surface their own typed
/// errors (eviction races, local DB/mempool failures) for accurate
/// classification.
#[tracing::instrument(level = "trace", skip_all, fields(block.hash = ?block.block_hash))]
pub async fn shadow_transactions_are_valid(
    config: &Config,
    block_tree_guard: &BlockTreeReadGuard,
    mempool_guard: &MempoolReadGuard,
    block: &IrysBlockHeader,
    db: &DatabaseProvider,
    execution_data: &ExecutionData,
    parent_epoch_snapshot: Arc<EpochSnapshot>,
    parent_commitment_snapshot: Arc<CommitmentSnapshot>,
    block_index: BlockIndex,
    transactions: &BlockTransactions,
) -> Result<(), ValidationError> {
    // Helper: payload-structure rejections are always consensus.
    fn reject(msg: impl Into<String>) -> ValidationError {
        ValidationError::ShadowTransactionInvalid(msg.into())
    }

    let ExecutionData { payload, sidecar } = execution_data.clone();

    let ExecutionPayload::V3(payload_v3) = payload else {
        return Err(reject("irys-reth expects that all payloads are of v3 type"));
    };
    if !payload_v3.withdrawals().is_empty() {
        return Err(reject("withdrawals must always be empty"));
    }

    // Reject any blob gas usage in the payload
    if payload_v3.blob_gas_used != 0 {
        tracing::debug!(
            block.hash = %block.block_hash,
            block.evm_block_hash = %block.evm_block_hash,
            payload.blob_gas_used = payload_v3.blob_gas_used,
            "Rejecting block: blob_gas_used must be zero",
        );
        return Err(reject("block has non-zero blob_gas_used which is disabled"));
    }
    if payload_v3.excess_blob_gas != 0 {
        tracing::debug!(
            block.block_hash = %block.block_hash,
            block.evm_block_hash = %block.evm_block_hash,
            payload.excess_blob_gas = payload_v3.excess_blob_gas,
            "Rejecting block: excess_blob_gas must be zero",
        );
        return Err(reject(
            "block has non-zero excess_blob_gas which is disabled",
        ));
    }

    // Reject any block that carries blob sidecars (EIP-4844).
    // We keep Cancun active but disable blobs/sidecars entirely.
    if let Some(versioned_hashes) = sidecar.versioned_hashes()
        && !versioned_hashes.is_empty()
    {
        tracing::debug!(
            block.block_hash = %block.block_hash,
            block.evm_block_hash = %block.evm_block_hash,
            block.versioned_hashes_len = versioned_hashes.len(),
            "Rejecting block: EIP-4844 blobs/sidecars are not supported",
        );
        return Err(reject(
            "block contains EIP-4844 blobs/sidecars which are disabled",
        ));
    }
    // Requests are disabled: reject if any present or if header-level requests hash is set.
    if let Some(requests) = sidecar.requests()
        && !requests.is_empty()
    {
        tracing::debug!(
            block.block_hash = %block.block_hash,
            block.evm_block_hash = %block.evm_block_hash,
            block.versioned_hashes_len = requests.len(),
            "Rejecting block: EIP-7685 requests which are disabled",
        );
        return Err(reject(
            "block contains EIP-7685 requests which are disabled",
        ));
    }
    // Note: `requests_hash` may be present even when the requests list is empty.
    // Do not reject on presence of the hash alone; only non-empty requests are disallowed.

    // ensure the execution payload timestamp matches the block timestamp
    // truncated to full seconds
    let payload_timestamp: u128 = payload_v3.timestamp().into();
    let block_timestamp_sec: u128 = block.timestamp_secs().as_secs().into();
    if payload_timestamp != block_timestamp_sec {
        return Err(reject(format!(
            "EVM payload timestamp {payload_timestamp} does not match block timestamp {block_timestamp_sec}"
        )));
    }

    let evm_block: Block = payload_v3
        .try_into_block()
        .map_err(|e| reject(format!("payload conversion failed: {e}")))?;

    // Reject presence of EIP-7685 requests via header-level requests_hash as we disable requests.
    if evm_block.header.requests_hash.is_some() {
        tracing::debug!(
            block.block_hash = %block.block_hash,
            block.evm_block_hash = %block.evm_block_hash,
            "Rejecting block: EIP-7685 requests_hash present which is disabled",
        );
        return Err(reject(
            "block contains EIP-7685 requests_hash which is disabled",
        ));
    }

    // 2. Enforce that no EIP-4844 (blob) transactions are present in the block
    for tx in evm_block.body.transactions.iter() {
        if tx.is_eip4844() {
            tracing::debug!(
                block.block_hash = %block.block_hash,
                block.evm_block_hash = %block.evm_block_hash,
                "Rejecting block: contains EIP-4844 transaction which is disabled",
            );
            return Err(reject(
                "block contains EIP-4844 transaction which is disabled",
            ));
        }
    }

    // 3. Extract shadow transactions from the beginning of the block lazily.
    // Per-item errors here (signer recovery, miner mismatch, malformed
    // shadow tx) are payload-level → consensus.
    let txs_slice = &evm_block.body.transactions;
    let block_miner_address: Address = block.miner_address.into();
    let actual_shadow_txs = extract_leading_shadow_txs(txs_slice).map(|res| {
        let (stx, tx_ref) = res?;
        let tx_signer = tx_ref.clone().into_signed().recover_signer()?;
        ensure!(
            block_miner_address == tx_signer,
            "Shadow tx signer is not the miner"
        );
        Ok(stx)
    });

    // 4. Generate expected shadow transactions. This call returns a
    // typed `ValidationError`; local/runtime failures (eviction races,
    // DB I/O, etc.) surface as `is_internal_failure` variants and are
    // propagated unchanged, while peer-attributable structural failures
    // (e.g. missing publish ledger) surface as `ShadowTransactionInvalid`.
    let expected_txs = generate_expected_shadow_transactions(
        config,
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

    // 5. Validate they match — any error here is a consensus mismatch.
    validate_shadow_transactions_match(actual_shadow_txs, expected_txs.into_iter(), block)
        .map_err(|e| reject(e.to_string()))?;

    Ok(())
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

/// Typed error from [`submit_payload_to_reth`]. Distinguishes local EL
/// transport failures (node fault) from genuine consensus rejections of
/// the payload, so the caller can dispatch the correct
/// `ValidationResult` variant instead of bucketing every failure as
/// `Invalid`.
#[derive(Debug, thiserror::Error)]
pub enum SubmitPayloadError {
    /// Local engine-RPC transport failure (HTTP client unreachable, request
    /// failed mid-flight, etc.). The local EL is broken, not the peer's
    /// block — classify as node fault.
    #[error("local reth engine transport failure: {0}")]
    LocalTransport(String),
    /// Payload structure rejected before submission (non-V3 payload,
    /// missing versioned hashes). Consensus rejection.
    #[error("payload structure invalid: {0}")]
    PayloadStructure(String),
    /// Reth's engine returned `PayloadStatusEnum::Invalid` for the payload.
    /// Consensus rejection — the block is genuinely bad.
    #[error("reth rejected payload: {0}")]
    PayloadRejected(String),
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
) -> Result<(), SubmitPayloadError> {
    let ExecutionData { payload, sidecar } = execution_data;

    let ExecutionPayload::V3(payload_v3) = payload else {
        return Err(SubmitPayloadError::PayloadStructure(
            "irys-reth expects that all payloads are of v3 type".to_string(),
        ));
    };

    let versioned_hashes = sidecar
        .versioned_hashes()
        .ok_or_else(|| {
            SubmitPayloadError::PayloadStructure("version hashes must be present".to_string())
        })?
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
            .await
            .map_err(|e| SubmitPayloadError::LocalTransport(e.to_string()))?;
        match payload_status.status {
            alloy_rpc_types_engine::PayloadStatusEnum::Invalid { validation_error } => {
                return Err(SubmitPayloadError::PayloadRejected(validation_error));
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

/// Classify a [`ShadowTxGenError`] into the appropriate
/// [`ValidationError`] variant.
///
/// `ShadowTxGenerator::new` and its iteration operate on already-loaded
/// local data (parent block, snapshots, mempool-resolved txs). The typed
/// `ShadowTxGenError` distinguishes failure classes so each lands on the
/// correct `ValidationResult`. The audited invariant (see
/// `ShadowTxGenError` doc) is:
///   - `SnapshotInvariant` / `SnapshotTreasuryUnderflow` → node fault
///     (local state is corrupt OR the running treasury at point of
///     failure depends on local snapshot state; two honest nodes cannot
///     reach this and we prefer a loud restart to silent canonical-fork).
///   - `TreasuryArithmetic` / `Structural` → consensus rejection
///     (operand is purely peer-supplied AND the running treasury at
///     point of operation has not been mutated by snapshot-derived
///     amounts, so a fault is safely peer-attributable — peer's tx set
///     produced bad fee math or out-of-range fee values).
///   - `Soft` → soft internal (existing retry-plausible fallback).
///
/// SAFETY: this mapping is the single point where producer-side typed
/// failures are translated to validator-side `ValidationResult` semantics.
/// Misclassifying e.g. `TreasuryArithmetic` as a node fault would cause a
/// validator to panic+restart on every peer block with bad fees — a DoS
/// vector. Misclassifying `SnapshotInvariant` as consensus would
/// peer-attribute a local corruption.
pub(crate) fn classify_shadow_tx_gen_err(
    e: crate::shadow_tx_generator::ShadowTxGenError,
) -> ValidationError {
    use crate::shadow_tx_generator::ShadowTxGenError;
    match e {
        ShadowTxGenError::SnapshotInvariant(s) => ValidationError::ShadowTxNodeFault(s),
        ShadowTxGenError::SnapshotTreasuryUnderflow(s) => {
            ValidationError::ShadowTxNodeFault(format!("snapshot treasury underflow: {s}"))
        }
        ShadowTxGenError::TreasuryArithmetic(s) => {
            ValidationError::ShadowTransactionInvalid(format!("treasury arithmetic: {s}"))
        }
        ShadowTxGenError::Structural(s) => ValidationError::ShadowTransactionInvalid(s),
    }
}

/// Classify a [`CommitmentRefundError`] into the appropriate
/// [`ValidationError`] variant.
///
/// Commitment-refund derivation operates on the parent's commitment
/// snapshot — purely local state. Any failure here is a snapshot-invariant
/// violation: the local state is internally inconsistent and retry cannot
/// heal it. Routes to node fault (loud abort+restart) rather than
/// peer-attributing to consensus.
///
/// The `commitment refund invariant:` prefix is load-bearing for log
/// disambiguation between refund-derived faults and shadow-tx-generator
/// faults.
pub(crate) fn classify_commitment_refund_err(
    e: crate::commitment_refunds::CommitmentRefundError,
) -> ValidationError {
    use crate::commitment_refunds::CommitmentRefundError;
    match e {
        CommitmentRefundError::SnapshotInvariant(s) => {
            ValidationError::ShadowTxNodeFault(format!("commitment refund invariant: {s}"))
        }
    }
}

/// Generates expected shadow transactions.
///
/// Returns a typed [`ValidationError`] on failure:
/// - parent/snapshot lookup races surface as `ParentBlockMissing`
///   (internal, retry-plausible);
/// - peer-supplied structural failures (e.g. missing publish ledger)
///   surface as `ShadowTransactionInvalid` (consensus rejection);
/// - hard local I/O failures (DB reads, MDBX corruption) surface as
///   `ShadowTxNodeFault` (internal + node-fault → abort+restart);
/// - other local computation / mempool / snapshot-arithmetic failures
///   surface as `ShadowTxNodeFault` (hard fault, abort+restart).
///
/// The caller's existing `.into()` dispatch handles routing each variant
/// to the correct `ValidationResult`.
#[tracing::instrument(level = "trace", skip_all, err)]
async fn generate_expected_shadow_transactions(
    config: &Config,
    block_tree_guard: &BlockTreeReadGuard,
    mempool_guard: &MempoolReadGuard,
    block: &IrysBlockHeader,
    db: &DatabaseProvider,
    parent_epoch_snapshot: Arc<EpochSnapshot>,
    parent_commitment_snapshot: Arc<CommitmentSnapshot>,
    block_index: BlockIndex,
    transactions: &BlockTransactions,
) -> Result<Vec<ShadowTransaction>, ValidationError> {
    // Helpers: classify common failure shapes.
    let parent_hash = block.previous_block_hash;
    let parent_missing = || ValidationError::ParentBlockMissing {
        block_hash: parent_hash,
    };
    // Hard local I/O failures (DB reads, MDBX corruption) where retry
    // cannot help — DB is broken on this node. Triggers node-fault
    // abort+restart rather than accumulating in cache.
    fn node_fault(err: impl std::fmt::Display) -> ValidationError {
        ValidationError::ShadowTxNodeFault(err.to_string())
    }
    fn consensus(err: impl std::fmt::Display) -> ValidationError {
        ValidationError::ShadowTransactionInvalid(err.to_string())
    }

    // Look up previous block to get EVM hash. The two failure shapes
    // are distinct: a DB I/O error from the in-memory-miss fallback
    // (`db.view_eyre`) is a hard node-local fault — retry cannot heal
    // a broken MDBX — so route through `node_fault` to trigger
    // abort+restart. A `None` result is the eviction race against the
    // in-memory window and surfaces as `ParentBlockMissing` (internal,
    // retry-plausible via depth-prune + re-gossip). Either way this
    // is not a consensus statement about the block.
    let prev_block = crate::block_tree_service::get_block_header(
        block_tree_guard,
        db,
        block.previous_block_hash,
        false,
    )
    .map_err(node_fault)?
    .ok_or_else(parent_missing)?;

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

    // Use pre-fetched data ledger transactions
    let data_txs = transactions.get_ledger_txs(DataLedger::Submit).to_vec();
    let one_year_txs = transactions.get_ledger_txs(DataLedger::OneYear).to_vec();
    let thirty_day_txs = transactions.get_ledger_txs(DataLedger::ThirtyDay).to_vec();

    // Use pre-fetched publish ledger transactions with proofs from block header.
    // `extract_data_ledgers` validates peer-supplied structure → consensus.
    let cascade_active = config
        .consensus
        .hardforks
        .is_cascade_active_for_epoch(&parent_epoch_snapshot);
    let (publish_ledger, _submit_ledger) =
        extract_data_ledgers(block, cascade_active).map_err(consensus)?;
    let publish_ledger_with_txs = PublishLedgerWithTxs {
        txs: transactions.get_ledger_txs(DataLedger::Publish).to_vec(),
        proofs: publish_ledger.proofs.clone(),
    };

    // Get treasury balance from previous block
    let initial_treasury_balance = prev_block.treasury;

    // Calculate expired ledger fees for epoch blocks. The walk transitively
    // performs MDBX reads (block-header lookups via `db.view_eyre` inside
    // `process_boundary_block` / `process_middle_blocks`, plus
    // `get_data_tx_in_parallel`), so a failure is dominantly DB-I/O —
    // `eyre::Result` doesn't let us cleanly distinguish DB faults from
    // pure-logic faults here, and retry can't heal a broken MDBX. Route
    // through `node_fault` (`ShadowTxNodeFault`) rather than `internal`.
    // TODO(P2 follow-up): differentiate DB I/O from logic errors here by
    // converting the helpers' return types to a typed error.
    let expired_ledger_fees = if is_epoch_block {
        let mut result = ledger_expiry::calculate_expired_ledger_fees(
            &parent_epoch_snapshot,
            block.height,
            DataLedger::Submit,
            config,
            block_index.clone(),
            block_tree_guard,
            mempool_guard,
            db,
            true, // expect txs to be promoted — return perm fee refund if not
        )
        .in_current_span()
        .await
        .map_err(node_fault)?;

        // When Cascade is active, also process OneYear and ThirtyDay term ledgers.
        let cascade_active = config
            .consensus
            .hardforks
            .is_cascade_active_for_epoch(&parent_epoch_snapshot);
        if cascade_active {
            for ledger in [DataLedger::OneYear, DataLedger::ThirtyDay] {
                let delta = ledger_expiry::calculate_expired_ledger_fees(
                    &parent_epoch_snapshot,
                    block.height,
                    ledger,
                    config,
                    block_index.clone(),
                    block_tree_guard,
                    mempool_guard,
                    db,
                    false, // no promotion for these ledgers
                )
                .in_current_span()
                .await
                .map_err(node_fault)?;
                result.merge(delta);
            }
        }

        result
    } else {
        ledger_expiry::LedgerExpiryBalanceDelta::default()
    };

    // Compute commitment refund events for epoch blocks from parent's commitment snapshot.
    // Failures here are snapshot-invariant violations — local state is
    // internally inconsistent and retry can't heal it → node fault.
    let commitment_refund_events: Vec<crate::block_producer::UnpledgeRefundEvent> =
        if is_epoch_block {
            crate::commitment_refunds::derive_unpledge_refunds_from_snapshot(
                &parent_commitment_snapshot,
                &config.consensus,
            )
            .map_err(classify_commitment_refund_err)?
        } else {
            Vec::new()
        };
    let unstake_refund_events: Vec<crate::block_producer::UnstakeRefundEvent> = if is_epoch_block {
        crate::commitment_refunds::derive_unstake_refunds_from_snapshot(
            &parent_commitment_snapshot,
            &config.consensus,
        )
        .map_err(classify_commitment_refund_err)?
    } else {
        Vec::new()
    };

    // Deterministic block-invariant: peer-attributable. See
    // `ShadowTransactionInvalid` doc — a tx in the publish ledger that also
    // has a perm_fee refund scheduled is a structural inconsistency in the
    // peer's block (promoted txs must not receive refunds). Routed here
    // (rather than only inside `ShadowTxGenerator::new`) so violations
    // surface as consensus rejection rather than soft-internal
    // `ShadowTxNodeFault` for invariant violations. The constructor keeps an identical guard
    // as defence-in-depth for non-validation callers.
    for tx in &publish_ledger_with_txs.txs {
        for (refund_tx_id, _, _) in &expired_ledger_fees.user_perm_fee_refunds {
            if tx.id == *refund_tx_id {
                return Err(consensus(format!(
                    "Transaction {} is in publish ledger but also has a perm_fee refund scheduled. \
                     Promoted transactions should not receive refunds.",
                    tx.id
                )));
            }
        }
    }

    // Classification rationale: see `classify_shadow_tx_gen_err` doc.
    let mut shadow_tx_generator = ShadowTxGenerator::new(
        &block.height,
        &block.reward_address,
        &block.reward_amount,
        &prev_block,
        &block.solution_hash,
        &config.consensus,
        commitment_txs,
        &data_txs,
        &one_year_txs,
        &thirty_day_txs,
        &publish_ledger_with_txs,
        initial_treasury_balance,
        &expired_ledger_fees,
        &commitment_refund_events,
        &unstake_refund_events,
        &parent_epoch_snapshot,
    )
    .map_err(classify_shadow_tx_gen_err)?;

    let mut shadow_txs_vec = Vec::new();
    for result in shadow_tx_generator.by_ref() {
        let metadata = result.map_err(classify_shadow_tx_gen_err)?;
        shadow_txs_vec.push(metadata.shadow_tx);
    }

    // Get final treasury balance after processing all transactions
    let expected_treasury = shadow_tx_generator.treasury_balance();

    // Treasury mismatch is a peer-attributable consensus rejection — the
    // peer's block claims a treasury value we can prove wrong.
    if block.treasury != expected_treasury {
        return Err(consensus(format!(
            "Treasury mismatch: expected {} but found {} at block height {}",
            expected_treasury, block.treasury, block.height
        )));
    }

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
            && *solution_hash != expected_hash
        {
            eyre::bail!(
                "Invalid solution hash reference in shadow transaction at idx {}. Expected {:?}, got {:?}",
                idx,
                H256::from(*expected_hash),
                H256::from(**solution_hash)
            );
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
        ValidationError::SeedDataInvalid(format!(
            "Expected: {:?}, got: {:?}",
            expected_seed_data, vdf_info
        ))
        .into()
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
                        idx,
                        expected.id(),
                        actual.id()
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
                return Err(ValidationError::CommitmentOrderingFailed(format!(
                    "Internal error: commitment ordering validation mismatch for block {} (height {})",
                    block.block_hash, block.height
                )));
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
        timestamp_secs,
    )
}

/// Helper function to calculate term storage fee using a specific EMA snapshot
/// Uses the same replica count as permanent storage but for the specified number of epochs
pub fn calculate_term_storage_base_network_fee(
    bytes_to_store: u64,
    epochs_for_storage: u64,
    ema_snapshot: &EmaSnapshot,
    config: &Config,
    replica_count: u64,
    timestamp_secs: UnixTimestamp,
) -> eyre::Result<U256> {
    irys_types::storage_pricing::calculate_term_fee(
        bytes_to_store,
        epochs_for_storage,
        &config.consensus,
        replica_count,
        ema_snapshot.ema_for_public_pricing(),
        timestamp_secs,
    )
}

/// Validates that a transaction's `perm_fee` meets the minimum expected amount.
///
/// Extracted from `validate_publish_price` so the threshold comparison can be
/// unit-tested directly. Behaviour is byte-identical to the inlined check:
/// a missing `perm_fee` is treated as zero, and the comparison is strict
/// less-than (so `actual == expected` is accepted as sufficient).
pub(crate) fn check_perm_fee_sufficient(
    tx_id: H256,
    actual_perm_fee: Option<BoundedFee>,
    expected: U256,
) -> Result<(), PreValidationError> {
    let actual = actual_perm_fee.unwrap_or(BoundedFee::zero());
    if actual < expected {
        return Err(PreValidationError::InsufficientPermFee {
            tx_id,
            expected,
            actual: actual.get(),
        });
    }
    Ok(())
}

/// Validates pricing for a transaction targeting the Publish ledger (Submit→Publish promotion path).
/// Checks both term_fee and perm_fee meet minimums, and that fee distribution structures are valid.
fn validate_publish_price(
    tx: &DataTransactionHeader,
    block_height: u64,
    timestamp_secs: UnixTimestamp,
    block_ema: &EmaSnapshot,
    config: &Config,
) -> Result<(), PreValidationError> {
    let epochs_for_storage = irys_types::ledger_expiry::calculate_submit_ledger_expiry(
        block_height,
        config.consensus.epoch.num_blocks_in_epoch,
        config.consensus.epoch.submit_ledger_epoch_length,
    );

    let number_of_ingress_proofs_total = config.number_of_ingress_proofs_total_at(timestamp_secs);
    let expected_term_fee = calculate_term_storage_base_network_fee(
        tx.data_size,
        epochs_for_storage,
        block_ema,
        config,
        number_of_ingress_proofs_total,
        timestamp_secs,
    )
    .map_err(|e| PreValidationError::FeeCalculationFailed(e.to_string()))?;
    let expected_perm_fee = calculate_perm_storage_total_fee(
        tx.data_size,
        expected_term_fee,
        block_ema,
        config,
        timestamp_secs,
    )
    .map_err(|e| PreValidationError::FeeCalculationFailed(e.to_string()))?;

    // Validate perm_fee is at least the expected amount
    check_perm_fee_sufficient(tx.id, tx.perm_fee, expected_perm_fee.amount)?;
    let actual_perm_fee = tx.perm_fee.unwrap_or(BoundedFee::zero());

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
    TermFeeCharges::new(actual_term_fee, &config.consensus).map_err(|e| {
        PreValidationError::InvalidTermFeeStructure {
            tx_id: tx.id,
            reason: e.to_string(),
        }
    })?;

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
}

/// Validates pricing for a term-only ledger transaction (OneYear/ThirtyDay).
/// Term-only txs must not carry a perm_fee and must meet the minimum term_fee.
fn validate_term_only_price(
    tx: &DataTransactionHeader,
    ledger: DataLedger,
    block_height: u64,
    timestamp_secs: UnixTimestamp,
    block_ema: &EmaSnapshot,
    config: &Config,
) -> Result<(), PreValidationError> {
    if tx.perm_fee.is_some() {
        return Err(PreValidationError::TermLedgerTxHasPermFee { tx_id: tx.id });
    }

    let cascade = config
        .consensus
        .hardforks
        .cascade
        .as_ref()
        .ok_or(PreValidationError::CascadeNotConfigured { tx_id: tx.id })?;
    let epoch_length = match ledger {
        DataLedger::OneYear => cascade.one_year_epoch_length,
        DataLedger::ThirtyDay => cascade.thirty_day_epoch_length,
        _ => unreachable!(),
    };
    let epochs_for_storage = irys_types::ledger_expiry::calculate_submit_ledger_expiry(
        block_height,
        config.consensus.epoch.num_blocks_in_epoch,
        epoch_length,
    );

    let expected_term_fee = calculate_term_storage_base_network_fee(
        tx.data_size,
        epochs_for_storage,
        block_ema,
        config,
        config.consensus.num_partitions_per_term_ledger_slot,
        timestamp_secs,
    )
    .map_err(|e| PreValidationError::FeeCalculationFailed(e.to_string()))?;

    let actual_term_fee = tx.term_fee;
    if actual_term_fee < expected_term_fee {
        return Err(PreValidationError::InsufficientTermFee {
            tx_id: tx.id,
            expected: expected_term_fee,
            actual: actual_term_fee.get(),
        });
    }

    TermFeeCharges::new(actual_term_fee, &config.consensus).map_err(|e| {
        PreValidationError::InvalidTermFeeStructure {
            tx_id: tx.id,
            reason: e.to_string(),
        }
    })?;
    Ok(())
}

/// Validates the `expires` field on each data ledger in the block.
/// - Publish: must have `expires == None`
/// - Submit: must have `expires == Some(submit_ledger_epoch_length)`
/// - OneYear/ThirtyDay (when Cascade is active): must match the configured epoch length
fn validate_term_ledger_expiry(
    block: &IrysBlockHeader,
    consensus: &ConsensusConfig,
    cascade_active: bool,
) -> Result<(), PreValidationError> {
    let cascade = consensus.hardforks.cascade.as_ref();

    for dl in &block.data_ledgers {
        let ledger = DataLedger::try_from(dl.ledger_id).map_err(|_| {
            PreValidationError::LedgerIdInvalid {
                ledger_id: dl.ledger_id,
            }
        })?;
        let cascade_config = || {
            cascade.ok_or(PreValidationError::TermLedgerExpiryMismatch {
                ledger_id: dl.ledger_id,
                expected: None,
                actual: dl.expires,
            })
        };
        let expected_expires = match ledger {
            DataLedger::Publish => None,
            DataLedger::Submit => Some(consensus.epoch.submit_ledger_epoch_length),
            DataLedger::OneYear if cascade_active => Some(cascade_config()?.one_year_epoch_length),
            DataLedger::ThirtyDay if cascade_active => {
                Some(cascade_config()?.thirty_day_epoch_length)
            }
            _ => continue, // non-active cascade ledgers handled by presence check
        };
        if dl.expires != expected_expires {
            return Err(PreValidationError::TermLedgerExpiryMismatch {
                ledger_id: dl.ledger_id,
                expected: expected_expires,
                actual: dl.expires,
            });
        }
    }
    Ok(())
}

/// Validates that data transactions in a block are correctly placed and have valid properties
/// based on their ledger placement (Submit or Publish) and ingress proof availability.
/// - Transactions in Publish ledger must have prior inclusion in Submit ledger
/// - Transactions should not appear in multiple blocks (duplicate inclusions)
/// - Submit ledger transactions must not have ingress proofs
/// - Publish ledger transactions must have valid ingress proofs
/// - All transactions must meet minimum fee requirements
/// - Fee structures must be valid for proper reward distribution
/// - Term ledger (OneYear/ThirtyDay) transactions must have valid term fees
#[tracing::instrument(level = "trace", skip_all, err)]
pub async fn data_txs_are_valid(
    config: &Config,
    service_senders: &ServiceSenders,
    block: &IrysBlockHeader,
    db: &DatabaseProvider,
    block_tree_guard: &BlockTreeReadGuard,
    transactions: &BlockTransactions,
    parent_epoch_snapshot: Arc<EpochSnapshot>,
    parent_ema_snapshot: Arc<EmaSnapshot>,
) -> Result<(), PreValidationError> {
    // Extract transaction slices from BlockTransactions
    let submit_txs = transactions.get_ledger_txs(DataLedger::Submit);
    let publish_txs = transactions.get_ledger_txs(DataLedger::Publish);
    let one_year_txs = transactions.get_ledger_txs(DataLedger::OneYear);
    let thirty_day_txs = transactions.get_ledger_txs(DataLedger::ThirtyDay);

    // Structural pre-pass: validate all Submit txs unconditionally
    for tx in submit_txs {
        if tx.ledger_id != DataLedger::Publish as u32 {
            return Err(PreValidationError::InvalidLedgerIdForTx {
                tx_id: tx.id,
                expected: DataLedger::Publish as u32,
                actual: tx.ledger_id,
            });
        }
        if tx.promoted_height().is_some() {
            return Err(PreValidationError::SubmitTxHasPromotedHeight { tx_id: tx.id });
        }
    }

    // Structural pre-pass: validate Publish txs have correct ledger_id
    for tx in publish_txs {
        if tx.ledger_id != DataLedger::Publish as u32 {
            return Err(PreValidationError::InvalidLedgerIdForTx {
                tx_id: tx.id,
                expected: DataLedger::Publish as u32,
                actual: tx.ledger_id,
            });
        }
    }

    // Structural pre-pass: validate term-only ledger txs have correct ledger_id and no perm_fee
    for tx in one_year_txs {
        if tx.ledger_id != DataLedger::OneYear as u32 {
            return Err(PreValidationError::InvalidLedgerIdForTx {
                tx_id: tx.id,
                expected: DataLedger::OneYear as u32,
                actual: tx.ledger_id,
            });
        }
        if tx.perm_fee.is_some() {
            return Err(PreValidationError::TermLedgerTxHasPermFee { tx_id: tx.id });
        }
    }
    for tx in thirty_day_txs {
        if tx.ledger_id != DataLedger::ThirtyDay as u32 {
            return Err(PreValidationError::InvalidLedgerIdForTx {
                tx_id: tx.id,
                expected: DataLedger::ThirtyDay as u32,
                actual: tx.ledger_id,
            });
        }
        if tx.perm_fee.is_some() {
            return Err(PreValidationError::TermLedgerTxHasPermFee { tx_id: tx.id });
        }
    }

    // Cascade activation is derived from the parent epoch snapshot the
    // caller fetched — single source of truth. The previous `cascade_active`
    // bool parameter computed it from a separate snapshot read, which gave
    // two reads where only one was authoritative; the reads always agreed
    // (same parent hash, immutable `Arc<EpochSnapshot>`) but the bool was a
    // footgun for any future caller computing it from a stale snapshot.
    let cascade_active = config
        .consensus
        .hardforks
        .is_cascade_active_for_epoch(&parent_epoch_snapshot);

    // Extract publish ledger for ingress proofs validation
    let (publish_ledger, _submit_ledger) = extract_data_ledgers(block, cascade_active)
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
    let mut txs_to_check: HashMap<H256, (&DataTransactionHeader, TxInclusionState)> =
        HashMap::new();

    // Insert publish + submit txs (with same-block promotion handling)
    for (tx, ledger) in publish_txs
        .iter()
        .map(|x| (x, DataLedger::Publish))
        .chain(submit_txs.iter().map(|x| (x, DataLedger::Submit)))
    {
        let state = if same_block_promotions.contains(&tx.id) {
            TxInclusionState::Found {
                // Same block promotion: both ledgers are in the current block
                ledger_current: (DataLedger::Publish, block.block_hash),
                ledger_historical: (DataLedger::Submit, block.block_hash),
            }
        } else {
            TxInclusionState::Searching {
                ledger_current: ledger,
            }
        };
        txs_to_check.insert(tx.id, (tx, state));
    }

    // Insert term txs with collision detection.
    // A signed tx cannot appear in a term ledger AND Submit/Publish in the same block
    // (ledger_id is part of the signed payload), but we check as defense-in-depth.
    // Note: Submit→Publish same-block promotions are valid and handled above.
    for (tx, ledger) in one_year_txs
        .iter()
        .map(|x| (x, DataLedger::OneYear))
        .chain(thirty_day_txs.iter().map(|x| (x, DataLedger::ThirtyDay)))
    {
        if txs_to_check.contains_key(&tx.id) {
            return Err(PreValidationError::TxInMultipleLedgers { tx_id: tx.id });
        }
        txs_to_check.insert(
            tx.id,
            (
                tx,
                TxInclusionState::Searching {
                    ledger_current: ledger,
                },
            ),
        );
    }

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

    let timestamp_secs = block.timestamp_secs();

    // Validate based on ledger rules and past inclusions
    for (tx, past_inclusion) in txs_to_check.values() {
        match past_inclusion {
            // no past inclusions
            TxInclusionState::Searching { ledger_current } => {
                match ledger_current {
                    DataLedger::Publish => {
                        // check the db — constrained to canonical chain at or before the parent
                        let parent_height = block.height.saturating_sub(1);
                        match tx_header_by_txid_canonical(&ro_tx, &tx.id, parent_height) {
                            Ok(Some(header)) => {
                                warn!(
                                    "had to fetch header {:#?} from DB for {}, (exp: {:#?}) as submit inclusion wasn't within anchor depth",
                                    &header, &tx.id, &tx
                                );
                            }
                            Ok(None) => {
                                // Publish tx with no past inclusion - INVALID
                                return Err(PreValidationError::PublishTxMissingPriorSubmit {
                                    tx_id: tx.id,
                                });
                            }
                            Err(e) => {
                                // Local MDBX I/O failure — not a statement about the peer's
                                // block. Route as NodeFault so the caller aborts rather than
                                // peer-attributing a local DB error as consensus rejection.
                                error!(
                                    "tx_header_by_txid_canonical DB error for tx {}: {}",
                                    &tx.id, &e
                                );
                                return Err(PreValidationError::DatabaseError {
                                    error: e.to_string(),
                                });
                            }
                        }
                    }
                    DataLedger::Submit => {
                        // Submit tx with no past inclusion - VALID (new transaction)
                        validate_publish_price(
                            tx,
                            block.height,
                            timestamp_secs,
                            &parent_ema_snapshot,
                            config,
                        )?;
                        debug!("Transaction {} is new in Submit ledger", tx.id);
                    }
                    DataLedger::OneYear | DataLedger::ThirtyDay => {
                        // Term-only ledger: validate term fee, no promotion
                        validate_term_only_price(
                            tx,
                            *ledger_current,
                            block.height,
                            timestamp_secs,
                            &parent_ema_snapshot,
                            config,
                        )?;
                        debug!(
                            "Transaction {} is new in {:?} ledger",
                            tx.id, ledger_current
                        );
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
                                    validate_publish_price(
                                        tx,
                                        block.height,
                                        timestamp_secs,
                                        &parent_ema_snapshot,
                                        config,
                                    )?;
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
                    DataLedger::OneYear | DataLedger::ThirtyDay => {
                        // Term-only ledger tx should not have any past inclusion
                        return Err(PreValidationError::SubmitTxAlreadyIncluded {
                            tx_id: tx.id,
                            ledger: *ledger_historical,
                            block_hash: *historical_block_hash,
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

    if publish_txs.is_empty()
        && let Some(proofs) = &publish_ledger.proofs
    {
        return Err(PreValidationError::PublishLedgerProofCountMismatch {
            proof_count: proofs.len(),
            tx_count: publish_txs.len(),
        });
    }

    if let Some(proofs_list) = &publish_ledger.proofs {
        // Compute the expected total number of proofs based on the number of
        // publish_tx and the number of proofs_per_tx
        let expected_proof_count = {
            let timestamp_secs = block.timestamp_secs();
            let number_of_ingress_proofs_total =
                config.number_of_ingress_proofs_total_at(timestamp_secs);
            publish_txs.len() * number_of_ingress_proofs_total as usize
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
                block_tree_guard,
                db,
                config,
                &parent_epoch_snapshot,
                Some(&service_senders.chunk_cache),
            )?;

            let timestamp_secs = block.timestamp_secs();
            let mut expected_assigned_proofs =
                config.number_of_ingress_proofs_from_assignees_at(timestamp_secs) as usize;

            // While the protocol can require X number of assigned proofs, if there
            // is less than that many assigned to the slot, it still needs to function.
            if assigned_miners < expected_assigned_proofs {
                warn!(
                    "Clamping expected_assigned_proofs from {} to {} to match number of assigned miners ",
                    expected_assigned_proofs, assigned_miners
                );
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
    cascade_active: bool,
) -> eyre::Result<(&DataTransactionLedger, &DataTransactionLedger)> {
    let (publish_ledger, submit_ledger) = match &block.data_ledgers[..] {
        [publish_ledger, submit_ledger] => {
            ensure!(
                !cascade_active,
                "Post-Cascade blocks must have 4 data ledgers, got 2"
            );
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
        [
            publish_ledger,
            submit_ledger,
            one_year_ledger,
            thirty_day_ledger,
        ] => {
            ensure!(
                cascade_active,
                "Pre-Cascade blocks must have 2 data ledgers, got 4"
            );
            ensure!(
                publish_ledger.ledger_id == DataLedger::Publish,
                "Publish ledger must be the first ledger in the data ledgers"
            );
            ensure!(
                submit_ledger.ledger_id == DataLedger::Submit,
                "Submit ledger must be the second ledger in the data ledgers"
            );
            ensure!(
                one_year_ledger.ledger_id == DataLedger::OneYear,
                "OneYear ledger must be the third ledger in the data ledgers"
            );
            ensure!(
                thirty_day_ledger.ledger_id == DataLedger::ThirtyDay,
                "ThirtyDay ledger must be the fourth ledger in the data ledgers"
            );
            // Validate term-only ledger properties
            ensure!(
                one_year_ledger.proofs.is_none(),
                "OneYear ledger must not have ingress proofs"
            );
            ensure!(
                one_year_ledger.required_proof_count.is_none(),
                "OneYear ledger must not have required_proof_count"
            );
            ensure!(
                thirty_day_ledger.proofs.is_none(),
                "ThirtyDay ledger must not have ingress proofs"
            );
            ensure!(
                thirty_day_ledger.required_proof_count.is_none(),
                "ThirtyDay ledger must not have required_proof_count"
            );
            (publish_ledger, submit_ledger)
        }
        [..] => eyre::bail!(
            "Expected {} data ledgers on the block, got {}",
            if cascade_active { 4 } else { 2 },
            block.data_ledgers.len()
        ),
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
                    .map_err(|e| {
                        PreValidationError::BlockBoundsLookupError(format!(
                            "db.view failed fetching parent block {}: {e}",
                            &block.0
                        ))
                    })?
                    .map_err(|e| {
                        PreValidationError::BlockBoundsLookupError(format!(
                            "block_header_by_hash failed for {}: {e}",
                            &block.0
                        ))
                    })?
                    .ok_or_else(|| {
                        PreValidationError::BlockBoundsLookupError(format!(
                            "parent block {} not found in database",
                            &block.0
                        ))
                    })?;
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
                    TxInclusionState::Found {
                        ledger_current,
                        ledger_historical,
                    } => {
                        if ledger_historical.1 == historical_block_hash
                            && let Some(merged_historical_ledger) =
                                merge_same_block_historical_ledgers(
                                    ledger_historical.0,
                                    ledger_type,
                                )
                        {
                            *state = TxInclusionState::Found {
                                ledger_current: *ledger_current,
                                ledger_historical: (
                                    merged_historical_ledger,
                                    historical_block_hash,
                                ),
                            };
                        } else {
                            // Transaction already found in a different historical block, or in an
                            // invalid same-block combination, so this is a real duplicate.
                            *state = TxInclusionState::Duplicate {
                                ledger_historical: (ledger_type, historical_block_hash),
                            };
                        }
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

/// Block layout is fixed-order (Publish then Submit) per chainspec, so
/// in practice only (Publish, Submit) is reachable. The symmetric arm
/// is defensive against deserialized or future block orderings.
fn merge_same_block_historical_ledgers(
    existing: DataLedger,
    incoming: DataLedger,
) -> Option<DataLedger> {
    match (existing, incoming) {
        (DataLedger::Submit, DataLedger::Publish) | (DataLedger::Publish, DataLedger::Submit) => {
            Some(DataLedger::Publish)
        }
        _ => None,
    }
}

/// FORK-DETERMINISM GAP (TODO, out of scope for this branch):
/// The `block_tree`/`db` lookups inside `get_block_header` are themselves
/// fork-deterministic (content-addressed by `block_hash`), but the SET of
/// hashes we feed into them comes from `CachedDataRoots.block_set`, a global
/// index keyed by `data_root` that records every block — across ALL forks —
/// the local node has ever observed a given data_root in. As a result, the
/// `block_ranges` computed below can include ranges from blocks on a fork
/// the block-being-validated is NOT on, biasing the
/// "any-intersection-with-slot-range" check that drives `assigned_miners`.
///
/// Validation should be parent-deterministic: given `(block, parent
/// snapshots)`, two honest peers on the same fork must produce identical
/// outcomes regardless of which other forks they've witnessed. Walking the
/// parent's submit-ledger lineage instead of indexing globally by data_root
/// is the fix, but it's a larger design pass — tracked separately.
pub fn get_assigned_ingress_proofs(
    tx_proofs: &[IngressProof],
    tx_header: &DataTransactionHeader,
    block_tree: &BlockTreeReadGuard,
    db: &DatabaseProvider,
    config: &Config,
    epoch_snapshot: &EpochSnapshot,
    cache_sender: Option<&crate::cache_service::CacheServiceSender>,
) -> Result<(Vec<IngressProof>, usize), PreValidationError> {
    // Returns (assigned_proofs, assigned_miners)
    let mut assigned_proofs = Vec::new();
    let mut assigned_miners = 0;

    //  a) Get the block hashes from the cached data_root (invariant across all proofs).
    //     NOTE: this set is fork-spanning — see the fork-determinism gap on the
    //     function's doc comment.
    let block_hashes = db
        .view(|tx| cached_data_root_by_data_root(tx, tx_header.data_root))
        .map_err(|e| PreValidationError::DatabaseError {
            error: format!(
                "Failed to open DB read tx for data_root {} (tx_id {}): {}",
                tx_header.data_root, tx_header.id, e
            ),
        })?
        .map_err(|e| PreValidationError::DatabaseError {
            error: format!(
                "DB query failed for data_root {} (tx_id {}): {}",
                tx_header.data_root, tx_header.id, e
            ),
        })?
        .ok_or_else(|| PreValidationError::DatabaseError {
            error: format!(
                "CachedDataRoot not found for data_root {} (tx_id {})",
                tx_header.data_root, tx_header.id
            ),
        })?
        .block_set;

    // Empty block_set is valid for single-block promotion: the data_root hasn't
    // been included in any confirmed block yet.  With no block_ranges, no proofs
    // will be classified as "assigned" and assigned_miners stays 0.  The caller
    // clamps expected_assigned_proofs to 0, so all proofs count as unassigned and
    // only the total-proof-count gate applies.
    if block_hashes.is_empty() {
        return Ok((vec![], 0));
    }

    //  b) Get the submit ledger offset intervals for each of the blocks (invariant across all proofs)
    //
    //  `block_hashes` comes from `CachedDataRoots.block_set` — explicitly
    //  fork-spanning (see this function's doc). The two failure modes from
    //  `get_ledger_range` are NOT the same root cause:
    //   - `Ok(None)` → the hash is no longer resolvable (predecessor missing,
    //     pruned side-fork block). Soft `AssignedProofBlockMissing` so the
    //     caller parks the block for retry.  We also send
    //     `PruneBlockHashFromBlockSet` to the cache service so the dead hash is
    //     removed and subsequent re-deliveries do not hit the same soft-fail loop.
    //   - `Err(_)` → local DB I/O failure or an explicit data-corruption
    //     assertion inside `get_ledger_range`. A real local fault, not a
    //     fork-determinism artifact. Routes through the node-fault
    //     `BlockBoundsLookupError` variant so the handler aborts and the
    //     supervisor restarts the node clean.
    let mut block_ranges = Vec::new();
    for block_hash in block_hashes.iter() {
        match get_ledger_range(block_hash, block_tree, db) {
            Ok(Some(block_range)) => block_ranges.push(block_range),
            Ok(None) => {
                if let Some(sender) = cache_sender {
                    let _ = sender
                        .send_traced(
                            crate::cache_service::CacheServiceAction::PruneBlockHashFromBlockSet {
                                data_root: tx_header.data_root,
                                block_hash: *block_hash,
                            },
                        )
                        .inspect_err(|e| {
                            warn!(
                                custom.error = ?e,
                                data_root = %tx_header.data_root,
                                block_hash = %block_hash,
                                "Failed to send PruneBlockHashFromBlockSet"
                            );
                        });
                }
                return Err(PreValidationError::AssignedProofBlockMissing {
                    block_hash: *block_hash,
                    tx_id: tx_header.id,
                });
            }
            Err(e) => {
                return Err(PreValidationError::BlockBoundsLookupError(format!(
                    "get_ledger_range failed for block {} (tx_id {}): {}",
                    block_hash, tx_header.id, e
                )));
            }
        }
    }

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

        //  c) Get the slots the proof address is assigned to store
        let slot_indexes = get_submit_ledger_slot_assignments(&proof_address, epoch_snapshot);

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
        let slot_address_counts = get_submit_ledger_slot_addresses(&slot_indexes, epoch_snapshot);

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

fn get_ledger_range(
    hash: &H256,
    block_tree: &BlockTreeReadGuard,
    db: &DatabaseProvider,
) -> eyre::Result<Option<LedgerChunkRange>> {
    let block = match get_block_by_hash(hash, block_tree, db)? {
        Some(b) => b,
        None => return Ok(None),
    };
    let prev_block_hash = block.previous_block_hash;

    let block_total_chunks = block.data_ledgers[DataLedger::Submit].total_chunks;

    if block.height == 0 {
        if block_total_chunks == 0 {
            return Ok(None);
        }
        Ok(Some(LedgerChunkRange(ii(
            LedgerChunkOffset::from(0),
            LedgerChunkOffset::from(block_total_chunks - 1),
        ))))
    } else {
        let prev_block = match get_block_by_hash(&prev_block_hash, block_tree, db)? {
            Some(b) => b,
            None => return Ok(None),
        };
        let prev_total_chunks = prev_block.data_ledgers[DataLedger::Submit].total_chunks;
        if block_total_chunks < prev_total_chunks {
            return Err(eyre::eyre!(
                "Block {} has total_chunks ({}) < prev block total_chunks ({}), data corruption",
                hash,
                block_total_chunks,
                prev_total_chunks
            ));
        }
        if block_total_chunks == 0 || block_total_chunks == prev_total_chunks {
            return Ok(None);
        }
        Ok(Some(LedgerChunkRange(ii(
            LedgerChunkOffset::from(prev_total_chunks),
            LedgerChunkOffset::from(block_total_chunks - 1),
        ))))
    }
}

fn get_block_by_hash(
    hash: &H256,
    block_tree: &BlockTreeReadGuard,
    db: &DatabaseProvider,
) -> eyre::Result<Option<IrysBlockHeader>> {
    crate::block_tree_service::get_block_header(block_tree, db, *hash, false)
}

fn get_submit_ledger_slot_assignments(
    address: &IrysAddress,
    epoch_snapshot: &EpochSnapshot,
) -> Vec<usize> {
    let mut partition_assignments = epoch_snapshot.get_partition_assignments(*address);
    partition_assignments.retain(|pa| pa.ledger_id == Some(DataLedger::Submit.into()));
    partition_assignments
        .iter()
        .map(|pa| pa.slot_index.unwrap())
        .collect()
}

fn get_submit_ledger_slot_addresses(
    slot_indexes: &Vec<usize>,
    epoch_snapshot: &EpochSnapshot,
) -> HashMap<usize, usize> {
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
    use irys_database::db::IrysDatabaseExt as _;
    use irys_domain::{
        BlockIndex, BlockTree, EpochSnapshot, block_index_guard::BlockIndexReadGuard,
    };
    use irys_testing_utils::new_mock_signed_header;
    use irys_testing_utils::tempfile::TempDir;
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::{
        Base64, BlockHash, DataTransaction, DataTransactionHeader, DataTransactionLedger,
        DbSyncMode, H256, H256List, IrysAddress, IrysBlockHeaderV1, NodeConfig, Signature, U256,
        hash_sha256, irys::IrysSigner, partition::PartitionAssignment,
    };
    use std::sync::{Arc, RwLock};
    use tracing::{debug, info};

    /// Build a minimal `BlockTreeReadGuard` for tests that exercise the
    /// `block_index` fast path of `poa_is_valid` — the helper's fallback is
    /// unreachable when `parent_height` is present in the index, so the
    /// tree's contents don't matter, only that the guard is constructible.
    /// (`BlockTree::new` validates the genesis signature, so we need a signed
    /// mock header rather than the unsigned `new_mock_header`.)
    fn dummy_block_tree_guard(consensus_config: &ConsensusConfig) -> BlockTreeReadGuard {
        let genesis = new_mock_signed_header();
        BlockTreeReadGuard::new(Arc::new(RwLock::new(BlockTree::new(
            &genesis,
            consensus_config.clone(),
        ))))
    }

    fn ledger_with_tx(ledger_id: DataLedger, tx_id: H256) -> DataTransactionLedger {
        DataTransactionLedger {
            ledger_id: ledger_id.into(),
            tx_root: H256::zero(),
            tx_ids: H256List(vec![tx_id]),
            total_chunks: 0,
            expires: None,
            proofs: None,
            required_proof_count: None,
        }
    }

    #[test]
    fn same_block_submit_then_publish_is_not_marked_duplicate() -> eyre::Result<()> {
        let tx_id = H256::from_slice(&[7; 32]);
        let historical_block_hash = BlockHash::from_slice(&[3; 32]);
        let current_block_hash = BlockHash::from_slice(&[9; 32]);
        let tx = DataTransactionHeader::default();
        let mut tx_states = HashMap::from([(
            tx_id,
            (
                &tx,
                TxInclusionState::Searching {
                    ledger_current: DataLedger::Submit,
                },
            ),
        )]);

        let ledgers = vec![
            ledger_with_tx(DataLedger::Submit, tx_id),
            ledger_with_tx(DataLedger::Publish, tx_id),
        ];

        process_block_ledgers_with_states(
            &ledgers,
            historical_block_hash,
            current_block_hash,
            &mut tx_states,
        )?;

        assert!(matches!(
            tx_states.get(&tx_id).map(|(_, state)| state),
            Some(TxInclusionState::Found {
                ledger_current: (DataLedger::Submit, hash_a),
                ledger_historical: (DataLedger::Publish, hash_b),
            }) if *hash_a == current_block_hash && *hash_b == historical_block_hash
        ));

        Ok(())
    }

    #[test]
    fn same_ledger_in_two_historical_blocks_is_marked_duplicate() -> eyre::Result<()> {
        let tx_id = H256::from_slice(&[8; 32]);
        let first_block_hash = BlockHash::from_slice(&[4; 32]);
        let second_block_hash = BlockHash::from_slice(&[5; 32]);
        let current_block_hash = BlockHash::from_slice(&[9; 32]);
        let tx = DataTransactionHeader::default();
        let mut tx_states = HashMap::from([(
            tx_id,
            (
                &tx,
                TxInclusionState::Searching {
                    ledger_current: DataLedger::Submit,
                },
            ),
        )]);

        process_block_ledgers_with_states(
            &[ledger_with_tx(DataLedger::Submit, tx_id)],
            first_block_hash,
            current_block_hash,
            &mut tx_states,
        )?;
        process_block_ledgers_with_states(
            &[ledger_with_tx(DataLedger::Submit, tx_id)],
            second_block_hash,
            current_block_hash,
            &mut tx_states,
        )?;

        assert!(matches!(
            tx_states.get(&tx_id).map(|(_, state)| state),
            Some(TxInclusionState::Duplicate {
                ledger_historical: (DataLedger::Submit, hash),
            }) if *hash == second_block_hash
        ));

        Ok(())
    }

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
        let data_dir = TempDirBuilder::new()
            .prefix("block_validation_tests")
            .build();
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

        let (commitments, initial_treasury) = add_genesis_commitments(&mut genesis_block, &config)
            .await
            .unwrap();
        genesis_block.treasury = initial_treasury;

        let arc_genesis = Arc::new(genesis_block.clone());
        let signer = config.irys_signer();
        let miner_address = signer.address();

        // Create epoch service with random miner address
        let db_env = irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db(
            data_dir.path(),
            DbSyncMode::UtterlyNoSync,
        )
        .expect("to create DB");
        let db = irys_types::DatabaseProvider(Arc::new(db_env));
        let block_index = BlockIndex::new_for_testing(db);

        let storage_submodules_config = StorageSubmodulesConfig::load(
            config.node_config.base_directory.clone(),
            config.node_config.node_mode,
        )
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
                &is_valid,
                ValidationResult::Invalid(rejection)
                    if matches!(rejection.err(), ValidationError::SeedDataInvalid(_))
            ),
            "Seed data should be invalid due to wrong reset frequency"
        );

        // Now let's try to set some random seeds that are not valid
        header_2.vdf_limiter_info.seed = BlockHash::from_slice(&[5; 32]);
        header_2.vdf_limiter_info.next_seed = BlockHash::from_slice(&[6; 32]);
        let is_valid = is_seed_data_valid(&header_2, &parent_header, reset_frequency);

        assert!(
            matches!(
                &is_valid,
                ValidationResult::Invalid(rejection)
                    if matches!(rejection.err(), ValidationError::SeedDataInvalid(_))
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

        // Parent-anchored pre-check: the indexed block sits at `height`
        // (just pushed above), so anchoring on its height lets the
        // pre-check use the same view of total_chunks the rest of the
        // test exercises (the chunks the PoA references were committed
        // by this block). The block_tree fallback in `poa_is_valid` is
        // unreachable here because `height` is in `block_index`; the dummy
        // guard exists only to satisfy the signature.
        let block_tree_guard = dummy_block_tree_guard(&context.consensus_config);
        let poa_valid = poa_is_valid(
            &poa,
            &block_index_guard,
            &block_tree_guard,
            H256::zero(),
            height,
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

        // See parent-anchor comment in `poa_test`: use the just-pushed
        // block's height so the pre-check sees the same `total_chunks`
        // the rest of the assertions exercise. The block_tree fallback in
        // `poa_is_valid` is unreachable here because `height` is in
        // `block_index`; the dummy guard exists only to satisfy the signature.
        let block_tree_guard = dummy_block_tree_guard(&context.consensus_config);
        let poa_valid = poa_is_valid(
            &poa,
            &block_index_guard,
            &block_tree_guard,
            H256::zero(),
            height,
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

    /// `DatabaseError` must classify as `NodeFault`, not `Consensus`.
    /// Regression guard: if the arm ever reverts to collapsing DB errors into a
    /// peer-attributed variant, `classify()` will return `Consensus` and this
    /// test will catch it.
    #[test]
    fn database_error_is_node_fault() {
        let err = PreValidationError::DatabaseError {
            error: "MDBX: I/O error".to_string(),
        };
        assert_eq!(
            err.classify(),
            ErrorClass::NodeFault,
            "DatabaseError must be NodeFault"
        );
        assert!(err.is_node_fault(), "is_node_fault() must return true");
        assert!(
            !matches!(err.classify(), ErrorClass::Consensus),
            "DatabaseError must not be Consensus"
        );
    }
}

#[cfg(test)]
mod commitment_version_tests {
    use super::*;
    use irys_types::{
        CommitmentTransactionV1, CommitmentTransactionV2, CommitmentTypeV1, CommitmentTypeV2,
        hardfork_config::{Aurora, FrontierParams, IrysHardforkConfig},
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
                cascade: None,
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
                cascade: None,
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
