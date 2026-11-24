// MDBX-SUBMODULE-INDEX: delete this module with the MDBX submodule engine. It moves each submodule index from MDBX (db/) to RocksDB v1 (rocks/) when the node starts.
//! Startup move of each submodule index from MDBX (`db/`) to RocksDB v1 (`rocks/`).
//!
//! RocksDB is the only engine the node opens. A submodule that still has a
//! live `db/` is converted before its module opens:
//!
//! 1. Convert `db/` into `rocks.migrating/` and verify the copy.
//! 2. Rename `rocks.migrating/` to `rocks/`.
//! 3. Rename `db/` to `db.migrated/`. The node does not read it again. It
//!    stays for a `submodule-index-convert rocks-to-mdbx` rollback.
//!
//! Each step leaves a state the next start can finish. `db/` does not change
//! until `rocks/` is complete, so an interrupted step 1 is redone from the
//! start. A complete `rocks/` beside `db/` is the state after step 2, or after
//! a manual conversion. In both, `rocks/` is the live index and only step 3
//! runs.

use std::fs;
use std::path::Path;

use eyre::WrapErr as _;
use irys_database::reth_db::DatabaseEnv;
use irys_database::submodule::tables::SUBMODULE_SCHEMA_ROCKS_V1;
use irys_database::submodule::{
    ConvertOptions, check_rocks_index_opens, convert_mdbx_to_rocks_at_startup,
};
use irys_database::submodule_db_args;
use irys_domain::{PACKING_PARAMS_FILE_NAME, PackingParams};
use irys_types::Config;
use tracing::{info, warn};

const MDBX_INDEX_DIR: &str = "db";
const ROCKS_INDEX_DIR: &str = "rocks";
/// Conversion target until the copy is verified.
const MIGRATING_DIR: &str = "rocks.migrating";
/// `db/` after `rocks/` is the live index.
const MIGRATED_MDBX_DIR: &str = "db.migrated";
/// Written last by the converter. See `convert.rs`.
const ROCKS_SCHEMA_FILE: &str = "SCHEMA";
/// Present while a conversion has not finished. See `convert.rs`.
const CONVERTING_FILE: &str = "CONVERTING";

/// Makes `rocks/` the live index of the submodule at `submodule_dir`.
///
/// A submodule without `db/mdbx.dat` has nothing to move. `consensus` is the
/// node's open consensus database. The converter derives every tx path from it.
/// `partition_chunks` is the active slot width from the epoch snapshot. The
/// index placed each row at `slot * partition_chunks + offset`, and a
/// partition resize makes that width differ from the genesis
/// `num_chunks_in_partition`.
pub(crate) fn migrate_submodule_index(
    submodule_dir: &Path,
    config: &Config,
    consensus: &DatabaseEnv,
    partition_chunks: u64,
) -> eyre::Result<()> {
    let mdbx = submodule_dir.join(MDBX_INDEX_DIR);
    if !mdbx.join("mdbx.dat").is_file() {
        return Ok(());
    }
    let rocks = submodule_dir.join(ROCKS_INDEX_DIR);
    let migrated = submodule_dir.join(MIGRATED_MDBX_DIR);
    if migrated.exists() {
        eyre::bail!(
            "cannot move the MDBX index {} to {}: that path exists. Move {} out of the submodule directory and start again.",
            mdbx.display(),
            migrated.display(),
            migrated.display()
        );
    }
    if rocks.exists() {
        if let Err(reason) = check_live_rocks_index(&rocks) {
            eyre::bail!(
                "{} is not a complete RocksDB v1 index ({reason:#}), and it is beside the MDBX index {}. The MDBX index was not changed. Move {} out of the submodule directory and start again. The node then converts {}.",
                rocks.display(),
                mdbx.display(),
                rocks.display(),
                mdbx.display()
            );
        }
        info!(
            rocks = %rocks.display(),
            mdbx = %mdbx.display(),
            "submodule index is already on RocksDB, retiring the MDBX index"
        );
    } else {
        let migrating = submodule_dir.join(MIGRATING_DIR);
        convert_into(
            &mdbx,
            &migrating,
            submodule_dir,
            config,
            consensus,
            partition_chunks,
        )?;
        rename_durable(&migrating, &rocks)?;
    }
    rename_durable(&mdbx, &migrated)?;
    info!(
        rocks = %rocks.display(),
        kept = %migrated.display(),
        "submodule index moved to RocksDB. The MDBX index is kept and not read"
    );
    Ok(())
}

/// `rocks/` is the live index only when no conversion is unfinished in it
/// (`CONVERTING`), `SCHEMA` holds the v1 marker, and RocksDB opens it
/// read-only (`CURRENT`, manifest, column families). Anything less keeps
/// `db/` as the live index.
fn check_live_rocks_index(rocks: &Path) -> eyre::Result<()> {
    if rocks.join(CONVERTING_FILE).exists() {
        eyre::bail!("{CONVERTING_FILE} is present");
    }
    let marker = fs::read(rocks.join(ROCKS_SCHEMA_FILE)).unwrap_or_default();
    if marker != SUBMODULE_SCHEMA_ROCKS_V1.as_bytes() {
        eyre::bail!("{ROCKS_SCHEMA_FILE} does not hold the RocksDB v1 marker");
    }
    check_rocks_index_opens(rocks)
}

fn convert_into(
    mdbx: &Path,
    target: &Path,
    submodule_dir: &Path,
    config: &Config,
    consensus: &DatabaseEnv,
    partition_chunks: u64,
) -> eyre::Result<()> {
    if target.exists() {
        // Only this module writes the directory. `db/` is unchanged, so the
        // copy is redone from the start.
        warn!(
            path = %target.display(),
            "removing a RocksDB copy left by an interrupted start"
        );
        fs::remove_dir_all(target).wrap_err_with(|| format!("remove {}", target.display()))?;
    }
    let ledger_slot = ledger_slot(&submodule_dir.join(PACKING_PARAMS_FILE_NAME))?;
    let mut options = ConvertOptions::new(
        submodule_db_args(&config.node_config.database)?,
        config.data_tree_geometry(),
    );
    options.consensus_db = Some(config.node_config.irys_consensus_data_dir());
    options.chunks_per_partition = partition_chunks;
    info!(
        mdbx = %mdbx.display(),
        target = %target.display(),
        "converting the submodule index to RocksDB. This runs once per submodule"
    );
    let (stats, report) =
        convert_mdbx_to_rocks_at_startup(mdbx, target, &options, consensus, ledger_slot)
            .wrap_err_with(|| {
                format!(
                    "convert the submodule index {} to RocksDB. {} is unchanged",
                    mdbx.display(),
                    mdbx.display()
                )
            })?;
    info!(
        mdbx = %mdbx.display(),
        intervals = stats.intervals,
        data_paths = stats.data_paths,
        data_roots = stats.data_roots,
        pending = stats.pending,
        offsets_verified = report.offsets_checked,
        elapsed_ms = stats.elapsed_ms,
        "converted and verified the submodule index"
    );
    Ok(())
}

/// Ledger id and slot of the module's data, from `packing_params.toml`.
/// `None` when the file is absent or names no ledger.
fn ledger_slot(params_path: &Path) -> eyre::Result<Option<(u32, u64)>> {
    if !params_path.is_file() {
        return Ok(None);
    }
    let params = PackingParams::from_toml(params_path)?;
    match (params.ledger, params.slot) {
        (Some(ledger), Some(slot)) => Ok(Some((ledger, u64::try_from(slot)?))),
        (None, _) => Ok(None),
        (Some(ledger), None) => eyre::bail!(
            "{} names ledger {ledger} but no slot",
            params_path.display()
        ),
    }
}

/// Rename, then sync the parent so the new name survives a power loss.
fn rename_durable(from: &Path, to: &Path) -> eyre::Result<()> {
    fs::rename(from, to)
        .wrap_err_with(|| format!("rename {} to {}", from.display(), to.display()))?;
    let parent = to
        .parent()
        .ok_or_else(|| eyre::eyre!("{} has no parent directory", to.display()))?;
    fs::File::open(parent)
        .and_then(|dir| dir.sync_all())
        .wrap_err_with(|| format!("sync directory {}", parent.display()))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use irys_database::db::IrysDatabaseExt as _;
    use irys_database::reth_db::{Database as _, DatabaseEnv};
    use irys_database::submodule::tables::{
        Metadata, SubmoduleTables, TxPathInterval, TxPathIntervalByStart,
    };
    use irys_database::submodule::{SubmoduleMigrationOptions, open_submodule_index};
    use irys_database::tables::ConsensusTables;
    use irys_database::{
        consensus_db_args, open_or_create_db, set_database_schema_version, submodule_db_args,
    };
    use irys_domain::{PACKING_PARAMS_FILE_NAME, PackingParams};
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::{Config, DatabaseVersion, H256, NodeConfig, PartitionChunkOffset};
    use reth_db::transaction::{DbTx as _, DbTxMut as _};

    /// Slot width the tests pass as the active partition width.
    const CHUNKS: u64 = 10;

    use super::{
        MDBX_INDEX_DIR, MIGRATED_MDBX_DIR, MIGRATING_DIR, ROCKS_INDEX_DIR, ROCKS_SCHEMA_FILE,
        migrate_submodule_index,
    };

    pub(crate) fn config(dir: &Path) -> Config {
        let mut node = NodeConfig::testing();
        node.base_directory = dir.to_path_buf();
        Config::new_with_random_peer_id(node)
    }

    /// Empty consensus database stamped with the current schema, as the node
    /// leaves it after its version check. Its block index is empty.
    pub(crate) fn consensus(config: &Config) -> eyre::Result<Arc<DatabaseEnv>> {
        let env = open_or_create_db(
            config.node_config.irys_consensus_data_dir(),
            ConsensusTables::ALL,
            consensus_db_args(&config.node_config.database)?,
        )?;
        env.update_eyre(|tx| Ok(set_database_schema_version(tx, DatabaseVersion::V4)?))?;
        Ok(Arc::new(env))
    }

    /// Schema v2 MDBX index with no placement.
    pub(crate) fn seed_empty_mdbx(config: &Config, submodule: &Path) -> eyre::Result<()> {
        let dir = submodule.join(MDBX_INDEX_DIR);
        std::fs::create_dir_all(&dir)?;
        let (env, _) = open_submodule_index(
            &dir,
            submodule_db_args(&config.node_config.database)?,
            SubmoduleMigrationOptions::default(),
        )?;
        drop(env);
        Ok(())
    }

    /// Schema v2 MDBX index with one placement interval.
    pub(crate) fn seed_placed_mdbx(config: &Config, submodule: &Path) -> eyre::Result<()> {
        seed_empty_mdbx(config, submodule)?;
        let env = open_or_create_db(
            submodule.join(MDBX_INDEX_DIR),
            SubmoduleTables::ALL,
            submodule_db_args(&config.node_config.database)?,
        )?;
        let tx = env.tx_mut()?;
        tx.put::<TxPathIntervalByStart>(
            PartitionChunkOffset(0),
            TxPathInterval {
                end: PartitionChunkOffset(3),
                tx_path_hash: H256::repeat_byte(9),
            },
        )?;
        tx.commit()?;
        Ok(())
    }

    /// MDBX index whose schema row belongs to no known release.
    pub(crate) fn seed_foreign_mdbx(config: &Config, submodule: &Path) -> eyre::Result<()> {
        let dir = submodule.join(MDBX_INDEX_DIR);
        std::fs::create_dir_all(&dir)?;
        let env = open_or_create_db(
            &dir,
            SubmoduleTables::ALL,
            submodule_db_args(&config.node_config.database)?,
        )?;
        let tx = env.tx_mut()?;
        tx.put::<Metadata>(
            irys_database::metadata::MetadataKey::DBSchemaVersion,
            b"not-this-binary\n".to_vec(),
        )?;
        tx.commit()?;
        Ok(())
    }

    fn write_ledger_params(config: &Config, submodule: &Path, slot: usize) -> eyre::Result<()> {
        PackingParams {
            packing_address: config.node_config.miner_address(),
            ledger: Some(0),
            slot: Some(slot),
            ..Default::default()
        }
        .write_to_disk(&submodule.join(PACKING_PARAMS_FILE_NAME))
    }

    pub(crate) fn assert_moved(submodule: &Path) {
        assert!(
            submodule
                .join(ROCKS_INDEX_DIR)
                .join(ROCKS_SCHEMA_FILE)
                .is_file(),
            "{} has no completed rocks/",
            submodule.display()
        );
        assert!(
            submodule.join(MIGRATED_MDBX_DIR).join("mdbx.dat").is_file(),
            "{} kept no db.migrated/",
            submodule.display()
        );
        assert!(!submodule.join(MDBX_INDEX_DIR).exists());
        assert!(!submodule.join(MIGRATING_DIR).exists());
    }

    /// `db/` is unchanged and no completed `rocks/` exists.
    fn assert_untouched(submodule: &Path) {
        assert!(submodule.join(MDBX_INDEX_DIR).join("mdbx.dat").is_file());
        assert!(!submodule.join(ROCKS_INDEX_DIR).exists());
        assert!(!submodule.join(MIGRATED_MDBX_DIR).exists());
    }

    struct Setup {
        _tmp: irys_testing_utils::utils::tempfile::TempDir,
        config: Config,
        consensus: Arc<DatabaseEnv>,
        submodule: PathBuf,
    }

    fn setup(prefix: &str) -> eyre::Result<Setup> {
        let tmp = TempDirBuilder::new().prefix(prefix).build();
        let config = config(tmp.path());
        let consensus = consensus(&config)?;
        let submodule = tmp.path().join("sub");
        std::fs::create_dir_all(&submodule)?;
        Ok(Setup {
            _tmp: tmp,
            config,
            consensus,
            submodule,
        })
    }

    #[test]
    fn no_mdbx_index_is_a_no_op() -> eyre::Result<()> {
        let s = setup("idx-move-none")?;
        migrate_submodule_index(&s.submodule, &s.config, &s.consensus, CHUNKS)?;
        assert_eq!(std::fs::read_dir(&s.submodule)?.count(), 0);
        Ok(())
    }

    #[test]
    fn an_interrupted_copy_is_redone() -> eyre::Result<()> {
        let s = setup("idx-move-redo")?;
        seed_empty_mdbx(&s.config, &s.submodule)?;
        let leftover = s.submodule.join(MIGRATING_DIR);
        std::fs::create_dir_all(&leftover)?;
        std::fs::write(leftover.join("CONVERTING"), b"")?;
        std::fs::write(leftover.join("000001.sst"), b"partial")?;
        migrate_submodule_index(&s.submodule, &s.config, &s.consensus, CHUNKS)?;
        assert_moved(&s.submodule);
        assert!(
            !s.submodule
                .join(ROCKS_INDEX_DIR)
                .join("000001.sst")
                .exists()
        );
        Ok(())
    }

    /// The state after the rocks/ rename, or after a manual conversion:
    /// rocks/ is live, so only db/ is retired. rocks/ is not rewritten.
    #[test]
    fn a_complete_rocks_index_only_retires_db() -> eyre::Result<()> {
        let s = setup("idx-move-retire")?;
        seed_empty_mdbx(&s.config, &s.submodule)?;
        migrate_submodule_index(&s.submodule, &s.config, &s.consensus, CHUNKS)?;
        let rocks = s.submodule.join(ROCKS_INDEX_DIR);
        std::fs::write(rocks.join("live-marker"), b"")?;
        std::fs::rename(
            s.submodule.join(MIGRATED_MDBX_DIR),
            s.submodule.join(MDBX_INDEX_DIR),
        )?;
        migrate_submodule_index(&s.submodule, &s.config, &s.consensus, CHUNKS)?;
        assert_moved(&s.submodule);
        assert!(rocks.join("live-marker").is_file(), "rocks/ was replaced");
        Ok(())
    }

    #[test]
    fn an_existing_db_migrated_is_refused() -> eyre::Result<()> {
        let s = setup("idx-move-taken")?;
        seed_empty_mdbx(&s.config, &s.submodule)?;
        std::fs::create_dir_all(s.submodule.join(MIGRATED_MDBX_DIR))?;
        let err =
            migrate_submodule_index(&s.submodule, &s.config, &s.consensus, CHUNKS).unwrap_err();
        assert!(format!("{err:#}").contains("that path exists"), "{err:#}");
        assert!(s.submodule.join(MDBX_INDEX_DIR).join("mdbx.dat").is_file());
        assert!(!s.submodule.join(ROCKS_INDEX_DIR).exists());
        Ok(())
    }

    #[test]
    fn an_incomplete_rocks_index_beside_db_is_refused() -> eyre::Result<()> {
        let s = setup("idx-move-incomplete")?;
        seed_empty_mdbx(&s.config, &s.submodule)?;
        let rocks = s.submodule.join(ROCKS_INDEX_DIR);
        std::fs::create_dir_all(&rocks)?;
        std::fs::write(rocks.join("CONVERTING"), b"")?;
        let err =
            migrate_submodule_index(&s.submodule, &s.config, &s.consensus, CHUNKS).unwrap_err();
        assert!(
            format!("{err:#}").contains("not a complete RocksDB v1 index"),
            "{err:#}"
        );
        assert!(s.submodule.join(MDBX_INDEX_DIR).join("mdbx.dat").is_file());
        assert!(!s.submodule.join(MIGRATED_MDBX_DIR).exists());
        Ok(())
    }

    /// `rocks/` beside `db/` is trusted only when RocksDB opens it. A marker,
    /// or a marker plus `CURRENT`, is not enough. Otherwise `db/` stays.
    #[rstest::rstest]
    #[case::marker_without_current(b"irys-submodule-index rocksdb 1\n".as_slice(), false)]
    #[case::foreign_marker_with_current(b"something else\n".as_slice(), true)]
    #[case::marker_with_a_current_naming_no_manifest(
        b"irys-submodule-index rocksdb 1\n".as_slice(),
        true
    )]
    fn a_rocks_index_that_cannot_open_keeps_db(
        #[case] marker: &[u8],
        #[case] with_current: bool,
    ) -> eyre::Result<()> {
        let s = setup("idx-move-unopenable")?;
        seed_empty_mdbx(&s.config, &s.submodule)?;
        let rocks = s.submodule.join(ROCKS_INDEX_DIR);
        std::fs::create_dir_all(&rocks)?;
        std::fs::write(rocks.join(ROCKS_SCHEMA_FILE), marker)?;
        if with_current {
            std::fs::write(rocks.join("CURRENT"), b"MANIFEST-000001\n")?;
        }
        let err =
            migrate_submodule_index(&s.submodule, &s.config, &s.consensus, CHUNKS).unwrap_err();
        assert!(
            format!("{err:#}").contains("not a complete RocksDB v1 index"),
            "{err:#}"
        );
        assert!(s.submodule.join(MDBX_INDEX_DIR).join("mdbx.dat").is_file());
        assert!(!s.submodule.join(MIGRATED_MDBX_DIR).exists());
        Ok(())
    }

    /// A placement needs a ledger to derive its tx path from.
    #[test]
    fn a_placement_without_a_ledger_is_refused() -> eyre::Result<()> {
        let s = setup("idx-move-no-ledger")?;
        seed_placed_mdbx(&s.config, &s.submodule)?;
        let err =
            migrate_submodule_index(&s.submodule, &s.config, &s.consensus, CHUNKS).unwrap_err();
        assert!(
            format!("{err:#}").contains("names no data ledger"),
            "{err:#}"
        );
        assert_untouched(&s.submodule);
        Ok(())
    }

    /// A placement the consensus database does not cover fails the start.
    #[test]
    fn an_uncovered_placement_is_refused() -> eyre::Result<()> {
        let s = setup("idx-move-uncovered")?;
        seed_placed_mdbx(&s.config, &s.submodule)?;
        write_ledger_params(&s.config, &s.submodule, 0)?;
        let err =
            migrate_submodule_index(&s.submodule, &s.config, &s.consensus, CHUNKS).unwrap_err();
        assert!(
            format!("{err:#}").contains("block index is empty"),
            "{err:#}"
        );
        assert!(s.submodule.join(MDBX_INDEX_DIR).join("mdbx.dat").is_file());
        assert!(!s.submodule.join(ROCKS_INDEX_DIR).exists());
        assert!(!s.submodule.join(MIGRATED_MDBX_DIR).exists());
        Ok(())
    }

    /// The ledger offset of a placement uses the width the caller passes,
    /// not the genesis `num_chunks_in_partition`: a partition resize changes
    /// the slot width. Slot 1 makes a wrong width visible.
    #[test]
    fn the_ledger_offset_uses_the_given_partition_width() -> eyre::Result<()> {
        use irys_types::IrysBlockHeader;

        let s = setup("idx-move-width")?;
        s.consensus.update_eyre(|tx| {
            irys_database::insert_block_index_items_for_block(
                tx,
                &IrysBlockHeader::new_mock_header(),
            )
        })?;
        seed_placed_mdbx(&s.config, &s.submodule)?;
        write_ledger_params(&s.config, &s.submodule, 1)?;
        let width = s.config.consensus.num_chunks_in_partition + 3;
        let err =
            migrate_submodule_index(&s.submodule, &s.config, &s.consensus, width).unwrap_err();
        // The placement ends at partition offset 3 (see `seed_placed_mdbx`).
        let expected = format!("partition offset 3 is ledger 0 offset {}", width + 3);
        assert!(format!("{err:#}").contains(&expected), "{err:#}");
        assert_untouched(&s.submodule);
        Ok(())
    }
}
