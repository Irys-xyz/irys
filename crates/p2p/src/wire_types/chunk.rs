use irys_types::{serialization::Base64, DataRoot, TxChunkOffset};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UnpackedChunk {
    pub data_root: DataRoot,
    #[serde(with = "irys_types::string_u64")]
    pub data_size: u64,
    pub data_path: Base64,
    pub bytes: Base64,
    pub tx_offset: TxChunkOffset,
}

super::impl_mirror_from!(irys_types::UnpackedChunk => UnpackedChunk {
    data_root, data_size, data_path, bytes, tx_offset,
});
