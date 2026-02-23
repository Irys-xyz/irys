use clap::Parser;

#[derive(Debug, Parser)]
pub struct MempoolArgs {
    /// Max addresses in LRU cache for out-of-order stakes/pledges
    #[arg(
        id = "mempool.max-pending-pledge-items",
        long = "mempool.max-pending-pledge-items",
        value_name = "N"
    )]
    pub max_pending_pledge_items: Option<usize>,

    /// Max pending pledge transactions per address
    #[arg(
        id = "mempool.max-pledges-per-item",
        long = "mempool.max-pledges-per-item",
        value_name = "N"
    )]
    pub max_pledges_per_item: Option<usize>,

    /// Max data roots in pending chunk cache
    #[arg(
        id = "mempool.max-pending-chunk-items",
        long = "mempool.max-pending-chunk-items",
        value_name = "N"
    )]
    pub max_pending_chunk_items: Option<usize>,

    /// Max chunks cached per data root
    #[arg(
        id = "mempool.max-chunks-per-item",
        long = "mempool.max-chunks-per-item",
        value_name = "N"
    )]
    pub max_chunks_per_item: Option<usize>,

    /// Max pre-header chunks per data root
    #[arg(
        id = "mempool.max-preheader-chunks-per-item",
        long = "mempool.max-preheader-chunks-per-item",
        value_name = "N"
    )]
    pub max_preheader_chunks_per_item: Option<usize>,

    /// Max pre-header data_path bytes for chunk proofs
    #[arg(
        id = "mempool.max-preheader-data-path-bytes",
        long = "mempool.max-preheader-data-path-bytes",
        value_name = "BYTES"
    )]
    pub max_preheader_data_path_bytes: Option<usize>,

    /// Max valid tx IDs to track
    #[arg(
        id = "mempool.max-valid-items",
        long = "mempool.max-valid-items",
        value_name = "N"
    )]
    pub max_valid_items: Option<usize>,

    /// Max invalid tx IDs to track
    #[arg(
        id = "mempool.max-invalid-items",
        long = "mempool.max-invalid-items",
        value_name = "N"
    )]
    pub max_invalid_items: Option<usize>,

    /// Max valid chunk hashes to track
    #[arg(
        id = "mempool.max-valid-chunks",
        long = "mempool.max-valid-chunks",
        value_name = "N"
    )]
    pub max_valid_chunks: Option<usize>,

    /// Max data transactions in mempool
    #[arg(
        id = "mempool.max-valid-submit-txs",
        long = "mempool.max-valid-submit-txs",
        value_name = "N"
    )]
    pub max_valid_submit_txs: Option<usize>,

    /// Max addresses with pending commitment transactions
    #[arg(
        id = "mempool.max-valid-commitment-addresses",
        long = "mempool.max-valid-commitment-addresses",
        value_name = "N"
    )]
    pub max_valid_commitment_addresses: Option<usize>,

    /// Max commitment transactions per address
    #[arg(
        id = "mempool.max-commitments-per-address",
        long = "mempool.max-commitments-per-address",
        value_name = "N"
    )]
    pub max_commitments_per_address: Option<usize>,

    /// Max concurrent mempool message handlers
    #[arg(
        id = "mempool.max-concurrent-tasks",
        long = "mempool.max-concurrent-tasks",
        value_name = "N"
    )]
    pub max_concurrent_mempool_tasks: Option<usize>,

    /// Chunk write-behind buffer capacity
    #[arg(
        id = "mempool.chunk-writer-buffer-size",
        long = "mempool.chunk-writer-buffer-size",
        value_name = "N"
    )]
    pub chunk_writer_buffer_size: Option<usize>,
}
