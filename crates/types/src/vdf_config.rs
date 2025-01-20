use crate::*;

/// Allows for overriding of the vdf steps generation parameters
#[derive(Debug, Clone)]
pub struct VDFStepsConfig {
    pub num_checkpoints_in_vdf_step: usize,
    pub vdf_reset_frequency: usize,
    pub vdf_difficulty: u64,
    pub vdf_parallel_verification_thread_limit: usize,
}

impl Default for VDFStepsConfig {
    fn default() -> Self {
        VDFStepsConfig {
            num_checkpoints_in_vdf_step: CONFIG.num_checkpoints_in_vdf_step,
            vdf_reset_frequency: CONFIG.vdf_reset_frequency,
            vdf_difficulty: if cfg!(test) || cfg!(debug_assertions) {
                7_000
            } else {
                CONFIG.vdf_sha_1s
            },
            vdf_parallel_verification_thread_limit: CONFIG.vdf_parallel_verification_thread_limit,
        }
    }
}
