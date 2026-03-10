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

/// Provides unpacked chunks to PD precompile.
/// Used as storage backend by PdChunkManager.
pub trait RethChunkProvider: Send + Sync + std::fmt::Debug {
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
    /// Get cached chunk during EVM execution (removed in Phase 2).
    GetChunk {
        ledger: u32,
        offset: u64,
        response: oneshot::Sender<Option<Arc<Bytes>>>,
    },
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

/// Mock chunk provider that returns zero-filled chunks.
#[cfg(any(test, feature = "test-utils"))]
#[derive(Debug, Clone)]
pub struct MockChunkProvider {
    config: ChunkConfig,
    cached_chunk: Bytes,
}

#[cfg(any(test, feature = "test-utils"))]
impl MockChunkProvider {
    #[inline]
    pub fn new() -> Self {
        let config = ChunkConfig {
            num_chunks_in_partition: 100,
            chunk_size: 256_000,
            entropy_packing_iterations: 0,
            chain_id: 1,
        };
        let cached_chunk = Bytes::from(vec![0_u8; config.chunk_size as usize]);
        Self {
            config,
            cached_chunk,
        }
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl Default for MockChunkProvider {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl RethChunkProvider for MockChunkProvider {
    fn get_unpacked_chunk_by_ledger_offset(
        &self,
        _ledger: u32,
        _ledger_offset: u64,
    ) -> eyre::Result<Option<Bytes>> {
        Ok(Some(self.cached_chunk.clone()))
    }

    #[inline]
    fn config(&self) -> ChunkConfig {
        self.config
    }
}
