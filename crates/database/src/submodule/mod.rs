#[cfg(feature = "rocksdb")]
// MDBX-SUBMODULE-INDEX: delete this module with the converters once the rollback window is closed.
mod convert;
// MDBX-SUBMODULE-INDEX: delete this module with the MDBX submodule engine. The consensus database stays MDBX.
pub mod db;
mod group;
mod interval;
// MDBX-SUBMODULE-INDEX: remove with the migrate CLI. This is the v1 to v2 in-place MDBX migration.
mod migrate;
mod packing;
mod placement;
#[cfg(feature = "rocksdb")]
mod records;
#[cfg(feature = "rocksdb")]
pub mod rocks;
pub mod store;
pub mod tables;
// MDBX-SUBMODULE-INDEX: remove with the migrate CLI. This compares MDBX schema v1 with the migrated schema v2.
mod verify;
// MDBX-SUBMODULE-INDEX: remove with the MDBX submodule engine. The consensus database stays MDBX.
mod warmup;

#[cfg(feature = "rocksdb")]
// MDBX-SUBMODULE-INDEX: drop convert_mdbx_to_rocks, convert_rocks_to_mdbx, and verify_mdbx_and_rocks from this export. verify_rocks_records can move out.
pub use convert::{
    ConvertOptions, ConvertStats, EngineEquivalenceReport, convert_mdbx_to_rocks,
    convert_rocks_to_mdbx, verify_mdbx_and_rocks, verify_rocks_records,
};
// MDBX-SUBMODULE-INDEX: delete this re-export with db.rs.
pub use db::*;
pub use interval::IntervalRow;
pub use packing::{
    PACKING_RECORD_LEN, PACKING_RECORD_VERSION, PackingRecord, packing_params_valid_for_module,
};
#[cfg(feature = "rocksdb")]
pub use records::{decode_offset_meta, encode_offset_meta};
#[cfg(feature = "rocksdb")]
pub use rocks::{
    BLOB_MIN_BYTES, BLOCK_CACHE_BYTES, ColumnFileUsage, RocksBackground, RocksIngestStats,
    RocksResourceLimits, RocksResources, RocksSubmoduleStore, RocksTuning,
    install_process_rocks_resources,
};
// MDBX-SUBMODULE-INDEX: drop MdbxSubmoduleStore from this export when the MDBX engine is deleted. The other names stay until the trait collapses onto RocksSubmoduleStore.
pub use store::{
    MdbxSubmoduleStore, PresenceReconcile, SubmoduleIndex, SubmoduleRead, SubmoduleStore,
    SubmoduleWrite,
};
pub use tables::{OffsetMeta, PackedOffsetRow};
// MDBX-SUBMODULE-INDEX: remove this re-export with the migrate CLI.
pub use verify::{
    MismatchSample, ReadEquivalenceReport, open_pristine_v1_read_only,
    verify_submodule_read_equivalence,
};
