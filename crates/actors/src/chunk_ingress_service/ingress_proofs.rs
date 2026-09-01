use super::ChunkIngressServiceInner;
use irys_database::db::IrysDatabaseExt as _;
use irys_database::db_cache::data_size_to_chunk_count;
use irys_database::store_ingress_proof;
use irys_database::{cached_data_root_by_data_root, complete_ingress_leaves};
use irys_domain::BlockTreeReadGuard;
use irys_types::irys::IrysSigner;
use irys_types::v2::GossipBroadcastMessageV2;
use irys_types::{
    BlockHash, Config, DataRoot, DatabaseProvider, H256, IngressMerkleLeaf, IngressProof,
    SendTraced as _, Traced,
};
use reth_db::DatabaseError;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tracing::{debug, error, warn};

/// Shared, process-local exclusion for proof generation by data root.
///
/// Cache eviction no longer depends on this state. It belongs to proof
/// generation itself and is acquired atomically, avoiding the former
/// check-then-notify race through the cache-service channel.
#[derive(Clone, Debug, Default)]
pub struct IngressProofGenerationState {
    inner: Arc<RwLock<IngressProofGenerationStateInner>>,
}

#[derive(Debug, Default)]
struct IngressProofGenerationStateInner {
    active: HashSet<DataRoot>,
    retry_attempts: HashMap<DataRoot, u8>,
    retry_scheduled: HashSet<DataRoot>,
}

impl IngressProofGenerationState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn try_acquire(&self, data_root: DataRoot) -> Option<IngressProofGenerationLease> {
        let inserted = self
            .inner
            .write()
            .expect("proof generation state lock poisoned")
            .active
            .insert(data_root);
        inserted.then(|| IngressProofGenerationLease {
            state: self.clone(),
            data_root,
        })
    }

    #[cfg(test)]
    pub(crate) fn is_generating(&self, data_root: DataRoot) -> bool {
        self.inner
            .read()
            .expect("proof generation state lock poisoned")
            .active
            .contains(&data_root)
    }

    /// Reserves one bounded, monotonic retry for a failed generation attempt.
    /// Concurrent failures coalesce, and persistent failures stop after the
    /// capped sequence instead of creating an unbounded retry loop.
    pub(crate) fn reserve_retry(&self, data_root: DataRoot) -> Option<Duration> {
        const MAX_RETRIES: u8 = 6;
        let mut inner = self
            .inner
            .write()
            .expect("proof generation state lock poisoned");
        if inner.retry_scheduled.contains(&data_root) {
            return None;
        }
        let attempt = *inner.retry_attempts.get(&data_root).unwrap_or(&0);
        if attempt >= MAX_RETRIES {
            return None;
        }
        inner.retry_attempts.insert(data_root, attempt + 1);
        inner.retry_scheduled.insert(data_root);
        Some(Duration::from_secs(1_u64 << attempt))
    }

    pub(crate) fn mark_retry_dispatched(&self, data_root: DataRoot) {
        self.inner
            .write()
            .expect("proof generation state lock poisoned")
            .retry_scheduled
            .remove(&data_root);
    }

    pub(crate) fn clear_retries(&self, data_root: DataRoot) {
        let mut inner = self
            .inner
            .write()
            .expect("proof generation state lock poisoned");
        inner.retry_attempts.remove(&data_root);
        inner.retry_scheduled.remove(&data_root);
    }
}

pub struct IngressProofGenerationLease {
    state: IngressProofGenerationState,
    data_root: DataRoot,
}

impl Drop for IngressProofGenerationLease {
    fn drop(&mut self) {
        self.state
            .inner
            .write()
            .expect("proof generation state lock poisoned")
            .active
            .remove(&self.data_root);
    }
}

/// Errors that can occur when ingesting an external ingress proof.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IngressProofError {
    /// The proofs signature is invalid
    #[error("Ingress proof signature is invalid")]
    InvalidSignature,
    /// There was a database error storing the proof
    #[error("Database error: {0}")]
    DatabaseError(String),
    /// The proof does not come from a staked address
    #[error("Unstaked address")]
    UnstakedAddress,
    /// The ingress proof is anchored to an unknown/expired anchor
    #[error("Invalid anchor: {0}")]
    InvalidAnchor(BlockHash),
    /// The service is at capacity and rejected the proof. Distinct from a
    /// network failure: the peer is fine, the receiver is just saturated.
    /// Callers should retry later.
    #[error("Ingress proof service overloaded")]
    Overloaded,
    /// Catch-all variant for other errors.
    #[error("Ingress proof error: {0}")]
    Other(String),
}

/// Errors that can occur when generating an ingress proof locally.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IngressProofGenerationError {
    /// Node is not staked in the current epoch - this is expected behavior for unstaked nodes.
    #[error("Node is not staked in current epoch")]
    NodeNotStaked,
    /// Proof generation is already in progress for this data root.
    #[error("Proof generation already in progress")]
    AlreadyGenerating,
    /// Invalid data size for the transaction.
    #[error("Invalid data size: {0}")]
    InvalidDataSize(String),
    /// The data root was removed before proof generation could begin.
    #[error("Cached data root is no longer available")]
    DataRootUnavailable,
    /// Validated compact leaves have not arrived for every expected position.
    #[error("Compact ingress leaves are incomplete")]
    IncompleteLeaves,
    /// Failed to generate the proof.
    #[error("Proof generation failed: {0}")]
    GenerationFailed(String),
}

impl IngressProofGenerationError {
    /// Returns true if this error is benign (e.g., node not staked) and should be logged at debug level.
    pub fn is_benign(&self) -> bool {
        matches!(
            self,
            Self::NodeNotStaked
                | Self::AlreadyGenerating
                | Self::DataRootUnavailable
                | Self::IncompleteLeaves
        )
    }
}

impl ChunkIngressServiceInner {
    #[tracing::instrument(level = "trace", skip_all, fields(data_root = %ingress_proof.data_root))]
    pub(crate) fn handle_ingest_ingress_proof(
        &self,
        ingress_proof: IngressProof,
    ) -> Result<(), IngressProofError> {
        // Validate the proofs signature and basic details
        let address = ingress_proof
            .pre_validate(&ingress_proof.data_root)
            .map_err(|_| IngressProofError::InvalidSignature)?;

        // Reject proofs from addresses not staked or pending stake (spam protection)
        let block_tree = self.block_tree_read_guard.read();
        let epoch_snapshot = block_tree.canonical_epoch_snapshot();
        let commitment_snapshot = block_tree.canonical_commitment_snapshot();
        drop(block_tree);

        if !epoch_snapshot.is_staked(address) && !commitment_snapshot.is_staked(address) {
            return Err(IngressProofError::UnstakedAddress);
        }

        // Validate the anchor
        self.validate_ingress_proof_anchor(&ingress_proof)?;

        // TODO: we should only overwrite a proof we already have if the new one has a newer anchor than the old one
        let res = self
            .irys_db
            .update_scoped(|rw_tx| -> Result<(), DatabaseError> {
                irys_database::store_external_ingress_proof_checked(rw_tx, &ingress_proof, address)
                    .map_err(|e| DatabaseError::Other(e.to_string()))?;
                Ok(())
            })
            .map_err(|e| IngressProofError::DatabaseError(e.to_string()))?;

        if let Err(e) = res {
            tracing::error!(
                ingress_proof.data_root = ?ingress_proof.data_root,
                "Failed to store ingress proof data root: {:?}",
                e
            );
            return Err(IngressProofError::DatabaseError(e.to_string()));
        }

        let gossip_sender = &self.service_senders.gossip_broadcast;
        let data_root = ingress_proof.data_root;
        let gossip_broadcast_message = GossipBroadcastMessageV2::from(ingress_proof);

        if let Err(error) = gossip_sender.send_traced(gossip_broadcast_message) {
            tracing::error!(
                "Failed to send gossip data for ingress proof data_root {:?}: {:?}",
                data_root,
                error
            );
        }

        Ok(())
    }

    pub(crate) fn validate_ingress_proof_anchor(
        &self,
        ingress_proof: &IngressProof,
    ) -> Result<(), IngressProofError> {
        Self::validate_ingress_proof_anchor_static(
            &self.block_tree_read_guard,
            &self.irys_db,
            &self.config,
            ingress_proof,
        )
    }

    pub(crate) fn validate_ingress_proof_anchor_static(
        block_tree_read_guard: &BlockTreeReadGuard,
        irys_db: &DatabaseProvider,
        config: &Config,
        ingress_proof: &IngressProof,
    ) -> Result<(), IngressProofError> {
        let latest_height = block_tree_read_guard
            .latest_canonical_block_height()
            .ok_or_else(|| {
                IngressProofError::Other("unable to get canonical chain from block tree".to_owned())
            })?;

        // TODO: add an ingress proof invalid LRU, like we have for txs
        let anchor_height = match crate::anchor_validation::get_anchor_height(
            block_tree_read_guard,
            irys_db,
            ingress_proof.anchor,
            false, /* does not need to be canonical */
        )
        .map_err(|db_err| IngressProofError::DatabaseError(db_err.to_string()))?
        {
            Some(height) => height,
            None => {
                // Unknown anchor
                return Err(IngressProofError::InvalidAnchor(ingress_proof.anchor));
            }
        };

        // check consensus config

        let min_anchor_height = latest_height
            .saturating_sub(config.consensus.mempool.ingress_proof_anchor_expiry_depth as u64);

        let too_old = anchor_height < min_anchor_height;

        if too_old {
            warn!(
                "Ingress proof anchor {} has height {}, which is too old (min: {})",
                ingress_proof.anchor, anchor_height, min_anchor_height
            );
            Err(IngressProofError::InvalidAnchor(ingress_proof.anchor))
        } else {
            Ok(())
        }
    }

    pub(crate) fn is_ingress_proof_expired_static(
        block_tree_read_guard: &BlockTreeReadGuard,
        irys_db: &DatabaseProvider,
        config: &Config,
        ingress_proof: &IngressProof,
    ) -> ProofCheckResult {
        match Self::validate_ingress_proof_anchor_static(
            block_tree_read_guard,
            irys_db,
            config,
            ingress_proof,
        ) {
            // Fully valid
            Ok(()) => {
                debug!(
                    ingress_proof.data_root = ?ingress_proof.data_root,
                    "Ingress proof anchor is valid"
                );
                ProofCheckResult {
                    expired_or_invalid: false,
                    regeneration_action: RegenAction::DoNotRegenerate,
                }
            }
            Err(e) => {
                match e {
                    IngressProofError::InvalidAnchor(_block_hash) => {
                        warn!(
                            ingress_proof.data_root = ?ingress_proof.data_root,
                            ingress_proof.anchor = ?ingress_proof.anchor,
                            "Ingress proof anchor has an invalid anchor",
                        );
                        // Prune, regenerate if not at capacity
                        ProofCheckResult {
                            expired_or_invalid: true,
                            regeneration_action: RegenAction::Reanchor,
                        }
                    }
                    IngressProofError::InvalidSignature => {
                        warn!(
                            ingress_proof.data_root = ?ingress_proof.data_root,
                            ingress_proof.anchor = ?ingress_proof.anchor,
                            "Ingress proof anchor has an invalid signature and is going to be pruned",
                        );
                        // Fully regenerate
                        ProofCheckResult {
                            expired_or_invalid: true,
                            regeneration_action: RegenAction::Regenerate,
                        }
                    }
                    IngressProofError::UnstakedAddress => {
                        warn!(
                            ingress_proof.data_root = ?ingress_proof.data_root,
                            ingress_proof.anchor = ?ingress_proof.anchor,
                            "Ingress proof has been created by an unstaked address and is going to be pruned",
                        );
                        // Should not happen; prune, our own address should not be unstaked unexpectedly
                        ProofCheckResult {
                            expired_or_invalid: true,
                            regeneration_action: RegenAction::DoNotRegenerate,
                        }
                    }
                    IngressProofError::DatabaseError(message) => {
                        // Don't do anything, we don't know the proof status
                        error!(
                            ingress_proof.data_root = ?ingress_proof.data_root,
                            "Database error during ingress proof expiration validation: {}", message
                        );
                        ProofCheckResult {
                            expired_or_invalid: false,
                            regeneration_action: RegenAction::DoNotRegenerate,
                        }
                    }
                    IngressProofError::Overloaded => {
                        warn!(
                            ingress_proof.data_root = ?ingress_proof.data_root,
                            "Ingress proof service overloaded during expiration validation"
                        );
                        ProofCheckResult {
                            expired_or_invalid: false,
                            regeneration_action: RegenAction::DoNotRegenerate,
                        }
                    }
                    IngressProofError::Other(reason_message) => {
                        error!(
                            ingress_proof.data_root = ?ingress_proof.data_root,
                            "Unexpected error during ingress proof expiration validation: {}", reason_message
                        );
                        ProofCheckResult {
                            expired_or_invalid: false,
                            regeneration_action: RegenAction::DoNotRegenerate,
                        }
                    }
                }
            }
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct ProofCheckResult {
    /// Whether the proof is expired/invalid and should be pruned
    pub expired_or_invalid: bool,
    /// Whether the proof should be reanchored after pruning if possible
    pub regeneration_action: RegenAction,
}

#[derive(Copy, Clone, Debug)]
pub enum RegenAction {
    /// The proof has expired - the anchor should be updated to the latest canonical block and
    ///  the proof re-signed.
    Reanchor,
    /// The proof is invalid (e.g., bad signature) - the proof should be fully regenerated.
    Regenerate,
    /// The proof should not be regenerated.
    DoNotRegenerate,
}

impl ProofCheckResult {
    pub fn is_expired(&self) -> bool {
        self.expired_or_invalid
    }
}

/// Generates and stores an ingress proof once every expected validated compact
/// leaf is present for the local signer.
/// Validates the generated proof's anchor against the canonical chain and gossips it if valid.
/// Returns the generated proof on success.
pub fn generate_and_store_ingress_proof(
    block_tree_guard: &BlockTreeReadGuard,
    db: &DatabaseProvider,
    config: &Config,
    data_root: DataRoot,
    anchor_hint: Option<H256>,
    gossip_sender: &tokio::sync::mpsc::UnboundedSender<Traced<GossipBroadcastMessageV2>>,
    generation_state: &IngressProofGenerationState,
) -> Result<IngressProof, IngressProofGenerationError> {
    // Resolve readiness before taking the exclusion lease. A caller holding
    // the lease with an incomplete snapshot could otherwise race the final
    // leaf writer, causing that writer to drop its only generation wake-up.
    let leaves = load_complete_ingress_leaves(
        db,
        data_root,
        config.irys_signer().address(),
        config.consensus.chunk_size,
    )?;
    let generation_lease = generation_state
        .try_acquire(data_root)
        .ok_or(IngressProofGenerationError::AlreadyGenerating)?;
    generate_and_store_ingress_proof_from_leaves(
        block_tree_guard,
        db,
        config,
        data_root,
        anchor_hint,
        leaves,
        gossip_sender,
        generation_lease,
    )
}

/// Generates a proof from a leaf sequence already verified by the readiness
/// check while holding the root's generation lease. This keeps the hot ingress
/// path to one ordered leaf walk.
pub(crate) fn generate_and_store_ingress_proof_from_leaves(
    block_tree_guard: &BlockTreeReadGuard,
    db: &DatabaseProvider,
    config: &Config,
    data_root: DataRoot,
    anchor_hint: Option<H256>,
    leaves: Vec<IngressMerkleLeaf>,
    gossip_sender: &tokio::sync::mpsc::UnboundedSender<Traced<GossipBroadcastMessageV2>>,
    generation_lease: IngressProofGenerationLease,
) -> Result<IngressProof, IngressProofGenerationError> {
    let _generation_lease = generation_lease;
    let signer: IrysSigner = config.irys_signer();

    // Only staked nodes should generate ingress proofs
    let epoch_snapshot = block_tree_guard.read().canonical_epoch_snapshot();
    if !epoch_snapshot.is_staked(signer.address()) {
        return Err(IngressProofGenerationError::NodeNotStaked);
    }

    let chain_id = config.consensus.chain_id;

    // Pick anchor: hint or latest canonical block
    let latest_anchor = block_tree_guard
        .read()
        .get_latest_canonical_entry()
        .block_hash();
    let anchor = anchor_hint.unwrap_or(latest_anchor);

    let proof = super::chunks::generate_ingress_proof(
        db.clone(),
        data_root,
        leaves,
        signer,
        chain_id,
        anchor,
    )
    .map_err(|error| IngressProofGenerationError::GenerationFailed(error.to_string()))?;

    gossip_ingress_proof(gossip_sender, &proof, block_tree_guard, db, config);
    Ok(proof)
}

pub fn reanchor_and_store_ingress_proof(
    block_tree_guard: &BlockTreeReadGuard,
    db: &DatabaseProvider,
    config: &Config,
    signer: &IrysSigner,
    proof: &IngressProof,
    gossip_sender: &tokio::sync::mpsc::UnboundedSender<Traced<GossipBroadcastMessageV2>>,
    generation_state: &IngressProofGenerationState,
) -> Result<IngressProof, IngressProofGenerationError> {
    // Only staked nodes should reanchor ingress proofs
    let epoch_snapshot = block_tree_guard.read().canonical_epoch_snapshot();
    if !epoch_snapshot.is_staked(signer.address()) {
        return Err(IngressProofGenerationError::NodeNotStaked);
    }

    load_complete_ingress_leaves(
        db,
        proof.data_root,
        signer.address(),
        config.consensus.chunk_size,
    )?;

    let _generation_lease = generation_state
        .try_acquire(proof.data_root)
        .ok_or(IngressProofGenerationError::AlreadyGenerating)?;

    let latest_anchor = block_tree_guard
        .read()
        .get_latest_canonical_entry()
        .block_hash();

    let mut proof = proof.clone();
    // Re-anchor and re-sign
    proof.anchor = latest_anchor;
    signer
        .sign_ingress_proof(&mut proof)
        .map_err(|error| IngressProofGenerationError::GenerationFailed(error.to_string()))?;

    store_ingress_proof(db, &proof, signer)
        .map_err(|error| IngressProofGenerationError::GenerationFailed(error.to_string()))?;

    gossip_ingress_proof(gossip_sender, &proof, block_tree_guard, db, config);
    Ok(proof)
}

pub fn gossip_ingress_proof(
    gossip_sender: &tokio::sync::mpsc::UnboundedSender<Traced<GossipBroadcastMessageV2>>,
    ingress_proof: &IngressProof,
    block_tree_guard: &BlockTreeReadGuard,
    db: &DatabaseProvider,
    config: &Config,
) {
    // Validate anchor freshness prior to broadcast
    match ChunkIngressServiceInner::validate_ingress_proof_anchor_static(
        block_tree_guard,
        db,
        config,
        ingress_proof,
    ) {
        Ok(()) => {
            let msg = GossipBroadcastMessageV2::from(ingress_proof.clone());
            if let Err(e) = gossip_sender.send_traced(msg) {
                tracing::error!(proof.data_root = ?ingress_proof.data_root, "Failed to gossip regenerated ingress proof: {e}");
            }
        }
        Err(e) => {
            // Skip gossip; proof stored for potential later use/regeneration.
            tracing::debug!(proof.data_root = ?ingress_proof.data_root, "Generated ingress proof anchor invalid (not gossiped): {e}");
        }
    }
}

/// Loads the exact gap-free compact leaf sequence for a confirmed data root.
/// This is the sole database scan used by proof generation.
pub fn load_complete_ingress_leaves(
    db: &DatabaseProvider,
    data_root: DataRoot,
    address: irys_types::IrysAddress,
    chunk_size: u64,
) -> Result<Vec<IngressMerkleLeaf>, IngressProofGenerationError> {
    let cdr = db
        .view_eyre(|tx| cached_data_root_by_data_root(tx, data_root))
        .map_err(|error| IngressProofGenerationError::GenerationFailed(error.to_string()))?
        .ok_or(IngressProofGenerationError::DataRootUnavailable)?;

    data_size_to_chunk_count(cdr.data_size, chunk_size)
        .map_err(|error| IngressProofGenerationError::InvalidDataSize(error.to_string()))?;

    db.view_eyre(|tx| complete_ingress_leaves(tx, data_root, address, cdr.data_size, chunk_size))
        .map_err(|error| IngressProofGenerationError::GenerationFailed(error.to_string()))?
        .ok_or(IngressProofGenerationError::IncompleteLeaves)
}

#[cfg(test)]
mod generation_state_tests {
    use super::*;

    #[test]
    fn generation_lease_is_atomic_and_released_on_drop() {
        let state = IngressProofGenerationState::new();
        let data_root = H256::random();

        let lease = state.try_acquire(data_root).expect("first acquisition");
        assert!(state.is_generating(data_root));
        assert!(state.try_acquire(data_root).is_none());

        drop(lease);
        assert!(!state.is_generating(data_root));
        assert!(state.try_acquire(data_root).is_some());
    }

    #[test]
    fn generation_retries_are_bounded_and_coalesced() {
        let state = IngressProofGenerationState::new();
        let data_root = H256::random();

        assert_eq!(state.reserve_retry(data_root), Some(Duration::from_secs(1)));
        assert_eq!(state.reserve_retry(data_root), None, "one retry at a time");

        for expected_secs in [2, 4, 8, 16, 32] {
            state.mark_retry_dispatched(data_root);
            assert_eq!(
                state.reserve_retry(data_root),
                Some(Duration::from_secs(expected_secs))
            );
        }
        state.mark_retry_dispatched(data_root);
        assert_eq!(state.reserve_retry(data_root), None, "retry cap");

        state.clear_retries(data_root);
        assert_eq!(state.reserve_retry(data_root), Some(Duration::from_secs(1)));
    }
}
