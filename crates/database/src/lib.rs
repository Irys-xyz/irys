//! This crate is a dependency for both [chain] and [actors] crates. It exposes
//! database methods for reading and writing from the database as well as some
//! database value types.
pub mod block_index_data;
pub mod commitment_snapshot;
pub mod config;
pub mod data_ledger;
pub mod database;
/// When data is unconfirmed it is stored in `db_cache` tables. Once the data
/// (which is part of transactions and blocks) is well confirmed it moves from
/// the cache to one of the `db_index` tables.
/// Data in the caches can be pending or in a block still subject to re-org so
/// it is not suitable for mining.
pub mod db_cache;
pub mod system_ledger;

pub mod db;
/// Data in the indexes is confirmed data
pub mod db_index;
pub mod metadata;
pub mod migration;
/// Extension traits for custom tables
pub mod reth_ext;
/// Tables & methods specific to submodule databases
pub mod submodule;
/// Local macro definition of chain specific mdbx tables
pub mod tables;

pub use block_index_data::*;
pub use commitment_snapshot::*;
pub use data_ledger::*;
pub use database::*;
pub use system_ledger::*;

pub mod reth_db {
    pub use reth_db::*;
}
