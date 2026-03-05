//! Configuration constants for the packing service

// ========================================================================
// Channel Capacities
// ========================================================================

/// Main packing request channel capacity (ingress from other services)
pub(crate) const DEFAULT_CHANNEL_CAPACITY: usize = 5_000;

/// Control message channel capacity (idle detection, introspection)
pub(crate) const CONTROL_CHANNEL_CAPACITY: usize = 64;

/// Per-storage-module work queue capacity
pub(crate) const PER_SM_CHANNEL_CAPACITY: usize = 100;

/// Unpacking request queue capacity
pub(crate) const DEFAULT_UNPACKING_QUEUE_CAPACITY: usize = 1000;

// ========================================================================
// Timing
// ========================================================================

/// Default poll duration for work-stealing queue checks (milliseconds)
pub(crate) const DEFAULT_POLL_DURATION_MS: u64 = 1000;

// ========================================================================
// Packing Parameters
// ========================================================================

/// Log progress every N chunks to balance visibility vs log volume
pub(crate) const LOG_PER_CHUNKS: u32 = 1000;

/// Buffer size multiplier for remote streaming (chunks * multiplier)
pub(crate) const REMOTE_STREAM_BUFFER_MULTIPLIER: u64 = 2;
