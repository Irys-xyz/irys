use crate::cache_service::{CacheServiceAction, CacheServiceSender};
use crate::mempool_service::{IngressProofError, Inner};
use irys_database::db::{IrysDatabaseExt as _, IrysDupCursorExt as _};
use irys_database::reth_db::transaction::DbTx as _;
use irys_database::{
    cached_data_root_by_data_root, db_cache::data_size_to_chunk_count, tables::CachedChunksIndex,
};
use irys_database::{delete_ingress_proof, store_ingress_proof};
use irys_domain::BlockTreeReadGuard;
use irys_types::irys::IrysSigner;
use irys_types::v2::GossipBroadcastMessageV2;
use irys_types::{Config, DataRoot, DatabaseProvider, IngressProof, H256};
use reth_db::{Database as _, DatabaseError};
use tracing::{debug, error, instrument, warn};

impl Inner {
    #[tracing::instrument(level = "trace", skip_all, fields(data_root = %ingress_proof.data_root()))]
    pub fn handle_ingest_ingress_proof(
        &self,
        ingress_proof: IngressProof,
    ) -> Result<(), IngressProofError> {
        ingress_proof
            .check_version_accepted(
                self.config.consensus.accept_kzg_ingress_proofs,
                self.config.consensus.require_kzg_ingress_proofs,
            )
            .map_err(|msg| IngressProofError::RejectedVersion(msg.into()))?;

        let data_root_val = ingress_proof.data_root();
        let address = ingress_proof
            .pre_validate(&data_root_val)
            .map_err(|_| IngressProofError::InvalidSignature)?;

        let epoch_snapshot = self.block_tree_read_guard.read().canonical_epoch_snapshot();
        let commitment_snapshot = self
            .block_tree_read_guard
            .read()
            .canonical_commitment_snapshot();

        if !epoch_snapshot.is_staked(address) && !commitment_snapshot.is_staked(address) {
            return Err(IngressProofError::UnstakedAddress);
        }

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
                ingress_proof.data_root = ?ingress_proof.data_root(),
                "Failed to store ingress proof data root: {:?}",
                e
            );
            return Err(IngressProofError::DatabaseError(e.to_string()));
        }

        let gossip_sender = &self.service_senders.gossip_broadcast;
        let data_root = ingress_proof.data_root();
        let gossip_broadcast_message = GossipBroadcastMessageV2::from(ingress_proof);

        if let Err(error) = gossip_sender.send(gossip_broadcast_message) {
            tracing::error!(
                "Failed to send gossip data for ingress proof data_root {:?}: {:?}",
                data_root,
                error
            );
        }

        Ok(())
    }

    pub fn validate_ingress_proof_anchor(
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

    pub fn validate_ingress_proof_anchor_static(
        block_tree_read_guard: &BlockTreeReadGuard,
        irys_db: &DatabaseProvider,
        config: &Config,
        ingress_proof: &IngressProof,
    ) -> Result<(), IngressProofError> {
        let latest_height =
            Self::get_latest_block_height_static(block_tree_read_guard).map_err(|_e| {
                IngressProofError::Other(
                    "unable to get canonical chain from block tree ".to_owned(),
                )
            })?;

        // TODO: add an ingress proof invalid LRU, like we have for txs
        let anchor = ingress_proof.anchor();
        let anchor_height = match Self::get_anchor_height_static(
            block_tree_read_guard,
            irys_db,
            anchor,
            false, /* does not need to be canonical */
        )
        .map_err(|db_err| IngressProofError::DatabaseError(db_err.to_string()))?
        {
            Some(height) => height,
            None => {
                return Err(IngressProofError::InvalidAnchor(anchor));
            }
        };

        let min_anchor_height = latest_height.saturating_sub(u64::from(
            config.consensus.mempool.ingress_proof_anchor_expiry_depth,
        ));

        let too_old = anchor_height < min_anchor_height;

        if too_old {
            warn!(
                "Ingress proof anchor {} has height {}, which is too old (min: {})",
                anchor, anchor_height, min_anchor_height
            );
            Err(IngressProofError::InvalidAnchor(anchor))
        } else {
            Ok(())
        }
    }

    pub fn remove_ingress_proof(
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

    /// Validate the ingress proof anchor, and if invalid, remove the ingress proof from the database.
    /// Returns `Ok(true)` if the proof is expired (anchor invalid), `Ok(false)` if it is still valid.
    /// This function DOES NOT delete the proof; deletion is performed exclusively by the cache service.
    #[instrument(skip_all, fields(proof.data_root = ?ingress_proof.data_root()))]
    pub fn is_ingress_proof_expired(&self, ingress_proof: &IngressProof) -> ProofCheckResult {
        Self::is_ingress_proof_expired_static(
            &self.block_tree_read_guard,
            &self.irys_db,
            &self.config,
            ingress_proof,
        )
    }

    pub fn is_ingress_proof_expired_static(
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
                    data_root = ?ingress_proof.data_root(),
                    "Ingress proof anchor is valid"
                );
                ProofCheckResult {
                    expired_or_invalid: false,
                    regeneration_action: RegenAction::DoNotRegenerate,
                }
            }
            Err(e) => match e {
                IngressProofError::InvalidAnchor(_block_hash) => {
                    warn!(
                        data_root = ?ingress_proof.data_root(),
                        anchor = ?ingress_proof.anchor(),
                        "Ingress proof anchor has an invalid anchor",
                    );
                    ProofCheckResult {
                        expired_or_invalid: true,
                        regeneration_action: RegenAction::Reanchor,
                    }
                }
                IngressProofError::InvalidSignature => {
                    warn!(
                        data_root = ?ingress_proof.data_root(),
                        anchor = ?ingress_proof.anchor(),
                        "Ingress proof anchor has an invalid signature and is going to be pruned",
                    );
                    ProofCheckResult {
                        expired_or_invalid: true,
                        regeneration_action: RegenAction::Regenerate,
                    }
                }
                IngressProofError::UnstakedAddress => {
                    warn!(
                        data_root = ?ingress_proof.data_root(),
                        anchor = ?ingress_proof.anchor(),
                        "Ingress proof has been created by an unstaked address and is going to be pruned",
                    );
                    ProofCheckResult {
                        expired_or_invalid: true,
                        regeneration_action: RegenAction::DoNotRegenerate,
                    }
                }
                IngressProofError::DatabaseError(message) => {
                    error!(
                        data_root = ?ingress_proof.data_root(),
                        "Database error during ingress proof expiration validation: {}", message
                    );
                    ProofCheckResult {
                        expired_or_invalid: false,
                        regeneration_action: RegenAction::DoNotRegenerate,
                    }
                }
                IngressProofError::RejectedVersion(reason) => {
                    warn!(
                        data_root = ?ingress_proof.data_root(),
                        "Ingress proof version rejected: {}", reason
                    );
                    ProofCheckResult {
                        expired_or_invalid: true,
                        regeneration_action: RegenAction::DoNotRegenerate,
                    }
                }
                IngressProofError::Other(reason_message) => {
                    error!(
                        data_root = ?ingress_proof.data_root(),
                        "Unexpected error during ingress proof expiration validation: {}", reason_message
                    );
                    ProofCheckResult {
                        expired_or_invalid: false,
                        regeneration_action: RegenAction::DoNotRegenerate,
                    }
                }
            },
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

/// RAII guard that notifies the cache service when proof generation completes.
/// Sends `NotifyProofGenerationStarted` on acquisition and `NotifyProofGenerationCompleted`
/// on drop — guaranteeing the completion signal even on early-return error paths.
struct ProofGenerationGuard<'a> {
    data_root: DataRoot,
    cache_sender: &'a CacheServiceSender,
}

impl<'a> ProofGenerationGuard<'a> {
    fn acquire(data_root: DataRoot, cache_sender: &'a CacheServiceSender) -> eyre::Result<Self> {
        let (response_sender, response_receiver) = std::sync::mpsc::channel();
        cache_sender
            .send(CacheServiceAction::RequestIngressProofGenerationState {
                data_root,
                response_sender,
            })
            .map_err(|err| {
                eyre::eyre!("Failed to request ingress proof generation state: {err}")
            })?;

        let is_already_generating = response_receiver.recv().map_err(|err| {
            eyre::eyre!("Failed to receive ingress proof generation state response: {err}")
        })?;

        if is_already_generating {
            return Err(eyre::eyre!(
                "Ingress proof generation already in progress for data_root {:?}",
                data_root
            ));
        }

        let _ = cache_sender.send(CacheServiceAction::NotifyProofGenerationStarted(data_root));

        Ok(Self {
            data_root,
            cache_sender,
        })
    }
}

impl Drop for ProofGenerationGuard<'_> {
    fn drop(&mut self) {
        let _ = self
            .cache_sender
            .send(CacheServiceAction::NotifyProofGenerationCompleted(
                self.data_root,
            ));
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
    gossip_sender: &tokio::sync::mpsc::UnboundedSender<GossipBroadcastMessageV2>,
    cache_sender: &CacheServiceSender,
) -> eyre::Result<IngressProof> {
    let signer: IrysSigner = config.irys_signer();
    let chain_id = config.consensus.chain_id;
    let chunk_size = config.consensus.chunk_size;

    let data_size = calculate_and_validate_data_size(db, data_root, chunk_size)?;

    let latest_anchor = block_tree_guard
        .read()
        .get_latest_canonical_entry()
        .block_hash;
    let anchor = anchor_hint.unwrap_or(latest_anchor);

    let _guard = ProofGenerationGuard::acquire(data_root, cache_sender)?;

    let proof = crate::mempool_service::chunks::generate_ingress_proof(
        db.clone(), // clone: Arc-wrapped DatabaseProvider — cheap ref-count bump
        data_root,
        data_size,
        chunk_size,
        signer,
        chain_id,
        anchor,
        config.consensus.enable_shadow_kzg_logging,
        config.consensus.use_kzg_ingress_proofs,
    )?;

    gossip_ingress_proof(gossip_sender, &proof, block_tree_guard, db, config);

    Ok(proof)
}

pub fn reanchor_and_store_ingress_proof(
    block_tree_guard: &BlockTreeReadGuard,
    db: &DatabaseProvider,
    config: &Config,
    signer: &IrysSigner,
    proof: &IngressProof,
    gossip_sender: &tokio::sync::mpsc::UnboundedSender<GossipBroadcastMessageV2>,
    cache_sender: &CacheServiceSender,
) -> eyre::Result<IngressProof> {
    let data_root = proof.data_root();
    let _guard = ProofGenerationGuard::acquire(data_root, cache_sender)?;

    calculate_and_validate_data_size(db, data_root, config.consensus.chunk_size)?;

    let latest_anchor = block_tree_guard
        .read()
        .get_latest_canonical_entry()
        .block_hash;

    let mut proof = proof.clone(); // clone: need owned value for set_anchor + sign mutation
    proof.set_anchor(latest_anchor);
    signer.sign_ingress_proof(&mut proof)?;
    store_ingress_proof(db, &proof, signer)?;

    gossip_ingress_proof(gossip_sender, &proof, block_tree_guard, db, config);

    Ok(proof)
}

pub fn gossip_ingress_proof(
    gossip_sender: &tokio::sync::mpsc::UnboundedSender<GossipBroadcastMessageV2>,
    ingress_proof: &IngressProof,
    block_tree_guard: &BlockTreeReadGuard,
    db: &DatabaseProvider,
    config: &Config,
) {
    // Validate anchor freshness prior to broadcast
    match Inner::validate_ingress_proof_anchor_static(block_tree_guard, db, config, ingress_proof) {
        Ok(()) => {
            let msg = GossipBroadcastMessageV2::from(ingress_proof.clone()); // clone: consumed by channel send
            if let Err(e) = gossip_sender.send(msg) {
                tracing::error!(proof.data_root = ?ingress_proof.data_root(), "Failed to gossip regenerated ingress proof: {e}");
            }
        }
        Err(e) => {
            // Skip gossip; proof stored for potential later use/regeneration.
            tracing::debug!(proof.data_root = ?ingress_proof.data_root(), "Generated ingress proof anchor invalid (not gossiped): {e}");
        }
    }
}

pub fn calculate_and_validate_data_size(
    db: &DatabaseProvider,
    data_root: DataRoot,
    chunk_size: u64,
) -> eyre::Result<u64> {
    // Load data_size & confirm we have metadata for this root
    let (data_size, chunk_count) = db.view_eyre(|tx| {
        let data_size = cached_data_root_by_data_root(tx, data_root)
            .map_err(|e| eyre::eyre!("Failed to load cached_data_root: {e}"))?
            .ok_or_else(|| eyre::eyre!("Missing cached_data_root for {data_root:?}"))?
            .data_size;
        let mut cursor = tx.cursor_dup_read::<CachedChunksIndex>()?;
        let count = cursor
            .dup_count(data_root)?
            .ok_or_else(|| eyre::eyre!("No chunks found for data_root {data_root:?}"))?;
        Ok((data_size, count))
    })?;

    let expected = data_size_to_chunk_count(data_size, chunk_size)?;
    if chunk_count != expected {
        return Err(eyre::eyre!(
            "Cannot generate ingress proof: have {chunk_count} chunks expected {expected}"
        ));
    }

    Ok(data_size)
}
