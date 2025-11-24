use crate::mempool_service::{IngressProofError, Inner};
use irys_database::delete_ingress_proof;
use irys_database::tables::{CompactCachedIngressProof, IngressProofs};
use irys_domain::BlockTreeReadGuard;
use irys_types::{
    ingress::CachedIngressProof, Config, DataRoot, DatabaseProvider, GossipBroadcastMessage,
    IngressProof,
};
use reth_db::{transaction::DbTxMut as _, Database as _, DatabaseError};
use tracing::{instrument, warn};

impl Inner {
    #[tracing::instrument(level = "trace", skip_all, fields(data_root = %ingress_proof.data_root))]
    pub fn handle_ingest_ingress_proof(
        &self,
        ingress_proof: IngressProof,
    ) -> Result<(), IngressProofError> {
        // Validate the proofs signature and basic details
        let address = ingress_proof
            .pre_validate(&ingress_proof.data_root)
            .map_err(|_| IngressProofError::InvalidSignature)?;

        // Validate the proof address is a staked address
        let epoch_snapshot = self.block_tree_read_guard.read().canonical_epoch_snapshot();
        let commitment_snapshot = self
            .block_tree_read_guard
            .read()
            .canonical_commitment_snapshot();

        if !epoch_snapshot.is_staked(address) && !commitment_snapshot.is_staked(address) {
            return Err(IngressProofError::UnstakedAddress);
        }

        // validate the anchor
        match self.validate_ingress_proof_anchor(&ingress_proof) {
            Ok(_) => {}
            Err(e) => {
                return Err(e);
            }
        }

        let res = self
            .irys_db
            .update(|rw_tx| -> Result<(), DatabaseError> {
                rw_tx.put::<IngressProofs>(
                    ingress_proof.data_root,
                    CompactCachedIngressProof(CachedIngressProof {
                        address,
                        proof: ingress_proof.clone(),
                    }),
                )?;
                Ok(())
            })
            .map_err(|_| IngressProofError::DatabaseError)?;

        if res.is_err() {
            return Err(IngressProofError::DatabaseError);
        }

        let gossip_sender = &self.service_senders.gossip_broadcast;
        let gossip_broadcast_message = GossipBroadcastMessage::from(ingress_proof);

        if let Err(error) = gossip_sender.send(gossip_broadcast_message) {
            tracing::error!("Failed to send gossip data: {:?}", error);
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
        let anchor_height = match Self::get_anchor_height_static(
            block_tree_read_guard,
            irys_db,
            ingress_proof.anchor,
            false, /* does not need to be canonical */
        )
        .map_err(|_e| IngressProofError::DatabaseError)?
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
                return Err(IngressProofError::InvalidAnchor(ingress_proof.anchor));
            }
            else {
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
            .map_err(|_| IngressProofError::DatabaseError)?
            .map_err(|_| IngressProofError::DatabaseError)?;

        Ok(())
    }

    /// Validate the ingress proof anchor, and if invalid, remove the ingress proof from the database.
    /// Returns `Ok(true)` if the proof was removed, `Ok(false)` if it was valid and not removed.
    #[instrument(skip_all, fields(proof.data_root = ?ingress_proof.data_root))]
    pub fn is_ingress_proof_expired(
        &self,
        ingress_proof: &IngressProof,
    ) -> Result<bool, IngressProofError> {
        match self.validate_ingress_proof_anchor(ingress_proof) {
            // Not removed
            Ok(()) => Ok(false),
            Err(e) => {
                warn!(
                    "Ingress proof anchor validation failed: {:?}. Pruning the proof",
                    e
                );
                Ok(true)
            }
        }
    }
}
