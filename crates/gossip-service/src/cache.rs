// I have absolutely no idea how to name this module to satisfy this lint
#![allow(
    clippy::module_name_repetitions,
    reason = "I have no idea how to name this module to satisfy this lint"
)]
use crate::types::{GossipError, GossipResult};
use core::net::SocketAddr;
use core::time::Duration;
use irys_types::{BlockHash, ChunkPathHash, GossipData, IrysTransactionId, H256};
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::Instant,
};

/// Tracks which peers have seen what data to avoid sending duplicates
#[derive(Debug, Default)]
pub struct GossipCache {
    /// Maps data identifiers to a map of peer IPs and when they last saw the data
    chunks: Arc<RwLock<HashMap<ChunkPathHash, HashMap<SocketAddr, Instant>>>>,
    transactions: Arc<RwLock<HashMap<IrysTransactionId, HashMap<SocketAddr, Instant>>>>,
    blocks: Arc<RwLock<HashMap<BlockHash, HashMap<SocketAddr, Instant>>>>,
}

impl GossipCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that a peer has seen some data
    ///
    /// # Errors
    ///
    /// This function will return an error if the cache cannot be accessed.
    pub fn record_seen(&self, peer_ip: SocketAddr, data: &GossipData) -> GossipResult<()> {
        let now = Instant::now();
        match data {
            GossipData::Chunk(unpacked_chunk) => {
                let mut chunks = self
                    .chunks
                    .write()
                    .map_err(|error| GossipError::Cache(error.to_string()))?;
                let peer_map = chunks.entry(unpacked_chunk.chunk_path_hash()).or_default();
                peer_map.insert(peer_ip, now);
            }
            GossipData::Transaction(irys_transaction_header) => {
                let mut txs = self
                    .transactions
                    .write()
                    .map_err(|error| GossipError::Cache(error.to_string()))?;
                let peer_map = txs.entry(irys_transaction_header.id).or_default();
                peer_map.insert(peer_ip, now);
            }
            GossipData::Block(irys_block_header) => {
                let mut blocks = self
                    .blocks
                    .write()
                    .map_err(|error| GossipError::Cache(error.to_string()))?;
                let peer_map = blocks.entry(irys_block_header.block_hash).or_default();
                peer_map.insert(peer_ip, now);
            }
        }
        Ok(())
    }

    /// Check if a peer has seen some data within the given duration
    ///
    /// # Errors
    ///
    /// This function will return an error if the cache cannot be accessed.
    pub fn has_seen(
        &self,
        peer_ip: &SocketAddr,
        data: &GossipData,
        within: Duration,
    ) -> GossipResult<bool> {
        let now = Instant::now();

        let result = match data {
            GossipData::Chunk(unpacked_chunk) => {
                let chunk_path_hash = unpacked_chunk.chunk_path_hash();
                let chunks = self
                    .chunks
                    .read()
                    .map_err(|error| GossipError::Cache(error.to_string()))?;
                chunks
                    .get(&chunk_path_hash)
                    .and_then(|peer_map| peer_map.get(peer_ip))
                    .is_some_and(|&last_seen| now.duration_since(last_seen) <= within)
            }
            GossipData::Transaction(transaction) => {
                let txs = self
                    .transactions
                    .read()
                    .map_err(|error| GossipError::Cache(error.to_string()))?;
                txs.get(&transaction.id)
                    .and_then(|peer_map| peer_map.get(peer_ip))
                    .is_some_and(|&last_seen| now.duration_since(last_seen) <= within)
            }
            GossipData::Block(block) => {
                let blocks = self
                    .blocks
                    .read()
                    .map_err(|error| GossipError::Cache(error.to_string()))?;
                blocks
                    .get(&block.block_hash)
                    .and_then(|peer_map| peer_map.get(peer_ip))
                    .is_some_and(|&last_seen| now.duration_since(last_seen) <= within)
            }
        };

        Ok(result)
    }

    /// Clean up old entries that are older than the given duration
    ///
    /// # Errors
    ///
    /// This function will return an error if the cache cannot be accessed.
    pub fn prune_expired(&self, older_than: Duration) -> GossipResult<()> {
        let now = Instant::now();

        let cleanup_chunks = |map: &mut HashMap<H256, HashMap<SocketAddr, Instant>>| {
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
            cleanup_chunks(chunks);
        };

        {
            let mut txs_guard = self
                .transactions
                .write()
                .map_err(|error| GossipError::Cache(error.to_string()))?;
            let txs = &mut *txs_guard;
            cleanup_chunks(txs);
        };

        {
            let mut blocks_guard = self
                .blocks
                .write()
                .map_err(|error| GossipError::Cache(error.to_string()))?;
            let blocks = &mut *blocks_guard;
            cleanup_chunks(blocks);
        };

        Ok(())
    }
}
