use irys_types::{ChunkRangeId, LedgerChunkOffset, UnpackedChunk};
use lru::LruCache;

#[derive(Clone)]
pub enum ChunkState {
    Requested,
    Provisioned,
}

impl Default for ChunkState {
    fn default() -> Self {
        ChunkState::Requested
    }
}

pub struct UnpackedChunksCache {
    pub cache: LruCache<LedgerChunkOffset, UnpackedChunkEntry>,
}

#[derive(Default, Clone)]
pub struct UnpackedChunkEntry {
    pub state: ChunkState,
    pub chunk: Option<UnpackedChunk>,
    pub chunk_range_ids: Vec<ChunkRangeId>,
}
