use std::sync::{atomic::AtomicU64, Arc};

use crate::*;

pub type AtomicVdfStepNumber = Arc<AtomicU64>;

/// Allows for overriding of the vdf steps generation parameters
#[derive(Debug, Clone, Default)]
pub struct VDFStepsConfig {
    pub num_checkpoints_in_vdf_step: usize,
    pub vdf_reset_frequency: usize,
    pub vdf_difficulty: u64,
    pub vdf_parallel_verification_thread_limit: usize,
}

impl VDFStepsConfig {
    pub const fn new(config: &Config) -> Self {
        Self {
            num_checkpoints_in_vdf_step: config.num_checkpoints_in_vdf_step,
            vdf_reset_frequency: config.vdf_reset_frequency,
            vdf_difficulty: config.vdf_sha_1s,
            vdf_parallel_verification_thread_limit: config.vdf_parallel_verification_thread_limit,
        }
    }
}
