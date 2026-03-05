//! Chunk provider trait for PD precompile integration.

use alloy_primitives::B256;
use bytes::Bytes;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, oneshot};

use crate::range_specifier::ChunkRangeSpecifier;

/// Pre-loaded chunk data for EVM execution. Key: (ledger, ledger_offset).
pub type ChunkTable = HashMap<(u32, u64), Arc<Bytes>>;

/// Entry in the chunk table store with creation time for TTL cleanup.
#[derive(Debug, Clone)]
pub struct ChunkTableEntry {
    pub table: Arc<ChunkTable>,
    pub created_at: Instant,
}

/// Side-channel store for passing pre-built chunk tables from CL to EL.
/// Keyed by block hash. CL inserts before calling engine API; EL reads
/// during `context_for_payload` / `context_for_block`.
#[derive(Debug, Clone, Default)]
pub struct ChunkTableStore {
    inner: Arc<RwLock<HashMap<B256, ChunkTableEntry>>>,
}

impl ChunkTableStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a pre-built chunk table for the given block hash.
    pub fn insert(&self, block_hash: B256, table: Arc<ChunkTable>) {
        self.inner.write().insert(
            block_hash,
            ChunkTableEntry {
                table,
                created_at: Instant::now(),
            },
        );
    }

    /// Look up and remove a chunk table by block hash.
    pub fn take(&self, block_hash: &B256) -> Option<Arc<ChunkTable>> {
        self.inner.write().remove(block_hash).map(|e| e.table)
    }

    /// Look up a chunk table by block hash without removing it.
    pub fn get(&self, block_hash: &B256) -> Option<Arc<ChunkTable>> {
        self.inner.read().get(block_hash).map(|e| e.table.clone())
    }

    /// Remove entries older than `max_age`.
    pub fn evict_stale(&self, max_age: std::time::Duration) {
        self.inner
            .write()
            .retain(|_, entry| entry.created_at.elapsed() < max_age);
    }

    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().is_empty()
    }
}

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
///
/// The manager handles the full lifecycle per PD transaction:
/// Pending → Provisioning → Ready → Completed
#[derive(Debug)]
pub enum PdChunkMessage {
    /// New PD transaction detected - start provisioning chunks.
    NewTransaction {
        tx_hash: B256,
        chunk_specs: Vec<ChunkRangeSpecifier>,
    },
    /// Transaction removed from mempool (included in block or evicted).
    TransactionRemoved { tx_hash: B256 },
    /// Query if chunks for a transaction are ready (response via channel).
    IsReady {
        tx_hash: B256,
        response: oneshot::Sender<bool>,
    },
    /// Batch-fetch chunks by (ledger, offset). Returns the loaded table.
    /// Cache hits are instant; cache misses fall back to storage.
    GetChunksBatch {
        keys: Vec<(u32, u64)>,
        response: oneshot::Sender<ChunkTable>,
    },
}

/// Sender for PD chunk messages.
pub type PdChunkSender = mpsc::UnboundedSender<PdChunkMessage>;

/// Receiver for PD chunk messages.
pub type PdChunkReceiver = mpsc::UnboundedReceiver<PdChunkMessage>;

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

#[cfg(test)]
mod chunk_table_store_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_insert_and_get() {
        let store = ChunkTableStore::new();
        let hash = B256::with_last_byte(0x01);
        let mut table = ChunkTable::new();
        table.insert((0, 42), Arc::new(Bytes::from_static(b"hello")));
        store.insert(hash, Arc::new(table));
        let retrieved = store.get(&hash).unwrap();
        assert_eq!(retrieved.len(), 1);
        assert!(retrieved.contains_key(&(0, 42)));
    }

    #[test]
    fn test_take_removes_entry() {
        let store = ChunkTableStore::new();
        let hash = B256::with_last_byte(0x02);
        store.insert(hash, Arc::new(ChunkTable::new()));
        assert!(store.take(&hash).is_some());
        assert!(store.get(&hash).is_none());
    }

    #[test]
    fn test_get_returns_none_for_missing() {
        let store = ChunkTableStore::new();
        let hash = B256::with_last_byte(0x03);
        assert!(store.get(&hash).is_none());
    }

    #[test]
    fn test_evict_stale() {
        let store = ChunkTableStore::new();
        let hash = B256::with_last_byte(0x04);
        store.insert(hash, Arc::new(ChunkTable::new()));
        store.evict_stale(Duration::from_secs(0));
        assert!(store.is_empty());
    }
}
