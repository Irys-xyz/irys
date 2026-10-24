pub mod chain;
pub mod genesis_builder;
pub mod genesis_utilities;
mod metrics;
pub mod peer_utilities;
pub mod utils;
pub mod vdf_seed_source;

pub use chain::*;

/// irys-chain's package version. The `irys` binary (crates/node) reports this, so the
/// release version stays defined in one place.
pub const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");
