use irys_types::{Config, StorageConfig};

/// Allows for overriding of the consensus parameters for ledgers and partitions
#[derive(Debug, Clone, Default)]
pub struct EpochServiceConfig {
    /// Capacity partitions are allocated on a logarithmic curve, this scalar
    /// shifts the curve on the Y axis. Allowing there to be more or less
    /// capacity partitions relative to data partitions.
    pub capacity_scalar: u64,
    /// The length of an epoch denominated in block heights
    pub num_blocks_in_epoch: u64,
    /// Sets the minimum number of capacity partitions for the protocol.
    pub num_capacity_partitions: Option<u64>,
    /// Reference to global storage config for node
    pub storage_config: StorageConfig,
}

impl EpochServiceConfig {
    pub fn new(config: &Config) -> Self {
        Self {
            capacity_scalar: config.capacity_scalar,
            num_blocks_in_epoch: config.num_blocks_in_epoch,
            num_capacity_partitions: config.num_capacity_partitions,
            storage_config: StorageConfig::new(config),
        }
    }
}
