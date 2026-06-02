use serde::{Deserialize, Serialize};

pub(crate) const SNAPSHOT_FORMAT_VERSION: u32 = 1;
pub(crate) const MANIFEST_FILENAME: &str = "manifest.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SnapshotManifest {
    pub format_version: u32,
    pub chain_id: u64,
    pub irys_schema_version: u32,
    pub irys_tip_height: Option<u64>,
    pub reth_tip_height: Option<u64>,
    pub includes_caches: bool,
    pub created_at_unix_secs: u64,
    pub created_by: String,
    pub files: Vec<ManifestFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ManifestFile {
    pub path: String,
    pub size_bytes: u64,
    pub sha256_hex: String,
}
