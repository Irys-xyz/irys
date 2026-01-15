use std::num::NonZeroU32;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailureThreshold(NonZeroU32);

impl FailureThreshold {
    #[inline]
    pub const fn new(value: NonZeroU32) -> Self {
        Self(value)
    }

    #[inline]
    pub const fn get(&self) -> u32 {
        self.0.get()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryAttempts(NonZeroU32);

impl RecoveryAttempts {
    #[inline]
    pub const fn new(value: NonZeroU32) -> Self {
        Self(value)
    }

    #[inline]
    pub const fn get(&self) -> u32 {
        self.0.get()
    }
}

#[derive(Debug, Clone)]
pub struct CircuitBreakerConfig {
    pub failure_threshold: FailureThreshold,
    pub cooldown_duration: Duration,
    pub stale_timeout: Duration,
    pub recovery_attempts: RecoveryAttempts,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self::p2p_defaults()
    }
}

impl CircuitBreakerConfig {
    pub fn p2p_defaults() -> Self {
        Self {
            failure_threshold: FailureThreshold::new(NonZeroU32::new(5).unwrap()),
            cooldown_duration: Duration::from_secs(30),
            stale_timeout: Duration::from_secs(600),
            recovery_attempts: RecoveryAttempts::new(NonZeroU32::new(3).unwrap()),
        }
    }
}
