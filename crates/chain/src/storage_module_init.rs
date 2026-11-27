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
    // The first RocksDB open in the process fixes the shared cache and write
    // buffer. A startup conversion opens RocksDB before any module open, and
    // without this it would fix the production defaults instead of the
    // configured limits.
    irys_database::submodule::install_process_rocks_resources(&config.node_config.database)?;
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
                let path = irys_domain::submodule_dir(config, dir);
                let label = path.display().to_string();
                let span = span.clone();
                let handle = scope.spawn(move || {
                    let _span = span.enter();
                    migrate_submodule_index(&path, config, consensus, partition_chunks)
                });
                (label, handle)
            })
            .collect();
        // Join every thread before returning, so a panic does not hide the
        // errors of the submodules joined after it.
        moves
            .into_iter()
            .filter_map(|(label, handle)| match handle.join() {
                Ok(Ok(())) => None,
                Ok(Err(err)) => Some(format!("{label}: {err:#}")),
                Err(panic) => Some(format!("{label}: panicked: {}", panic_message(&*panic))),
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

fn panic_message(panic: &(dyn std::any::Any + Send)) -> &str {
    panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload")
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

#[cfg(test)]
mod relative_path_tests {
    use irys_config::submodules::StorageSubmodulesConfig;
    use irys_domain::drive_verdict::{self, DriveFile};
    use irys_domain::{PACKING_PARAMS_FILE_NAME, PackingParams, StorageModule};
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::partition::PartitionAssignment;
    use irys_types::{
        Config, ConsensusConfig, ConsensusOptions, DataLedger, H256, NodeConfig, NodeMode,
    };
    use std::path::PathBuf;

    /// A relative base directory and the generated relative submodule paths,
    /// with the process directory not the base: the file a store writes is
    /// the one the startup verdict and the next open read, beside the
    /// chunks, the index and the intervals.
    #[test]
    fn relative_paths_store_where_open_and_the_verdict_read() -> eyre::Result<()> {
        let tmp = TempDirBuilder::new().prefix("relative_paths").build();
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&cwd)?;
        // Each nextest test runs in its own process.
        std::env::set_current_dir(&cwd)?;
        let cwd = std::env::current_dir()?;
        let base = PathBuf::from("node");

        let mut node_config = NodeConfig::testing();
        node_config.node_mode = NodeMode::Miner;
        node_config.base_directory = base.clone();
        node_config.consensus = ConsensusOptions::Custom(ConsensusConfig {
            chunk_size: 32,
            num_chunks_in_partition: 10,
            ..ConsensusConfig::testing()
        });
        let config = Config::new_with_random_peer_id(node_config);

        // The generated config records `node/storage_modules/submodule_N`,
        // relative. Its data lives under the base directory joined with it.
        let submodules = StorageSubmodulesConfig::load(base, NodeMode::Miner)?;
        let dir = cwd.join("node/node/storage_modules/submodule_0");
        assert_eq!(submodules.submodule_paths[0], dir);
        let paths = &submodules.submodule_paths[..1];

        let genesis = H256::repeat_byte(0x9E);
        let startup = drive_verdict::read_startup_drives(&config, paths, genesis)?;
        let module = StorageModule::new(&startup.parked[0], &config)?;
        module.set_chain_identity(genesis);
        let next = PartitionAssignment {
            partition_hash: H256::repeat_byte(0x42),
            miner_address: config.node_config.miner_address(),
            ledger_id: Some(DataLedger::Publish.into()),
            slot_index: Some(3),
        };
        module.hold(Some(next), 7, false)?;
        drop(module);

        let stored = PackingParams::from_toml(dir.join(PACKING_PARAMS_FILE_NAME))?;
        assert!(stored.records(&next), "{stored:?}");
        assert_eq!(stored.assigned_height, Some(7));
        assert_eq!(stored.last_updated_height, Some(7));
        assert!(
            !cwd.join("node/storage_modules/submodule_0")
                .join(PACKING_PARAMS_FILE_NAME)
                .exists(),
            "no packing file is written beside the unjoined path"
        );

        let startup = drive_verdict::read_startup_drives(&config, paths, genesis)?;
        assert_eq!(startup.drives[0].1, DriveFile::Recorded(stored));
        let reopened = StorageModule::new(&startup.parked[0], &config)?;
        assert_eq!(reopened.recorded_params(), stored);
        Ok(())
    }
}
