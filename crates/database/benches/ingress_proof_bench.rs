//! Benchmark: Ingress proof store operation
//!
//! Regression signal: store_external_ingress_proof_checked cost per proof.
//! Uses the delete-then-insert DupSort pattern. Per ingress proof during validation.

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use irys_database::{
    IrysDatabaseArgs as _, db::IrysDatabaseExt as _, open_or_create_db,
    store_external_ingress_proof_checked, tables::IrysTables,
};
use irys_testing_utils::utils::TempDirBuilder;
use irys_types::{H256, IngressProof, IrysAddress, ingress::IngressProofV1};
use reth_db::{mdbx::DatabaseArguments, transaction::DbTxMut as _};

fn bench_store_ingress_proof(c: &mut Criterion) {
    let mut group = c.benchmark_group("ingress_proof/store");

    for num_existing in [0_u32, 1, 10] {
        let db_path = TempDirBuilder::new().build();
        let db = open_or_create_db(
            db_path.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();

        let data_root = H256::from([0xCD; 32]);

        db.update_eyre(|tx| {
            tx.put::<irys_database::tables::CachedDataRoots>(
                data_root,
                irys_database::db_cache::CachedDataRoot {
                    data_size: 256 * 1024,
                    ..Default::default()
                },
            )?;

            for i in 0..num_existing {
                let existing_addr = IrysAddress::from([u8::try_from(i + 1).unwrap(); 20]);
                let proof = IngressProof::V1(IngressProofV1 {
                    data_root,
                    proof: H256::from([u8::try_from(i + 1).unwrap(); 32]),
                    ..Default::default()
                });
                store_external_ingress_proof_checked(tx, &proof, existing_addr)?;
            }
            Ok(())
        })
        .unwrap();

        let new_proof = IngressProof::V1(IngressProofV1 {
            data_root,
            proof: H256::from([0xFF; 32]),
            ..Default::default()
        });

        group.bench_function(
            BenchmarkId::from_parameter(format!("{num_existing}_existing")),
            |b| {
                b.iter_batched(
                    || {
                        let addr = IrysAddress::from([0xAB; 20]);
                        (new_proof.clone(), addr) // clone: need owned copy per iteration
                    },
                    |(proof, addr)| {
                        db.update_eyre(|tx| {
                            black_box(store_external_ingress_proof_checked(tx, &proof, addr))
                        })
                        .unwrap();
                    },
                    criterion::BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_store_ingress_proof);
criterion_main!(benches);
