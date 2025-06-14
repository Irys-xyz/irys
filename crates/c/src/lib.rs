#![allow(clippy::allow_attributes)]
// these are so we don't get clippy warnings on generated files
#[allow(clippy::all)]
pub mod capacity;
#[allow(clippy::all)]
pub mod capacity_single;
#[allow(clippy::all)]
pub mod vdf;

#[cfg(feature = "nvidia")]
#[allow(clippy::all)]
pub mod capacity_cuda;
