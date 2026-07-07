use std::ops::RangeBounds;
use std::path::Path;

use irys_types::DbSyncMode;

use crate::db_cache::{
    CachedChunk, CachedChunkIndexEntry, CachedChunkIndexMetadata, CachedDataRoot,
};
use crate::tables::{
    CachedChunks, CachedChunksIndex, CachedDataRoots, CompactCachedIngressProof,
    CompactLedgerIndexItem, IngressProofs, IrysBlockHeaders, IrysBlockIndexItems, IrysCommitments,
    IrysDataTxHeaders, IrysPoAChunks, Metadata, MigratedBlockHashes, PeerListItems,
};

use crate::db::IrysDatabaseExt as _;
use crate::metadata::MetadataKey;
use crate::reth_ext::IrysRethDatabaseEnvMetricsExt as _;
use irys_types::ingress::CachedIngressProof;
use irys_types::irys::IrysSigner;
use irys_types::{
    BlockHash, BlockHeight, BlockIndexItem, ChunkPathHash, CommitmentTransaction, DataLedger,
    DataRoot, DataTransactionHeader, DataTransactionMetadata, DatabaseProvider, DatabaseVersion,
    H256, IngressProof, IrysAddress, IrysBlockHeader, IrysPeerId, IrysTransactionId,
    LedgerIndexItem, MEGABYTE, PeerListItem, TxChunkOffset, UnixTimestamp, UnpackedChunk,
};
use reth_db::TableSet;
use reth_db::cursor::DbDupCursorRO as _;
use reth_db::mdbx::init_db_for;
use reth_db::table::{Table, TableInfo};
use reth_db::transaction::DbTx;
use reth_db::transaction::DbTxMut;
use reth_db::{
    ClientVersion, DatabaseEnv, DatabaseError,
    cursor::*,
    mdbx::{DatabaseArguments, MaxReadTransactionDuration},
};
use tracing::{debug, warn};

/// Extension trait adding Irys preset constructors to [`DatabaseArguments`].
pub trait IrysDatabaseArgs {
    /// Default args with the given sync mode.
    ///
    /// Unbounded read transaction duration, 10 MB growth step, 20 MB shrink threshold.
    fn irys_default(sync_mode: DbSyncMode) -> eyre::Result<DatabaseArguments>;

    /// Default args with `UtterlyNoSync` — for tests only.
    fn irys_testing() -> eyre::Result<DatabaseArguments>;

    /// Cache-tuned args: larger growth/shrink thresholds (50 MB / 100 MB).
    fn irys_cache(sync_mode: DbSyncMode) -> eyre::Result<DatabaseArguments>;
}

impl IrysDatabaseArgs for DatabaseArguments {
    fn irys_default(sync_mode: DbSyncMode) -> eyre::Result<DatabaseArguments> {
        Ok(Self::new(ClientVersion::default())
            .with_max_read_transaction_duration(Some(MaxReadTransactionDuration::Unbounded))
            // see https://github.com/isar/libmdbx/blob/0e8cb90d0622076ce8862e5ffbe4f5fcaa579006/mdbx.h#L3608
            .with_growth_step((10 * MEGABYTE).into())
            .with_shrink_threshold((20 * MEGABYTE).try_into()?)
            .with_sync_mode(Some(sync_mode.into())))
    }

    fn irys_testing() -> eyre::Result<DatabaseArguments> {
        // Cap the geometry so each unit-test DB doesn't reserve reth's 8 TiB
        // default map — many concurrent test envs would otherwise exhaust the
        // process's virtual address space (see `TEST_DB_GEOMETRY_MAX_SIZE`).
        Ok(Self::irys_default(DbSyncMode::UtterlyNoSync)?
            .with_geometry_max_size(Some(irys_types::TEST_DB_GEOMETRY_MAX_SIZE)))
    }

    fn irys_cache(sync_mode: DbSyncMode) -> eyre::Result<DatabaseArguments> {
        Ok(Self::new(ClientVersion::default())
            .with_max_read_transaction_duration(Some(MaxReadTransactionDuration::Unbounded))
            // see https://github.com/isar/libmdbx/blob/0e8cb90d0622076ce8862e5ffbe4f5fcaa579006/mdbx.h#L3608
            .with_growth_step((50 * MEGABYTE).into())
            .with_shrink_threshold((100 * MEGABYTE).try_into()?)
            // Cache preset adjusts geometry for larger write batches (bigger growth
            // step / shrink threshold). Durability semantics are determined by the
            // caller-provided sync_mode, not by this preset.
            .with_sync_mode(Some(sync_mode.into())))
    }
}

/// Builds [`DatabaseArguments`] for the **main consensus** database from a
/// [`irys_types::DatabaseConfig`]. Production uses reth's large default geometry;
/// when the config sets `geometry_max_size` (test configs do) that caps both the
/// max DB size and the virtual address-space reservation MDBX makes at open, so
/// many concurrent node databases don't exhaust the process's address space.
pub fn consensus_db_args(
    db_config: &irys_types::DatabaseConfig,
) -> eyre::Result<DatabaseArguments> {
    let mut args = DatabaseArguments::irys_default(db_config.sync_mode)?;
    if let Some(max_size) = db_config.geometry_max_size {
        args = args.with_geometry_max_size(Some(max_size));
    }
    Ok(args)
}

/// Builds [`DatabaseArguments`] for a **storage submodule** database from a
/// [`irys_types::DatabaseConfig`]. Production reserves 2 TiB; `geometry_max_size`
/// (set by test configs) overrides that with a small cap.
pub fn submodule_db_args(
    db_config: &irys_types::DatabaseConfig,
) -> eyre::Result<DatabaseArguments> {
    let max_size = db_config
        .geometry_max_size
        .unwrap_or(2 * irys_types::TERABYTE);
    Ok(
        DatabaseArguments::irys_default(db_config.sync_mode)?
            .with_geometry_max_size(Some(max_size)),
    )
}

/// Opens up an existing database or creates a new one at the specified path. Creates tables if
/// necessary. Read/Write mode.
pub fn open_or_create_db<P: AsRef<Path>, T: TableSet + TableInfo>(
    path: P,
    tables: &[T],
    args: DatabaseArguments,
) -> eyre::Result<DatabaseEnv> {
    // `with_metrics_and_tables` registers per-table counter/histogram handles
    // eagerly. A metrics recorder must be installed first; handles bound while
    // the no-op recorder is active stay no-op for the process lifetime.
    let db = init_db_for::<P, T>(path, args)?.with_metrics_and_tables(tables);

    Ok(db)
}

pub fn open_or_create_cache_db<P: AsRef<Path>, T: TableSet + TableInfo>(
    path: P,
    tables: &[T],
    sync_mode: DbSyncMode,
) -> eyre::Result<DatabaseEnv> {
    open_or_create_db(path, tables, DatabaseArguments::irys_cache(sync_mode)?)
}

/// Inserts a [`IrysBlockHeader`] into [`IrysBlockHeaders`]
#[tracing::instrument(level = "debug", skip_all, fields(block_hash = ?block.block_hash))]
pub fn insert_block_header<T: DbTxMut>(tx: &T, block: &IrysBlockHeader) -> eyre::Result<()> {
    if let Some(chunk) = &block.poa.chunk {
        tx.put::<IrysPoAChunks>(block.block_hash, chunk.clone().into())?;
    } else {
        tracing::error!(block.hash = ?block.block_hash, target = "db::block_header", "poa chunk not present when writing the header");
    };
    let mut block_without_chunk = block.clone();
    block_without_chunk.poa.chunk = None;
    tx.put::<IrysBlockHeaders>(block.block_hash, block_without_chunk.into())?;
    Ok(())
}

/// Gets a [`IrysBlockHeader`] by it's [`BlockHash`]
pub fn block_header_by_hash<T: DbTx>(
    tx: &T,
    block_hash: &BlockHash,
    include_chunk: bool,
    // TODO: we should be typing these Results correctly (DatabaseError instead of eyre::Report)
) -> eyre::Result<Option<IrysBlockHeader>> {
    let mut block = tx
        .get::<IrysBlockHeaders>(*block_hash)?
        .map(IrysBlockHeader::from);

    if include_chunk && let Some(ref mut b) = block {
        b.poa.chunk = tx.get::<IrysPoAChunks>(*block_hash)?.map(Into::into);
        if b.poa.chunk.is_none() && b.height != 0 {
            tracing::error!(block.hash = ?b.block_hash, height = b.height,  target = "db::block_header", "poa chunk not present when reading the header");
        }
    }

    Ok(block)
}

/// Resolves a block hash to its height, verifying canonical membership.
///
/// Looks up the hash in [`IrysBlockHeaders`] to get the height, then cross-checks
/// [`MigratedBlockHashes`] to confirm the canonical block at that height matches
/// the requested hash. Returns `None` if the block is unknown or non-canonical
/// (e.g., an orphan from a resolved fork).
///
/// Both reads happen in the same MDBX read transaction to avoid split-view races
/// during concurrent migration or rollback.
pub fn canonical_block_height_by_hash<T: DbTx>(
    tx: &T,
    block_hash: &BlockHash,
) -> eyre::Result<Option<u64>> {
    let Some(hdr) = tx
        .get::<IrysBlockHeaders>(*block_hash)?
        .map(IrysBlockHeader::from)
    else {
        return Ok(None);
    };

    let canonical_hash = tx.get::<MigratedBlockHashes>(hdr.height)?;
    if canonical_hash == Some(*block_hash) {
        Ok(Some(hdr.height))
    } else {
        Ok(None)
    }
}

/// Returns the canonical block header at `height`, verified against
/// [`MigratedBlockHashes`].
///
/// `Ok(None)` when no canonical block is recorded at `height` (MBH has no row).
/// `Err` when MBH attests a canonical block at `height` but `IrysBlockHeaders`
/// has no entry for that hash — the two tables disagree, a hard inconsistency.
pub fn canonical_header_at_height<T: DbTx>(
    tx: &T,
    height: u64,
) -> eyre::Result<Option<IrysBlockHeader>> {
    let Some(canonical_hash) = tx.get::<MigratedBlockHashes>(height)? else {
        return Ok(None);
    };
    // MBH attested a canonical block at this height; a missing header means the
    // two tables disagree.
    let header = block_header_by_hash(tx, &canonical_hash, false)?.ok_or_else(|| {
        eyre::eyre!(
            "canonical metadata inconsistent: MigratedBlockHashes[{}] = {} but IrysBlockHeaders has no entry for that hash",
            height,
            canonical_hash
        )
    })?;
    if header.height != height {
        eyre::bail!(
            "canonical metadata inconsistent: MigratedBlockHashes[{}] = {} but IrysBlockHeaders[{}].height = {}",
            height,
            canonical_hash,
            canonical_hash,
            header.height
        );
    }
    Ok(Some(header))
}

/// Inserts a [`DataTransactionHeader`] into [`IrysDataTxHeaders`]
pub fn insert_tx_header<T: DbTxMut>(tx: &T, tx_header: &DataTransactionHeader) -> eyre::Result<()> {
    Ok(tx.put::<IrysDataTxHeaders>(tx_header.id, tx_header.clone().into())?)
}

/// Gets a [`DataTransactionHeader`] by it's [`IrysTransactionId`]
pub fn tx_header_by_txid<T: DbTx>(
    tx: &T,
    txid: &IrysTransactionId,
) -> eyre::Result<Option<DataTransactionHeader>> {
    if let Some(mut header) = tx
        .get::<IrysDataTxHeaders>(*txid)?
        .map(DataTransactionHeader::from)
    {
        // Load metadata from separate table if it exists
        if let Some(metadata) = crate::get_data_tx_metadata(tx, txid)? {
            *header.metadata_mut() = metadata;
        }
        Ok(Some(header))
    } else {
        Ok(None)
    }
}

/// Fork-aware canonical-height lookups
/// ====================================
///
/// `IrysDataTxMetadata` (the non-fork-aware hint table written from
/// whatever block last became the local node's tip at a given height) is
/// not safe to trust on its own when answering "is this tx canonically
/// included on the chain a validator is currently evaluating?".  An
/// orphaned tip can leave a stranded `included_height` / `promoted_height`
/// behind, and Phase-1 scrub only fires on reorg.
///
/// `MigratedBlockHashes` (MBH) records the local node's canonical block hash
/// at each migrated height.  It is *branch-invariant* — attests the same hash
/// from every chain's perspective — ONLY for heights below the reorg floor.
///
/// **Reorg ceiling moved.**  Network-partition / deep-reorg recovery
/// removed the old `validate_reorg_within_migration_depth` gate (now dead): a
/// reorg can rewind and rewrite already-migrated MBH rows up to
/// `block_tree_depth` deep (`recover_from_network_partition` →
/// `truncate_to_height` → `delete_block_index_range`).  So within
/// `(tip - block_tree_depth, tip - block_migration_depth]`, MBH is a
/// LOCAL-canonical view that can change across a reorg and disagree between
/// nodes evaluating competing forks.  Only heights at/below
/// `tip - block_tree_depth` are finalized and safe to key on by existence
/// alone.
///
/// The two helpers in this section return a height from `IrysDataTxMetadata`
/// only if MBH has a row for it ≤ `max_height`.  They return primitive
/// `Option<u64>` (no header mutation) so call sites can't accidentally treat a
/// stranded hint as canonical.  Because the existence check is NOT
/// branch-invariant in the window above, every caller must either (a) only
/// consult these helpers for heights guaranteed below the reorg floor, or
/// (b) treat the result as a hint and confirm membership against the evaluated
/// branch's own ancestry.  Current callers:
///   - `data_txs_are_valid` (`block_validation.rs`) uses the content-based
///     ancestry walk (`get_previous_tx_inclusions`) as its primary path and
///     only falls back to these helpers for inclusions older than
///     `tx_anchor_expiry_depth`.
///   - the NC-0042 expiry fast path
///     (`ledger_expiry::resolve_submit_inclusion`) relies on the config
///     invariant that a ledger slot's lifetime exceeds `block_tree_depth`
///     (enforced in `Config::validate`), so any *expired* tx's inclusion is
///     necessarily below the reorg floor.
///   - `lookup_via_migrated_metadata` (`tx_inclusion.rs`) re-reads the block
///     and re-checks Submit membership before trusting the height.
/// Re-audit all of these together (and the `Searching { Publish }` arm in
/// `data_txs_are_valid`) whenever the reorg ceiling or those windows change.
fn canonical_metadata_height<T: DbTx>(
    tx: &T,
    txid: &IrysTransactionId,
    max_height: u64,
    pick: impl FnOnce(&DataTransactionMetadata) -> Option<u64>,
    field_name: &'static str,
) -> eyre::Result<Option<u64>> {
    let Some(metadata) = crate::get_data_tx_metadata(tx, txid)? else {
        return Ok(None);
    };
    let Some(height) = pick(&metadata) else {
        return Ok(None);
    };
    if height > max_height {
        debug!(
            tx.id = %txid,
            tx.height = height,
            max_height,
            field = field_name,
            "canonical lookup: metadata height exceeds max_height — rejecting hint"
        );
        return Ok(None);
    }
    if tx.get::<MigratedBlockHashes>(height)?.is_none() {
        debug!(
            tx.id = %txid,
            tx.height = height,
            field = field_name,
            "canonical lookup: MigratedBlockHashes has no row for hint — rejecting (stranded write or unmigrated)"
        );
        return Ok(None);
    }
    Ok(Some(height))
}

/// Returns the MBH-verified Submit-ledger inclusion height of `txid`, if
/// any, capped at `max_height`.
///
/// `Some(h)` means: `IrysDataTxMetadata` carries `included_height = h ≤
/// max_height`, AND `MigratedBlockHashes[h]` is `Some` (canonical).  The
/// raw header table is not consulted — Submit confirmation is purely a
/// metadata-vs-MBH question.
///
/// `None` means: tx unknown, metadata missing, hint > `max_height`, OR
/// hint not confirmed by MBH (stranded local-tip write).  Callers cannot
/// distinguish these cases from the return value alone — by design, since
/// they all share the same outcome at every existing call site ("no
/// canonical Submit at or below `max_height`").
pub fn canonical_submit_height<T: DbTx>(
    tx: &T,
    txid: &IrysTransactionId,
    max_height: u64,
) -> eyre::Result<Option<u64>> {
    canonical_metadata_height(
        tx,
        txid,
        max_height,
        |m| m.included_height,
        "included_height",
    )
}

/// Returns the MBH-verified Publish (promotion) height of `txid`, if any,
/// capped at `max_height`.
///
/// `Some(h)` means: `IrysDataTxMetadata` carries `promoted_height = h ≤
/// max_height`, AND `MigratedBlockHashes[h]` is `Some` (canonical).
///
/// `None` is the validator-safe answer for *all* of: tx unknown, no
/// `promoted_height` hint, hint > `max_height`, OR hint not confirmed by
/// MBH.  This is the explicit fix for the stranded-`promoted_height` bug
/// where an orphaned local-tip write left a hint behind that
/// `MigratedBlockHashes` never recorded — previously read as cross-table
/// corruption, now correctly classified as "no canonical promotion."
pub fn canonical_promoted_height<T: DbTx>(
    tx: &T,
    txid: &IrysTransactionId,
    max_height: u64,
) -> eyre::Result<Option<u64>> {
    canonical_metadata_height(
        tx,
        txid,
        max_height,
        |m| m.promoted_height,
        "promoted_height",
    )
}

/// Returns the content-verified commitment inclusion height of `txid`, if any,
/// capped at `max_height`.
///
/// `Some(h)` means: `IrysCommitmentTxMetadata` carries `included_height = h ≤
/// max_height`, `MigratedBlockHashes[h]` is `Some` (canonical), AND the
/// canonical block at `h` actually carries `txid` in its commitment ledger.
///
/// The final content check exists because the metadata row is written at
/// tip-confirmation (depth 0), not at migration: an orphaned local tip can
/// leave an `included_height` hint behind that no `ReorgEvent` ever clears if
/// the node restarts before the orphan is processed (the tree is rebuilt from
/// the block index, which forgets confirmed-but-unmigrated tips). Once ANY
/// winning block later migrates at `h`, the MBH check alone reads that stranded
/// hint as canonical truth. Requiring the canonical header to actually include
/// the tx makes such stranded rows harmless — the invariant is that
/// `Some(h)` proves canonical inclusion at `h`, not merely that a hint exists.
/// Like the data-tx `canonical_submit_height`, this is only branch-invariant
/// BELOW the reorg floor — callers must pass `max_height ≤ tip -
/// block_tree_depth` and cover the reorg window by-hash separately.
///
/// `None` = tx unknown / no `included_height` / hint > `max_height` / hint not
/// confirmed by MBH / canonical block at the hint does not include the tx
/// (stranded write from an orphaned block).
///
/// Returns `Err` on cross-table inconsistency: MBH attests a canonical block at
/// `h` but `IrysBlockHeaders` has no entry for that hash.
pub fn canonical_commitment_included_height<T: DbTx>(
    tx: &T,
    txid: &IrysTransactionId,
    max_height: u64,
) -> eyre::Result<Option<u64>> {
    let Some(metadata) = crate::db_index::get_commitment_tx_metadata(tx, txid)? else {
        return Ok(None);
    };
    let Some(height) = metadata.included_height else {
        return Ok(None);
    };
    if height > max_height {
        return Ok(None);
    }
    let Some(canonical_header) = canonical_header_at_height(tx, height)? else {
        return Ok(None);
    };
    // Content check: a stranded hint from an orphaned tip points at a canonical
    // block that does not actually carry this commitment tx — treat it as no
    // canonical inclusion.
    if !canonical_header.commitment_tx_ids().contains(txid) {
        return Ok(None);
    }
    Ok(Some(height))
}

/// Writes `included_height = 0` for every commitment tx in the genesis block.
///
/// The commitment replay-dedup's finalized lookup
/// ([`canonical_commitment_included_height`]) keys on
/// `IrysCommitmentTxMetadata.included_height`, which is written for ordinary
/// blocks at confirm/migration by `BlockMigrationService`. Genesis
/// first-includes its commitments but is the block-index head, so it never
/// flows through the confirm/migration metadata writers and no row is ever
/// written for its commitment tx ids. Once genesis falls below the reorg floor
/// (`tip - block_tree_depth`), that finalized lookup is the ONLY dedup path
/// that can see a replayed genesis commitment — the by-hash ancestry walk only
/// covers the reorg window — so the row must exist for the dedup to be sound.
///
/// The write is unconditional and trivially idempotent (same value every boot):
/// a genesis commitment tx's only canonical inclusion IS genesis, so
/// `included_height = 0` is always correct; any pre-existing different value
/// would itself be a stray row this corrects. Running it at every boot (not
/// only at fresh genesis init) makes already-initialized data dirs converge on
/// upgrade restart — init-only would leave existing nodes without the row while
/// fresh nodes have it, yielding node-divergent replay verdicts.
///
/// Genesis must already be persisted before this runs. A missing
/// `MigratedBlockHashes[0]` or missing genesis header is therefore a hard
/// invariant violation and returns `Err` (fail loud, node refuses to start)
/// rather than silently skipping.
pub fn backfill_genesis_commitment_included_height(db: &DatabaseProvider) -> eyre::Result<()> {
    db.update_eyre(|tx| {
        let Some(genesis_header) = canonical_header_at_height(tx, 0)? else {
            eyre::bail!("genesis commitment backfill: MigratedBlockHashes has no row at height 0")
        };
        let commitment_ids = genesis_header.commitment_tx_ids();
        if !commitment_ids.is_empty() {
            crate::db_index::batch_set_commitment_tx_included_height(tx, commitment_ids.iter(), 0)?;
        }
        Ok(())
    })
}

/// Inserts a [`CommitmentTransaction`] into [`IrysCommitments`]
pub fn insert_commitment_tx<T: DbTxMut>(
    tx: &T,
    commitment_tx: &CommitmentTransaction,
) -> eyre::Result<()> {
    Ok(tx.put::<IrysCommitments>(commitment_tx.id(), commitment_tx.clone().into())?)
}

/// Gets a [`CommitmentTransaction`] by it's [`IrysTransactionId`]
pub fn commitment_tx_by_txid<T: DbTx>(
    tx: &T,
    txid: &IrysTransactionId,
) -> eyre::Result<Option<CommitmentTransaction>> {
    Ok(tx
        .get::<IrysCommitments>(*txid)?
        .map(CommitmentTransaction::from))
}

/// Confirms the data size of a cached data root entry after validating its rightmost chunk.
///
/// Updates the entry with the proven data size extracted from the last leaf's merkle proof.
/// Returns `Ok(Some(entry))` if updated, `Ok(None)` if entry not found.
pub fn confirm_data_size_for_data_root<T: DbTx + DbTxMut>(
    tx: &T,
    data_root: &H256,
    data_size: u64,
) -> eyre::Result<Option<CachedDataRoot>> {
    // Access the current cached entry from the database
    let result = tx.get::<CachedDataRoots>(*data_root)?;

    if let Some(mut cached_data_root) = result {
        cached_data_root.data_size = data_size;
        cached_data_root.data_size_confirmed = true;
        tx.put::<CachedDataRoots>(*data_root, cached_data_root.clone())?;
        Ok(Some(cached_data_root))
    } else {
        Ok(None)
    }
}

/// Takes a [`DataTransactionHeader`] and caches its `data_root` and tx.id in a
/// cache database table ([`CachedDataRoots`]). Tracks all the tx.ids' that share the same `data_root`.
pub fn cache_data_root<T: DbTx + DbTxMut>(
    tx: &T,
    tx_header: &DataTransactionHeader,
    block_header: Option<&IrysBlockHeader>,
) -> eyre::Result<Option<CachedDataRoot>> {
    let key = tx_header.data_root;

    // Access the current cached entry from the database
    let result = tx.get::<CachedDataRoots>(key)?;

    // Create or update the CachedDataRoot
    let mut cached_data_root = match result {
        Some(existing) => existing,
        None => CachedDataRoot {
            data_size: tx_header.data_size,
            data_size_confirmed: false,
            txid_set: vec![tx_header.id],
            block_set: vec![],
            expiry_height: None,
            cached_at: UnixTimestamp::now()
                .map_err(|e| eyre::eyre!("Failed to get current timestamp: {}", e))?,
        },
    };

    // If the entry exists, update the timestamp and add the txid if necessary
    if !cached_data_root.txid_set.contains(&tx_header.id) {
        cached_data_root.txid_set.push(tx_header.id);
    }

    // If the data_size is not yet confirmed and the tx_headers data_size is larger than the one in the cache, update it
    if cached_data_root.data_size < tx_header.data_size && !cached_data_root.data_size_confirmed {
        cached_data_root.data_size = tx_header.data_size;
    }

    // If a block header is provided, record it in block_set and clear any
    // pre-confirmation expiry.  Authoritative block_set maintenance happens
    // atomically with the tip change in
    // `BlockMigrationService::persist_metadata` (see
    // `update_data_root_block_set` / `remove_data_root_block_set_entry`) —
    // by the time this is called from mempool's `on_block_confirmed`, the
    // hash is already present.  The append is kept as a defensive idempotent
    // write so direct callers of `cache_data_root` (tests, etc.) get
    // consistent state.
    if let Some(block_header) = block_header {
        if !cached_data_root
            .block_set
            .contains(&block_header.block_hash)
        {
            cached_data_root.block_set.push(block_header.block_hash);
        }
        cached_data_root.expiry_height = None;
    }

    // Update the database with the modified or new entry
    tx.put::<CachedDataRoots>(key, cached_data_root.clone())?;

    // Surface anomalous writes: a CDR with `block_set.is_empty()` and
    // `expiry_height.is_none()` has no lifetime bound and will land in the
    // `(None, None)` arm of `prune_data_root_cache` (evicted unless a local
    // ingress proof is present).  Under the authoritative-writer model this
    // should not be produced by the normal flow: confirmed entries get
    // their block_set populated by `BlockMigrationService::persist_metadata`
    // Phase 3, and unconfirmed entries get `expiry_height` set by
    // `cache_data_root_with_expiry`.  Direct callers of `cache_data_root`
    // that bypass both paths (e.g. test fixtures) can produce
    // this state — warn so operators see them.
    if cached_data_root.block_set.is_empty() && cached_data_root.expiry_height.is_none() {
        warn!(
            data_root = %key,
            tx.id = %tx_header.id,
            had_block_header = block_header.is_some(),
            "cached_data_root.anomalous_write: block_set empty and expiry_height None — entry has no lifetime bound"
        );
    }

    Ok(Some(cached_data_root))
}

/// Gets a [`CachedDataRoot`] by it's [`DataRoot`] from [`CachedDataRoots`] .
pub fn cached_data_root_by_data_root<T: DbTx>(
    tx: &T,
    data_root: DataRoot,
) -> eyre::Result<Option<CachedDataRoot>> {
    Ok(tx.get::<CachedDataRoots>(data_root)?)
}

/// Ensures `block_hash` is present in the `block_set` of an existing
/// [`CachedDataRoot`] and clears `expiry_height` (matching `cache_data_root`'s
/// confirmation semantics).  No-op if no entry exists for `data_root` — this
/// helper is intended for migration-time consistency and must not fabricate
/// cache entries for data_roots whose chunks the node never tracked.
///
/// **Append-before-clear ordering** (mirrors the `(empty, None)` warn in
/// `remove_data_root_block_set_entry`): the `block_set.push` happens before
/// `expiry_height` is cleared in the same write tx, so the
/// `(block_set.is_empty(), expiry_height.is_none())` post-condition is
/// unreachable here — the new block_hash anchors the lifetime bound before
/// the pre-confirmation expiry is dropped.  No warn arm needed.
///
/// Returns `true` if the row was modified.
pub fn update_data_root_block_set<T: DbTx + DbTxMut>(
    tx: &T,
    data_root: DataRoot,
    block_hash: H256,
) -> eyre::Result<bool> {
    let Some(mut cdr) = tx.get::<CachedDataRoots>(data_root)? else {
        return Ok(false);
    };
    let already_present = cdr.block_set.contains(&block_hash);
    let needs_expiry_clear = cdr.expiry_height.is_some();
    if already_present && !needs_expiry_clear {
        return Ok(false);
    }
    if !already_present {
        cdr.block_set.push(block_hash);
    }
    if needs_expiry_clear {
        cdr.expiry_height = None;
    }
    tx.put::<CachedDataRoots>(data_root, cdr)?;
    Ok(true)
}

/// Removes `block_hash` from the `block_set` of an existing [`CachedDataRoot`].
/// Used when migration clears an orphaned block during reorg so `block_set`
/// stops accumulating dead references.  No-op if the entry is absent or the
/// hash is not present.  `expiry_height` is intentionally left untouched: any
/// re-anchoring of an orphaned tx is handled by the mempool reorg path.
///
/// Returns `true` if the row was modified.
pub fn remove_data_root_block_set_entry<T: DbTx + DbTxMut>(
    tx: &T,
    data_root: DataRoot,
    block_hash: H256,
) -> eyre::Result<bool> {
    let Some(mut cdr) = tx.get::<CachedDataRoots>(data_root)? else {
        return Ok(false);
    };
    let before = cdr.block_set.len();
    cdr.block_set.retain(|h| h != &block_hash);
    if cdr.block_set.len() == before {
        return Ok(false);
    }
    let now_anomalous = cdr.block_set.is_empty() && cdr.expiry_height.is_none();
    tx.put::<CachedDataRoots>(data_root, cdr)?;
    if now_anomalous {
        // Reorg scrub left the CDR with no canonical block hashes and no
        // pre-confirmation expiry.  Mempool's orphan re-anchor path
        // (`handle_confirmed_data_tx_reorg`) is expected to re-ingest the
        // orphaned tx and restore `expiry_height` via
        // `cache_data_root_with_expiry`.  If that doesn't happen, the entry
        // sits in the `(None, None)` arm and is only evicted by
        // `prune_data_root_cache` after the local-proof exemption check.
        warn!(
            %data_root,
            removed_block_hash = %block_hash,
            "cached_data_root.anomalous_state_after_scrub: block_set now empty and expiry_height None — awaiting mempool orphan re-anchor or prune"
        );
    }
    Ok(true)
}

type IsDuplicate = bool;

/// Caches a [`UnpackedChunk`] - returns `true` if the chunk was a duplicate (present in [`CachedChunks`])
/// and was not inserted into [`CachedChunksIndex`] or [`CachedChunks`]
/// This function ensures that the DataRoot exists in CachedDataRoots before storing the chunk.
pub fn cache_chunk<T: DbTx + DbTxMut>(tx: &T, chunk: &UnpackedChunk) -> eyre::Result<IsDuplicate> {
    if tx.get::<CachedDataRoots>(chunk.data_root)?.is_none() {
        return Err(eyre::eyre!(
            "Data root {} not found in CachedDataRoots",
            chunk.data_root
        ));
    }

    cache_chunk_verified(tx, chunk)
}

/// Caches a [`UnpackedChunk`] whose data root has already been verified to exist in [`CachedDataRoots`].
///
/// # SAFETY REQUIREMENT
///
/// The caller MUST ensure the chunk's `data_root` exists in [`CachedDataRoots`] before
/// calling this function. Failure to do so will leave orphaned chunk data in the cache
/// with no parent data-root entry. Use [`cache_chunk`] instead if you cannot guarantee this.
///
/// Skips the redundant `CachedDataRoots` lookup that [`cache_chunk`] performs, intended for
/// callers (e.g. the write-behind writer) that have already validated the data root.
/// Returns `true` if the chunk was a duplicate and was not inserted.
#[tracing::instrument(level = "trace", skip_all)]
pub fn cache_chunk_verified<T: DbTx + DbTxMut>(
    tx: &T,
    chunk: &UnpackedChunk,
) -> eyre::Result<IsDuplicate> {
    let data_root = chunk.data_root;
    let chunk_path_hash: ChunkPathHash = chunk.chunk_path_hash();

    if cached_chunk_by_chunk_path_hash(tx, &chunk_path_hash)?.is_some() {
        warn!(
            "Chunk {} of {} is already cached, skipping..",
            &chunk_path_hash, &data_root
        );
        return Ok(true);
    }

    let value = CachedChunkIndexEntry {
        index: chunk.tx_offset,
        meta: CachedChunkIndexMetadata {
            chunk_path_hash,
            updated_at: UnixTimestamp::now()
                .map_err(|e| eyre::eyre!("Failed to get current timestamp: {}", e))?,
        },
    };

    debug!(
        "Caching chunk {} ({}) of {}",
        &chunk.tx_offset, &chunk_path_hash, &data_root
    );

    tx.put::<CachedChunksIndex>(data_root, value)?;
    tx.put::<CachedChunks>(chunk_path_hash, chunk.into())?;
    Ok(false)
}

/// Retrieves a cached chunk ([`CachedChunkIndexMetadata`]) from the [`CachedChunksIndex`] using its parent [`DataRoot`] and [`TxChunkOffset`]
pub fn cached_chunk_meta_by_offset<T: DbTx>(
    tx: &T,
    data_root: DataRoot,
    chunk_offset: TxChunkOffset,
) -> eyre::Result<Option<CachedChunkIndexMetadata>> {
    let mut cursor = tx.cursor_dup_read::<CachedChunksIndex>()?;
    Ok(cursor
        .seek_by_key_subkey(data_root, *chunk_offset)?
        // make sure we find the exact subkey - dupsort seek can seek to the value, or a value greater than if it doesn't exist.
        .filter(|result| result.index == chunk_offset)
        .map(|index_entry| index_entry.meta))
}
/// Retrieves a cached chunk ([`(CachedChunkIndexMetadata, CachedChunk)`]) from the cache ([`CachedChunks`] and [`CachedChunksIndex`]) using its parent  [`DataRoot`] and [`TxChunkOffset`]
pub fn cached_chunk_by_chunk_offset<T: DbTx>(
    tx: &T,
    data_root: DataRoot,
    chunk_offset: TxChunkOffset,
) -> eyre::Result<Option<(CachedChunkIndexMetadata, CachedChunk)>> {
    let mut cursor = tx.cursor_dup_read::<CachedChunksIndex>()?;

    if let Some(index_entry) = cursor
        .seek_by_key_subkey(data_root, *chunk_offset)?
        .filter(|e| e.index == chunk_offset)
    {
        let meta: CachedChunkIndexMetadata = index_entry.into();
        // the cached chunk always has an entry if the index entry exists
        Ok(Some((
            meta.clone(),
            tx.get::<CachedChunks>(meta.chunk_path_hash)?
                .ok_or_else(|| eyre::eyre!("Chunk has an index entry but no data entry"))?,
        )))
    } else {
        Ok(None)
    }
}

/// Retrieves a [`CachedChunk`] from [`CachedChunks`] using its [`ChunkPathHash`]
pub fn cached_chunk_by_chunk_path_hash<T: DbTx>(
    tx: &T,
    key: &ChunkPathHash,
) -> Result<Option<CachedChunk>, DatabaseError> {
    tx.get::<CachedChunks>(*key)
}

/// Deletes [`CachedChunk`]s from [`CachedChunks`] by looking up the [`ChunkPathHash`] in [`CachedChunksIndex`]
/// It also removes the index values
pub fn delete_cached_chunks_by_data_root<T: DbTxMut>(
    tx: &T,
    data_root: DataRoot,
) -> eyre::Result<u64> {
    let mut chunks_pruned = 0;
    // get all chunks specified by the `CachedChunksIndex`
    let mut cursor = tx.cursor_dup_write::<CachedChunksIndex>()?;
    let mut walker = cursor.walk_dup(Some(data_root), None)?; // iterate a specific key's subkeys
    while let Some((_k, c)) = walker.next().transpose()? {
        // delete them
        tx.delete::<CachedChunks>(c.meta.chunk_path_hash, None)?;
        chunks_pruned += 1;
    }
    // delete the key (and all subkeys) from the index
    tx.delete::<CachedChunksIndex>(data_root, None)?;
    Ok(chunks_pruned)
}

/// Deletes [`CachedChunk`]s from [`CachedChunks`] by looking up the [`ChunkPathHash`] in [`CachedChunksIndex`]
/// It also removes the index values
pub fn delete_cached_chunks_by_data_root_older_than<T: DbTxMut>(
    tx: &T,
    data_root: DataRoot,
    older_than: UnixTimestamp,
) -> eyre::Result<u64> {
    let mut chunks_pruned = 0;
    // get all chunks specified by the `CachedChunksIndex`
    let mut cursor = tx.cursor_dup_write::<CachedChunksIndex>()?;
    let mut walker = cursor.walk_dup(Some(data_root), None)?; // iterate a specific key's subkeys
    while let Some((_k, c)) = walker.next().transpose()? {
        if c.meta.updated_at >= older_than {
            continue;
        }
        // delete them
        tx.delete::<CachedChunks>(c.meta.chunk_path_hash, None)?;
        // delete the specific index entry instead of nuking the whole key
        tx.delete::<CachedChunksIndex>(data_root, Some(c))?;
        chunks_pruned += 1;
    }
    // If we removed all subkeys, remove the empty key bucket
    let mut check_cursor = tx.cursor_dup_write::<CachedChunksIndex>()?;
    let mut remaining = check_cursor.walk_dup(Some(data_root), None)?;
    if remaining.next().transpose()?.is_none() {
        tx.delete::<CachedChunksIndex>(data_root, None)?;
    }
    Ok(chunks_pruned)
}

pub fn get_cache_size<T: Table, TX: DbTx>(tx: &TX, chunk_size: u64) -> eyre::Result<(u64, u64)> {
    let chunk_count: usize = tx.entries::<T>()?;
    let chunk_count_u64 = u64::try_from(chunk_count)
        .map_err(|_| eyre::eyre!("Cache size chunk_count does not fit into u64"))?;
    let total_size = chunk_count_u64
        .checked_mul(chunk_size)
        .ok_or_else(|| eyre::eyre!("Cache size calculation overflow"))?;
    Ok((chunk_count_u64, total_size))
}

pub fn insert_peer_list_item<T: DbTxMut>(
    tx: &T,
    peer_id: &IrysPeerId,
    peer_list_entry: &PeerListItem,
) -> eyre::Result<()> {
    // Convert PeerListItem to PeerListItemInner for database storage
    let inner = peer_list_entry.to_inner();

    // Validate that the peer_id in the payload matches the supplied peer_id
    if peer_list_entry.peer_id != *peer_id {
        eyre::bail!(
            "Peer ID mismatch: supplied peer_id {:?} does not match PeerListItem.peer_id {:?}",
            peer_id,
            peer_list_entry.peer_id
        );
    }

    Ok(tx.put::<PeerListItems>(*peer_id, inner.into())?)
}

/// Deletes a peer from the persistent peer list. Returns `true` if a row was
/// present. Used to evict a peer we can no longer peer with (e.g. a `chain_id`
/// mismatch surfaced during a handshake) so it is not reloaded on restart.
pub fn delete_peer_list_item<T: DbTxMut>(tx: &T, peer_id: &IrysPeerId) -> eyre::Result<bool> {
    Ok(tx.delete::<PeerListItems>(*peer_id, None)?)
}

/// Gets all ingress proofs associated with a specific data_root
///
pub fn ingress_proofs_by_data_root<TX: DbTx>(
    read_tx: &TX,
    data_root: DataRoot,
) -> eyre::Result<Vec<(DataRoot, CompactCachedIngressProof)>> {
    let mut cursor = read_tx.cursor_dup_read::<IngressProofs>()?;
    let walker = cursor.walk_dup(Some(data_root), None)?; // iterate over all subkeys
    let proofs: Vec<(irys_types::H256, CompactCachedIngressProof)> =
        walker.collect::<Result<Vec<_>, DatabaseError>>()?;

    Ok(proofs)
}

pub fn ingress_proof_by_data_root_address<TX: DbTx>(
    read_tx: &TX,
    data_root: DataRoot,
    address: IrysAddress,
) -> eyre::Result<Option<CompactCachedIngressProof>> {
    let mut cursor = read_tx.cursor_dup_read::<IngressProofs>()?;

    if let Some(index_entry) = cursor
        .seek_by_key_subkey(data_root, address)?
        .filter(|e| e.address == address)
    {
        Ok(Some(index_entry))
    } else {
        Ok(None)
    }
}

/// Deletes only the ingress proof row for a specific (data_root, address) pair,
/// leaving proofs from other signers intact.
/// Returns `true` if a row was deleted, `false` if no matching row was found.
///
/// **Test-only** (`#[cfg(test)]`). This performs **no content check** (no TOCTOU
/// guard), so it must never become a production deletion path: gating it on
/// `cfg(test)` makes that misuse impossible at compile time. Production callers
/// that need TOCTOU safety must use [`delete_ingress_proof_if_unchanged`].
#[cfg(test)]
pub(crate) fn delete_ingress_proof_by_signer<T: DbTx + DbTxMut>(
    tx: &T,
    data_root: DataRoot,
    address: IrysAddress,
) -> eyre::Result<bool> {
    // O(1) seek: no need to scan all dup values for this data_root.
    if let Some(existing) = ingress_proof_by_data_root_address(tx, data_root, address)? {
        tx.delete::<IngressProofs>(data_root, Some(existing))?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Deletes the ingress proof for (data_root, address) ONLY if the stored row
/// still matches `expected`. Used by the prune loop to close the TOCTOU window
/// between the scan and the delete: if another task stored a fresh proof for the
/// same key between the scan and this call, the stale scanned value will not
/// match and the fresh proof is preserved.
/// Returns `true` if the row was deleted, `false` if absent or content differs.
pub fn delete_ingress_proof_if_unchanged<T: DbTx + DbTxMut>(
    tx: &T,
    data_root: DataRoot,
    expected: CompactCachedIngressProof,
) -> eyre::Result<bool> {
    let address = expected.address;
    match ingress_proof_by_data_root_address(tx, data_root, address)? {
        // Compare inner CachedIngressProof (derives PartialEq); the wrapper does not.
        Some(current) if current.0 == expected.0 => {
            tx.delete::<IngressProofs>(data_root, Some(current))?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

pub fn store_ingress_proof(
    db: &DatabaseProvider,
    ingress_proof: &IngressProof,
    signer: &IrysSigner,
) -> eyre::Result<()> {
    db.update_scoped(|rw_tx| store_ingress_proof_checked(rw_tx, ingress_proof, signer))?
}

pub fn store_ingress_proof_checked<T: DbTx + DbTxMut>(
    tx: &T,
    ingress_proof: &IngressProof,
    signer: &IrysSigner,
) -> eyre::Result<()> {
    if tx
        .get::<CachedDataRoots>(ingress_proof.data_root)?
        .is_none()
    {
        return Err(eyre::eyre!(
            "Data root {} not found in CachedDataRoots",
            ingress_proof.data_root
        ));
    }

    // Delete all existing proofs for this signer before inserting, as DupSort
    // tables don't upsert — re-anchoring would otherwise produce duplicates.
    let address = signer.address();
    for (_, existing) in ingress_proofs_by_data_root(tx, ingress_proof.data_root)?
        .into_iter()
        .filter(|(_, proof)| proof.address == address)
    {
        tx.delete::<IngressProofs>(ingress_proof.data_root, Some(existing))?;
    }

    tx.put::<IngressProofs>(
        ingress_proof.data_root,
        CompactCachedIngressProof(CachedIngressProof {
            address,
            proof: ingress_proof.clone(),
        }),
    )?;
    Ok(())
}

pub fn store_external_ingress_proof_checked<T: DbTx + DbTxMut>(
    tx: &T,
    ingress_proof: &IngressProof,
    address: IrysAddress,
) -> eyre::Result<()> {
    if tx
        .get::<CachedDataRoots>(ingress_proof.data_root)?
        .is_none()
    {
        return Err(eyre::eyre!(
            "Data root {} not found in CachedDataRoots",
            ingress_proof.data_root
        ));
    }

    // Delete all existing proofs for this address before inserting (see store_ingress_proof_checked).
    for (_, existing) in ingress_proofs_by_data_root(tx, ingress_proof.data_root)?
        .into_iter()
        .filter(|(_, proof)| proof.address == address)
    {
        tx.delete::<IngressProofs>(ingress_proof.data_root, Some(existing))?;
    }

    tx.put::<IngressProofs>(
        ingress_proof.data_root,
        CompactCachedIngressProof(CachedIngressProof {
            address,
            proof: ingress_proof.clone(),
        }),
    )?;
    Ok(())
}

/// Stores block index data in the database for a given block.
///
/// Decomposes the block header into individual LedgerIndexItems for efficient storage
/// and pruning. Each ledger (Publish, Submit, etc.) is stored as a separate SubKey under the
/// block_height, allowing ledgers to be pruned independently without affecting others in the block.
pub fn insert_block_index_items_for_block<T: DbTxMut>(
    tx: &T,
    block: &IrysBlockHeader,
) -> eyre::Result<()> {
    // Loop though each ledger in the blocks data_ledger list  and
    // create a CompactLedgerIndexItem for it at that height
    // Build a CompactIrysBlockIndexItem wrapper for each LedgerIndexItem
    // Post all of them to the DB with a single write TX

    // Delete any existing dups for this height to make the insert idempotent
    let _ = tx.delete::<IrysBlockIndexItems>(block.height, None);

    for data_ledger in block.data_ledgers.iter() {
        // Create a LedgerIndexItem for each data ledger in the block
        let ledger_enum = DataLedger::try_from(data_ledger.ledger_id)?;
        let ledger_index_item = LedgerIndexItem {
            total_chunks: data_ledger.total_chunks,
            tx_root: data_ledger.tx_root,
            ledger: ledger_enum,
        };

        // Convert it to a compact type for db persistence
        let compact_ledger_index_item = CompactLedgerIndexItem(ledger_index_item);

        // Use the db write tx to persist the entry at the specified block_height
        tx.put::<IrysBlockIndexItems>(block.height, compact_ledger_index_item)?;
    }

    // Update the block hash index as well
    tx.put::<MigratedBlockHashes>(block.height, block.block_hash)?;

    Ok(())
}

/// Reconstructs a BlockIndexItem from the db for a given block height.
///
/// Retrieves block_hash and all LedgerIndexItems at the specified height, then combines them
/// into a single BlockIndexItem structure used for chunk validation during mining.
pub fn block_index_item_by_height<T: DbTx>(
    tx: &T,
    height: &BlockHeight,
) -> eyre::Result<BlockIndexItem> {
    // Step 1: Retrieve CompactLedgerIndexItems for each DataLedger at the given block_height.
    let mut cursor = tx.cursor_dup_read::<IrysBlockIndexItems>()?;
    let walker = cursor.walk_dup(Some(*height), None)?; // iterate over all subkeys
    let ledger_index_items: Vec<(BlockHeight, CompactLedgerIndexItem)> =
        walker.collect::<Result<Vec<_>, DatabaseError>>()?;

    // Step 2: Retrieve the block_hash
    let block_hash = tx
        .get::<MigratedBlockHashes>(*height)?
        .ok_or_else(|| eyre::eyre!("No block hash found at height {}", height))?;

    // Step 3: Transform the CompactLedgerIndexItems into a single BlockIndexItem.
    // This transformation is necessary because:
    // - CompactLedgerIndexItem: Optimized for pruning term ledgers efficiently
    // - BlockIndexItem: Optimized for validation of chunks within a block (mining)
    let mut ledgers: Vec<LedgerIndexItem> = Vec::new();
    for ledger_item in &ledger_index_items {
        ledgers.push(LedgerIndexItem {
            total_chunks: ledger_item.1.total_chunks,
            tx_root: ledger_item.1.tx_root,
            ledger: ledger_item.1.ledger,
        });
    }

    // Step 4: Build and return the BlockIndexItem
    let num_ledgers: u8 = ledger_index_items.len().try_into()?;
    let block_index_item = BlockIndexItem {
        num_ledgers,
        block_hash,
        ledgers,
    };

    Ok(block_index_item)
}

/// Deletes all LedgerIndexItems for the specified ledger bounded by the height range.
///
/// This function preserves LedgerIndexItems for other ledgers at the same heights,
/// only removing entries that match the target ledger.
pub fn prune_ledger_range<T: DbTxMut, U: RangeBounds<BlockHeight>>(
    tx: &T,
    ledger: DataLedger,
    range: U,
) -> eyre::Result<()> {
    let mut cursor = tx.cursor_write::<IrysBlockIndexItems>()?;
    let mut range_walker = cursor.walk_range(range)?;

    while let Some(result) = range_walker.next() {
        let (_height, item) = result?;
        if item.ledger == ledger {
            range_walker.delete_current()?;
        }
    }
    Ok(())
}

/// Inserts a single [`BlockIndexItem`] into the block index tables.
///
/// Writes the `block_hash` to [`MigratedBlockHashes`] and each [`LedgerIndexItem`]
/// to [`IrysBlockIndexItems`]. Used by the migration path and `push_item`.
pub fn insert_block_index_item<T: DbTxMut>(
    tx: &T,
    height: BlockHeight,
    item: &BlockIndexItem,
) -> eyre::Result<()> {
    // Delete any existing dups for this height to make the insert idempotent
    let _ = tx.delete::<IrysBlockIndexItems>(height, None);

    tx.put::<MigratedBlockHashes>(height, item.block_hash)?;
    for ledger_item in &item.ledgers {
        tx.put::<IrysBlockIndexItems>(height, CompactLedgerIndexItem(ledger_item.clone()))?;
    }
    Ok(())
}

/// Returns the latest (highest) block height in the block index, or `None` if empty.
pub fn block_index_latest_height<T: DbTx>(tx: &T) -> eyre::Result<Option<u64>> {
    let mut cursor = tx.cursor_read::<MigratedBlockHashes>()?;
    Ok(cursor.last()?.map(|(height, _)| height))
}

/// Returns the number of blocks stored in the block index.
pub fn block_index_num_blocks<T: DbTx>(tx: &T) -> eyre::Result<u64> {
    let count = tx.entries::<MigratedBlockHashes>()?;
    Ok(u64::try_from(count)?)
}

/// Deletes block index entries for all heights in the given range from both tables.
///
/// Used for rollback operations. Removes entries from both [`IrysBlockIndexItems`]
/// and [`MigratedBlockHashes`].
pub fn delete_block_index_range<T: DbTxMut, U: RangeBounds<BlockHeight> + Clone>(
    tx: &T,
    range: U,
) -> eyre::Result<()> {
    // Delete from IrysBlockIndexItems
    let mut cursor = tx.cursor_write::<IrysBlockIndexItems>()?;
    let mut range_walker = cursor.walk_range(range.clone())?;
    while let Some(result) = range_walker.next() {
        let (_height, _item) = result?;
        range_walker.delete_current()?;
    }
    drop(range_walker);
    drop(cursor);

    // Delete from MigratedBlockHashes
    let mut cursor = tx.cursor_write::<MigratedBlockHashes>()?;
    let mut range_walker = cursor.walk_range(range)?;
    while let Some(result) = range_walker.next() {
        let (_height, _hash) = result?;
        range_walker.delete_current()?;
    }

    Ok(())
}

pub fn walk_all<T: Table, TX: DbTx>(
    read_tx: &TX,
) -> eyre::Result<Vec<(<T as Table>::Key, <T as Table>::Value)>> {
    let mut read_cursor = read_tx.cursor_read::<T>()?;
    let walker = read_cursor.walk(None)?;
    Ok(walker.collect::<Result<Vec<_>, _>>()?)
}

pub fn set_database_schema_version<T: DbTxMut>(
    tx: &T,
    version: DatabaseVersion,
) -> Result<(), DatabaseError> {
    tx.put::<Metadata>(
        MetadataKey::DBSchemaVersion,
        (version as u32).to_le_bytes().to_vec(),
    )
}

pub fn database_schema_version<T: DbTx>(tx: &mut T) -> Result<Option<u32>, DatabaseError> {
    if let Some(bytes) = tx.get::<Metadata>(MetadataKey::DBSchemaVersion)? {
        let arr: [u8; 4] = bytes.as_slice().try_into().map_err(|_| {
            DatabaseError::Other(
                "Db schema version metadata does not have exactly 4 bytes".to_string(),
            )
        })?;

        Ok(Some(u32::from_le_bytes(arr)))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        block_header_by_hash, commitment_tx_by_txid, db::IrysDatabaseExt as _,
        insert_commitment_tx, tables::IrysTables,
    };
    use arbitrary::Arbitrary as _;
    use irys_types::{CommitmentTransaction, DataTransactionHeader, H256, IrysBlockHeader};
    use rand::Rng as _;
    use reth_db::Database as _;

    use super::{
        IrysDatabaseArgs as _, insert_block_header, insert_tx_header, open_or_create_db,
        tx_header_by_txid,
    };
    use reth_db::mdbx::DatabaseArguments;

    #[test]
    fn insert_and_get_tests() -> eyre::Result<()> {
        let path = irys_testing_utils::utils::TempDirBuilder::new().build();
        println!("TempDir: {:?}", path);

        // Generate arbitrary metadata using Arbitrary trait with properly sized buffer
        let mut rng = rand::thread_rng();
        let (min, max) = irys_types::DataTransactionMetadata::size_hint(0);
        let length = max.unwrap_or(min.saturating_mul(4).max(256));
        let bytes: Vec<u8> = (0..length).map(|_| rng.r#gen()).collect();
        let mut u = arbitrary::Unstructured::new(&bytes);

        let tx_header =
            DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
                tx: irys_types::DataTransactionHeaderV1::default(),
                metadata: irys_types::DataTransactionMetadata::arbitrary(&mut u)?,
            });

        let db = open_or_create_db(
            path.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();

        // Write a Tx
        let _ = db.update(|tx| insert_tx_header(tx, &tx_header))?;

        // Read a Tx
        let result = db.view_eyre(|tx| tx_header_by_txid(tx, &tx_header.id))?;
        let result_as_v1 = result
            .as_ref()
            .and_then(|h| h.try_as_header_v1().cloned())
            .unwrap();
        assert_eq!(result_as_v1, tx_header.try_as_header_v1().cloned().unwrap());

        // Write a commitment tx
        let commitment_tx = CommitmentTransaction::V2(irys_types::CommitmentV2WithMetadata {
            tx: irys_types::CommitmentTransactionV2 {
                // Override some defaults to insure deserialization is working
                id: H256::from([10_u8; 32]),
                ..Default::default()
            },
            metadata: Default::default(),
        });
        let _ = db.update(|tx| insert_commitment_tx(tx, &commitment_tx))?;

        // Read a commitment tx
        let result = db.view_eyre(|tx| commitment_tx_by_txid(tx, &commitment_tx.id()))?;
        assert_eq!(result, Some(commitment_tx));

        let mut block_header = IrysBlockHeader::new_mock_header();
        block_header.block_hash.0[0] = 1;

        // Write a Block
        let _ = db.update(|tx| insert_block_header(tx, &block_header))?;

        // Read a Block
        let result = db.view_eyre(|tx| block_header_by_hash(tx, &block_header.block_hash, true))?;
        let result2 = db
            .view_eyre(|tx| block_header_by_hash(tx, &block_header.block_hash, false))?
            .unwrap();

        assert_eq!(result, Some(block_header.clone()));

        // check block is retrieved without its chunk
        block_header.poa.chunk = None;
        assert_eq!(result2, block_header);
        Ok(())
    }

    #[test]
    fn delete_peer_list_item_removes_it() -> eyre::Result<()> {
        use super::{delete_peer_list_item, insert_peer_list_item};
        use crate::tables::PeerListItems;
        use reth_db::transaction::DbTx as _;

        let path = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = open_or_create_db(
            path.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();

        let peer_id = irys_types::IrysPeerId::from([7_u8; 20]);
        let peer = irys_types::PeerListItem {
            peer_id,
            mining_address: irys_types::IrysAddress::from([7_u8; 20]),
            reputation_score: irys_types::PeerScore::new(irys_types::PeerScore::INITIAL),
            response_time: 10,
            address: irys_types::PeerAddress {
                gossip: "127.0.0.1:8000".parse().unwrap(),
                api: "127.0.0.1:9000".parse().unwrap(),
                execution: irys_types::RethPeerInfo::default(),
            },
            last_seen: 0,
            is_online: true,
            protocol_version: irys_types::ProtocolVersion::default(),
        };

        db.update_eyre(|tx| insert_peer_list_item(tx, &peer_id, &peer))?;
        assert!(
            db.view_eyre(|tx| Ok(tx.get::<PeerListItems>(peer_id)?.is_some()))?,
            "peer should be present after insert"
        );

        let deleted = db.update_eyre(|tx| delete_peer_list_item(tx, &peer_id))?;
        assert!(deleted, "delete should report the row existed");
        assert!(
            db.view_eyre(|tx| Ok(tx.get::<PeerListItems>(peer_id)?.is_none()))?,
            "peer should be gone after delete"
        );
        Ok(())
    }

    /// Builds a stored-able header at `height` whose commitment ledger carries
    /// `commitment_tx_ids`.
    fn header_with_commitments(height: u64, commitment_tx_ids: Vec<H256>) -> IrysBlockHeader {
        irys_testing_utils::mock_header_with_commitments(height, commitment_tx_ids)
    }

    #[test]
    fn canonical_commitment_included_height_requires_mbh() -> eyre::Result<()> {
        use crate::tables::MigratedBlockHashes;
        use crate::{canonical_commitment_included_height, set_commitment_tx_included_height};
        use reth_db::transaction::DbTxMut as _;

        let path = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = open_or_create_db(
            path.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();
        let txid = H256::random();

        // Write inclusion height 10, but no MBH row yet -> not canonical.
        db.update(|tx| set_commitment_tx_included_height(tx, &txid, 10))??;
        assert_eq!(
            db.view(|tx| canonical_commitment_included_height(tx, &txid, 100))??,
            None
        );

        // MBH points at height 10 but no stored header for that hash -> the
        // MBH/IrysBlockHeaders cross-table disagreement now fails loud.
        db.update(|tx| tx.put::<MigratedBlockHashes>(10, H256::random()))??;
        assert!(
            db.view(|tx| canonical_commitment_included_height(tx, &txid, 100))?
                .is_err(),
            "missing canonical header for the MBH hash must be a cross-table Err"
        );

        // MBH points at a real stored header that does NOT carry the txid ->
        // stranded row, no canonical inclusion.
        let other_block = header_with_commitments(10, vec![H256::random()]);
        db.update(|tx| {
            insert_block_header(tx, &other_block)?;
            tx.put::<MigratedBlockHashes>(10, other_block.block_hash)?;
            Ok::<_, eyre::Report>(())
        })??;
        assert_eq!(
            db.view(|tx| canonical_commitment_included_height(tx, &txid, 100))??,
            None
        );

        // MBH points at a stored header that DOES carry the txid -> canonical.
        let including_block = header_with_commitments(10, vec![txid]);
        db.update(|tx| {
            insert_block_header(tx, &including_block)?;
            tx.put::<MigratedBlockHashes>(10, including_block.block_hash)?;
            Ok::<_, eyre::Report>(())
        })??;
        assert_eq!(
            db.view(|tx| canonical_commitment_included_height(tx, &txid, 100))??,
            Some(10)
        );

        // max_height below the inclusion height -> filtered out.
        assert_eq!(
            db.view(|tx| canonical_commitment_included_height(tx, &txid, 5))??,
            None
        );

        // Unknown tx -> None.
        assert_eq!(
            db.view(|tx| canonical_commitment_included_height(tx, &H256::random(), 100))??,
            None
        );
        Ok(())
    }

    /// A stranded metadata row (an orphaned tip left an `included_height` hint,
    /// and a different winning block later migrated at that height) must NOT
    /// read as canonical inclusion: the content check on the canonical header
    /// makes such rows harmless.
    #[test]
    fn canonical_commitment_included_height_ignores_stranded_row() -> eyre::Result<()> {
        use crate::tables::MigratedBlockHashes;
        use crate::{canonical_commitment_included_height, set_commitment_tx_included_height};
        use reth_db::transaction::DbTxMut as _;

        let path = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = open_or_create_db(
            path.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();
        let txid = H256::random();

        // Stranded hint: the tx was confirmed at the orphaned tip at height 10,
        // but the block that actually migrated there carries different txs.
        let winning_block = header_with_commitments(10, vec![H256::random(), H256::random()]);
        db.update(|tx| {
            set_commitment_tx_included_height(tx, &txid, 10)?;
            insert_block_header(tx, &winning_block)?;
            tx.put::<MigratedBlockHashes>(10, winning_block.block_hash)?;
            Ok::<_, eyre::Report>(())
        })??;

        assert_eq!(
            db.view(|tx| canonical_commitment_included_height(tx, &txid, 100))??,
            None,
            "stranded row must not be treated as canonical inclusion"
        );
        Ok(())
    }

    /// Genesis first-includes its commitments but never flows through the
    /// confirm/migration metadata writers, so no dedup row is written for it at
    /// init. The startup backfill must write `included_height = 0` for each
    /// genesis commitment so the finalized lookup resolves them once genesis
    /// passes the reorg floor — and must be idempotent across boots.
    #[test]
    fn backfill_genesis_commitment_included_height_is_idempotent() -> eyre::Result<()> {
        use crate::backfill_genesis_commitment_included_height;
        use crate::canonical_commitment_included_height;
        use crate::tables::MigratedBlockHashes;
        use irys_types::DatabaseProvider;
        use reth_db::transaction::DbTxMut as _;
        use std::sync::Arc;

        let path = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = DatabaseProvider(Arc::new(
            open_or_create_db(
                path.path(),
                IrysTables::ALL,
                DatabaseArguments::irys_testing().unwrap(),
            )
            .unwrap(),
        ));

        // Persist genesis the way `persist_genesis_block_and_commitments` does:
        // header at height 0 carrying its commitment ledger, plus MBH[0].
        let c0 = H256::random();
        let c1 = H256::random();
        let genesis = header_with_commitments(0, vec![c0, c1]);
        db.update(|tx| {
            insert_block_header(tx, &genesis)?;
            tx.put::<MigratedBlockHashes>(0, genesis.block_hash)?;
            Ok::<_, eyre::Report>(())
        })??;

        // No dedup row exists before the backfill — this is the gap the fix closes.
        assert_eq!(
            db.view(|tx| canonical_commitment_included_height(tx, &c0, u64::MAX))??,
            None,
            "no genesis commitment row before backfill"
        );

        backfill_genesis_commitment_included_height(&db)?;

        // Each genesis commitment now resolves to height 0 (also exercises the
        // content check against the genesis header).
        for id in [c0, c1] {
            assert_eq!(
                db.view(|tx| canonical_commitment_included_height(tx, &id, u64::MAX))??,
                Some(0),
                "genesis commitment must resolve to included_height 0 after backfill"
            );
        }

        // Second boot writes the same value — trivially idempotent.
        backfill_genesis_commitment_included_height(&db)?;
        for id in [c0, c1] {
            assert_eq!(
                db.view(|tx| canonical_commitment_included_height(tx, &id, u64::MAX))??,
                Some(0),
                "backfill must be idempotent across boots"
            );
        }
        Ok(())
    }

    /// A missing `MigratedBlockHashes[0]` at backfill time is a hard invariant
    /// violation (genesis must be persisted first) and must fail loud rather
    /// than silently skip.
    #[test]
    fn backfill_genesis_commitment_included_height_fails_without_mbh() -> eyre::Result<()> {
        use crate::backfill_genesis_commitment_included_height;
        use irys_types::DatabaseProvider;
        use std::sync::Arc;

        let path = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = DatabaseProvider(Arc::new(
            open_or_create_db(
                path.path(),
                IrysTables::ALL,
                DatabaseArguments::irys_testing().unwrap(),
            )
            .unwrap(),
        ));

        assert!(
            backfill_genesis_commitment_included_height(&db).is_err(),
            "missing MigratedBlockHashes[0] must fail loud"
        );
        Ok(())
    }

    #[test]
    fn delete_ingress_proof_by_signer_deletes_only_that_signer() -> eyre::Result<()> {
        use super::{
            delete_ingress_proof_by_signer, ingress_proof_by_data_root_address,
            ingress_proofs_by_data_root,
        };
        use crate::{
            db::IrysDatabaseExt as _,
            db_cache::CachedDataRoot,
            tables::{CachedDataRoots, CompactCachedIngressProof, IngressProofs},
        };
        use irys_types::{IrysAddress, ingress::CachedIngressProof};
        use reth_db::transaction::DbTxMut as _;

        let path = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = open_or_create_db(
            path.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();

        let data_root = H256::random();
        let addr_a = IrysAddress::random();
        let addr_b = IrysAddress::random();
        let addr_c = IrysAddress::random();

        // Build minimal distinct proofs for addr_a and addr_b.
        let make_proof = |address: IrysAddress, proof_hash: H256| {
            let mut p = irys_types::IngressProof::default();
            p.data_root = data_root;
            p.proof = proof_hash;
            CompactCachedIngressProof(CachedIngressProof { address, proof: p })
        };

        let proof_a = make_proof(addr_a, H256::from([1_u8; 32]));
        let proof_b = make_proof(addr_b, H256::from([2_u8; 32]));

        // Seed: minimal CDR + two proof rows.
        db.update_eyre(|tx| {
            tx.put::<CachedDataRoots>(
                data_root,
                CachedDataRoot {
                    data_size: 64,
                    data_size_confirmed: false,
                    txid_set: vec![],
                    block_set: vec![],
                    expiry_height: None,
                    cached_at: irys_types::UnixTimestamp::default(),
                },
            )?;
            tx.put::<IngressProofs>(data_root, proof_a.clone())?;
            tx.put::<IngressProofs>(data_root, proof_b.clone())?;
            Ok(())
        })?;

        // Both rows present initially.
        assert_eq!(
            db.view_eyre(|tx| ingress_proofs_by_data_root(tx, data_root))?
                .len(),
            2,
            "expected 2 proofs after seeding"
        );

        // Delete addr_a — must return true and remove only that row.
        let deleted = db.update_eyre(|tx| delete_ingress_proof_by_signer(tx, data_root, addr_a))?;
        assert!(deleted, "deleting addr_a row must return true");

        assert!(
            db.view_eyre(|tx| ingress_proof_by_data_root_address(tx, data_root, addr_a))?
                .is_none(),
            "addr_a proof must be gone"
        );
        assert!(
            db.view_eyre(|tx| ingress_proof_by_data_root_address(tx, data_root, addr_b))?
                .is_some(),
            "addr_b proof must survive"
        );

        // Deleting addr_a again returns false (already absent).
        let deleted_again =
            db.update_eyre(|tx| delete_ingress_proof_by_signer(tx, data_root, addr_a))?;
        assert!(!deleted_again, "second delete of addr_a must return false");

        // Deleting a never-stored addr_c returns false.
        let deleted_c =
            db.update_eyre(|tx| delete_ingress_proof_by_signer(tx, data_root, addr_c))?;
        assert!(!deleted_c, "delete of absent addr_c must return false");

        Ok(())
    }

    #[test]
    fn delete_ingress_proof_if_unchanged_preserves_refreshed_proof() -> eyre::Result<()> {
        use super::{delete_ingress_proof_if_unchanged, ingress_proof_by_data_root_address};
        use crate::{
            db::IrysDatabaseExt as _,
            db_cache::CachedDataRoot,
            tables::{CachedDataRoots, CompactCachedIngressProof, IngressProofs},
        };
        use irys_types::{IrysAddress, ingress::CachedIngressProof};
        use reth_db::transaction::DbTxMut as _;

        let path = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = open_or_create_db(
            path.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();

        let data_root = H256::random();
        let addr_a = IrysAddress::random();

        let make_proof = |proof_hash: H256| {
            let mut p = irys_types::IngressProof::default();
            p.data_root = data_root;
            p.proof = proof_hash;
            CompactCachedIngressProof(CachedIngressProof {
                address: addr_a,
                proof: p,
            })
        };

        let proof_p1 = make_proof(H256::from([0xAA_u8; 32]));
        let proof_p2 = make_proof(H256::from([0xBB_u8; 32]));

        // absent-key case: returns false without error.
        let absent = db
            .update_eyre(|tx| delete_ingress_proof_if_unchanged(tx, data_root, proof_p1.clone()))?;
        assert!(!absent, "absent key must return false");

        // Insert CDR and P1.
        db.update_eyre(|tx| {
            tx.put::<CachedDataRoots>(
                data_root,
                CachedDataRoot {
                    data_size: 64,
                    data_size_confirmed: false,
                    txid_set: vec![],
                    block_set: vec![],
                    expiry_height: None,
                    cached_at: irys_types::UnixTimestamp::default(),
                },
            )?;
            tx.put::<IngressProofs>(data_root, proof_p1.clone())?;
            Ok(())
        })?;

        // Simulate refresh: replace P1 with P2 in the store (overwrite between scan and delete).
        db.update_eyre(|tx| {
            tx.delete::<IngressProofs>(data_root, Some(proof_p1.clone()))?;
            tx.put::<IngressProofs>(data_root, proof_p2.clone())?;
            Ok(())
        })?;

        // CAS with stale P1 must return false and leave P2 intact.
        let stale = db
            .update_eyre(|tx| delete_ingress_proof_if_unchanged(tx, data_root, proof_p1.clone()))?;
        assert!(
            !stale,
            "stale expected value must return false (TOCTOU guard)"
        );

        assert!(
            db.view_eyre(|tx| ingress_proof_by_data_root_address(tx, data_root, addr_a))?
                .is_some(),
            "P2 must still be present after stale CAS"
        );

        // CAS with current P2 must return true and remove the row.
        let deleted = db
            .update_eyre(|tx| delete_ingress_proof_if_unchanged(tx, data_root, proof_p2.clone()))?;
        assert!(deleted, "matching expected value must return true");

        assert!(
            db.view_eyre(|tx| ingress_proof_by_data_root_address(tx, data_root, addr_a))?
                .is_none(),
            "row must be gone after successful CAS delete"
        );

        Ok(())
    }

    mod canonical_height_tests {
        use crate::{
            canonical_promoted_height, canonical_submit_height, db::IrysDatabaseExt as _,
            db_index::set_data_tx_included_height, db_index::set_data_tx_promoted_height,
            insert_tx_header, tables::IrysTables, tables::MigratedBlockHashes,
        };
        use irys_types::{DataTransactionHeader, H256};
        use reth_db::{Database as _, DatabaseError, transaction::DbTxMut as _};
        use rstest::rstest;

        use super::open_or_create_db;
        use crate::IrysDatabaseArgs as _;
        use reth_db::mdbx::DatabaseArguments;

        fn make_tx_header(tx_id: H256) -> DataTransactionHeader {
            DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
                tx: irys_types::DataTransactionHeaderV1 {
                    id: tx_id,
                    ..Default::default()
                },
                metadata: Default::default(),
            })
        }

        #[rstest]
        #[case::within_height_and_migrated(10, true, true)]
        #[case::height_equals_max(5, true, true)]
        #[case::height_exceeds_max(3, true, false)]
        #[case::unmigrated_height(10, false, false)]
        /// `canonical_submit_height` returns Some iff metadata is set, ≤ max_height,
        /// and confirmed by `MigratedBlockHashes`.
        fn submit_height_respects_bounds_and_migration(
            #[case] max_height: u64,
            #[case] insert_migration: bool,
            #[case] expect_found: bool,
        ) {
            let path = irys_testing_utils::utils::TempDirBuilder::new().build();
            let db = open_or_create_db(
                path.path(),
                IrysTables::ALL,
                DatabaseArguments::irys_testing().unwrap(),
            )
            .unwrap();

            let tx_id = H256::random();
            let tx_header = make_tx_header(tx_id);
            let block_hash_at_5 = H256::random();

            db.update(|tx| -> Result<(), DatabaseError> {
                insert_tx_header(tx, &tx_header)
                    .map_err(|e| DatabaseError::Other(e.to_string()))?;
                set_data_tx_included_height(tx, &tx_id, 5)?;
                if insert_migration {
                    tx.put::<MigratedBlockHashes>(5, block_hash_at_5)?;
                }
                Ok(())
            })
            .unwrap()
            .unwrap();

            let result = db
                .view_eyre(|tx| canonical_submit_height(tx, &tx_id, max_height))
                .unwrap();
            assert_eq!(
                result.is_some(),
                expect_found,
                "max_height={max_height}, migrated={insert_migration} => found={expect_found}"
            );
        }

        /// Regression for the stranded-`promoted_height` bug.  Setup mirrors
        /// the production fault that crashed the validator with
        /// `BlockBoundsLookupError`:
        ///   - tx has `included_height = 5` with `MBH[5]` set (canonical Submit)
        ///   - tx has `promoted_height = 24688` with `MBH[24688] = None`
        ///     (writer block became a local tip via FCFS tie-break, was
        ///     later orphaned, never migrated, and the node didn't reorg
        ///     away from it so Phase 1 scrub never ran).
        ///
        /// Pre-fix the validator's DB fallback raised
        /// `BlockBoundsLookupError` (NodeFault → restart) on the MBH
        /// mismatch.  Post-fix `canonical_promoted_height` returns `None`
        /// for the same input — the explicit "no canonical promotion per
        /// the migration index" signal the validator now relies on to fall
        /// through to accept.
        ///
        /// `canonical_submit_height` independently still returns
        /// `Some(submit_height)` — the Submit row is MBH-confirmed, only
        /// the Publish row is stranded.
        #[test]
        fn promoted_height_returns_none_when_mbh_disagrees() {
            let path = irys_testing_utils::utils::TempDirBuilder::new().build();
            let db = open_or_create_db(
                path.path(),
                IrysTables::ALL,
                DatabaseArguments::irys_testing().unwrap(),
            )
            .unwrap();

            let tx_id = H256::random();
            let tx_header = make_tx_header(tx_id);
            let canonical_submit_hash = H256::random();
            let submit_height = 5_u64;
            let stranded_promote_height = 24_688_u64;
            let parent_height = 25_000_u64;

            db.update(|tx| -> Result<(), DatabaseError> {
                insert_tx_header(tx, &tx_header)
                    .map_err(|e| DatabaseError::Other(e.to_string()))?;
                // Canonical Submit inclusion: metadata hint + MBH agree.
                set_data_tx_included_height(tx, &tx_id, submit_height)?;
                tx.put::<MigratedBlockHashes>(submit_height, canonical_submit_hash)?;
                // Stranded promotion: metadata hint set, MBH deliberately absent.
                set_data_tx_promoted_height(tx, &tx_id, stranded_promote_height)?;
                Ok(())
            })
            .unwrap()
            .unwrap();

            let submit = db
                .view_eyre(|tx| canonical_submit_height(tx, &tx_id, parent_height))
                .unwrap();
            let promoted = db
                .view_eyre(|tx| canonical_promoted_height(tx, &tx_id, parent_height))
                .unwrap();

            assert_eq!(
                submit,
                Some(submit_height),
                "canonical_submit_height must still attest the MBH-confirmed Submit row"
            );
            assert_eq!(
                promoted, None,
                "canonical_promoted_height MUST be None when MBH[promoted_height] is None; \
                 a Some(_) here would let the validator treat the stranded \
                 non-fork-aware hint as canonical evidence of a prior promotion \
                 (the original BlockBoundsLookupError-then-NodeFault bug)"
            );
        }

        /// Companion to `promoted_height_returns_none_when_mbh_disagrees`:
        /// when MBH agrees, `canonical_promoted_height` MUST surface the
        /// hint — otherwise the validator's legitimate double-publish
        /// signal disappears.
        #[test]
        fn promoted_height_returns_some_when_mbh_agrees() {
            let path = irys_testing_utils::utils::TempDirBuilder::new().build();
            let db = open_or_create_db(
                path.path(),
                IrysTables::ALL,
                DatabaseArguments::irys_testing().unwrap(),
            )
            .unwrap();

            let tx_id = H256::random();
            let tx_header = make_tx_header(tx_id);
            let submit_hash = H256::random();
            let publish_hash = H256::random();
            let submit_height = 5_u64;
            let promote_height = 7_u64;
            let parent_height = 25_000_u64;

            db.update(|tx| -> Result<(), DatabaseError> {
                insert_tx_header(tx, &tx_header)
                    .map_err(|e| DatabaseError::Other(e.to_string()))?;
                set_data_tx_included_height(tx, &tx_id, submit_height)?;
                set_data_tx_promoted_height(tx, &tx_id, promote_height)?;
                tx.put::<MigratedBlockHashes>(submit_height, submit_hash)?;
                tx.put::<MigratedBlockHashes>(promote_height, publish_hash)?;
                Ok(())
            })
            .unwrap()
            .unwrap();

            let submit = db
                .view_eyre(|tx| canonical_submit_height(tx, &tx_id, parent_height))
                .unwrap();
            let promoted = db
                .view_eyre(|tx| canonical_promoted_height(tx, &tx_id, parent_height))
                .unwrap();

            assert_eq!(submit, Some(submit_height));
            assert_eq!(
                promoted,
                Some(promote_height),
                "MBH-confirmed promotion must survive the cross-check"
            );
        }

        /// `canonical_promoted_height` rejects hints above `max_height`,
        /// mirroring the existing `included_height > max_height` rejection.
        /// A peer could otherwise present metadata claiming a future
        /// promotion that hasn't happened yet from the validator's
        /// vantage point.
        #[test]
        fn promoted_height_returns_none_when_above_max_height() {
            let path = irys_testing_utils::utils::TempDirBuilder::new().build();
            let db = open_or_create_db(
                path.path(),
                IrysTables::ALL,
                DatabaseArguments::irys_testing().unwrap(),
            )
            .unwrap();

            let tx_id = H256::random();
            let tx_header = make_tx_header(tx_id);
            let submit_height = 3_u64;
            let promote_height = 10_u64;
            let parent_height = 5_u64; // < promote_height
            let submit_hash = H256::random();
            let publish_hash = H256::random();

            db.update(|tx| -> Result<(), DatabaseError> {
                insert_tx_header(tx, &tx_header)
                    .map_err(|e| DatabaseError::Other(e.to_string()))?;
                set_data_tx_included_height(tx, &tx_id, submit_height)?;
                set_data_tx_promoted_height(tx, &tx_id, promote_height)?;
                tx.put::<MigratedBlockHashes>(submit_height, submit_hash)?;
                tx.put::<MigratedBlockHashes>(promote_height, publish_hash)?;
                Ok(())
            })
            .unwrap()
            .unwrap();

            let promoted = db
                .view_eyre(|tx| canonical_promoted_height(tx, &tx_id, parent_height))
                .unwrap();
            assert_eq!(
                promoted, None,
                "promoted_height > max_height must return None — \
                 it's not visible to the validator's parent_height window"
            );
        }
    }

    /// Direct tests for the `update_data_root_block_set` /
    /// `remove_data_root_block_set_entry` helpers introduced for the
    /// `BlockMigrationService`-as-authoritative-writer refactor.  These pin
    /// the idempotency / no-op / expiry-handling semantics independently of
    /// the migration call site.
    mod data_root_block_set_helpers {
        use crate::{
            IrysDatabaseArgs as _, cache_data_root,
            db::IrysDatabaseExt as _,
            open_or_create_db, remove_data_root_block_set_entry,
            tables::{CachedDataRoots, IrysTables},
            update_data_root_block_set,
        };
        use irys_testing_utils::utils::tempfile;
        use irys_types::{DataTransactionHeader, H256};
        use reth_db::{
            Database as _,
            mdbx::DatabaseArguments,
            transaction::{DbTx as _, DbTxMut as _},
        };

        /// Returns `(env, tempdir)`.  The caller must bind the tempdir to a
        /// variable that lives the duration of the test — dropping it before
        /// the env removes the underlying directory.
        fn open_db() -> (reth_db::mdbx::DatabaseEnv, tempfile::TempDir) {
            let tmp = irys_testing_utils::utils::TempDirBuilder::new().build();
            let env = open_or_create_db(
                tmp.path(),
                IrysTables::ALL,
                DatabaseArguments::irys_testing().unwrap(),
            )
            .unwrap();
            (env, tmp)
        }

        /// Build a CDR for `data_root` with the given txid in its txid_set
        /// and `block_set = initial_block_set`, `expiry_height = expiry`.
        fn seed_cdr(
            db: &reth_db::mdbx::DatabaseEnv,
            data_root: H256,
            tx_id: H256,
            initial_block_set: Vec<H256>,
            expiry: Option<u64>,
        ) {
            let tx_header =
                DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
                    tx: irys_types::DataTransactionHeaderV1 {
                        id: tx_id,
                        data_root,
                        data_size: 64,
                        ..Default::default()
                    },
                    metadata: Default::default(),
                });
            db.update(|tx| -> eyre::Result<()> {
                cache_data_root(tx, &tx_header, None)?;
                let mut cdr = tx
                    .get::<CachedDataRoots>(data_root)?
                    .expect("seed_cdr: cache_data_root must produce an entry");
                cdr.block_set = initial_block_set;
                cdr.expiry_height = expiry;
                tx.put::<CachedDataRoots>(data_root, cdr)?;
                Ok(())
            })
            .unwrap()
            .unwrap();
        }

        fn read_cdr(
            db: &reth_db::mdbx::DatabaseEnv,
            data_root: H256,
        ) -> Option<crate::db_cache::CachedDataRoot> {
            db.view(|tx| tx.get::<CachedDataRoots>(data_root))
                .unwrap()
                .unwrap()
        }

        #[test]
        fn update_returns_false_when_cdr_missing() {
            let (db, _tmp) = open_db();
            let res = db
                .update_eyre(|tx| update_data_root_block_set(tx, H256::random(), H256::random()))
                .unwrap();
            assert!(!res, "missing CDR must be a no-op returning false");
        }

        #[test]
        fn update_appends_hash_and_clears_expiry() {
            let (db, _tmp) = open_db();
            let data_root = H256::random();
            let tx_id = H256::random();
            let new_hash = H256::random();
            seed_cdr(&db, data_root, tx_id, vec![], Some(42));

            let res = db
                .update_eyre(|tx| update_data_root_block_set(tx, data_root, new_hash))
                .unwrap();
            assert!(res, "append + clear-expiry must report a modification");

            let cdr = read_cdr(&db, data_root).expect("CDR present");
            assert_eq!(cdr.block_set, vec![new_hash]);
            assert!(cdr.expiry_height.is_none(), "expiry must be cleared");
        }

        #[test]
        fn update_is_idempotent_when_hash_present_and_expiry_already_cleared() {
            let (db, _tmp) = open_db();
            let data_root = H256::random();
            let tx_id = H256::random();
            let existing_hash = H256::random();
            seed_cdr(&db, data_root, tx_id, vec![existing_hash], None);

            let res = db
                .update_eyre(|tx| update_data_root_block_set(tx, data_root, existing_hash))
                .unwrap();
            assert!(
                !res,
                "no-op when hash already present and expiry already cleared"
            );

            let cdr = read_cdr(&db, data_root).expect("CDR present");
            assert_eq!(cdr.block_set, vec![existing_hash], "block_set unchanged");
        }

        #[test]
        fn update_clears_expiry_even_if_hash_already_present() {
            let (db, _tmp) = open_db();
            let data_root = H256::random();
            let tx_id = H256::random();
            let existing_hash = H256::random();
            seed_cdr(&db, data_root, tx_id, vec![existing_hash], Some(99));

            let res = db
                .update_eyre(|tx| update_data_root_block_set(tx, data_root, existing_hash))
                .unwrap();
            assert!(res, "modification reported when expiry is cleared");

            let cdr = read_cdr(&db, data_root).expect("CDR present");
            assert_eq!(cdr.block_set, vec![existing_hash], "block_set unchanged");
            assert!(cdr.expiry_height.is_none(), "expiry cleared");
        }

        #[test]
        fn remove_returns_false_when_cdr_missing() {
            let (db, _tmp) = open_db();
            let res = db
                .update_eyre(|tx| {
                    remove_data_root_block_set_entry(tx, H256::random(), H256::random())
                })
                .unwrap();
            assert!(!res, "missing CDR must be a no-op returning false");
        }

        #[test]
        fn remove_returns_false_when_hash_absent() {
            let (db, _tmp) = open_db();
            let data_root = H256::random();
            let tx_id = H256::random();
            let other_hash = H256::random();
            seed_cdr(&db, data_root, tx_id, vec![other_hash], Some(5));

            let res = db
                .update_eyre(|tx| remove_data_root_block_set_entry(tx, data_root, H256::random()))
                .unwrap();
            assert!(!res, "hash not in block_set must be a no-op");

            let cdr = read_cdr(&db, data_root).expect("CDR present");
            assert_eq!(cdr.block_set, vec![other_hash], "block_set unchanged");
            assert_eq!(cdr.expiry_height, Some(5), "expiry unchanged");
        }

        #[test]
        fn remove_strips_hash_and_leaves_expiry_alone() {
            let (db, _tmp) = open_db();
            let data_root = H256::random();
            let tx_id = H256::random();
            let target_hash = H256::random();
            let keep_hash = H256::random();
            seed_cdr(
                &db,
                data_root,
                tx_id,
                vec![target_hash, keep_hash],
                Some(50),
            );

            let res = db
                .update_eyre(|tx| remove_data_root_block_set_entry(tx, data_root, target_hash))
                .unwrap();
            assert!(res, "hash present must report a modification");

            let cdr = read_cdr(&db, data_root).expect("CDR present");
            assert_eq!(cdr.block_set, vec![keep_hash], "only target removed");
            assert_eq!(
                cdr.expiry_height,
                Some(50),
                "expiry_height intentionally left untouched per helper docblock"
            );
        }

        /// Scrubbing the last hash from `block_set` while `expiry_height` is
        /// already `None` leaves the CDR in the anomalous (None, None)
        /// state.  The helper still completes successfully; the warn! at the
        /// call site (logged but not test-asserted here) is the observable
        /// signal for operators.
        #[test]
        fn remove_can_leave_cdr_in_anomalous_state() {
            let (db, _tmp) = open_db();
            let data_root = H256::random();
            let tx_id = H256::random();
            let only_hash = H256::random();
            seed_cdr(&db, data_root, tx_id, vec![only_hash], None);

            let res = db
                .update_eyre(|tx| remove_data_root_block_set_entry(tx, data_root, only_hash))
                .unwrap();
            assert!(res, "hash present must report a modification");

            let cdr = read_cdr(&db, data_root).expect("CDR present");
            assert!(cdr.block_set.is_empty(), "block_set scrubbed to empty");
            assert!(
                cdr.expiry_height.is_none(),
                "expiry intentionally left untouched"
            );
        }
    }
}
