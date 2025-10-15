//! Contains a common set of types used across all of the `irys` crates.
//!
//! This module implements a single location where these types are managed,
//! making them easy to reference and maintain.
pub mod app_state;
pub mod arbiter_handle;
pub mod block;
pub mod block_production;
pub mod chunk;
pub mod chunked;
pub mod config;
pub mod conversions;
pub mod difficulty_adjustment_config;
pub mod gossip;
pub mod ingress;
pub mod irys;
pub mod ledger_expiry;
mod merkle;
pub mod partition;
pub mod peer_list;
pub mod serialization;
pub mod signature;
pub mod simple_rng;
pub mod storage;
pub mod storage_pricing;
pub mod time;
pub mod transaction;
pub mod tx_source;
pub mod version;
pub mod versioning;

pub mod block_provider;
pub mod h256;
pub mod remote_packing;
pub mod rlp;

use std::sync::{atomic::AtomicU64, Arc};

pub use block::*;
pub use config::*;
pub use difficulty_adjustment_config::*;
pub use gossip::*;
pub use serialization::*;
pub use signature::*;
pub use storage::*;
pub use time::*;
pub use transaction::*;
pub use tx_source::*;

pub use alloy_primitives::{Address, Signature};
pub use app_state::*;
pub use arbiter_handle::*;
pub use arbitrary::Arbitrary;
pub use chunk::*;
pub use conversions::{parse_address, u256_from_le_bytes, AddressParseError};
pub use merkle::*;
pub use nodit::Interval;
pub use peer_list::*;
pub use reth_codecs::Compact;
pub use rlp::*;
pub use simple_rng::*;
pub use version::*;
pub use versioning::*;

pub type AtomicVdfStepNumber = Arc<AtomicU64>;
