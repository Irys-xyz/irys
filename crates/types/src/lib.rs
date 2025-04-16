//! Contains a common set of types used across all of the `irys` crates.
//!
//! This module implements a single location where these types are managed,
//! making them easy to reference and maintain.
pub mod app_state;
pub mod block;
pub mod block_production;
pub mod chunk;
pub mod config;
pub mod difficulty_adjustment_config;
pub mod gossip;
pub mod ingress;
pub mod irys;
mod merkle;
pub mod partition;
pub mod peer_list;
pub mod serialization;
pub mod signature;
pub mod simple_rng;
pub mod storage;
pub mod storage_pricing;
pub mod transaction;
pub mod version;

use std::sync::{atomic::AtomicU64, Arc};

pub use block::*;
pub use config::*;
pub use difficulty_adjustment_config::*;
pub use gossip::*;
pub use serialization::*;
pub use signature::*;
pub use storage::*;
pub use transaction::*;

pub use alloy_primitives::{Address, Signature};
pub use app_state::*;
pub use arbitrary::Arbitrary;
pub use chunk::*;
pub use merkle::*;
pub use nodit::Interval;
pub use peer_list::*;
pub use reth_codecs::Compact;
pub use simple_rng::*;
pub use version::*;

pub type AtomicVdfStepNumber = Arc<AtomicU64>;
