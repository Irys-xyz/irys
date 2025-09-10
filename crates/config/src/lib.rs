//! Crate dedicated to the `IrysNodeConfig` to avoid dependency cycles
#![deny(clippy::float_arithmetic)]
pub mod chain;
pub mod submodules;
pub use submodules::*;
