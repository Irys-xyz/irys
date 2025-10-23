use irys_types::Config;

/// Common parameters shared across all packing strategies
#[derive(Debug, Clone)]
pub(crate) struct PackingParams {
    pub chunk_size: usize,
    pub entropy_iterations: u32,
    pub chain_id: u64,
}

impl PackingParams {
    /// Extract packing parameters from config
    pub(crate) fn from_config(config: &Config) -> Self {
        Self {
            chunk_size: config.consensus.chunk_size as usize,
            entropy_iterations: config.consensus.entropy_packing_iterations,
            chain_id: config.consensus.chain_id,
        }
    }
}
