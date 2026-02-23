use irys_domain::StorageModulesReadGuard;
use irys_types::custody::{
    select_challenged_offsets, CustodyChallenge, CustodyOpening, CustodyProof,
};
use irys_types::kzg::{compute_chunk_opening_proof, default_kzg_settings, derive_challenge_point};
use irys_types::v2::{GossipBroadcastMessageV2, GossipDataV2};
use irys_types::{Config, GossipCacheKey, PartitionChunkOffset};
use reth::revm::primitives::FixedBytes;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{debug, warn};

#[derive(Debug)]
pub enum CustodyProofMessage {
    Challenge(CustodyChallenge),
}

pub struct CustodyProofService {
    config: Config,
    storage_modules_guard: StorageModulesReadGuard,
    gossip_sender: UnboundedSender<GossipBroadcastMessageV2>,
}

impl CustodyProofService {
    pub fn spawn_service(
        config: Config,
        storage_modules_guard: StorageModulesReadGuard,
        gossip_sender: UnboundedSender<GossipBroadcastMessageV2>,
        rx: UnboundedReceiver<CustodyProofMessage>,
        runtime_handle: tokio::runtime::Handle,
    ) {
        let service = Self {
            config,
            storage_modules_guard,
            gossip_sender,
        };

        runtime_handle.spawn(service.start(rx));
    }

    async fn start(self, mut rx: UnboundedReceiver<CustodyProofMessage>) {
        debug!("Custody proof service started");
        while let Some(msg) = rx.recv().await {
            match msg {
                CustodyProofMessage::Challenge(challenge) => {
                    if let Err(e) = self.handle_challenge(&challenge) {
                        warn!(
                            partition.hash = %challenge.partition_hash,
                            error = %e,
                            "Failed to handle custody challenge",
                        );
                    }
                }
            }
        }
        debug!("Custody proof service stopped");
    }

    fn handle_challenge(&self, challenge: &CustodyChallenge) -> eyre::Result<()> {
        let storage_modules = self.storage_modules_guard.read();
        let sm = storage_modules
            .iter()
            .find(|sm| sm.partition_hash() == Some(challenge.partition_hash));

        let sm = match sm {
            Some(sm) => sm,
            None => {
                debug!(
                    partition.hash = %challenge.partition_hash,
                    "No local storage module for challenged partition, skipping",
                );
                return Ok(());
            }
        };

        let offsets = select_challenged_offsets(
            &challenge.challenge_seed,
            self.config.consensus.custody_challenge_count,
            self.config.consensus.num_chunks_in_partition,
        )?;

        let kzg_settings = default_kzg_settings();
        let chunk_size = usize::try_from(self.config.consensus.chunk_size)
            .map_err(|_| eyre::eyre!("chunk_size overflow"))?;

        let mut openings = Vec::with_capacity(offsets.len());

        for offset in offsets {
            let partition_offset = PartitionChunkOffset::from(offset);

            let packed_chunk = match sm.generate_full_chunk(partition_offset)? {
                Some(c) => c,
                None => {
                    warn!(
                        partition.hash = %challenge.partition_hash,
                        chunk.offset = offset,
                        "Chunk not found at challenged offset, skipping proof generation",
                    );
                    return Ok(());
                }
            };

            let unpacked = irys_packing::unpack(
                &packed_chunk,
                self.config.consensus.entropy_packing_iterations,
                chunk_size,
                self.config.consensus.chain_id,
            );

            let z = derive_challenge_point(&challenge.challenge_seed, offset);
            let (proof_bytes, y_bytes) =
                compute_chunk_opening_proof(&unpacked.bytes.0, &z, kzg_settings)?;

            openings.push(CustodyOpening {
                chunk_offset: offset,
                data_root: packed_chunk.data_root,
                tx_chunk_index: *packed_chunk.tx_offset,
                evaluation_point: FixedBytes::from(z),
                evaluation_value: FixedBytes::from(y_bytes),
                opening_proof: FixedBytes::from(proof_bytes),
            });
        }

        let proof = CustodyProof {
            challenged_miner: challenge.challenged_miner,
            partition_hash: challenge.partition_hash,
            challenge_seed: challenge.challenge_seed,
            openings,
        };

        debug!(
            partition.hash = %proof.partition_hash,
            openings.count = proof.openings.len(),
            "Generated custody proof",
        );

        let key = GossipCacheKey::CustodyProof(proof.partition_hash);
        let msg = GossipBroadcastMessageV2::new(key, GossipDataV2::CustodyProof(proof));

        if let Err(e) = self.gossip_sender.send(msg) {
            warn!(error = %e, "Failed to send custody proof to gossip broadcast");
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::{Config, IrysAddress, NodeConfig, H256};
    use std::sync::{Arc, RwLock};
    use tokio::sync::mpsc::unbounded_channel;

    fn test_config_with_custody() -> Config {
        let mut node_config = NodeConfig::testing();
        let consensus = node_config.consensus.get_mut();
        consensus.enable_custody_proofs = true;
        consensus.accept_kzg_ingress_proofs = true;
        consensus.custody_challenge_count = 3;
        Config::new_with_random_peer_id(node_config)
    }

    fn empty_storage_guard() -> irys_domain::StorageModulesReadGuard {
        irys_domain::StorageModulesReadGuard::new(Arc::new(RwLock::new(Vec::new())))
    }

    #[test]
    fn handle_challenge_unknown_partition_returns_ok() {
        let config = test_config_with_custody();
        let (gossip_tx, mut gossip_rx) = unbounded_channel();
        let service = CustodyProofService {
            config,
            storage_modules_guard: empty_storage_guard(),
            gossip_sender: gossip_tx,
        };

        let challenge = CustodyChallenge {
            challenged_miner: IrysAddress::from([0xAA; 20]),
            partition_hash: H256::from([0xBB; 32]),
            challenge_seed: H256::from([0xCC; 32]),
            challenge_block_height: 100,
        };

        let result = service.handle_challenge(&challenge);
        assert!(result.is_ok());
        assert!(gossip_rx.try_recv().is_err());
    }
}
