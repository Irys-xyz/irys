//! Contains a common set of types used across all of the `irys` crates.
//!
//! This module implements a single location where these types are managed,
//! making them easy to reference and maintain.
pub mod app_state;
pub mod block;
pub mod block_production;
pub mod chunk;
pub mod consensus;
pub mod difficulty_adjustment_config;
pub mod ingress;
pub mod irys;
mod merkle;
pub mod partition;
pub mod reth_provider;
pub mod serialization;
pub mod signature;
pub mod simple_rng;
pub mod storage;
pub mod storage_config;
pub mod transaction;
pub mod vdf_config;

pub use block::*;
pub use consensus::*;
pub use difficulty_adjustment_config::*;
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
pub use reth_codecs::Compact;
pub use simple_rng::*;
pub use storage_config::*;
pub use vdf_config::*;

mod t {
    use std::ops::{Deref, DerefMut};

    use nodit::interval::ii;

    #[derive(Debug, Default, Copy, Clone)]
    struct PartitionOffset(pub u32);
    impl Deref for PartitionOffset {
        type Target = u32;

        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }
    impl DerefMut for PartitionOffset {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.0
        }
    }
    #[derive(Debug, Default, Copy, Clone)]

    struct LedgerOffset(pub u64);

    impl Deref for LedgerOffset {
        type Target = u64;

        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }
    impl DerefMut for LedgerOffset {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.0
        }
    }

    #[test]
    fn ergo_test() {
        let part_range = ii(100, 200);

        let ledger_offset = LedgerOffset(150);
        let mut lo2 = ledger_offset.clone();
        let r = *lo2 + 123;
        dbg!(r);
    }
}
