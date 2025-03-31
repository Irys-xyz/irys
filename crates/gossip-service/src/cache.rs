use crate::types::{GossipError, GossipResult};
use irys_types::{BlockHash, ChunkPathHash, GossipData, H256};
use std::net::SocketAddr;
use std::ops::DerefMut;
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
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
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that a peer has seen some data
    pub fn record_seen(&self, peer_ip: SocketAddr, data: &GossipData) -> GossipResult<()> {
        let now = Instant::now();
        match data {
            GossipData::Chunk(unpacked_chunk) => {
                let mut chunks = self
                    .chunks
                    .write()
                    .map_err(|e| GossipError::Cache(e.to_string()))?;
                let peer_map = chunks.entry(unpacked_chunk.chunk_path_hash()).or_default();
                peer_map.insert(peer_ip, now);
            }
            GossipData::Transaction(irys_transaction_header) => {
                let mut txs = self
                    .transactions
                    .write()
                    .map_err(|e| GossipError::Cache(e.to_string()))?;
                let peer_map = txs.entry(irys_transaction_header.id).or_default();
                peer_map.insert(peer_ip, now);
            }
            GossipData::Block(combined_block) => {
                let mut blocks = self
                    .blocks
                    .write()
                    .map_err(|e| GossipError::Cache(e.to_string()))?;
                let peer_map = blocks.entry(combined_block.irys.block_hash).or_default();
                peer_map.insert(peer_ip, now);
            }
        }
        Ok(())
    }

    /// Check if a peer has seen some data within the given duration
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
                    .map_err(|e| GossipError::Cache(e.to_string()))?;
                chunks
                    .get(&chunk_path_hash)
                    .and_then(|peer_map| peer_map.get(peer_ip))
                    .map(|&last_seen| now.duration_since(last_seen) <= within)
                    .unwrap_or(false)
            }
            GossipData::Transaction(transaction) => {
                let txs = self
                    .transactions
                    .read()
                    .map_err(|e| GossipError::Cache(e.to_string()))?;
                txs.get(&transaction.id)
                    .and_then(|peer_map| peer_map.get(peer_ip))
                    .map(|&last_seen| now.duration_since(last_seen) <= within)
                    .unwrap_or(false)
            }
            GossipData::Block(block) => {
                let blocks = self
                    .blocks
                    .read()
                    .map_err(|e| GossipError::Cache(e.to_string()))?;
                blocks
                    .get(&block.irys.block_hash)
                    .and_then(|peer_map| peer_map.get(peer_ip))
                    .map(|&last_seen| now.duration_since(last_seen) <= within)
                    .unwrap_or(false)
            }
        };

        Ok(result)
    }

    /// Clean up old entries that are older than the given duration
    pub fn cleanup(&self, older_than: Duration) -> GossipResult<()> {
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
                .map_err(|e| GossipError::Cache(e.to_string()))?;
            let mut chunks = chunks_guard.deref_mut();
            cleanup_chunks(&mut chunks);
        }

        {
            let mut txs_guard = self
                .transactions
                .write()
                .map_err(|e| GossipError::Cache(e.to_string()))?;
            let mut txs = txs_guard.deref_mut();
            cleanup_chunks(&mut txs);
        }

        {
            let mut blocks_guard = self
                .blocks
                .write()
                .map_err(|e| GossipError::Cache(e.to_string()))?;
            let mut blocks = blocks_guard.deref_mut();
            cleanup_chunks(&mut blocks);
        }

        Ok(())
    }
}
