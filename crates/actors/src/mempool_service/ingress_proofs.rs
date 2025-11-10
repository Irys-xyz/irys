use irys_database::tables::{CompactCachedIngressProof, IngressProofs};
use irys_types::{ingress::CachedIngressProof, GossipBroadcastMessage, IngressProof};
use reth_db::{transaction::DbTxMut as _, Database as _, DatabaseError};
use tracing::warn;

use crate::mempool_service::{IngressProofError, Inner};

impl Inner {
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
        {
            let latest_height = self.get_latest_block_height().map_err(|_e| {
                IngressProofError::Other(
                    "unable to get canonical chain from block tree ".to_owned(),
                )
            })?;

            // TODO: add an ingress proof invalid LRU, like we have for txs
            let anchor_height = match self
                .get_anchor_height(ingress_proof.anchor)
                .map_err(|_e| IngressProofError::DatabaseError)?
            {
                Some(height) => height,
                None => {
                    // Self::mark_tx_as_invalid(self.mempool_state.write().await, tx_id, "Unknown anchor");
                    return Err(IngressProofError::InvalidAnchor(ingress_proof.anchor));
                }
            };

            // check consensus config

            let min_anchor_height = latest_height.saturating_sub(
                self.config
                    .consensus
                    .mempool
                    .ingress_proof_anchor_expiry_depth as u64,
            );

            let too_old = anchor_height < min_anchor_height;

            if too_old {
                warn!("Ingress proof anchor is too old");
                return Err(IngressProofError::InvalidAnchor(ingress_proof.anchor));
            }
        }

        let db = self.irys_db.clone();

        let res = db
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
}
