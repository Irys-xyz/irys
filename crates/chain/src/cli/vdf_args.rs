use clap::Parser;

#[derive(Debug, Parser)]
pub struct VdfArgs {
    /// Maximum threads for parallel VDF verification
    #[arg(
        id = "vdf.parallel-threads",
        long = "vdf.parallel-threads",
        value_name = "N"
    )]
    pub parallel_verification_thread_limit: Option<usize>,
}
