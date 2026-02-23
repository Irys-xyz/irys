use clap::Parser;

#[derive(Debug, Parser)]
pub struct StorageArgs {
    /// Number of write operations before forcing a sync to disk
    #[arg(
        id = "storage.num-writes-before-sync",
        long = "storage.num-writes-before-sync",
        value_name = "N"
    )]
    pub num_writes_before_sync: Option<u64>,

    /// Maximum pending chunk requests for data sync
    #[arg(
        id = "storage.max-pending-chunk-requests",
        long = "storage.max-pending-chunk-requests",
        value_name = "N"
    )]
    pub max_pending_chunk_requests: Option<u64>,

    /// Maximum storage throughput in bytes per second
    #[arg(
        id = "storage.max-throughput-bps",
        long = "storage.max-throughput-bps",
        value_name = "BPS"
    )]
    pub max_storage_throughput_bps: Option<u64>,

    /// Bandwidth adjustment interval in seconds
    #[arg(
        id = "storage.bandwidth-adjust-interval",
        long = "storage.bandwidth-adjust-interval",
        value_name = "SECS"
    )]
    pub bandwidth_adjustment_interval_secs: Option<u64>,

    /// Chunk request timeout in seconds
    #[arg(
        id = "storage.chunk-request-timeout",
        long = "storage.chunk-request-timeout",
        value_name = "SECS"
    )]
    pub chunk_request_timeout_secs: Option<u64>,
}
