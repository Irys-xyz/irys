use clap::Parser;

#[derive(Debug, Parser)]
pub struct CacheArgs {
    /// Blocks cache cleaning lags behind finalization
    #[arg(id = "cache.clean-lag", long = "cache.clean-lag", value_name = "N")]
    pub cache_clean_lag: Option<u8>,

    /// Maximum cache size in bytes
    #[arg(
        id = "cache.max-size-bytes",
        long = "cache.max-size-bytes",
        value_name = "BYTES"
    )]
    pub max_cache_size_bytes: Option<u64>,

    /// Prune cache when this capacity percent is reached (0-100)
    #[arg(
        id = "cache.prune-at-percent",
        long = "cache.prune-at-percent",
        value_name = "PCT"
    )]
    pub prune_at_capacity_percent: Option<f64>,
}
