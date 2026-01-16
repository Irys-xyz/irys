use std::num::{NonZeroU32, NonZeroUsize};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BreakerCapacity(NonZeroUsize);

impl BreakerCapacity {
    #[inline]
    pub const fn new(value: NonZeroUsize) -> Self {
        Self(value)
    }

    #[inline]
    pub const fn get(&self) -> usize {
        self.0.get()
    }

    #[inline]
    pub const fn as_non_zero(&self) -> NonZeroUsize {
        self.0
    }
}

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
    pub capacity: BreakerCapacity,
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
            capacity: BreakerCapacity::new(NonZeroUsize::new(1000).unwrap()),
            failure_threshold: FailureThreshold::new(NonZeroU32::new(5).unwrap()),
            cooldown_duration: Duration::from_secs(30),
            stale_timeout: Duration::from_secs(600),
            recovery_attempts: RecoveryAttempts::new(NonZeroU32::new(3).unwrap()),
        }
    }

    pub fn testing() -> Self {
        Self {
            capacity: BreakerCapacity::new(NonZeroUsize::new(100).unwrap()),
            failure_threshold: FailureThreshold::new(NonZeroU32::new(100).unwrap()),
            cooldown_duration: Duration::from_millis(100),
            stale_timeout: Duration::from_secs(60),
            recovery_attempts: RecoveryAttempts::new(NonZeroU32::new(50).unwrap()),
        }
    }
}
