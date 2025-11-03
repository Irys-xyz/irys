#![allow(dead_code)]
// Centralized channel capacity definitions for the actors crate.
//
// These constants provide sensible defaults for bounded mpsc channels used by
// tokio services. They are intended to prevent unbounded memory growth while
// allowing enough buffer for normal bursts.
//
// Notes:
// - todo: Tune these based on production telemetry
// - Keep control/command channels relatively small
// - Keep high-throughput pipelines larger, but still bounded

/// Block index service control messages
pub const CAP_BLOCK_INDEX: usize = 256;

/// Block tree service control/messages
pub const CAP_BLOCK_TREE: usize = 256;

/// Storage module service control/messages
pub const CAP_STORAGE_MODULES: usize = 256;

/// Chunk migration service control/messages
pub const CAP_CHUNK_MIGRATION: usize = 256;

/// Validation service control/messages
pub const CAP_VALIDATION: usize = 256;

/// Block producer control/messages
pub const CAP_BLOCK_PRODUCER: usize = 256;

/// Reth service control/messages
pub const CAP_RETH_SERVICE: usize = 256;

/// Block discovery service control/messages
pub const CAP_BLOCK_DISCOVERY: usize = 256;

/// VDF fast-forward steps during node sync
pub const CAP_VDF_FAST_FORWARD: usize = 256;

/// Data sync service pipeline
pub const CAP_DATA_SYNC: usize = 1024;

/// Peer network service pipeline
pub const CAP_PEER_NETWORK: usize = 1024;

/// Chain sync service pipeline
pub const CAP_CHAIN_SYNC: usize = 2048;

/// Gossip broadcast data path
pub const CAP_GOSSIP_BROADCAST: usize = 4096;

/// Mempool service pipeline
pub const CAP_MEMPOOL: usize = 8192;

/// Packing requests (used by PackingService::channel)
pub const CAP_PACKING_REQUESTS: usize = 5_000;

/// Mining broadcast events (recommended when migrating to tokio::broadcast)
pub const CAP_MINING_BROADCAST: usize = 1024;
