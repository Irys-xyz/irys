//! Chunk provider trait for PD precompile integration.

use alloy_primitives::B256;
use bytes::Bytes;
use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

use crate::range_specifier::ChunkRangeSpecifier;

/// Configuration values needed for chunk operations.
#[derive(Debug, Clone, Copy)]
pub struct ChunkConfig {
    pub num_chunks_in_partition: u64,
    pub chunk_size: u64,
    pub entropy_packing_iterations: u8,
    pub chain_id: u16,
}

impl ChunkConfig {
    pub fn from_consensus(consensus: &crate::ConsensusConfig) -> Self {
        Self {
            num_chunks_in_partition: consensus.num_chunks_in_partition,
            chunk_size: consensus.chunk_size,
            entropy_packing_iterations: consensus.entropy_packing_iterations as u8,
            chain_id: consensus.chain_id as u16,
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

    #[must_use]
    fn config(&self) -> ChunkConfig;
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
        chunk_specs: Vec<ChunkRangeSpecifier>,
    },
    /// Transaction removed from mempool (included in block or evicted).
    TransactionRemoved { tx_hash: B256 },
    /// Provision chunks needed for validating a peer block.
    ProvisionBlockChunks {
        block_hash: B256,
        chunk_specs: Vec<ChunkRangeSpecifier>,
        response: oneshot::Sender<Result<(), Vec<(u32, u64)>>>,
    },
    /// Release chunks provisioned for a block after validation completes.
    ReleaseBlockChunks { block_hash: B256 },
}

/// Sender for PD chunk messages.
pub type PdChunkSender = mpsc::UnboundedSender<PdChunkMessage>;

/// Receiver for PD chunk messages.
pub type PdChunkReceiver = mpsc::UnboundedReceiver<PdChunkMessage>;

/// Shared chunk data index for lock-free chunk reads during EVM execution.
/// PdService populates this during provisioning; the PD precompile reads it directly.
/// Keys are `(ledger: u32, offset: u64)` tuples matching `ChunkKey` semantics.
pub type ChunkDataIndex = Arc<DashMap<(u32, u64), Arc<Bytes>>>;
