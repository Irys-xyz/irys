// - [ ] irys block defines 2 stake system txs; reth block only has a single stake system tx; expect rejection
// - [ ] irys block defines 2 stake system txs; reth block only has 3 stake system tx; expect rejection
// - [ ] irys block defines 1 stake, 1 unstake system txs; reth has the same, but in different order; expect rejection
// - [ ] irys block defines 1 stake, 1 unstake system txs; reth has the same; expect success
use crate::utils::IrysNodeTest;
use irys_testing_utils::*;
use irys_types::{NodeConfig, H256};
