use irys_types::{serialization::Base64, DataRoot, TxChunkOffset};
use serde::{Deserialize, Serialize};

/// Sovereign wire type for UnpackedChunk.
/// This type owns its serde configuration independently of irys_types::UnpackedChunk.
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

impl From<&irys_types::UnpackedChunk> for UnpackedChunk {
    fn from(c: &irys_types::UnpackedChunk) -> Self {
        Self {
            data_root: c.data_root,
            data_size: c.data_size,
            data_path: c.data_path.clone(),
            bytes: c.bytes.clone(),
            tx_offset: c.tx_offset,
        }
    }
}

impl From<UnpackedChunk> for irys_types::UnpackedChunk {
    fn from(c: UnpackedChunk) -> Self {
        Self {
            data_root: c.data_root,
            data_size: c.data_size,
            data_path: c.data_path,
            bytes: c.bytes,
            tx_offset: c.tx_offset,
        }
    }
}
