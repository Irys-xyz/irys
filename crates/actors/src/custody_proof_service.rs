use crate::block_validation::{CustodyProofFailure, check_custody_proof};
use irys_domain::{
    BlockIndexReadGuard, BlockTreeReadGuard, EpochSnapshot, StorageModulesReadGuard,
};
use irys_types::custody::{
    CustodyChallenge, CustodyOpening, CustodyProof, derive_challenge_seed,
    select_challenged_offsets,
};
use irys_types::kzg::{compute_chunk_opening_proof, default_kzg_settings, derive_challenge_point};
use irys_types::v2::{GossipBroadcastMessageV2, GossipDataV2};
use irys_types::{
    Config, DatabaseProvider, GossipCacheKey, H256, IrysAddress, IrysBlockHeader,
    PartitionChunkOffset, SendTraced as _, TokioServiceHandle, Traced,
};
use reth::revm::primitives::FixedBytes;
use std::pin::pin;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use tracing::{debug, warn};

/// Outcome of checking one gossiped custody proof.
///
/// The gossip handler records the cache key only for [`Self::Valid`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustodyGossipVerdict {
    /// The opening matches the canonical challenge. Safe to deduplicate.
    Valid,
    /// A checked opening failed. Do not cache: a later proof may be honest.
    Invalid,
    /// Local state was missing. Do not cache: the same proof can be retried.
    Unavailable,
}

#[derive(Debug)]
pub enum CustodyProofMessage {
    Challenge(CustodyChallenge),
    ReceivedProof {
        proof: CustodyProof,
        outcome: oneshot::Sender<CustodyGossipVerdict>,
    },
    TakePendingProofs(oneshot::Sender<Vec<CustodyProof>>),
    /// Drop proofs that a produced block has accepted.
    ReleaseIncluded(Vec<CustodyProof>),
    NewBlock {
        vdf_output: H256,
        block_height: u64,
        block_hash: H256,
    },
}

/// The block whose VDF output seeds proofs for the next height.
#[derive(Debug, Clone, Copy)]
struct ChallengeContext {
    block_hash: H256,
}

pub struct CustodyProofService {
    config: Config,
    storage_modules_guard: StorageModulesReadGuard,
    block_index_guard: BlockIndexReadGuard,
    block_tree_guard: BlockTreeReadGuard,
    gossip_sender: UnboundedSender<Traced<GossipBroadcastMessageV2>>,
    irys_db: DatabaseProvider,
    pending_proofs: Vec<CustodyProof>,
    context: Option<ChallengeContext>,
}

impl CustodyProofService {
    pub fn spawn_service(
        config: Config,
        storage_modules_guard: StorageModulesReadGuard,
        block_index_guard: BlockIndexReadGuard,
        block_tree_guard: BlockTreeReadGuard,
        gossip_sender: UnboundedSender<Traced<GossipBroadcastMessageV2>>,
        irys_db: DatabaseProvider,
        rx: UnboundedReceiver<CustodyProofMessage>,
        runtime_handle: tokio::runtime::Handle,
    ) -> TokioServiceHandle {
        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();
        let service = Self {
            config,
            storage_modules_guard,
            block_index_guard,
            block_tree_guard,
            gossip_sender,
            irys_db,
            pending_proofs: Vec::new(),
            context: None,
        };
        let handle = runtime_handle.spawn(service.start(rx, shutdown_rx));
        TokioServiceHandle {
            name: "custody_proof_service".to_owned(),
            handle,
            shutdown_signal: shutdown_tx,
        }
    }

    async fn start(
        mut self,
        mut rx: UnboundedReceiver<CustodyProofMessage>,
        shutdown: reth::tasks::shutdown::Shutdown,
    ) {
        debug!("Custody proof service started");
        let mut shutdown_future = pin!(shutdown);
        loop {
            tokio::select! {
                biased;
                () = &mut shutdown_future => {
                    debug!("Custody proof service shutting down");
                    break;
                }
                msg = rx.recv() => {
                    match msg {
                        Some(msg) => self.dispatch(msg).await,
                        None => break,
                    }
                }
            }
        }
        debug!("Custody proof service stopped");
    }

    async fn dispatch(&mut self, msg: CustodyProofMessage) {
        match msg {
            CustodyProofMessage::Challenge(challenge) => {
                if let Err(e) = self.handle_challenge(&challenge).await {
                    warn!(
                        partition.hash = %challenge.partition_hash,
                        error = %e,
                        "Failed to handle custody challenge",
                    );
                }
            }
            CustodyProofMessage::ReceivedProof { proof, outcome } => {
                let verdict = self.handle_received_proof(proof).await;
                if let Err(error) = outcome.send(verdict) {
                    debug!(?error, "custody gossip verdict was dropped");
                }
            }
            CustodyProofMessage::TakePendingProofs(sender) => {
                // clone: the producer needs an owned list; this service keeps the
                // proofs until the block that includes them is accepted
                let proofs = self.pending_proofs.clone();
                if let Err(error) = sender.send(proofs) {
                    debug!(?error, "pending custody proofs were not taken");
                }
            }
            CustodyProofMessage::ReleaseIncluded(included) => {
                self.pending_proofs
                    .retain(|proof| !included.contains(proof));
            }
            CustodyProofMessage::NewBlock {
                vdf_output,
                block_height,
                block_hash,
            } => {
                self.handle_new_block(vdf_output, block_height, block_hash)
                    .await;
            }
        }
    }

    async fn handle_received_proof(&mut self, proof: CustodyProof) -> CustodyGossipVerdict {
        if !self.config.consensus.enable_custody_proofs {
            return CustodyGossipVerdict::Invalid;
        }
        let Some(context) = self.context else {
            return CustodyGossipVerdict::Unavailable;
        };

        let (parent, epoch) = {
            let tree = self.block_tree_guard.read();
            // clone: the parent header is used after the block-tree lock is released
            let parent = tree.get_block(&context.block_hash).cloned();
            let epoch = tree.get_epoch_snapshot(&context.block_hash);
            (parent, epoch)
        };
        let (Some(parent), Some(epoch)) = (parent, epoch) else {
            return CustodyGossipVerdict::Unavailable;
        };

        let verdict = self
            .check_received_proof(proof.clone(), parent, epoch)
            .await;
        match verdict {
            CustodyGossipVerdict::Valid => {
                if !self.pending_proofs.contains(&proof) {
                    self.pending_proofs.push(proof);
                }
                CustodyGossipVerdict::Valid
            }
            other => other,
        }
    }

    async fn check_received_proof(
        &self,
        proof: CustodyProof,
        parent: IrysBlockHeader,
        epoch: std::sync::Arc<EpochSnapshot>,
    ) -> CustodyGossipVerdict {
        // clone: the blocking task outlives this method
        let db = self.irys_db.clone();
        // clone: ConsensusConfig is owned by the blocking task
        let consensus = self.config.consensus.clone();
        let block_index_guard = self.block_index_guard.clone();
        let block_tree_guard = self.block_tree_guard.clone();
        let joined = tokio::task::spawn_blocking(move || {
            check_custody_proof(
                &proof,
                &parent,
                &epoch,
                &consensus,
                &block_index_guard,
                &block_tree_guard,
                &db,
            )
        })
        .await;

        match joined {
            Ok(Ok(())) => CustodyGossipVerdict::Valid,
            Ok(Err(CustodyProofFailure::Invalid(reason))) => {
                warn!(reason, "Received invalid custody proof, discarding",);
                CustodyGossipVerdict::Invalid
            }
            Ok(Err(CustodyProofFailure::Unavailable(reason))) => {
                warn!(reason, "Received custody proof could not be checked");
                CustodyGossipVerdict::Unavailable
            }
            Err(error) => {
                warn!(error = %error, "custody proof check task failed");
                CustodyGossipVerdict::Unavailable
            }
        }
    }

    async fn handle_challenge(&mut self, challenge: &CustodyChallenge) -> eyre::Result<()> {
        let packed_chunks = {
            let storage_modules = self.storage_modules_guard.read();
            let Some(storage_module) = storage_modules
                .iter()
                .find(|sm| sm.partition_hash() == Some(challenge.partition_hash))
            else {
                debug!(
                    partition.hash = %challenge.partition_hash,
                    "No local storage module for challenged partition, skipping",
                );
                return Ok(());
            };

            let offsets = select_challenged_offsets(
                &challenge.challenge_seed,
                self.config.consensus.custody_challenge_count,
                self.config.consensus.num_chunks_in_partition,
            )?;

            let mut packed_chunks = Vec::with_capacity(offsets.len());
            for offset in offsets {
                let partition_offset = PartitionChunkOffset::from(offset);
                match storage_module.generate_full_chunk(partition_offset)? {
                    Some(chunk) => packed_chunks.push((offset, chunk)),
                    None => {
                        warn!(
                            partition.hash = %challenge.partition_hash,
                            chunk.offset = offset,
                            "Chunk not found at challenged offset, skipping proof generation",
                        );
                        return Ok(());
                    }
                }
            }
            packed_chunks
        };

        let iterations = self.config.consensus.entropy_packing_iterations;
        let chunk_size = usize::try_from(self.config.consensus.chunk_size)
            .map_err(|_| eyre::eyre!("chunk_size overflow"))?;
        let chain_id = self.config.consensus.chain_id;
        let challenge_seed = challenge.challenge_seed;
        let openings = tokio::task::spawn_blocking(move || -> eyre::Result<Vec<CustodyOpening>> {
            let kzg_settings = default_kzg_settings();
            let mut openings = Vec::with_capacity(packed_chunks.len());
            for (offset, packed_chunk) in packed_chunks {
                let unpacked =
                    irys_packing::unpack(&packed_chunk, iterations, chunk_size, chain_id);
                let z = derive_challenge_point(&challenge_seed, offset);
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
            Ok(openings)
        })
        .await
        .map_err(|e| eyre::eyre!("custody opening task failed: {e}"))??;

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

        // clone: pending keeps a copy; gossip takes the other
        self.pending_proofs.push(proof.clone());
        let key = GossipCacheKey::CustodyProof(proof.gossip_cache_id());
        let msg = GossipBroadcastMessageV2::new(key, GossipDataV2::CustodyProof(proof));
        if let Err(e) = self.gossip_sender.send_traced(msg) {
            warn!(%e, "Failed to send custody proof to gossip broadcast");
        }

        Ok(())
    }

    async fn handle_new_block(&mut self, vdf_output: H256, block_height: u64, block_hash: H256) {
        if !self.config.consensus.enable_custody_proofs {
            return;
        }
        if self
            .context
            .is_some_and(|context| context.block_hash == block_hash)
        {
            return;
        }

        let epoch = {
            let tree = self.block_tree_guard.read();
            tree.get_epoch_snapshot(&block_hash)
        };
        let Some(epoch) = epoch else {
            warn!(
                block.hash = %block_hash,
                "epoch snapshot missing; keeping pending custody proofs"
            );
            return;
        };

        self.pending_proofs.clear();
        self.context = Some(ChallengeContext { block_hash });

        let mining_address = IrysAddress::from_private_key(&self.config.node_config.mining_key);
        let partitions = {
            let storage_modules = self.storage_modules_guard.read();
            storage_modules
                .iter()
                .filter_map(|storage_module| {
                    let assignment = storage_module.partition_assignment()?;
                    if assignment.miner_address != mining_address
                        || assignment.ledger_id.is_none()
                        || assignment.slot_index.is_none()
                    {
                        return None;
                    }
                    let epoch_assignment =
                        epoch.get_data_partition_assignment(assignment.partition_hash)?;
                    if epoch_assignment.miner_address != mining_address
                        || epoch_assignment.ledger_id.is_none()
                        || epoch_assignment.slot_index.is_none()
                    {
                        return None;
                    }
                    Some(assignment.partition_hash)
                })
                .collect::<Vec<_>>()
        };

        for partition_hash in partitions {
            let challenge_seed = derive_challenge_seed(&vdf_output.0, &partition_hash);
            let challenge = CustodyChallenge {
                challenged_miner: mining_address,
                partition_hash,
                challenge_seed,
                challenge_block_height: block_height,
            };
            if let Err(e) = self.handle_challenge(&challenge).await {
                warn!(
                    partition.hash = %partition_hash,
                    error = %e,
                    "Failed to generate self-custody proof",
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_domain::{BlockIndex, BlockTree};
    use irys_testing_utils::new_mock_signed_header;
    use irys_types::{Config, NodeConfig};
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

    fn empty_storage_guard() -> StorageModulesReadGuard {
        StorageModulesReadGuard::new(Arc::new(RwLock::new(Vec::new())))
    }

    fn test_db() -> (
        irys_testing_utils::tempfile::TempDir,
        irys_types::DatabaseProvider,
    ) {
        use irys_database::IrysDatabaseArgs as _;

        let tmp = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = irys_database::open_or_create_db(
            tmp.path(),
            irys_database::tables::IrysTables::ALL,
            reth_db::mdbx::DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();
        (tmp, irys_types::DatabaseProvider(Arc::new(db)))
    }

    fn test_service(config: Config) -> CustodyProofService {
        let (gossip_tx, _gossip_rx) = unbounded_channel();
        let (_tmp, irys_db) = test_db();
        let block_index = BlockIndex::new_for_testing(irys_db.clone());
        let block_index_guard = BlockIndexReadGuard::new(block_index);
        let genesis = new_mock_signed_header();
        let tree = BlockTree::new(&genesis, config.consensus.clone());
        let block_tree_guard = BlockTreeReadGuard::new(Arc::new(RwLock::new(tree)));
        CustodyProofService {
            config,
            storage_modules_guard: empty_storage_guard(),
            block_index_guard,
            block_tree_guard,
            gossip_sender: gossip_tx,
            irys_db,
            pending_proofs: Vec::new(),
            context: None,
        }
    }

    #[tokio::test]
    async fn handle_challenge_unknown_partition_returns_ok() {
        let config = test_config_with_custody();
        let (gossip_tx, mut gossip_rx) = unbounded_channel();
        let mut service = test_service(config);
        service.gossip_sender = gossip_tx;

        let challenge = CustodyChallenge {
            challenged_miner: IrysAddress::from([0xAA; 20]),
            partition_hash: H256::from([0xBB; 32]),
            challenge_seed: H256::from([0xCC; 32]),
            challenge_block_height: 100,
        };

        let result = service.handle_challenge(&challenge).await;
        assert!(result.is_ok());
        assert!(gossip_rx.try_recv().is_err());
        assert!(service.pending_proofs.is_empty());
    }
}
