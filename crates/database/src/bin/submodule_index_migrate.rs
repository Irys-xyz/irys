//! Migrate a copy of a schema v1 submodule `db/` and print the counts.
//!
//! The directory name must contain `copy`. A symlink whose target name does
//! not contain `copy` is refused. This tool never opens a live `db/`.
//!
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
//! `--verify-against` opens the pristine v1 copy read-only and compares the
//! reads the node makes. The pristine copy is not written. A mismatch exits 1.
//!
//! Open uses the production submodule arguments: durable sync and a 2 TiB
//! geometry cap.

use std::path::{Path, PathBuf};

use irys_database::reth_db::Database as _;
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
    require_submodule_index_copy(&tool.copy)?;
    if !tool.copy.is_dir() {
        eyre::bail!("{} is not a directory", tool.copy.display());
    }
    if let Some(pristine) = &tool.verify_against {
        require_submodule_index_copy(pristine)?;
        if !pristine.is_dir() {
            eyre::bail!("{} is not a directory", pristine.display());
        }
        if paths_are_same(&tool.copy, pristine) {
            eyre::bail!("pristine v1 copy and migrated copy are the same path");
        }
    }

    let open_args = submodule_db_args(&DatabaseConfig::default())?;
    let (env, stats) = open_submodule_index(
        &tool.copy,
        open_args.clone(),
        SubmoduleMigrationOptions::default(),
    )?;
    print!("{stats}");

    let Some(pristine) = &tool.verify_against else {
        return Ok(());
    };
    eprintln!("verifying reads against {}", pristine.display());
    let v1 = open_pristine_v1_read_only(pristine, open_args)?;
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
