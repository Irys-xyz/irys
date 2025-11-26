//! Hardfork-specific consensus parameters.
//!
//! This module re-exports types from `hardfork_config` for backward compatibility.
//! New code should import directly from `hardfork_config`.

pub use crate::hardfork_config::HardforkParams;
