use irys_database::open_or_create_db;
use irys_database::tables::{CacheTables, ConsensusTables};
use reth_db::DatabaseEnv;
use reth_db::mdbx::DatabaseArguments;
use std::path::Path;

// Only the test-gated convenience fns below need these; keep them out of
// production builds where those fns are `cfg`-compiled away (avoids unused-import).
#[cfg(any(test, feature = "test-utils"))]
use irys_database::IrysDatabaseArgs as _;
#[cfg(any(test, feature = "test-utils"))]
use irys_types::DbSyncMode;

/// Test-oriented convenience for the sync-mode-only case. It caps the geometry to
/// [`irys_types::TEST_DB_GEOMETRY_MAX_SIZE`] so the many concurrent unit-test
/// consensus DBs don't each reserve reth's large default map and exhaust the
/// process's virtual address space. **Production** opens the consensus DB via
/// [`irys_database::consensus_db_args`] + [`open_or_create_irys_consensus_data_db_with_args`]
/// (the node in `chain.rs` and the CLI both do), which honors the configured
/// `geometry_max_size`.
///
/// Gated to test builds (`cfg(test)`) and the `test-utils` feature so a production
/// call site — which would silently inherit the tiny test geometry cap — is a
/// compile error rather than a latent consensus-DB-sizing bug.
#[cfg(any(test, feature = "test-utils"))]
pub fn open_or_create_irys_consensus_data_db(
    path: impl AsRef<Path>,
    sync_mode: DbSyncMode,
) -> eyre::Result<DatabaseEnv> {
    open_or_create_irys_consensus_data_db_with_args(
        path,
        DatabaseArguments::irys_default(sync_mode)?
            .with_geometry_max_size(Some(irys_types::TEST_DB_GEOMETRY_MAX_SIZE)),
    )
}

/// Like [`open_or_create_irys_consensus_data_db`] but the caller supplies fully-
/// built [`DatabaseArguments`] (sync mode, geometry, etc.) — typically via
/// [`irys_database::consensus_db_args`] — so DB tuning lives in one place instead
/// of being threaded through as individual parameters.
pub fn open_or_create_irys_consensus_data_db_with_args(
    path: impl AsRef<Path>,
    args: DatabaseArguments,
) -> eyre::Result<DatabaseEnv> {
    open_or_create_db(path, ConsensusTables::ALL, args)
}

/// Test-oriented convenience for opening the cache-side MDBX environment, with the
/// same [`irys_types::TEST_DB_GEOMETRY_MAX_SIZE`] cap as
/// [`open_or_create_irys_consensus_data_db`]. **Production** opens the cache DB via
/// [`irys_database::cache_db_args`] + [`open_or_create_irys_cache_data_db_with_args`],
/// which honors the configured `cache_sync_mode` and `geometry_max_size`.
///
/// Uses the wider growth/shrink geometry from `DatabaseArguments::irys_cache`
/// because the cache tables churn in large batches during pruning.
#[cfg(any(test, feature = "test-utils"))]
pub fn open_or_create_irys_cache_data_db(
    path: impl AsRef<Path>,
    sync_mode: DbSyncMode,
) -> eyre::Result<DatabaseEnv> {
    open_or_create_irys_cache_data_db_with_args(
        path,
        DatabaseArguments::irys_cache(sync_mode)?
            .with_geometry_max_size(Some(irys_types::TEST_DB_GEOMETRY_MAX_SIZE)),
    )
}

/// Like [`open_or_create_irys_cache_data_db`] but the caller supplies fully-built
/// [`DatabaseArguments`] — typically via [`irys_database::cache_db_args`].
pub fn open_or_create_irys_cache_data_db_with_args(
    path: impl AsRef<Path>,
    args: DatabaseArguments,
) -> eyre::Result<DatabaseEnv> {
    open_or_create_db(path, CacheTables::ALL, args)
}
