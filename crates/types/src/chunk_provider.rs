//! Chunk provider trait for PD precompile integration.

use alloy_primitives::B256;
use bytes::Bytes;
use dashmap::DashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

use crate::ChunkFormat;
use crate::range_specifier::PdDataRead;

/// Configuration values needed for chunk operations.
#[derive(Debug, Clone, Copy)]
pub struct ChunkConfig {
    pub num_chunks_in_partition: u64,
    pub chunk_size: u64,
    pub entropy_packing_iterations: u32,
    pub chain_id: u64,
}

impl ChunkConfig {
    pub fn from_consensus(consensus: &crate::ConsensusConfig) -> Self {
        Self {
            num_chunks_in_partition: consensus.num_chunks_in_partition,
            chunk_size: consensus.chunk_size,
            entropy_packing_iterations: consensus.entropy_packing_iterations,
            chain_id: consensus.chain_id,
        }
    }
}

/// Provides unpacked chunks to PD precompile.
/// Used as storage backend by PdService.
pub trait ChunkStorageProvider: Send + Sync + std::fmt::Debug {
    /// Returns unpacked chunk bytes or `None` if not found.
    fn get_unpacked_chunk_by_ledger_offset(
        &self,
        ledger: u32,
        ledger_offset: u64,
    ) -> eyre::Result<Option<Bytes>>;

    /// Returns a chunk for PD serving. Currently always returns packed data from
    /// storage modules. In the future, may check MDBX CachedChunks first and
    /// return unpacked data when available (avoiding unpacking cost for the caller).
    fn get_chunk_for_pd(
        &self,
        ledger: u32,
        ledger_offset: u64,
    ) -> eyre::Result<Option<ChunkFormat>>;

    #[must_use]
    fn config(&self) -> ChunkConfig;
}

/// Error returned when a PD chunk fetch fails.
#[derive(Debug)]
pub struct PdChunkFetchFailure {
    pub message: String,
    /// API addresses of peers that were tried and failed.
    pub failed_peers: Vec<SocketAddr>,
}

/// Success result from a PD chunk fetch, including which peer served it.
#[derive(Debug)]
pub struct PdChunkFetchSuccess {
    pub chunk: ChunkFormat,
    /// API address of the peer that served this chunk (for attribution on
    /// verification failure).
    pub serving_peer: SocketAddr,
}

/// Fetches PD chunks from remote peers (HTTP + gossip fallback).
/// Defined in irys-types so irys-actors can use it without depending on irys-p2p.
#[async_trait::async_trait]
pub trait PdChunkFetcher: Send + Sync + 'static {
    /// Fetch a chunk from the given peers. Tries each peer in order.
    /// Returns the chunk and which peer served it, or an error with the list of
    /// failed peer addresses.
    async fn fetch_chunk(
        &self,
        peers: &[crate::PeerAddress],
        ledger: u32,
        offset: u64,
    ) -> Result<PdChunkFetchSuccess, PdChunkFetchFailure>;
}

/// Pushes a PD chunk to a peer. Fire-and-forget (detached tokio task).
pub trait PdChunkPusher: Send + Sync + 'static {
    fn push_pd_chunk(
        &self,
        peer_id: crate::IrysPeerId,
        peer_addr: &crate::PeerAddress,
        push: &crate::gossip::PdChunkPush,
    );
}

// ============================================================================
// PD Chunk Manager Message Types
// ============================================================================

/// Messages for the unified PD chunk manager.
#[derive(Debug)]
pub enum PdChunkMessage {
    /// New PD transaction detected — start provisioning chunks.
    NewTransaction {
        tx_hash: B256,
        chunk_specs: Vec<PdDataRead>,
    },
    /// Transaction removed from mempool (included in block or evicted).
    TransactionRemoved { tx_hash: B256 },
    /// Provision chunks needed for validating a peer block.
    ProvisionBlockChunks {
        block_hash: B256,
        chunk_specs: Vec<PdDataRead>,
        response: oneshot::Sender<Result<(), Vec<(u32, u64)>>>,
    },
    /// Release chunks provisioned for a block after validation completes.
    ReleaseBlockChunks { block_hash: B256 },
    /// A PD chunk pushed optimistically by a peer before block validation.
    OptimisticPush {
        peer_id: crate::IrysPeerId,
        push: crate::gossip::PdChunkPush,
    },
}

/// Sender for PD chunk messages.
pub type PdChunkSender = mpsc::UnboundedSender<PdChunkMessage>;

/// Receiver for PD chunk messages.
pub type PdChunkReceiver = mpsc::UnboundedReceiver<PdChunkMessage>;

/// Shared chunk data index for lock-free chunk reads during EVM execution.
/// PdService populates this during provisioning; the PD precompile reads it directly.
/// Keys are `(ledger: u32, offset: u64)` tuples matching `ChunkKey` semantics.
pub type ChunkDataIndex = Arc<DashMap<(u32, u64), Arc<Bytes>>>;
