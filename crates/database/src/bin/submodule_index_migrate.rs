// MDBX-SUBMODULE-INDEX: remove this binary with the v1 to v2 MDBX migration.
//! Migrate a copy of a schema v1 submodule `db/` and print the counts.
//!
//! The directory name must contain `copy`. This tool never opens a live `db/`.
//! Stop the node before copying. A copy taken while the node is writing can
//! be torn.
//!
//! ```text
//! cp -a path/to/submodule/db /tmp/submodule-db.copy
//! cp -a /tmp/submodule-db.copy /tmp/submodule-db.migrated-copy
//! cargo run -p irys-database --bin submodule-index-migrate -- /tmp/submodule-db.migrated-copy
//! cargo run -p irys-database --bin submodule-index-migrate -- \
//!     --verify-against /tmp/submodule-db.copy /tmp/submodule-db.migrated-copy
//! ```
//!
//! `--verify-against` compares node reads against a pristine v1 copy that has
//! no schema row. That copy is not written. A mismatch exits 1.
//!
//! The copy stops at the schema v2 layout, which keys data by the data root.
//! Open then refuses a copy that has data rows: this build keys data by the
//! data commitment, so the network must reset that data.

use std::path::{Path, PathBuf};

use irys_database::metadata::MetadataKey;
use irys_database::reth_db::Database as _;
use irys_database::reth_db::mdbx::DatabaseArguments;
use irys_database::reth_db::transaction::DbTx as _;
use irys_database::submodule::tables::Metadata;
use irys_database::submodule::{
    SubmoduleMigrationOptions, open_pristine_v1_read_only, open_submodule_index,
    require_submodule_index_copy, verify_submodule_read_equivalence,
};
use irys_database::submodule_db_args;
use irys_types::config::DatabaseConfig;

const USAGE: &str = "\
usage: submodule-index-migrate <copy-dir>
       submodule-index-migrate --verify-against <pristine-v1-copy> <migrated-copy>
the directory name must contain \"copy\". the tool does not open a live db/.
--verify-against opens the pristine v1 copy read-only and compares node reads.
the pristine copy must have no schema row. the tool checks that before it migrates.
the pristine copy is not written. a mismatch exits 1.";

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> eyre::Result<()> {
    let Some(tool) = parse_args()? else {
        eprintln!("{USAGE}");
        return Ok(());
    };
    let open_args = submodule_db_args(&DatabaseConfig::default())?;
    execute(&tool, open_args)
}

fn execute(tool: &ToolArgs, open_args: DatabaseArguments) -> eyre::Result<()> {
    require_submodule_index_copy(&tool.copy)?;
    if !tool.copy.is_dir() {
        eyre::bail!("{} is not a directory", tool.copy.display());
    }

    // Open the pristine copy before migrating. A swapped path fails here,
    // while the positional directory is still schema v1.
    let pristine = if let Some(path) = &tool.verify_against {
        require_submodule_index_copy(path)?;
        if !path.is_dir() {
            eyre::bail!("{} is not a directory", path.display());
        }
        if paths_are_same(&tool.copy, path) {
            eyre::bail!("pristine v1 copy and migrated copy are the same path");
        }
        let env = open_pristine_v1_read_only(path, open_args.clone())?;
        require_absent_schema_row(&env, path, &tool.copy)?;
        Some((path.clone(), env))
    } else {
        None
    };

    let (env, stats) =
        open_submodule_index(&tool.copy, open_args, SubmoduleMigrationOptions::default())?;
    print!("{stats}");

    let Some((path, v1)) = pristine else {
        return Ok(());
    };
    eprintln!("verifying reads against {}", path.display());
    let report = {
        let v1_tx = v1.tx()?;
        let v2_tx = env.tx()?;
        verify_submodule_read_equivalence(&v1_tx, &v2_tx)?
    };
    print!("{report}");
    if report.has_mismatch() {
        eyre::bail!("read equivalence mismatch");
    }
    Ok(())
}

fn require_absent_schema_row(
    env: &impl irys_database::reth_db::Database,
    pristine: &Path,
    copy: &Path,
) -> eyre::Result<()> {
    let tx = env.tx()?;
    if tx.get::<Metadata>(MetadataKey::DBSchemaVersion)?.is_some() {
        eyre::bail!(
            "pristine v1 copy {} already has a schema row. Refusing to migrate {}. The pristine copy must still be schema v1.",
            pristine.display(),
            copy.display()
        );
    }
    Ok(())
}

struct ToolArgs {
    copy: PathBuf,
    verify_against: Option<PathBuf>,
}

fn parse_args() -> eyre::Result<Option<ToolArgs>> {
    let mut positional = Vec::new();
    let mut verify_against = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--verify-against" => {
                let Some(path) = args.next() else {
                    eprintln!("{USAGE}");
                    std::process::exit(2);
                };
                if verify_against.is_some() {
                    eprintln!("{USAGE}");
                    std::process::exit(2);
                }
                verify_against = Some(PathBuf::from(path));
            }
            other if other.starts_with('-') => {
                eprintln!("{USAGE}");
                std::process::exit(2);
            }
            other => positional.push(PathBuf::from(other)),
        }
    }
    if positional.len() != 1 {
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    Ok(Some(ToolArgs {
        copy: positional.remove(0),
        verify_against,
    }))
}

fn paths_are_same(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use irys_database::metadata::MetadataKey;
    use irys_database::reth_db::Database as _;
    use irys_database::reth_db::mdbx::DatabaseArguments;
    use irys_database::reth_db::transaction::{DbTx as _, DbTxMut as _};
    use irys_database::submodule::tables::{
        ChunkPathHashes, ChunkPathHashesByOffset, Metadata, SubmoduleTables,
    };
    use irys_database::submodule::{SubmoduleMigrationOptions, open_submodule_index};
    use irys_database::{IrysDatabaseArgs as _, open_or_create_db};
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::{H256, PartitionChunkOffset};

    use super::execute;

    fn testing_args() -> eyre::Result<DatabaseArguments> {
        DatabaseArguments::irys_testing()
    }

    fn write_unmigrated(path: &Path) -> eyre::Result<()> {
        let db = open_or_create_db(path, SubmoduleTables::ALL, testing_args()?)?;
        let tx = db.tx_mut()?;
        tx.put::<ChunkPathHashesByOffset>(
            PartitionChunkOffset(0),
            ChunkPathHashes {
                data_path_hash: None,
                tx_path_hash: Some(H256::repeat_byte(7)),
            },
        )?;
        tx.commit()?;
        Ok(())
    }

    fn schema_and_row(path: &Path) -> eyre::Result<(Option<Vec<u8>>, Option<ChunkPathHashes>)> {
        let env = open_or_create_db(path, SubmoduleTables::ALL, testing_args()?)?;
        let tx = env.tx()?;
        let schema = tx.get::<Metadata>(MetadataKey::DBSchemaVersion)?;
        let row = tx.get::<ChunkPathHashesByOffset>(PartitionChunkOffset(0))?;
        Ok((schema, row))
    }

    /// Swapping the two paths must not migrate the positional v1 copy.
    #[test]
    fn verify_against_a_schema_row_does_not_migrate_the_positional_copy() -> eyre::Result<()> {
        let parent = TempDirBuilder::new().prefix("sm-migrate-swapped").build();
        let positional = parent.path().join("oracle.copy");
        let already = parent.path().join("already.copy");
        std::fs::create_dir(&positional)?;
        std::fs::create_dir(&already)?;
        write_unmigrated(&positional)?;
        open_submodule_index(
            &already,
            testing_args()?,
            SubmoduleMigrationOptions::default(),
        )?;

        let err = execute(
            &super::ToolArgs {
                copy: positional.clone(),
                verify_against: Some(already),
            },
            testing_args()?,
        )
        .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("schema row"), "{text}");
        let (schema, row) = schema_and_row(&positional)?;
        assert_eq!(schema, None);
        assert_eq!(
            row,
            Some(ChunkPathHashes {
                data_path_hash: None,
                tx_path_hash: Some(H256::repeat_byte(7)),
            })
        );
        Ok(())
    }

    /// The copy migrates to the schema v2 layout, then open refuses its data
    /// rows: they are keyed by the data root. The pristine copy is not written.
    #[test]
    fn verify_against_pristine_v1_migrates_only_the_positional_copy() -> eyre::Result<()> {
        let parent = TempDirBuilder::new().prefix("sm-migrate-order").build();
        let pristine = parent.path().join("pristine.copy");
        let migrated = parent.path().join("migrated.copy");
        std::fs::create_dir(&pristine)?;
        std::fs::create_dir(&migrated)?;
        write_unmigrated(&pristine)?;
        write_unmigrated(&migrated)?;

        let err = execute(
            &super::ToolArgs {
                copy: migrated.clone(),
                verify_against: Some(pristine.clone()),
            },
            testing_args()?,
        )
        .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("network must reset"), "{text}");

        let (schema, row) = schema_and_row(&pristine)?;
        assert_eq!(schema, None);
        assert_eq!(
            row,
            Some(ChunkPathHashes {
                data_path_hash: None,
                tx_path_hash: Some(H256::repeat_byte(7)),
            })
        );
        let (schema, _) = schema_and_row(&migrated)?;
        assert!(schema.is_some());
        Ok(())
    }
}
