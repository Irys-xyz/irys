#[cfg(feature = "rocksdb")]
mod convert;
pub mod db;
mod group;
mod interval;
mod migrate;
mod packing;
mod placement;
#[cfg(feature = "rocksdb")]
mod records;
#[cfg(feature = "rocksdb")]
pub mod rocks;
pub mod store;
pub mod tables;
mod verify;

#[cfg(feature = "rocksdb")]
pub use convert::{
    ConvertOptions, ConvertStats, EngineEquivalenceReport, convert_mdbx_to_rocks,
    convert_rocks_to_mdbx, verify_mdbx_and_rocks, verify_rocks_records,
};
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
pub use store::{
    MdbxSubmoduleStore, PresenceReconcile, SubmoduleIndex, SubmoduleRead, SubmoduleStore,
    SubmoduleWrite,
};
pub use tables::{OffsetMeta, PackedOffsetRow};
pub use verify::{
    MismatchSample, ReadEquivalenceReport, open_pristine_v1_read_only,
    verify_submodule_read_equivalence,
};
