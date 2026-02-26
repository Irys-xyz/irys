use super::ChunkIngressServiceInner;
use crate::cache_service::{CacheServiceAction, CacheServiceSender};
use irys_database::db::{IrysDatabaseExt as _, IrysDupCursorExt as _};
use irys_database::reth_db::transaction::DbTx as _;
use irys_database::{
    cached_data_root_by_data_root, db_cache::data_size_to_chunk_count, tables::CachedChunksIndex,
};
use irys_database::{delete_ingress_proof, store_ingress_proof};
use irys_domain::BlockTreeReadGuard;
use irys_types::irys::IrysSigner;
use irys_types::v2::GossipBroadcastMessageV2;
use irys_types::{
    BlockHash, Config, DataRoot, DatabaseProvider, H256, IngressProof, SendTraced as _, Traced,
};
use reth_db::{Database as _, DatabaseError};
use tracing::{debug, error, warn};

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
    /// Failed to communicate with cache service.
    #[error("Cache service error: {0}")]
    CacheServiceError(String),
    /// Invalid data size for the transaction.
    #[error("Invalid data size: {0}")]
    InvalidDataSize(String),
    /// Failed to generate the proof.
    #[error("Proof generation failed: {0}")]
    GenerationFailed(String),
}

impl IngressProofGenerationError {
    /// Returns true if this error is benign (e.g., node not staked) and should be logged at debug level.
    pub fn is_benign(&self) -> bool {
        matches!(self, Self::NodeNotStaked | Self::AlreadyGenerating)
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
            .update(|rw_tx| -> Result<(), DatabaseError> {
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
        let latest_height = crate::anchor_validation::get_latest_block_height(
            block_tree_read_guard,
        )
        .map_err(|_e| {
            IngressProofError::Other("unable to get canonical chain from block tree ".to_owned())
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

    pub(crate) fn remove_ingress_proof(
        irys_db: &DatabaseProvider,
        data_root: DataRoot,
    ) -> Result<(), IngressProofError> {
        irys_db
            .update(|rw_tx| -> Result<(), DatabaseError> {
                delete_ingress_proof(rw_tx, data_root)
                    .map_err(|report| DatabaseError::Other(report.to_string()))?;
                Ok(())
            })
            .map_err(|db_err| IngressProofError::DatabaseError(db_err.to_string()))?
            .map_err(|db_err| IngressProofError::DatabaseError(db_err.to_string()))?;

        Ok(())
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

/// Generates (and stores) an ingress proof for the provided `data_root` if all chunks are present.
/// Validates the generated proof's anchor against the canonical chain and gossips it if valid.
/// Returns the generated proof on success.
pub fn generate_and_store_ingress_proof(
    block_tree_guard: &BlockTreeReadGuard,
    db: &DatabaseProvider,
    config: &Config,
    data_root: DataRoot,
    anchor_hint: Option<H256>,
    gossip_sender: &tokio::sync::mpsc::UnboundedSender<Traced<GossipBroadcastMessageV2>>,
    cache_sender: &CacheServiceSender,
) -> Result<IngressProof, IngressProofGenerationError> {
    let signer: IrysSigner = config.irys_signer();

    // Only staked nodes should generate ingress proofs
    let epoch_snapshot = block_tree_guard.read().canonical_epoch_snapshot();
    if !epoch_snapshot.is_staked(signer.address()) {
        return Err(IngressProofGenerationError::NodeNotStaked);
    }

    let chain_id = config.consensus.chain_id;
    let chunk_size = config.consensus.chunk_size;

    let data_size = calculate_and_validate_data_size(db, data_root, chunk_size)?;

    // Pick anchor: hint or latest canonical block
    let latest_anchor = block_tree_guard
        .read()
        .get_latest_canonical_entry()
        .block_hash();
    let anchor = anchor_hint.unwrap_or(latest_anchor);

    let is_already_generating = {
        let (response_sender, response_receiver) = std::sync::mpsc::channel();
        if let Err(err) =
            cache_sender.send_traced(CacheServiceAction::RequestIngressProofGenerationState {
                data_root,
                response_sender,
            })
        {
            return Err(IngressProofGenerationError::CacheServiceError(format!(
                "Failed to request ingress proof generation state: {err}"
            )));
        }

        response_receiver.recv().map_err(|err| {
            IngressProofGenerationError::CacheServiceError(format!(
                "Failed to receive ingress proof generation state response: {err}"
            ))
        })?
    };

    if is_already_generating {
        return Err(IngressProofGenerationError::AlreadyGenerating);
    }

    // Notify start of proof generation
    if let Err(e) =
        cache_sender.send_traced(CacheServiceAction::NotifyProofGenerationStarted(data_root))
    {
        warn!(
            ?data_root,
            "Failed to notify cache of proof generation start: {e}"
        );
    }

    let proof_res = super::chunks::generate_ingress_proof(
        db.clone(),
        data_root,
        data_size,
        chunk_size,
        signer,
        chain_id,
        anchor,
    );

    let proof = match proof_res {
        Ok(p) => p,
        Err(e) => {
            if let Err(e) = cache_sender.send_traced(
                CacheServiceAction::NotifyProofGenerationCompleted(data_root),
            ) {
                warn!(
                    ?data_root,
                    "Failed to notify cache of proof generation completion: {e}"
                );
            }
            return Err(IngressProofGenerationError::GenerationFailed(e.to_string()));
        }
    };

    gossip_ingress_proof(gossip_sender, &proof, block_tree_guard, db, config);

    if let Err(e) = cache_sender.send_traced(CacheServiceAction::NotifyProofGenerationCompleted(
        data_root,
    )) {
        warn!(
            ?data_root,
            "Failed to notify cache of proof generation completion: {e}"
        );
    }
    Ok(proof)
}

pub fn reanchor_and_store_ingress_proof(
    block_tree_guard: &BlockTreeReadGuard,
    db: &DatabaseProvider,
    config: &Config,
    signer: &IrysSigner,
    proof: &IngressProof,
    gossip_sender: &tokio::sync::mpsc::UnboundedSender<Traced<GossipBroadcastMessageV2>>,
    cache_sender: &CacheServiceSender,
) -> Result<IngressProof, IngressProofGenerationError> {
    // Only staked nodes should reanchor ingress proofs
    let epoch_snapshot = block_tree_guard.read().canonical_epoch_snapshot();
    if !epoch_snapshot.is_staked(signer.address()) {
        return Err(IngressProofGenerationError::NodeNotStaked);
    }

    let is_already_generating = {
        let (response_sender, response_receiver) = std::sync::mpsc::channel();
        if let Err(err) =
            cache_sender.send_traced(CacheServiceAction::RequestIngressProofGenerationState {
                data_root: proof.data_root,
                response_sender,
            })
        {
            return Err(IngressProofGenerationError::CacheServiceError(format!(
                "Failed to request ingress proof generation state: {err}"
            )));
        }

        response_receiver.recv().map_err(|err| {
            IngressProofGenerationError::CacheServiceError(format!(
                "Failed to receive ingress proof generation state response: {err}"
            ))
        })?
    };

    if is_already_generating {
        return Err(IngressProofGenerationError::AlreadyGenerating);
    }

    if let Err(e) = cache_sender.send_traced(CacheServiceAction::NotifyProofGenerationStarted(
        proof.data_root,
    )) {
        warn!(data_root = ?proof.data_root, "Failed to notify cache of proof generation start: {e}");
    }

    if let Err(e) =
        calculate_and_validate_data_size(db, proof.data_root, config.consensus.chunk_size)
    {
        if let Err(e) = cache_sender.send_traced(
            CacheServiceAction::NotifyProofGenerationCompleted(proof.data_root),
        ) {
            warn!(data_root = ?proof.data_root, "Failed to notify cache of proof generation completion: {e}");
        }
        return Err(e);
    }

    let latest_anchor = block_tree_guard
        .read()
        .get_latest_canonical_entry()
        .block_hash();

    let mut proof = proof.clone();
    // Re-anchor and re-sign
    proof.anchor = latest_anchor;
    if let Err(e) = signer.sign_ingress_proof(&mut proof) {
        if let Err(e) = cache_sender.send_traced(
            CacheServiceAction::NotifyProofGenerationCompleted(proof.data_root),
        ) {
            warn!(data_root = ?proof.data_root, "Failed to notify cache of proof generation completion: {e}");
        }
        return Err(IngressProofGenerationError::GenerationFailed(e.to_string()));
    }

    if let Err(e) = store_ingress_proof(db, &proof, signer) {
        if let Err(e) = cache_sender.send_traced(
            CacheServiceAction::NotifyProofGenerationCompleted(proof.data_root),
        ) {
            warn!(data_root = ?proof.data_root, "Failed to notify cache of proof generation completion: {e}");
        }
        return Err(IngressProofGenerationError::GenerationFailed(e.to_string()));
    }

    gossip_ingress_proof(gossip_sender, &proof, block_tree_guard, db, config);

    if let Err(e) = cache_sender.send_traced(CacheServiceAction::NotifyProofGenerationCompleted(
        proof.data_root,
    )) {
        warn!(data_root = ?proof.data_root, "Failed to notify cache of proof generation completion: {e}");
    }
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

pub fn calculate_and_validate_data_size(
    db: &DatabaseProvider,
    data_root: DataRoot,
    chunk_size: u64,
) -> Result<u64, IngressProofGenerationError> {
    let err = |msg: String| IngressProofGenerationError::InvalidDataSize(msg);

    // Load data_size & confirm we have metadata for this root
    let (data_size, chunk_count) = db
        .view_eyre(|tx| {
            let data_size = cached_data_root_by_data_root(tx, data_root)
                .map_err(|e| eyre::eyre!("Failed to load cached_data_root: {e}"))?
                .ok_or_else(|| eyre::eyre!("Missing cached_data_root for {data_root:?}"))?
                .data_size;
            let mut cursor = tx.cursor_dup_read::<CachedChunksIndex>()?;
            let count = cursor
                .dup_count(data_root)?
                .ok_or_else(|| eyre::eyre!("No chunks found for data_root {data_root:?}"))?;
            Ok((data_size, count))
        })
        .map_err(|e| err(e.to_string()))?;

    let expected =
        data_size_to_chunk_count(data_size, chunk_size).map_err(|e| err(e.to_string()))?;
    if chunk_count != expected {
        return Err(err(format!(
            "have {chunk_count} chunks, expected {expected}"
        )));
    }

    Ok(data_size)
}
