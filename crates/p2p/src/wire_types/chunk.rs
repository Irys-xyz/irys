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
