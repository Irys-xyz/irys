//! DB/header-replay [`VdfSeedSource`]: the inverted home of the seed-replay
//! logic formerly in `irys_vdf::state::create_state`. Moving it here lets
//! `irys-vdf` drop its direct `irys-database`/`reth-db` dependencies while the
//! storage stack stays in `irys-chain`.

use irys_database::block_header_by_hash;
use irys_domain::BlockIndex;
use irys_types::{BlockHash, DatabaseProvider, IrysBlockHeader, block_production::Seed};
use irys_vdf::state::{VdfBootstrap, VdfSeedSource};
use reth_db::Database as _;
use std::collections::VecDeque;

/// DB/header-replay [`VdfSeedSource`].
pub struct DbVdfSeedSource<'a> {
    pub block_index: &'a BlockIndex,
    pub db: &'a DatabaseProvider,
}

impl VdfSeedSource for DbVdfSeedSource<'_> {
    fn vdf_bootstrap(&self, capacity: usize) -> VdfBootstrap {
        let block_hash = self
            .block_index
            .get_latest_item()
            .map(|item| item.block_hash)
            .expect("To have at least genesis block");

        let tx = self.db.tx().unwrap();
        replay_vdf_seeds(block_hash, capacity, |hash| {
            block_header_by_hash(&tx, hash, false).unwrap().unwrap()
        })
    }
}

/// Pure replay of the VDF seed history, parameterised over the block-header
/// lookup so the genesis / shallow / deep / capacity-boundary cases are
/// unit-testable without a database.
///
/// Behaviour is identical to the legacy `create_state` replay, including the
/// **`capacity + 1`** case: when `capacity` steps are exhausted within a
/// height-1 block the loop still advances to the genesis block and
/// unconditionally prepends its seed, so the returned window can hold
/// `capacity + 1` seeds. `first_step` is derived as `global_step -
/// ordered_seeds.len() + 1` (matching `VdfState::get_steps`), which yields the
/// one-based genesis contract (`{1, 1, [genesis.steps[0]]}` for a genesis-only
/// chain).
fn replay_vdf_seeds(
    latest_block_hash: BlockHash,
    capacity: usize,
    mut get_header: impl FnMut(&BlockHash) -> IrysBlockHeader,
) -> VdfBootstrap {
    let mut seeds: VecDeque<Seed> = VecDeque::with_capacity(capacity);
    let mut block = get_header(&latest_block_hash);
    let global_step_number = block.vdf_limiter_info.global_step_number;
    let mut steps_remaining = capacity;

    while steps_remaining > 0 && block.height > 0 {
        // get all the steps out of the block
        for step in block.vdf_limiter_info.steps.0.iter().rev() {
            seeds.push_front(Seed(*step));
            steps_remaining -= 1;
            if steps_remaining == 0 {
                break;
            }
        }
        // get the previous block
        block = get_header(&block.previous_block_hash);
    }

    if block.height == 0 {
        seeds.push_front(Seed(block.vdf_limiter_info.steps[0]));
    }

    let first_step = global_step_number.saturating_sub(seeds.len() as u64) + 1;
    VdfBootstrap {
        global_step: global_step_number,
        first_step,
        ordered_seeds: seeds,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::{H256, H256List};
    use std::collections::HashMap;

    /// Build a mock header with the given height, global step number, steps and
    /// links. `block_hash`/`previous_block_hash` drive the replay walk.
    fn header(
        block_hash: H256,
        previous_block_hash: H256,
        height: u64,
        global_step_number: u64,
        steps: &[H256],
    ) -> IrysBlockHeader {
        let mut h = IrysBlockHeader::new_mock_header();
        h.block_hash = block_hash;
        h.previous_block_hash = previous_block_hash;
        h.height = height;
        h.vdf_limiter_info.global_step_number = global_step_number;
        h.vdf_limiter_info.steps = H256List(steps.to_vec());
        h
    }

    fn reader(headers: Vec<IrysBlockHeader>) -> impl FnMut(&H256) -> IrysBlockHeader {
        let map: HashMap<H256, IrysBlockHeader> =
            headers.into_iter().map(|h| (h.block_hash, h)).collect();
        move |hash: &H256| map.get(hash).cloned().expect("header present in fixture")
    }

    fn h(n: u8) -> H256 {
        H256::from([n; 32])
    }

    /// Genesis-only chain (post `run_vdf_for_genesis_block`): global step 1, one
    /// step. One-based contract: `{1, 1, [genesis.steps[0]]}`.
    #[test]
    fn genesis_only_is_one_based() {
        let genesis = header(h(0), H256::zero(), 0, 1, &[h(1)]);
        let bootstrap = replay_vdf_seeds(genesis.block_hash, 64, reader(vec![genesis]));

        assert_eq!(bootstrap.global_step, 1);
        assert_eq!(bootstrap.first_step, 1);
        assert_eq!(bootstrap.ordered_seeds, VecDeque::from(vec![Seed(h(1))]));
    }

    /// Shallow chain (total steps < capacity): every step replayed, genesis
    /// prepended, contiguous from step 1.
    #[test]
    fn shallow_chain_replays_all_steps_with_genesis_anchor() {
        let genesis = header(h(0), H256::zero(), 0, 1, &[h(1)]);
        let block1 = header(h(10), h(0), 1, 4, &[h(2), h(3), h(4)]);
        let bootstrap = replay_vdf_seeds(block1.block_hash, 64, reader(vec![genesis, block1]));

        assert_eq!(bootstrap.global_step, 4);
        assert_eq!(bootstrap.first_step, 1);
        assert_eq!(
            bootstrap.ordered_seeds,
            VecDeque::from(vec![Seed(h(1)), Seed(h(2)), Seed(h(3)), Seed(h(4))]),
        );
    }

    /// Deep chain truncated to `capacity`: keep the newest `capacity` seeds, no
    /// genesis prepend (the walk stops on a non-genesis block).
    #[test]
    fn deep_chain_truncates_to_capacity() {
        // genesis(step1) <- b1(steps 2,3) <- b2(steps 4,5)
        let genesis = header(h(0), H256::zero(), 0, 1, &[h(1)]);
        let b1 = header(h(10), h(0), 1, 3, &[h(2), h(3)]);
        let b2 = header(h(20), h(10), 2, 5, &[h(4), h(5)]);
        let bootstrap = replay_vdf_seeds(b2.block_hash, 2, reader(vec![genesis, b1, b2]));

        assert_eq!(bootstrap.global_step, 5);
        // exactly `capacity` (2) newest seeds, no genesis anchor.
        assert_eq!(
            bootstrap.ordered_seeds,
            VecDeque::from(vec![Seed(h(4)), Seed(h(5))])
        );
        assert_eq!(bootstrap.first_step, 5 - 2 + 1);
    }

    /// Capacity boundary: a height-1 block alone holding exactly `capacity`
    /// steps still gets the genesis seed prepended, so the window is
    /// `capacity + 1` long (the deliberately-preserved legacy behaviour).
    #[test]
    fn height_one_holding_capacity_steps_returns_capacity_plus_one() {
        let capacity = 3;
        let genesis = header(h(0), H256::zero(), 0, 1, &[h(1)]);
        let block1 = header(h(10), h(0), 1, 4, &[h(2), h(3), h(4)]);
        let bootstrap =
            replay_vdf_seeds(block1.block_hash, capacity, reader(vec![genesis, block1]));

        assert_eq!(bootstrap.ordered_seeds.len(), capacity + 1);
        assert_eq!(bootstrap.global_step, 4);
        assert_eq!(bootstrap.first_step, 1);
        assert_eq!(
            bootstrap.ordered_seeds,
            VecDeque::from(vec![Seed(h(1)), Seed(h(2)), Seed(h(3)), Seed(h(4))]),
        );
    }
}
