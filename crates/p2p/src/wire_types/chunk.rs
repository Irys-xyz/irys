use irys_types::{DataTreeRoot, TxChunkOffset, serialization::Base64};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UnpackedChunk {
    #[serde(rename = "dataRoot")]
    pub data_tree_root: DataTreeRoot,
    #[serde(with = "irys_types::string_u64")]
    pub data_size: u64,
    pub data_path: Base64,
    pub bytes: Base64,
    pub tx_offset: TxChunkOffset,
}

super::impl_mirror_from!(irys_types::UnpackedChunk => UnpackedChunk {
    data_tree_root, data_size, data_path, bytes, tx_offset,
});

impl From<std::sync::Arc<irys_types::UnpackedChunk>> for UnpackedChunk {
    fn from(arc: std::sync::Arc<irys_types::UnpackedChunk>) -> Self {
        std::sync::Arc::unwrap_or_clone(arc).into()
    }
}

/// "I hold this chunk." Sent by a node re-broadcasting a chunk it received
/// from gossip or data sync; also the body of a chunk-fetch request.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChunkAnnouncement {
    #[serde(rename = "dataRoot")]
    pub data_tree_root: DataTreeRoot,
    #[serde(with = "irys_types::string_u64")]
    pub data_size: u64,
    pub tx_offset: TxChunkOffset,
}

impl From<&irys_types::UnpackedChunk> for ChunkAnnouncement {
    fn from(chunk: &irys_types::UnpackedChunk) -> Self {
        Self {
            data_tree_root: chunk.data_tree_root,
            data_size: chunk.data_size,
            tx_offset: chunk.tx_offset,
        }
    }
}
