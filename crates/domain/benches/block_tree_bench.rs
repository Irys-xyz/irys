//! Benchmark: BlockTree::mark_tip — fork choice and cache rebuild
//!
//! Regression signal: mark_tip latency by chain depth. Covers
//! update_longest_chain_cache internally. Per received block during validation.

use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use irys_domain::{BlockTree, CommitmentSnapshot, dummy_ema_snapshot, dummy_epoch_snapshot};
use irys_testing_utils::IrysBlockHeaderTestExt as _;
use irys_types::{BlockBody, ConsensusConfig, H256, IrysBlockHeader, SealedBlock, U256};

fn random_block(cumulative_diff: U256) -> IrysBlockHeader {
    let mut block = IrysBlockHeader::new_mock_header();
    block.solution_hash = H256::random();
    block.height = 0;
    block.cumulative_diff = cumulative_diff;
    block.test_sign();
    block
}

fn extend_chain(
    mut new_block: IrysBlockHeader,
    previous_block: &IrysBlockHeader,
) -> IrysBlockHeader {
    new_block.previous_block_hash = previous_block.block_hash();
    new_block.height = previous_block.height() + 1;
    new_block.previous_cumulative_diff = previous_block.cumulative_diff;
    new_block.test_sign();
    new_block
}

fn seal_block(header: &mut IrysBlockHeader) -> Arc<SealedBlock> {
    header.ensure_test_signed();
    let body = BlockBody {
        block_hash: header.block_hash,
        ..Default::default()
    };
    Arc::new(SealedBlock::new(header.clone(), body).expect("sealing block")) // clone: SealedBlock takes ownership
}

fn build_linear_chain(depth: usize) -> (BlockTree, Vec<IrysBlockHeader>) {
    let config = ConsensusConfig::testing();
    let genesis = random_block(U256::from(0));
    let mut tree = BlockTree::new(&genesis, config);

    let comm = Arc::new(CommitmentSnapshot::default());
    let ema = dummy_ema_snapshot();
    let epoch = dummy_epoch_snapshot();

    let mut sealed_genesis = genesis.clone(); // clone: need owned copy for seal_block
    let sealed = seal_block(&mut sealed_genesis);
    tree.add_block(&sealed, comm.clone(), epoch.clone(), ema.clone()) // clone: Arc, cheap
        .unwrap();
    tree.mark_tip(&genesis.block_hash).unwrap();

    let mut blocks = vec![genesis];

    for i in 1..depth {
        let mut block = extend_chain(
            random_block(U256::from(u64::try_from(i).unwrap())),
            &blocks[i - 1],
        );
        let sealed = seal_block(&mut block);
        tree.add_block(&sealed, comm.clone(), epoch.clone(), ema.clone()) // clone: Arc, cheap
            .unwrap();
        tree.mark_tip(&block.block_hash).unwrap();
        blocks.push(block);
    }

    (tree, blocks)
}

fn bench_mark_tip_linear(c: &mut Criterion) {
    let mut group = c.benchmark_group("block_tree/mark_tip");

    for depth in [10_usize, 100, 500] {
        let (mut tree, blocks) = build_linear_chain(depth);
        let tip_hash = blocks.last().unwrap().block_hash;

        group.bench_function(
            BenchmarkId::from_parameter(format!("{depth}_blocks")),
            |b| {
                b.iter(|| {
                    black_box(tree.mark_tip(&tip_hash).unwrap());
                });
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_mark_tip_linear);
criterion_main!(benches);
