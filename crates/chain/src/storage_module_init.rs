//! Open every storage module before the node serves traffic.
//!
//! Each module migrates its own index. The migration is limited by that
//! module's disk. The infos do not name a device, so this uses one thread
//! per module. A CPU cap would leave the other disks idle. Threads join
//! before the function returns. A failure does not cancel the others.

use std::sync::Arc;
#[cfg(test)]
use std::time::Duration;

use irys_domain::{StorageModule, StorageModuleInfo};
use irys_types::Config;

pub(super) fn open_storage_modules(
    config: &Config,
    infos: Vec<StorageModuleInfo>,
    #[cfg(test)] open_delay: Duration,
) -> eyre::Result<Vec<Arc<StorageModule>>> {
    if infos.is_empty() {
        return Ok(Vec::new());
    }
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(infos.len());
        for info in &infos {
            // `info` and `config` are shared for the scope. `move` copies the
            // references into the thread. The infos do not name a device.
            handles.push(scope.spawn(move || {
                #[cfg(test)]
                if !open_delay.is_zero() {
                    std::thread::sleep(open_delay);
                }
                StorageModule::new(info, config).map(Arc::new)
            }));
        }
        let mut opened = Vec::with_capacity(infos.len());
        let mut failures = Vec::new();
        for (info, handle) in infos.iter().zip(handles) {
            match handle.join() {
                Ok(Ok(module)) => opened.push(module),
                Ok(Err(err)) => {
                    failures.push(format!("{}: {err:#}", module_label(info)));
                }
                Err(_) => {
                    failures.push(format!("{}: panicked while opening", module_label(info)));
                }
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
    })
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

    use irys_database::metadata::MetadataKey;
    use irys_database::reth_db::Database as _;
    use irys_database::submodule::tables::{
        ChunkPathHashes, ChunkPathHashesByOffset, Metadata, SUBMODULE_SCHEMA, SubmoduleTables,
        TxPathIntervalByStart,
    };
    use irys_database::{open_or_create_db, submodule_db_args};
    use irys_domain::StorageModuleInfo;
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::storage::ii;
    use irys_types::{Config, H256, NodeConfig, PartitionChunkOffset};
    use reth_db::transaction::{DbTx as _, DbTxMut as _};

    use super::open_storage_modules;

    const CHUNKS: u32 = 10;
    const DELAY: Duration = Duration::from_millis(300);

    fn config(dir: &Path) -> Config {
        let mut node = NodeConfig::testing();
        node.base_directory = dir.to_path_buf();
        Config::new_with_random_peer_id(node)
    }

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

    fn db_dir(base: &Path, name: &str) -> std::path::PathBuf {
        base.join(name).join("db")
    }

    fn db_args(config: &Config) -> eyre::Result<reth_db::mdbx::DatabaseArguments> {
        // Same geometry the module open uses. A different cap refuses the env.
        submodule_db_args(&config.node_config.database)
    }

    fn seed_v1(config: &Config, base: &Path, name: &str) -> eyre::Result<()> {
        let dir = db_dir(base, name);
        std::fs::create_dir_all(&dir)?;
        let env = open_or_create_db(&dir, SubmoduleTables::ALL, db_args(config)?)?;
        let tx = env.tx_mut()?;
        let tx_hash = H256::repeat_byte(7);
        for offset in 0..CHUNKS {
            tx.put::<ChunkPathHashesByOffset>(
                PartitionChunkOffset(offset),
                ChunkPathHashes {
                    data_path_hash: None,
                    tx_path_hash: Some(tx_hash),
                },
            )?;
        }
        tx.commit()?;
        // Open drops tx intervals when this file is missing. A live module
        // already has it. `Data` keeps the rows the migration just wrote.
        let mut intervals = String::from("[[{\"start\":0,\"end\":");
        intervals.push_str(&(CHUNKS - 1).to_string());
        intervals.push_str("},\"Data\"]]");
        std::fs::write(base.join(name).join("intervals.json"), intervals)?;
        Ok(())
    }

    fn seed_foreign(config: &Config, base: &Path, name: &str) -> eyre::Result<()> {
        let dir = db_dir(base, name);
        std::fs::create_dir_all(&dir)?;
        let env = open_or_create_db(&dir, SubmoduleTables::ALL, db_args(config)?)?;
        let tx = env.tx_mut()?;
        tx.put::<Metadata>(MetadataKey::DBSchemaVersion, b"not-this-binary\n".to_vec())?;
        tx.commit()?;
        Ok(())
    }

    fn assert_migrated(config: &Config, base: &Path, name: &str) -> eyre::Result<()> {
        let env = open_or_create_db(db_dir(base, name), SubmoduleTables::ALL, db_args(config)?)?;
        let tx = env.tx()?;
        let schema = tx.get::<Metadata>(MetadataKey::DBSchemaVersion)?;
        assert_eq!(
            schema.as_deref(),
            Some(SUBMODULE_SCHEMA.as_bytes()),
            "{name} did not finish migration"
        );
        let interval = tx
            .get::<TxPathIntervalByStart>(PartitionChunkOffset::from(0))?
            .unwrap_or_else(|| panic!("{name} has no interval"));
        assert_eq!(interval.end, PartitionChunkOffset(CHUNKS - 1));
        assert_eq!(interval.tx_path_hash, H256::repeat_byte(7));
        Ok(())
    }

    /// Opens several v1 modules together. The break this catches is a serial
    /// loop, a reordered result, or an error that names only the first failure.
    #[test]
    fn parallel_v1_open_keeps_order_reports_every_failure_and_overlaps() -> eyre::Result<()> {
        let tmp = TempDirBuilder::new().prefix("sm-open-parallel").build();
        let base = tmp.path();
        let config = config(base);
        let names = ["good-0", "good-1", "good-2"];
        for name in names {
            seed_v1(&config, base, name)?;
        }
        let infos = vec![info(10, "good-0"), info(20, "good-1"), info(30, "good-2")];
        let started = Instant::now();
        let opened = open_storage_modules(&config, infos, DELAY)?;
        let elapsed = started.elapsed();
        let ids: Vec<_> = opened.iter().map(|module| module.id).collect();
        assert_eq!(ids, vec![10, 20, 30]);
        drop(opened);
        for name in names {
            assert_migrated(&config, base, name)?;
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
        seed_foreign(&config, base, bad_first)?;
        seed_v1(&config, base, kept_a)?;
        seed_foreign(&config, base, bad_second)?;
        seed_v1(&config, base, kept_b)?;
        let err = open_storage_modules(
            &config,
            vec![
                info(1, bad_first),
                info(2, kept_a),
                info(3, bad_second),
                info(4, kept_b),
            ],
            Duration::ZERO,
        )
        .expect_err("two modules have a foreign schema");
        let text = format!("{err:#}");
        assert!(
            text.contains(bad_first) && text.contains(bad_second),
            "every failure must be named, got {text}"
        );
        assert!(
            text.contains("storage module 1") && text.contains("storage module 3"),
            "{text}"
        );
        assert_migrated(&config, base, kept_a)?;
        assert_migrated(&config, base, kept_b)?;
        Ok(())
    }
}
