//! Open every storage module before the node serves traffic.
//!
//! Each module migrates its own index, limited by that module's disk. Every
//! open runs on the tokio blocking pool at once, so the disks work in
//! parallel and no async worker blocks on a migration. A failure does not
//! cancel the others.

use std::sync::Arc;
#[cfg(test)]
use std::time::Duration;

use irys_domain::{StorageModule, StorageModuleInfo};
use irys_types::Config;

pub(super) async fn open_storage_modules(
    config: &Config,
    infos: Vec<StorageModuleInfo>,
    #[cfg(test)] open_delay: Duration,
) -> eyre::Result<Vec<Arc<StorageModule>>> {
    let labels: Vec<String> = infos.iter().map(module_label).collect();
    let opens = infos.into_iter().map(|info| {
        let config = config.clone();
        // spawn_blocking does not carry the caller's span. Give each open its
        // own child span so interleaved migration progress names its module.
        let span = tracing::info_span!("open_storage_module", storage_module.id = info.id);
        tokio::task::spawn_blocking(move || {
            let _span = span.enter();
            #[cfg(test)]
            if !open_delay.is_zero() {
                std::thread::sleep(open_delay);
            }
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
        TxPathInterval, TxPathIntervalByStart,
    };
    use irys_database::{open_or_create_db, submodule_db_args};
    use irys_domain::{PACKING_PARAMS_FILE_NAME, PackingParams, StorageModuleInfo};
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
        write_data_intervals(config, base, name)
    }

    /// `Data` labels plus the matching `packing_params.toml`. Open refuses
    /// labels that have no packing identity.
    fn write_data_intervals(config: &Config, base: &Path, name: &str) -> eyre::Result<()> {
        PackingParams {
            packing_address: config.node_config.miner_address(),
            entropy_packing_iterations: Some(config.consensus.entropy_packing_iterations),
            chunk_size: Some(config.consensus.chunk_size),
            ..Default::default()
        }
        .write_to_disk(&base.join(name).join(PACKING_PARAMS_FILE_NAME))?;
        let mut intervals = String::from("[[{\"start\":0,\"end\":");
        intervals.push_str(&(CHUNKS - 1).to_string());
        intervals.push_str("},\"Data\"]]");
        std::fs::write(base.join(name).join("intervals.json"), intervals)?;
        Ok(())
    }

    /// Schema v2 already. Open must leave the interval row where it is.
    fn seed_v2(config: &Config, base: &Path, name: &str) -> eyre::Result<()> {
        let dir = db_dir(base, name);
        std::fs::create_dir_all(&dir)?;
        let env = open_or_create_db(&dir, SubmoduleTables::ALL, db_args(config)?)?;
        let tx = env.tx_mut()?;
        tx.put::<Metadata>(
            MetadataKey::DBSchemaVersion,
            SUBMODULE_SCHEMA.as_bytes().to_vec(),
        )?;
        tx.put::<TxPathIntervalByStart>(
            PartitionChunkOffset(0),
            TxPathInterval {
                end: PartitionChunkOffset(CHUNKS - 1),
                tx_path_hash: H256::repeat_byte(9),
            },
        )?;
        tx.commit()?;
        drop(env);
        write_data_intervals(config, base, name)
    }

    /// `rocks/SCHEMA` with an MDBX config. Open must refuse this directory.
    fn seed_rocks(base: &Path, name: &str) -> eyre::Result<()> {
        let rocks = base.join(name).join("rocks");
        std::fs::create_dir_all(&rocks)?;
        std::fs::write(rocks.join("SCHEMA"), b"irys-submodule-index rocksdb 1\n")?;
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

    fn assert_v2_unchanged(config: &Config, base: &Path, name: &str) -> eyre::Result<()> {
        let env = open_or_create_db(db_dir(base, name), SubmoduleTables::ALL, db_args(config)?)?;
        let tx = env.tx()?;
        let schema = tx.get::<Metadata>(MetadataKey::DBSchemaVersion)?;
        assert_eq!(
            schema.as_deref(),
            Some(SUBMODULE_SCHEMA.as_bytes()),
            "{name} schema changed"
        );
        let interval = tx
            .get::<TxPathIntervalByStart>(PartitionChunkOffset::from(0))?
            .unwrap_or_else(|| panic!("{name} lost its interval"));
        assert_eq!(interval.end, PartitionChunkOffset(CHUNKS - 1));
        assert_eq!(interval.tx_path_hash, H256::repeat_byte(9));
        assert_eq!(
            tx.entries::<ChunkPathHashesByOffset>()?,
            0,
            "{name} gained path-hash rows"
        );
        Ok(())
    }

    /// Opens several v1 modules together. The break this catches is a serial
    /// loop, a reordered result, or an error that names only the first failure.
    #[tokio::test(flavor = "multi_thread")]
    async fn parallel_v1_open_keeps_order_reports_every_failure_and_overlaps() -> eyre::Result<()> {
        let tmp = TempDirBuilder::new().prefix("sm-open-parallel").build();
        let base = tmp.path();
        let config = config(base);
        let names = ["good-0", "good-1", "good-2"];
        for name in names {
            seed_v1(&config, base, name)?;
        }
        let infos = vec![info(10, "good-0"), info(20, "good-1"), info(30, "good-2")];
        let started = Instant::now();
        let opened = open_storage_modules(&config, infos, DELAY).await?;
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
        let already = "already-v2";
        let rocks = "rocks-mod";
        seed_foreign(&config, base, bad_first)?;
        seed_v1(&config, base, kept_a)?;
        seed_foreign(&config, base, bad_second)?;
        seed_v1(&config, base, kept_b)?;
        seed_v2(&config, base, already)?;
        seed_rocks(base, rocks)?;
        let err = open_storage_modules(
            &config,
            vec![
                info(1, bad_first),
                info(2, kept_a),
                info(3, bad_second),
                info(4, kept_b),
                info(5, already),
                info(6, rocks),
            ],
            Duration::ZERO,
        )
        .await
        .expect_err("foreign schemas and a rocks directory must fail");
        let text = format!("{err:#}");
        assert!(
            text.contains(bad_first) && text.contains(bad_second),
            "every failure must be named, got {text}"
        );
        assert!(
            text.contains("storage module 1") && text.contains("storage module 3"),
            "{text}"
        );
        assert!(
            text.contains("storage module 6")
                && text.contains(rocks)
                && text.contains("the Rocks index was not copied"),
            "{text}"
        );
        assert!(
            !text.contains("storage module 5"),
            "already-v2 module failed to open: {text}"
        );
        assert_migrated(&config, base, kept_a)?;
        assert_migrated(&config, base, kept_b)?;
        assert_v2_unchanged(&config, base, already)?;
        Ok(())
    }
}
