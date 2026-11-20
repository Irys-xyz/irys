//! Open every storage module before the node serves traffic.
//!
//! Each module first moves any MDBX submodule index to RocksDB, limited by
//! that module's disk. Every open runs on the tokio blocking pool at once, so
//! the disks work in parallel and no async worker blocks on a migration. A
//! failure does not cancel the others.

use std::sync::Arc;
#[cfg(test)]
use std::time::Duration;

use irys_database::reth_db::DatabaseEnv;
use irys_domain::{StorageModule, StorageModuleInfo};
use irys_types::Config;

use crate::submodule_index_migration::migrate_submodule_index;

pub(super) async fn open_storage_modules(
    config: &Config,
    infos: Vec<StorageModuleInfo>,
    consensus: Arc<DatabaseEnv>,
    partition_chunks: u64,
    #[cfg(test)] open_delay: Duration,
) -> eyre::Result<Vec<Arc<StorageModule>>> {
    let labels: Vec<String> = infos.iter().map(module_label).collect();
    let opens = infos.into_iter().map(|info| {
        let config = config.clone();
        let consensus = Arc::clone(&consensus);
        // spawn_blocking does not carry the caller's span. Give each open its
        // own child span so interleaved migration progress names its module.
        let span = tracing::info_span!("open_storage_module", storage_module.id = info.id);
        tokio::task::spawn_blocking(move || {
            let _span = span.enter();
            #[cfg(test)]
            if !open_delay.is_zero() {
                std::thread::sleep(open_delay);
            }
            // MDBX-SUBMODULE-INDEX: delete this call with submodule_index_migration.
            migrate_submodule_indexes(&info, &config, &consensus, partition_chunks)?;
            StorageModule::new(&info, &config).map(Arc::new)
        })
    });
    let results = futures::future::join_all(opens).await;

    let mut opened = Vec::with_capacity(results.len());
    let mut failures = Vec::new();
    for (label, result) in labels.iter().zip(results) {
        match result {
            Ok(Ok(module)) => opened.push(module),
            Ok(Err(err)) => failures.push(format!("{label}: {err:#}")),
            Err(err) => failures.push(format!("{label}: {err}")),
        }
    }
    if failures.is_empty() {
        Ok(opened)
    } else {
        Err(eyre::eyre!(
            "failed to open {} storage module(s): {}",
            failures.len(),
            failures.join("; ")
        ))
    }
}

/// Moves each submodule index of one module to RocksDB on its own thread.
/// Submodules are often on different disks, so the moves overlap. Every
/// failure is reported, and a failure does not stop the other moves.
/// `partition_chunks` is the active slot width (see
/// [`migrate_submodule_index`]).
// MDBX-SUBMODULE-INDEX: delete this function with submodule_index_migration.
fn migrate_submodule_indexes(
    info: &StorageModuleInfo,
    config: &Config,
    consensus: &DatabaseEnv,
    partition_chunks: u64,
) -> eyre::Result<()> {
    let span = tracing::Span::current();
    let failures: Vec<String> = std::thread::scope(|scope| {
        let moves: Vec<_> = info
            .submodules
            .iter()
            .map(|(_, dir)| {
                let path = config.node_config.base_directory.join(dir);
                let span = span.clone();
                scope.spawn(move || {
                    let _span = span.enter();
                    migrate_submodule_index(&path, config, consensus, partition_chunks)
                        .map_err(|err| format!("{}: {err:#}", path.display()))
                })
            })
            .collect();
        moves
            .into_iter()
            .filter_map(|handle| match handle.join() {
                Ok(result) => result.err(),
                Err(panic) => std::panic::resume_unwind(panic),
            })
            .collect()
    });
    if failures.is_empty() {
        Ok(())
    } else {
        Err(eyre::eyre!(
            "failed to move {} submodule index(es) to RocksDB: {}",
            failures.len(),
            failures.join("; ")
        ))
    }
}

fn module_label(info: &StorageModuleInfo) -> String {
    let paths = info
        .submodules
        .iter()
        .map(|(_, path)| path.display().to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!("storage module {} ({paths})", info.id)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use irys_domain::StorageModuleInfo;
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::PartitionChunkOffset;
    use irys_types::storage::ii;

    use super::{migrate_submodule_indexes, open_storage_modules};
    use crate::submodule_index_migration::tests::{
        assert_moved, config, consensus, seed_empty_mdbx, seed_foreign_mdbx, seed_placed_mdbx,
    };

    const CHUNKS: u32 = 10;
    const DELAY: Duration = Duration::from_millis(300);

    fn info(id: usize, name: &str) -> StorageModuleInfo {
        let last = CHUNKS - 1;
        StorageModuleInfo {
            id,
            partition_assignment: None,
            submodules: vec![(
                ii(PartitionChunkOffset(0), PartitionChunkOffset(last)),
                name.into(),
            )],
        }
    }

    fn fresh_rocks(base: &Path, name: &str) {
        assert!(base.join(name).join("rocks").join("SCHEMA").is_file());
        assert!(!base.join(name).join("db").exists());
        assert!(!base.join(name).join("db.migrated").exists());
    }

    /// A failed submodule move does not stop its sibling in the same module.
    #[test]
    fn a_failed_submodule_move_does_not_stop_its_sibling() -> eyre::Result<()> {
        let tmp = TempDirBuilder::new().prefix("sm-submodules").build();
        let base = tmp.path();
        let config = config(base);
        let consensus = consensus(&config)?;
        seed_foreign_mdbx(&config, &base.join("sub-bad"))?;
        seed_empty_mdbx(&config, &base.join("sub-good"))?;
        let info = StorageModuleInfo {
            id: 7,
            partition_assignment: None,
            submodules: vec![
                (
                    ii(PartitionChunkOffset(0), PartitionChunkOffset(4)),
                    "sub-bad".into(),
                ),
                (
                    ii(PartitionChunkOffset(5), PartitionChunkOffset(CHUNKS - 1)),
                    "sub-good".into(),
                ),
            ],
        };
        let err =
            migrate_submodule_indexes(&info, &config, &consensus, u64::from(CHUNKS)).unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("1 submodule index(es)") && text.contains("sub-bad"),
            "{text}"
        );
        assert!(!text.contains("sub-good"), "{text}");
        assert_moved(&base.join("sub-good"));
        assert!(base.join("sub-bad").join("db").join("mdbx.dat").is_file());
        Ok(())
    }

    /// After the move, `db.migrated/` holds the only copy of the old rows. A
    /// lost `rocks/` must not be replaced by an empty index.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_missing_rocks_index_beside_db_migrated_is_refused() -> eyre::Result<()> {
        let tmp = TempDirBuilder::new().prefix("sm-open-lost-rocks").build();
        let base = tmp.path();
        let config = config(base);
        let consensus = consensus(&config)?;
        seed_empty_mdbx(&config, &base.join("moved"))?;
        let open = |consensus| {
            open_storage_modules(
                &config,
                vec![info(1, "moved")],
                consensus,
                u64::from(CHUNKS),
                Duration::ZERO,
            )
        };
        drop(open(consensus.clone()).await?);
        assert_moved(&base.join("moved"));
        std::fs::remove_dir_all(base.join("moved").join("rocks"))?;
        let err = open(consensus)
            .await
            .expect_err("a lost rocks/ must be refused");
        assert!(
            format!("{err:#}").contains("the live RocksDB index is missing"),
            "{err:#}"
        );
        assert!(!base.join("moved").join("rocks").exists());
        Ok(())
    }

    /// Opens several modules together and moves each MDBX index to RocksDB.
    /// The break this catches is a serial loop, a reordered result, an error
    /// that names only the first failure, or a failure that stops another
    /// module's move.
    #[tokio::test(flavor = "multi_thread")]
    async fn parallel_open_moves_every_index_keeps_order_reports_every_failure_and_overlaps()
    -> eyre::Result<()> {
        let tmp = TempDirBuilder::new().prefix("sm-open-parallel").build();
        let base = tmp.path();
        let config = config(base);
        let consensus = consensus(&config)?;
        let names = ["good-0", "good-1", "good-2"];
        for name in names {
            seed_empty_mdbx(&config, &base.join(name))?;
        }
        let infos = vec![info(10, "good-0"), info(20, "good-1"), info(30, "good-2")];
        let started = Instant::now();
        let opened =
            open_storage_modules(&config, infos, consensus.clone(), u64::from(CHUNKS), DELAY)
                .await?;
        let elapsed = started.elapsed();
        let ids: Vec<_> = opened.iter().map(|module| module.id).collect();
        assert_eq!(ids, vec![10, 20, 30]);
        drop(opened);
        for name in names {
            assert_moved(&base.join(name));
        }
        let serial = DELAY * names.len() as u32;
        assert!(
            elapsed + Duration::from_millis(20) >= DELAY,
            "each open sleeps {DELAY:?}, wall was {elapsed:?}"
        );
        assert!(
            elapsed < serial,
            "parallel open took {elapsed:?}, serial sleep sum is {serial:?}"
        );

        let bad_first = "bad-1";
        let bad_second = "bad-3";
        let kept_a = "kept-a";
        let kept_b = "kept-b";
        let fresh = "fresh";
        seed_foreign_mdbx(&config, &base.join(bad_first))?;
        seed_empty_mdbx(&config, &base.join(kept_a))?;
        seed_placed_mdbx(&config, &base.join(bad_second))?;
        seed_empty_mdbx(&config, &base.join(kept_b))?;
        let err = open_storage_modules(
            &config,
            vec![
                info(1, bad_first),
                info(2, kept_a),
                info(3, bad_second),
                info(4, kept_b),
                info(5, fresh),
            ],
            consensus,
            u64::from(CHUNKS),
            Duration::ZERO,
        )
        .await
        .expect_err("a foreign schema and an unconvertible index must fail");
        let text = format!("{err:#}");
        assert!(
            text.contains("storage module 1")
                && text.contains(bad_first)
                && text.contains("not a submodule index"),
            "{text}"
        );
        assert!(
            text.contains("storage module 3")
                && text.contains(bad_second)
                && text.contains("names no data ledger"),
            "{text}"
        );
        for id in [2, 4, 5] {
            assert!(
                !text.contains(&format!("storage module {id} ")),
                "module {id} failed to open: {text}"
            );
        }
        assert_moved(&base.join(kept_a));
        assert_moved(&base.join(kept_b));
        fresh_rocks(base, fresh);
        for name in [bad_first, bad_second] {
            assert!(base.join(name).join("db").join("mdbx.dat").is_file());
            assert!(!base.join(name).join("rocks").exists());
        }
        Ok(())
    }
}
