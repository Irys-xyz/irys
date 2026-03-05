//! Chunk provider trait for PD precompile integration.

use bytes::Bytes;

/// Configuration values needed for chunk operations.
#[derive(Debug, Clone, Copy)]
pub struct ChunkConfig {
    pub num_chunks_in_partition: u64,
    pub chunk_size: u64,
    pub entropy_packing_iterations: u8,
    pub chain_id: u16,
}

/// Provides unpacked chunks to PD precompile.
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
