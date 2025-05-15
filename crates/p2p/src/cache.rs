// I have absolutely no idea how to name this module to satisfy this lint
#![allow(
    clippy::module_name_repetitions,
    reason = "I have no idea how to name this module to satisfy this lint"
)]
use crate::types::{GossipError, GossipResult};
use core::time::Duration;
use irys_types::{Address, BlockHash, ChunkPathHash, GossipData, IrysTransactionId, H256};
use std::collections::HashSet;
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::Instant,
};

/// Tracks which peers have seen what data to avoid sending duplicates
#[derive(Debug, Default)]
pub(crate) struct GossipCache {
    /// Maps data identifiers to a map of peer IPs and when they last saw the data
    chunks: Arc<RwLock<HashMap<ChunkPathHash, HashMap<Address, Instant>>>>,
    transactions: Arc<RwLock<HashMap<IrysTransactionId, HashMap<Address, Instant>>>>,
    blocks: Arc<RwLock<HashMap<BlockHash, HashMap<Address, Instant>>>>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum GossipCacheKey {
    Chunk(ChunkPathHash),
    Transaction(IrysTransactionId),
    Block(BlockHash),
}

impl From<&GossipData> for GossipCacheKey {
    fn from(data: &GossipData) -> Self {
        match data {
            GossipData::Chunk(chunk) => GossipCacheKey::Chunk(chunk.chunk_path_hash()),
            GossipData::Transaction(transaction) => GossipCacheKey::Transaction(transaction.id),
            GossipData::Block(block) => GossipCacheKey::Block(block.block_hash),
        }
    }
}

impl GossipCache {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn seen_block_from_any_peer(&self, block_hash: &BlockHash) -> GossipResult<bool> {
        let blocks = self
            .blocks
            .read()
            .map_err(|error| GossipError::Cache(error.to_string()))?;

        Ok(blocks.contains_key(block_hash))
    }

    pub(crate) fn seen_transaction_from_any_peer(
        &self,
        transaction_id: &IrysTransactionId,
    ) -> GossipResult<bool> {
        let txs = self
            .transactions
            .read()
            .map_err(|error| GossipError::Cache(error.to_string()))?;

        Ok(txs.contains_key(transaction_id))
    }

    /// Record that a peer has seen some data
    ///
    /// # Errors
    ///
    /// This function will return an error if the cache cannot be accessed.
    pub(crate) fn record_seen(&self, miner_address: Address, key: GossipCacheKey) -> GossipResult<()> {
        let now = Instant::now();
        match key {
            GossipCacheKey::Chunk(chunk_path_hash) => {
                let mut chunks = self
                    .chunks
                    .write()
                    .map_err(|error| GossipError::Cache(error.to_string()))?;
                let peer_map = chunks.entry(chunk_path_hash).or_default();
                peer_map.insert(miner_address, now);
            }
            GossipCacheKey::Transaction(irys_transaction_hash) => {
                let mut txs = self
                    .transactions
                    .write()
                    .map_err(|error| GossipError::Cache(error.to_string()))?;
                let peer_map = txs.entry(irys_transaction_hash).or_default();
                peer_map.insert(miner_address, now);
            }
            GossipCacheKey::Block(irys_block_hash) => {
                let mut blocks = self
                    .blocks
                    .write()
                    .map_err(|error| GossipError::Cache(error.to_string()))?;
                let peer_map = blocks.entry(irys_block_hash).or_default();
                peer_map.insert(miner_address, now);
            }
        }
        Ok(())
    }

    pub(crate) fn peers_that_have_seen(&self, data: &GossipData) -> GossipResult<HashSet<Address>> {
        let result = match data {
            GossipData::Chunk(unpacked_chunk) => {
                let chunk_path_hash = unpacked_chunk.chunk_path_hash();
                let chunks = self
                    .chunks
                    .read()
                    .map_err(|error| GossipError::Cache(error.to_string()))?;
                chunks.get(&chunk_path_hash).cloned().unwrap_or_default()
            }
            GossipData::Transaction(transaction) => {
                let txs = self
                    .transactions
                    .read()
                    .map_err(|error| GossipError::Cache(error.to_string()))?;
                txs.get(&transaction.id).cloned().unwrap_or_default()
            }
            GossipData::Block(block) => {
                let blocks = self
                    .blocks
                    .read()
                    .map_err(|error| GossipError::Cache(error.to_string()))?;
                blocks.get(&block.block_hash).cloned().unwrap_or_default()
            }
        };

        Ok(result.keys().copied().collect())
    }

    /// Clean up old entries that are older than the given duration
    ///
    /// # Errors
    ///
    /// This function will return an error if the cache cannot be accessed.
    pub(crate) fn prune_expired(&self, older_than: Duration) -> GossipResult<()> {
        let now = Instant::now();

        let cleanup_map = |map: &mut HashMap<H256, HashMap<Address, Instant>>| {
            map.retain(|_, peer_map| {
                peer_map.retain(|_, &mut last_seen| now.duration_since(last_seen) <= older_than);
                !peer_map.is_empty()
            });
        };

        {
            let mut chunks_guard = self
                .chunks
                .write()
                .map_err(|error| GossipError::Cache(error.to_string()))?;
            let chunks = &mut *chunks_guard;
            cleanup_map(chunks);
        };

        {
            let mut txs_guard = self
                .transactions
                .write()
                .map_err(|error| GossipError::Cache(error.to_string()))?;
            let txs = &mut *txs_guard;
            cleanup_map(txs);
        };

        {
            let mut blocks_guard = self
                .blocks
                .write()
                .map_err(|error| GossipError::Cache(error.to_string()))?;
            let blocks = &mut *blocks_guard;
            cleanup_map(blocks);
        };

        Ok(())
    }
}
