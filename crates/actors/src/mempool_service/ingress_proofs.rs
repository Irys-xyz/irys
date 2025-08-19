use irys_database::tables::{CompactCachedIngressProof, IngressProofs};
use irys_types::{ingress::CachedIngressProof, GossipBroadcastMessage, IngressProof};
use reth_db::{transaction::DbTxMut as _, Database as _, DatabaseError};

use crate::mempool_service::{IngressProofError, Inner};

impl Inner {
    pub fn handle_ingest_ingress_proof_message(
        &self,
        ingress_proof: IngressProof,
    ) -> Result<(), IngressProofError> {
        // TODO: Validate signature
        // Validate Address is staked

        let db = self.irys_db.clone();
        let address = ingress_proof
            .recover_signer()
            .map_err(|_| IngressProofError::InvalidSignature)?;

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

        let gossip_sender = &self.service_senders.gossip_broadcast.clone();
        let gossip_broadcast_message = GossipBroadcastMessage::from(ingress_proof);

        if let Err(error) = gossip_sender.send(gossip_broadcast_message) {
            tracing::error!("Failed to send gossip data: {:?}", error);
        }

        Ok(())
    }
}
