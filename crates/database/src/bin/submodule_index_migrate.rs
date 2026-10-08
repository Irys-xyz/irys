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
//! cargo run -p irys-database --bin submodule-index-migrate -- /tmp/submodule-db.copy
//! ```
//!
//! Open uses the production submodule arguments: durable sync and a 2 TiB
//! geometry cap.

use std::path::PathBuf;

use irys_database::submodule::{
    SubmoduleMigrationOptions, open_submodule_index, require_submodule_index_copy,
};
use irys_database::submodule_db_args;
use irys_types::config::DatabaseConfig;

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> eyre::Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!(
            "usage: submodule-index-migrate <copy-dir>\n\
             the directory name must contain \"copy\". the tool does not open a live db/."
        );
        std::process::exit(2);
    };
    if path == "-h" || path == "--help" {
        eprintln!(
            "usage: submodule-index-migrate <copy-dir>\n\
             the directory name must contain \"copy\". the tool does not open a live db/."
        );
        return Ok(());
    }
    if args.next().is_some() {
        eyre::bail!("unexpected extra argument");
    }
    let path = PathBuf::from(path);
    require_submodule_index_copy(&path)?;
    if !path.is_dir() {
        eyre::bail!("{} is not a directory", path.display());
    }
    let open_args = submodule_db_args(&DatabaseConfig::default())?;
    let (_env, stats) =
        open_submodule_index(&path, open_args, SubmoduleMigrationOptions::default())?;
    print!("{stats}");
    Ok(())
}
