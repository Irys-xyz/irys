use crate::*;

/// Allows for overriding of the vdf steps generation parameters
#[derive(Debug, Clone)]
pub struct VDFStepsConfig {
    pub num_checkpoints_in_vdf_step: usize,
    pub nonce_limiter_reset_frequency: usize,
    pub vdf_difficulty: u64,
}

impl Default for VDFStepsConfig {
    fn default() -> Self {
        VDFStepsConfig {
            num_checkpoints_in_vdf_step: NUM_CHECKPOINTS_IN_VDF_STEP,
            nonce_limiter_reset_frequency: NONCE_LIMITER_RESET_FREQUENCY,
            vdf_difficulty: if cfg!(test) || cfg!(debug_assertions) {
                7_000
            } else {
                VDF_SHA_1S
            },
        }
    }
}
