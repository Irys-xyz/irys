//! Benchmark: Block index insert, read, and delete operations
//!
//! Regression signal: insert_block_index_items_for_block per-block cost,
//! block_index_item_by_height read latency, and delete_block_index_range
//! rollback cost by range size.

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use irys_database::{
    IrysDatabaseArgs as _, block_index_item_by_height, db::IrysDatabaseExt as _,
    delete_block_index_range, insert_block_index_items_for_block, open_or_create_db,
    tables::IrysTables,
};
use irys_testing_utils::utils::TempDirBuilder;
use irys_types::IrysBlockHeader;
use reth_db::{Database as _, mdbx::DatabaseArguments};

fn bench_insert_block_index(c: &mut Criterion) {
    let mut group = c.benchmark_group("block_index/insert");
    let db_path = TempDirBuilder::new().build();
    let db = open_or_create_db(
        db_path.path(),
        IrysTables::ALL,
        DatabaseArguments::irys_testing().unwrap(),
    )
    .unwrap();

    let mut height_counter = 0_u64;

    group.bench_function(BenchmarkId::from_parameter("single_block"), |b| {
        b.iter_batched(
            || {
                height_counter += 1;
                let mut header = IrysBlockHeader::new_mock_header();
                header.height = height_counter;
                header.block_hash.0[0] = u8::try_from(height_counter % 256).unwrap();
                header
            },
            |header| {
                db.update(|tx| {
                    insert_block_index_items_for_block(tx, black_box(&header)).unwrap();
                    Ok::<(), reth_db::DatabaseError>(())
                })
                .unwrap()
                .unwrap();
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.finish();
}

fn bench_read_block_index(c: &mut Criterion) {
    let mut group = c.benchmark_group("block_index/read");

    for count in [10_u64, 100, 1000] {
        let db_path = TempDirBuilder::new().build();
        let db = open_or_create_db(
            db_path.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();

        db.update(|tx| {
            for i in 1..=count {
                let mut header = IrysBlockHeader::new_mock_header();
                header.height = i;
                header.block_hash.0[0] = u8::try_from(i % 256).unwrap();
                insert_block_index_items_for_block(tx, &header).unwrap();
            }
            Ok::<(), reth_db::DatabaseError>(())
        })
        .unwrap()
        .unwrap();

        let target_height = count / 2;

        group.bench_function(
            BenchmarkId::from_parameter(format!("{count}_blocks")),
            |b| {
                b.iter(|| {
                    db.view_eyre(|tx| black_box(block_index_item_by_height(tx, &target_height)))
                        .unwrap()
                });
            },
        );
    }

    group.finish();
}

fn bench_delete_block_index_range(c: &mut Criterion) {
    let mut group = c.benchmark_group("block_index/delete_range");

    for range_size in [10_u64, 100, 1000] {
        group.bench_function(
            BenchmarkId::from_parameter(format!("{range_size}_blocks")),
            |b| {
                b.iter_batched(
                    || {
                        let db_path = TempDirBuilder::new().build();
                        let db = open_or_create_db(
                            db_path.path(),
                            IrysTables::ALL,
                            DatabaseArguments::irys_testing().unwrap(),
                        )
                        .unwrap();

                        db.update(|tx| {
                            for i in 1..=range_size {
                                let mut header = IrysBlockHeader::new_mock_header();
                                header.height = i;
                                header.block_hash.0[0] = u8::try_from(i % 256).unwrap();
                                insert_block_index_items_for_block(tx, &header).unwrap();
                            }
                            Ok::<(), reth_db::DatabaseError>(())
                        })
                        .unwrap()
                        .unwrap();

                        (db, db_path)
                    },
                    |(db, _db_path)| {
                        db.update(|tx| {
                            delete_block_index_range(tx, black_box(1..=range_size)).unwrap();
                            Ok::<(), reth_db::DatabaseError>(())
                        })
                        .unwrap()
                        .unwrap();
                    },
                    criterion::BatchSize::LargeInput,
                );
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_insert_block_index,
    bench_read_block_index,
    bench_delete_block_index_range,
);
criterion_main!(benches);
