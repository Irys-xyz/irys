//! Contains a common set of types used across all of the `irys` crates.
//!
//! This module implements a single location where these types are managed,
//! making them easy to reference and maintain.
pub mod app_state;
pub mod arbiter_handle;
pub mod block;
pub mod block_production;
pub mod chainspec;
pub mod chunk;
pub mod chunk_provider;
pub mod chunked;
pub mod commitment_common;
pub mod commitment_v1;
pub mod commitment_v2;
pub mod config;
pub mod conversions;
pub mod difficulty_adjustment_config;
pub mod gossip;
pub mod hardfork_config;
pub mod ingress;
pub mod irys;
pub mod ledger_expiry;
mod merkle;
pub mod partition;
pub mod peer_list;
pub mod precompile;
pub mod range_specifier;
pub mod serialization;
pub mod shutdown;
pub mod signature;
pub mod simple_rng;
pub mod storage;
pub mod storage_pricing;
pub mod system_ledger;
pub mod time;
pub mod transaction;
pub mod transaction_metadata;
pub mod tx_known_status;
pub mod tx_source;
pub mod version;
pub mod versioning;

pub mod canonical;

pub mod block_provider;
pub mod h256;
pub mod remote_packing;
pub mod rlp;

pub mod address;
mod peer_id;

pub use address::IrysAddress;

use std::sync::{atomic::AtomicU64, Arc};

pub use block::*;
pub use commitment_common::*;
pub use commitment_v1::*;
pub use commitment_v2::*;
pub use config::*;
pub use difficulty_adjustment_config::*;
pub use gossip::*;
pub use peer_id::*;
pub use serialization::*;
pub use shutdown::*;
pub use signature::*;
pub use storage::*;
pub use storage_pricing::*;
pub use system_ledger::*;
pub use time::*;
pub use transaction::*;
pub use transaction_metadata::*;
pub use tx_source::*;

pub use alloy_primitives::{/* Address, */ Signature};

pub use app_state::*;
pub use arbiter_handle::*;
pub use arbitrary::Arbitrary;
pub use chunk::*;
pub use conversions::{parse_address, u256_from_le_bytes, AddressParseError};
pub use merkle::*;
pub use nodit::Interval;
pub use peer_list::*;
pub use range_specifier::*;
pub use reth_codecs::Compact;
pub use rlp::*;
pub use simple_rng::*;
pub use tx_known_status::*;
pub use version::*;
pub use versioning::*;

pub type AtomicVdfStepNumber = Arc<AtomicU64>;
