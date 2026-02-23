use clap::Parser;

#[derive(Debug, Parser)]
pub struct SyncArgs {
    /// Blocks to fetch in parallel per sync batch
    #[arg(
        id = "sync.block-batch-size",
        long = "sync.block-batch-size",
        value_name = "N"
    )]
    pub block_batch_size: Option<usize>,

    /// Periodic sync check interval in seconds
    #[arg(
        id = "sync.check-interval",
        long = "sync.check-interval",
        value_name = "SECS"
    )]
    pub periodic_sync_check_interval_secs: Option<u64>,

    /// Block request retry timeout in seconds
    #[arg(
        id = "sync.retry-timeout",
        long = "sync.retry-timeout",
        value_name = "SECS"
    )]
    pub retry_block_request_timeout_secs: Option<u64>,

    /// Enable periodic sync checks
    #[arg(
        id = "sync.enable-periodic-check",
        long = "sync.enable-periodic-check",
        value_name = "BOOL"
    )]
    pub enable_periodic_sync_check: Option<bool>,

    /// Queue slot wait timeout in seconds
    #[arg(
        id = "sync.queue-slot-timeout",
        long = "sync.queue-slot-timeout",
        value_name = "SECS"
    )]
    pub wait_queue_slot_timeout_secs: Option<u64>,

    /// Max consecutive queue slot timeout attempts
    #[arg(
        id = "sync.queue-slot-max-attempts",
        long = "sync.queue-slot-max-attempts",
        value_name = "N"
    )]
    pub wait_queue_slot_max_attempts: Option<usize>,
}
