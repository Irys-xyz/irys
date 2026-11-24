//! Defect catalogue: a curated list of defect classes found and fixed before, and a log of each
//! instance. See `docs/99-reference/defect-catalogue/README.md`.

pub mod catalogue;
pub mod scan;
pub mod trend;

use std::path::PathBuf;

/// The catalogue directory in this checkout: the workspace root is the parent of the xtask crate.
pub fn default_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask crate has a parent directory")
        .join(catalogue::CATALOGUE_DIR)
}
