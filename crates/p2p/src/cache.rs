// I have absolutely no idea how to name this module to satisfy this lint
#![allow(
    clippy::module_name_repetitions,
    reason = "I have no idea how to name this module to satisfy this lint"
)]
use crate::types::GossipResult;
use core::time::Duration;
use irys_types::{BlockHash, ChunkPathHash, GossipCacheKey, IrysPeerId, IrysTransactionId, H256};
use moka::sync::Cache;
use reth::revm::primitives::B256;
use std::collections::HashSet;
use std::sync::{Arc, RwLock};

/// TTL duration for cache entries
const GOSSIP_CACHE_TTL: Duration = Duration::from_secs(300); // 5 minutes

/// Tracks which peers have seen what data to avoid sending duplicates
#[derive(Debug)]
pub struct GossipCache {
    /// Maps data identifiers to a set of peer IDs that have seen the data
    chunks: Cache<ChunkPathHash, Arc<RwLock<HashSet<IrysPeerId>>>>,
    transactions: Cache<IrysTransactionId, Arc<RwLock<HashSet<IrysPeerId>>>>,
    blocks: Cache<BlockHash, Arc<RwLock<HashSet<IrysPeerId>>>>,
    payloads: Cache<B256, Arc<RwLock<HashSet<IrysPeerId>>>>,
    ingress_proofs: Cache<H256, Arc<RwLock<HashSet<IrysPeerId>>>>,
    custody_proofs: Cache<H256, Arc<RwLock<HashSet<IrysPeerId>>>>,
}

impl Default for GossipCache {
    fn default() -> Self {
        Self::new()
    }
}

impl GossipCache {
    #[must_use]
    pub fn new() -> Self {
        Self {
            chunks: Cache::builder().time_to_live(GOSSIP_CACHE_TTL).build(),
            transactions: Cache::builder().time_to_live(GOSSIP_CACHE_TTL).build(),
            blocks: Cache::builder().time_to_live(GOSSIP_CACHE_TTL).build(),
            payloads: Cache::builder().time_to_live(GOSSIP_CACHE_TTL).build(),
            ingress_proofs: Cache::builder().time_to_live(GOSSIP_CACHE_TTL).build(),
            custody_proofs: Cache::builder().time_to_live(GOSSIP_CACHE_TTL).build(),
        }
    }

    pub(crate) fn seen_block_from_any_peer(&self, block_hash: &BlockHash) -> GossipResult<bool> {
        Ok(self.blocks.contains_key(block_hash))
    }

    pub(crate) fn seen_execution_payload_from_any_peer(
        &self,
        evm_block_hash: &B256,
    ) -> GossipResult<bool> {
        Ok(self.payloads.contains_key(evm_block_hash))
    }

    pub(crate) fn seen_transaction_from_any_peer(
        &self,
        transaction_id: &IrysTransactionId,
    ) -> GossipResult<bool> {
        Ok(self.transactions.contains_key(transaction_id))
    }

    pub(crate) fn seen_ingress_proof_from_any_peer(
        &self,
        ingress_proof_hash: &H256,
    ) -> GossipResult<bool> {
        Ok(self.ingress_proofs.contains_key(ingress_proof_hash))
    }

    pub(crate) fn seen_custody_proof_from_any_peer(
        &self,
        partition_hash: &H256,
    ) -> GossipResult<bool> {
        Ok(self.custody_proofs.contains_key(partition_hash))
    }

    /// Record that a peer has seen some data
    ///
    /// # Errors
    ///
    /// This function will return an error if the cache cannot be accessed.
    pub(crate) fn record_seen(&self, peer_id: IrysPeerId, key: GossipCacheKey) -> GossipResult<()> {
        match key {
            GossipCacheKey::Chunk(chunk_path_hash) => {
                let peer_set = self.chunks.get(&chunk_path_hash).unwrap_or_else(|| {
                    let new_set = Arc::new(RwLock::new(HashSet::new()));
                    self.chunks.insert(chunk_path_hash, new_set.clone());
                    new_set
                });
                peer_set.write().unwrap().insert(peer_id);
            }
            GossipCacheKey::Transaction(irys_transaction_hash) => {
                let peer_set = self
                    .transactions
                    .get(&irys_transaction_hash)
                    .unwrap_or_else(|| {
                        let new_set = Arc::new(RwLock::new(HashSet::new()));
                        self.transactions
                            .insert(irys_transaction_hash, new_set.clone());
                        new_set
                    });
                peer_set.write().unwrap().insert(peer_id);
            }
            GossipCacheKey::Block(irys_block_hash) => {
                let peer_set = self.blocks.get(&irys_block_hash).unwrap_or_else(|| {
                    let new_set = Arc::new(RwLock::new(HashSet::new()));
                    self.blocks.insert(irys_block_hash, new_set.clone());
                    new_set
                });
                peer_set.write().unwrap().insert(peer_id);
            }
            GossipCacheKey::ExecutionPayload(payload_block_hash) => {
                let peer_set = self.payloads.get(&payload_block_hash).unwrap_or_else(|| {
                    let new_set = Arc::new(RwLock::new(HashSet::new()));
                    self.payloads.insert(payload_block_hash, new_set.clone());
                    new_set
                });
                peer_set.write().unwrap().insert(peer_id);
            }
            GossipCacheKey::IngressProof(proof_hash) => {
                let peer_set = self.ingress_proofs.get(&proof_hash).unwrap_or_else(|| {
                    let new_set = Arc::new(RwLock::new(HashSet::new()));
                    self.ingress_proofs.insert(proof_hash, new_set.clone());
                    new_set
                });
                peer_set.write().unwrap().insert(peer_id);
            }
            GossipCacheKey::CustodyProof(partition_hash) => {
                let peer_set = self.custody_proofs.get(&partition_hash).unwrap_or_else(|| {
                    let new_set = Arc::new(RwLock::new(HashSet::new()));
                    self.custody_proofs.insert(partition_hash, new_set.clone());
                    new_set
                });
                peer_set.write().unwrap().insert(peer_id);
            }
        }
        Ok(())
    }

    pub(crate) fn peers_that_have_seen(
        &self,
        cache_key: &GossipCacheKey,
    ) -> GossipResult<HashSet<IrysPeerId>> {
        let result = match cache_key {
            GossipCacheKey::Chunk(chunk_path_hash) => self
                .chunks
                .get(chunk_path_hash)
                .map(|arc| arc.read().unwrap().clone())
                .unwrap_or_default(),
            GossipCacheKey::Transaction(transaction_id) => self
                .transactions
                .get(transaction_id)
                .map(|arc| arc.read().unwrap().clone())
                .unwrap_or_default(),
            GossipCacheKey::Block(block_hash) => self
                .blocks
                .get(block_hash)
                .map(|arc| arc.read().unwrap().clone())
                .unwrap_or_default(),
            GossipCacheKey::ExecutionPayload(evm_block_hash) => self
                .payloads
                .get(evm_block_hash)
                .map(|arc| arc.read().unwrap().clone())
                .unwrap_or_default(),
            GossipCacheKey::IngressProof(proof_hash) => self
                .ingress_proofs
                .get(proof_hash)
                .map(|arc| arc.read().unwrap().clone())
                .unwrap_or_default(),
            GossipCacheKey::CustodyProof(partition_hash) => self
                .custody_proofs
                .get(partition_hash)
                .map(|arc| arc.read().unwrap().clone())
                .unwrap_or_default(),
        };

        Ok(result)
    }
}
