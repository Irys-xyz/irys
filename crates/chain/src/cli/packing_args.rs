use clap::Parser;

#[derive(Debug, Parser)]
pub struct PackingArgs {
    /// Number of CPU threads for data packing operations
    #[arg(
        id = "packing.cpu-concurrency",
        long = "packing.cpu-concurrency",
        value_name = "N"
    )]
    pub cpu_packing_concurrency: Option<u16>,

    /// Batch size for GPU-accelerated packing (0 = disabled)
    #[arg(
        id = "packing.gpu-batch-size",
        long = "packing.gpu-batch-size",
        value_name = "N"
    )]
    pub gpu_packing_batch_size: Option<u32>,
}
