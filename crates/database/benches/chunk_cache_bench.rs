//! Benchmark: Chunk cache write and read operations
//!
//! Regression signal: cache_chunk_verified write throughput and
//! cached_chunk_by_chunk_offset read latency.
//! These run per-chunk during data ingestion and PoA mining.

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use irys_database::{
    IrysDatabaseArgs as _, cache_chunk_verified, cached_chunk_by_chunk_offset,
    db::IrysDatabaseExt as _, open_or_create_db, tables::IrysTables,
};
use irys_testing_utils::utils::TempDirBuilder;
use irys_types::{Base64, DataRoot, H256, MAX_CHUNK_SIZE, TxChunkOffset, UnpackedChunk};
use reth_db::{Database as _, mdbx::DatabaseArguments, transaction::DbTxMut as _};

fn make_chunk(data_root: DataRoot, offset: u32) -> UnpackedChunk {
    let mut data = vec![0xAB_u8; MAX_CHUNK_SIZE];
    data[0] = u8::try_from(offset % 256).unwrap();
    UnpackedChunk {
        data_root,
        data_size: u64::try_from(MAX_CHUNK_SIZE).unwrap(),
        data_path: Base64(vec![u8::try_from(offset % 256).unwrap(); 64]),
        bytes: Base64(data),
        tx_offset: TxChunkOffset(offset),
    }
}

fn bench_cache_chunk_write(c: &mut Criterion) {
    let mut group = c.benchmark_group("chunk_cache/write");
    let db_path = TempDirBuilder::new().build();
    let db = open_or_create_db(
        db_path.path(),
        IrysTables::ALL,
        DatabaseArguments::irys_testing().unwrap(),
    )
    .unwrap();

    let data_root = H256::from([0xCD; 32]);
    let mut offset_counter = 0_u32;

    db.update(|tx| {
        tx.put::<irys_database::tables::CachedDataRoots>(
            data_root,
            irys_database::db_cache::CachedDataRoot {
                data_size: u64::try_from(MAX_CHUNK_SIZE).unwrap(),
                ..Default::default()
            },
        )?;
        Ok::<(), reth_db::DatabaseError>(())
    })
    .unwrap()
    .unwrap();

    group.bench_function(BenchmarkId::from_parameter("single_chunk"), |b| {
        b.iter_batched(
            || {
                offset_counter += 1;
                make_chunk(data_root, offset_counter)
            },
            |chunk| {
                db.update(|tx| {
                    black_box(cache_chunk_verified(tx, &chunk).unwrap());
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

fn bench_cache_chunk_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("chunk_cache/read");

    for count in [1_u32, 10, 100] {
        let db_path = TempDirBuilder::new().build();
        let db = open_or_create_db(
            db_path.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();

        let data_root = H256::from([0xCD; 32]);

        db.update(|tx| {
            tx.put::<irys_database::tables::CachedDataRoots>(
                data_root,
                irys_database::db_cache::CachedDataRoot {
                    data_size: u64::try_from(MAX_CHUNK_SIZE).unwrap(),
                    ..Default::default()
                },
            )?;
            for i in 0..count {
                let chunk = make_chunk(data_root, i);
                cache_chunk_verified(tx, &chunk).unwrap();
            }
            Ok::<(), reth_db::DatabaseError>(())
        })
        .unwrap()
        .unwrap();

        let target_offset = TxChunkOffset(0);

        group.bench_function(
            BenchmarkId::from_parameter(format!("{count}_chunks")),
            |b| {
                b.iter(|| {
                    db.view_eyre(|tx| {
                        black_box(cached_chunk_by_chunk_offset(tx, data_root, target_offset))
                    })
                    .unwrap()
                });
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_cache_chunk_write, bench_cache_chunk_read);
criterion_main!(benches);
