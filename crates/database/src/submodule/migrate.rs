//! Upgrade a schema v1 submodule MDBX directory in place.
//!
//! v1 stores a tx hash on every offset row and the data path under that
//! path's hash. v2 stores one interval per contiguous placement and the data
//! path under the offset. Tx paths, data-root infos, tx-leaf bindings, and
//! pending body migrations stay as they are.
//!
//! Each batch is one MDBX transaction. The schema row holds the resume point
//! until the last batch writes [`SUBMODULE_SCHEMA`]. Open does not return
//! until that row is current, so a caller never reads a half-migrated index.

use std::path::Path;
use std::time::{Duration, Instant};

use irys_types::{ChunkDataPath, ChunkPathHash, H256, PartitionChunkOffset};
use reth_db::cursor::DbCursorRO as _;
use reth_db::mdbx::Error as MdbxError;
use reth_db::transaction::{DbTx, DbTxMut as _};
use reth_db::{Database as _, DatabaseEnv, DatabaseError};

use crate::metadata::MetadataKey;

use super::tables::v1::ChunkDataPathByPathHash;
use super::tables::{
    ChunkDataPathByOffset, ChunkPathHashes, ChunkPathHashesByOffset, Metadata,
    SUBMODULE_CONVERT_INCOMPLETE, SUBMODULE_SCHEMA, TxPathInterval, TxPathIntervalByStart,
};

pub(super) const PROGRESS_PREFIX: &str = "irys-submodule-index migrate-v1 ";
const PROGRESS_LOG_ROWS: u64 = 100_000;
const MAX_MARKER_BYTES: usize = 512;

/// What the schema row was when open began.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetectedSubmoduleSchema {
    /// No v1 index rows. The schema row was written.
    Empty,
    /// Schema v1 rows were copied into the v2 tables.
    V1,
    /// The schema row already matched this binary.
    V2,
}

impl DetectedSubmoduleSchema {
    fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::V1 => "v1",
            Self::V2 => "v2",
        }
    }
}

/// Counts from one open. Elapsed time is monotonic and, for a resumed
/// migration, includes the work committed by earlier processes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationStats {
    pub detected: DetectedSubmoduleSchema,
    pub migrated: bool,
    /// Rows walked during a v1 migration. Live path-hash count when `migrated` is false.
    pub path_hash_rows: u64,
    pub data_paths_copied: u64,
    pub intervals_written: u64,
    pub legacy_data_paths_deleted: u64,
    pub elapsed: Duration,
}

impl std::fmt::Display for MigrationStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let elapsed_ms = u64::try_from(self.elapsed.as_millis()).unwrap_or(u64::MAX);
        writeln!(f, "detected: {}", self.detected.as_str())?;
        writeln!(f, "migrated: {}", self.migrated)?;
        writeln!(f, "path_hash_rows: {}", self.path_hash_rows)?;
        writeln!(f, "data_paths_copied: {}", self.data_paths_copied)?;
        writeln!(f, "intervals_written: {}", self.intervals_written)?;
        writeln!(
            f,
            "legacy_data_paths_deleted: {}",
            self.legacy_data_paths_deleted
        )?;
        writeln!(f, "elapsed_ms: {elapsed_ms}")?;
        Ok(())
    }
}

/// Knobs for [`super::open_submodule_index`].
///
/// `fail_after_commits` is a test hook. `Some(0)` returns before the first
/// migration commit. `Some(n)` commits `n` migration transactions, then
/// returns an error. The committed batches stay on disk.
#[derive(Clone, Debug)]
pub struct SubmoduleMigrationOptions {
    pub fail_after_commits: Option<u32>,
    pub batch_rows: usize,
    pub batch_value_bytes: usize,
}

impl Default for SubmoduleMigrationOptions {
    fn default() -> Self {
        Self {
            fail_after_commits: None,
            batch_rows: 2048,
            batch_value_bytes: 8 * 1024 * 1024,
        }
    }
}

/// Refuse a path whose name does not mark it as a copy.
///
/// The directory name, and the canonical name when the path exists, must
/// contain `copy`. A symlink from `db.copy` to a live `db` is refused.
pub fn require_submodule_index_copy(path: &Path) -> eyre::Result<()> {
    if !name_marks_copy(path) {
        eyre::bail!(
            "refusing to open {}: directory name must contain \"copy\" so a live submodule db/ is not migrated by this tool",
            path.display()
        );
    }
    if let Ok(canonical) = path.canonicalize()
        && !name_marks_copy(&canonical)
    {
        eyre::bail!(
            "refusing to open {}: it resolves to {}, whose name does not contain \"copy\"",
            path.display(),
            canonical.display()
        );
    }
    Ok(())
}

fn name_marks_copy(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.to_ascii_lowercase().contains("copy"))
}

pub(super) fn ensure_mdbx_schema(
    env: &DatabaseEnv,
    path: &Path,
    options: &SubmoduleMigrationOptions,
) -> eyre::Result<MigrationStats> {
    let started = Instant::now();
    let (row, path_hash_rows, legacy_rows) = {
        let read = env.tx()?;
        let row = read.get::<Metadata>(MetadataKey::DBSchemaVersion)?;
        let path_hash_rows = count_u64(read.entries::<ChunkPathHashesByOffset>()?);
        let legacy_rows = legacy_row_count(&read)?;
        (row, path_hash_rows, legacy_rows)
    };

    let stats = match classify(&row) {
        SchemaRow::Current => MigrationStats {
            detected: DetectedSubmoduleSchema::V2,
            migrated: false,
            path_hash_rows,
            data_paths_copied: 0,
            intervals_written: 0,
            legacy_data_paths_deleted: 0,
            elapsed: started.elapsed(),
        },
        SchemaRow::Foreign => {
            eyre::bail!(
                "submodule index schema row does not match this binary ({})",
                path.display()
            );
        }
        SchemaRow::ConvertIncomplete => return Err(incomplete_mdbx_conversion(path)),
        SchemaRow::Absent if path_hash_rows == 0 && legacy_rows == 0 => {
            // `CONVERTING` is synced before the convert-incomplete schema row.
            // A crash in that window has no row and no v1 rows. Stamping it
            // would publish an empty current index.
            if converting_sentinel_present(path)? {
                return Err(incomplete_mdbx_conversion(path));
            }
            stamp_schema(env)?;
            MigrationStats {
                detected: DetectedSubmoduleSchema::Empty,
                migrated: false,
                path_hash_rows: 0,
                data_paths_copied: 0,
                intervals_written: 0,
                legacy_data_paths_deleted: 0,
                elapsed: started.elapsed(),
            }
        }
        SchemaRow::Absent => run_migration(env, path, options, Progress::new(), started)?,
        SchemaRow::Progress(progress) => run_migration(env, path, options, progress, started)?,
    };
    log_open(path, &stats);
    Ok(stats)
}

#[cfg(test)]
thread_local! {
    static DURING_MIGRATION: std::cell::RefCell<Option<Box<dyn FnOnce(&Path)>>> =
        const { std::cell::RefCell::new(None) };
}

fn run_migration(
    env: &DatabaseEnv,
    path: &Path,
    options: &SubmoduleMigrationOptions,
    mut progress: Progress,
    started: Instant,
) -> eyre::Result<MigrationStats> {
    if options.fail_after_commits == Some(0) {
        eyre::bail!("injected migration interrupt after 0 commits");
    }
    // The env for this run is already open. A test uses that window to try a
    // second open before any migration commit.
    #[cfg(test)]
    if let Some(hook) = DURING_MIGRATION.with(|cell| cell.borrow_mut().take()) {
        hook(path);
    }
    log_started(path, &progress);
    let ctx = MigrateCtx {
        env,
        path,
        batch_rows: options.batch_rows.max(1),
        batch_value_bytes: options.batch_value_bytes.max(1),
        base_ms: progress.elapsed_ms,
        started,
    };
    let mut commits = 0_u32;
    loop {
        let phase = progress.phase;
        match phase {
            Phase::Copy => {
                let next_offset = progress.next_offset;
                let batch = {
                    let read = env.tx()?;
                    read_copy_batch(&read, next_offset, ctx.batch_rows, ctx.batch_value_bytes)?
                };
                apply_copy(&ctx, batch, &mut progress)?;
                commits += 1;
                interrupt_after(options, commits)?;
            }
            Phase::Drop => {
                let drop_next = progress.drop_next;
                let batch = {
                    let read = env.tx()?;
                    read_drop_batch(&read, drop_next, ctx.batch_rows, ctx.batch_value_bytes)?
                };
                let done = apply_drop(&ctx, batch, &mut progress)?;
                commits += 1;
                interrupt_after(options, commits)?;
                if done {
                    return Ok(stats_from(&progress));
                }
            }
        }
    }
}

fn interrupt_after(options: &SubmoduleMigrationOptions, commits: u32) -> eyre::Result<()> {
    if options.fail_after_commits == Some(commits) {
        eyre::bail!("injected migration interrupt after {commits} commits");
    }
    Ok(())
}

struct MigrateCtx<'a> {
    env: &'a DatabaseEnv,
    path: &'a Path,
    batch_rows: usize,
    batch_value_bytes: usize,
    base_ms: u64,
    started: Instant,
}

struct PreparedRow {
    offset: PartitionChunkOffset,
    hashes: ChunkPathHashes,
    data_path: Option<ChunkDataPath>,
}

struct CopyBatch {
    rows: Vec<PreparedRow>,
    exhausted: bool,
    next_offset: u64,
}

struct DropBatch {
    keys: Vec<ChunkPathHash>,
    next_key: Option<ChunkPathHash>,
    exhausted: bool,
}

fn read_copy_batch<T: DbTx>(
    tx: &T,
    next_offset: u64,
    batch_rows: usize,
    batch_value_bytes: usize,
) -> eyre::Result<CopyBatch> {
    let Ok(start_offset) = u32::try_from(next_offset) else {
        return Ok(CopyBatch {
            rows: Vec::new(),
            exhausted: true,
            next_offset,
        });
    };
    let start = PartitionChunkOffset(start_offset);
    let mut cursor = tx.cursor_read::<ChunkPathHashesByOffset>()?;
    let mut current = cursor.seek(start)?;
    let mut rows = Vec::new();
    let mut bytes = 0_usize;
    let mut exhausted = true;
    while let Some((offset, hashes)) = current {
        if offset < start {
            current = cursor.next()?;
            continue;
        }
        let at_limit = !rows.is_empty() && (rows.len() >= batch_rows || bytes >= batch_value_bytes);
        if at_limit {
            exhausted = false;
            break;
        }
        let data_path = read_data_path(tx, offset, hashes.data_path_hash)?;
        if let Some(path) = &data_path {
            bytes = bytes.saturating_add(path.len());
        }
        rows.push(PreparedRow {
            offset,
            hashes,
            data_path,
        });
        current = cursor.next()?;
    }
    let next_offset = match rows.last() {
        Some(row) => u64::from(row.offset.0).saturating_add(1),
        None => next_offset,
    };
    Ok(CopyBatch {
        rows,
        exhausted,
        next_offset,
    })
}

fn read_data_path<T: DbTx>(
    tx: &T,
    offset: PartitionChunkOffset,
    hash: Option<ChunkPathHash>,
) -> eyre::Result<Option<ChunkDataPath>> {
    let Some(hash) = hash else {
        return Ok(None);
    };
    match tx.get::<ChunkDataPathByPathHash>(hash) {
        Ok(Some(path)) => Ok(Some(path)),
        Ok(None) => eyre::bail!(
            "submodule index v1 data path for offset {} hash {hash:x} is missing",
            offset.0
        ),
        Err(err) if missing_table(&err) => eyre::bail!(
            "submodule index v1 data path table is missing for offset {} hash {hash:x}",
            offset.0
        ),
        Err(err) => Err(err.into()),
    }
}

fn apply_copy(ctx: &MigrateCtx<'_>, batch: CopyBatch, progress: &mut Progress) -> eyre::Result<()> {
    if batch.rows.is_empty() && !batch.exhausted {
        eyre::bail!("submodule index migration scan made no progress");
    }
    if !batch.rows.is_empty() && batch.next_offset <= progress.next_offset {
        eyre::bail!("submodule index migration cursor did not advance");
    }
    let scanned_before = progress.scanned;
    let mut open = progress.open;
    let mut closed = Vec::new();
    let write = ctx.env.tx_mut()?;
    // A peer can finish and publish the schema between the read of this batch
    // and this commit. The batch was read from the old rows. Writing it back
    // would replace the current schema with a progress marker after the v1
    // path table is already gone.
    refuse_finished_schema(&write)?;
    for row in batch.rows {
        absorb(
            &mut open,
            row.offset.0,
            row.hashes.tx_path_hash,
            &mut closed,
        );
        if let Some(data_path) = row.data_path {
            write.put::<ChunkDataPathByOffset>(row.offset, data_path)?;
            progress.copied += 1;
        }
        progress.scanned += 1;
    }
    if batch.exhausted
        && let Some(run) = open.take()
    {
        closed.push(run);
    }
    for run in closed {
        write.put::<TxPathIntervalByStart>(
            PartitionChunkOffset(run.start),
            TxPathInterval {
                end: PartitionChunkOffset(run.end),
                tx_path_hash: run.tx_hash,
            },
        )?;
        progress.intervals += 1;
    }
    progress.open = open;
    progress.next_offset = batch.next_offset;
    let finished_copy = batch.exhausted;
    if finished_copy {
        progress.phase = Phase::Drop;
        progress.drop_next = None;
    }
    touch_elapsed(progress, ctx.base_ms, ctx.started);
    write.put::<Metadata>(MetadataKey::DBSchemaVersion, encode_progress(progress))?;
    write.commit()?;
    if progress.scanned / PROGRESS_LOG_ROWS != scanned_before / PROGRESS_LOG_ROWS {
        log_progress(ctx.path, progress);
    }
    if finished_copy {
        tracing::info!(
            path = %ctx.path.display(),
            path_hash_rows = progress.scanned,
            data_paths_copied = progress.copied,
            intervals = progress.intervals,
            "submodule index v1 rows copied"
        );
    }
    Ok(())
}

pub(super) fn absorb(
    open: &mut Option<OpenRun>,
    offset: u32,
    tx_hash: Option<H256>,
    closed: &mut Vec<OpenRun>,
) {
    let continues = open
        .as_ref()
        .is_some_and(|run| tx_hash == Some(run.tx_hash) && run.end.checked_add(1) == Some(offset));
    if continues && let Some(run) = open.as_mut() {
        run.end = offset;
        return;
    }
    if let Some(run) = open.take() {
        closed.push(run);
    }
    if let Some(tx_hash) = tx_hash {
        *open = Some(OpenRun {
            start: offset,
            end: offset,
            tx_hash,
        });
    }
}

fn read_drop_batch<T: DbTx>(
    tx: &T,
    drop_next: Option<H256>,
    batch_rows: usize,
    batch_value_bytes: usize,
) -> eyre::Result<DropBatch> {
    let mut cursor = match tx.cursor_read::<ChunkDataPathByPathHash>() {
        Ok(cursor) => cursor,
        Err(err) if missing_table(&err) => {
            return Ok(DropBatch {
                keys: Vec::new(),
                next_key: None,
                exhausted: true,
            });
        }
        Err(err) => return Err(err.into()),
    };
    let mut current = match drop_next {
        Some(key) => cursor.seek(key)?,
        None => cursor.first()?,
    };
    let mut keys = Vec::new();
    let mut bytes = 0_usize;
    while let Some((key, value)) = current {
        let at_limit = !keys.is_empty() && (keys.len() >= batch_rows || bytes >= batch_value_bytes);
        if at_limit {
            return Ok(DropBatch {
                keys,
                next_key: Some(key),
                exhausted: false,
            });
        }
        bytes = bytes.saturating_add(value.len());
        keys.push(key);
        current = cursor.next()?;
    }
    Ok(DropBatch {
        keys,
        next_key: None,
        exhausted: true,
    })
}

fn apply_drop(
    ctx: &MigrateCtx<'_>,
    batch: DropBatch,
    progress: &mut Progress,
) -> eyre::Result<bool> {
    if batch.keys.is_empty() && !batch.exhausted {
        eyre::bail!("submodule index migration delete made no progress");
    }
    if !batch.exhausted && batch.next_key.is_none() {
        eyre::bail!("submodule index migration delete cursor did not advance");
    }
    let write = ctx.env.tx_mut()?;
    refuse_finished_schema(&write)?;
    for key in &batch.keys {
        write.delete::<ChunkDataPathByPathHash>(*key, None)?;
    }
    progress.deleted += count_u64(batch.keys.len());
    progress.drop_next = batch.next_key;
    touch_elapsed(progress, ctx.base_ms, ctx.started);
    if batch.exhausted {
        write.put::<Metadata>(
            MetadataKey::DBSchemaVersion,
            SUBMODULE_SCHEMA.as_bytes().to_vec(),
        )?;
        write.commit()?;
        return Ok(true);
    }
    write.put::<Metadata>(MetadataKey::DBSchemaVersion, encode_progress(progress))?;
    write.commit()?;
    Ok(false)
}

fn refuse_finished_schema<T: DbTx>(tx: &T) -> eyre::Result<()> {
    let row = tx.get::<Metadata>(MetadataKey::DBSchemaVersion)?;
    if row.as_deref() == Some(SUBMODULE_SCHEMA.as_bytes()) {
        eyre::bail!("submodule index migration refuses to replace the current schema row");
    }
    Ok(())
}

fn incomplete_mdbx_conversion(path: &Path) -> eyre::Report {
    eyre::eyre!(
        "submodule index {} is an incomplete MDBX conversion and has no completion marker. Delete this directory and rerun `submodule-index-convert rocks-to-mdbx` with --replace-incomplete. Do not start the node on it. The RocksDB source is unchanged. If a completed rocks/ index has rows written after the switch, that command is also the rollback.",
        path.display()
    )
}

/// `true` when `CONVERTING` is the convert-incomplete sentinel.
pub(super) fn converting_sentinel_present(path: &Path) -> eyre::Result<bool> {
    let file_path = path.join("CONVERTING");
    if !file_path.is_file() {
        return Ok(false);
    }
    // Any file counts: a crash between create and write leaves it short.
    Ok(true)
}

fn stamp_schema(env: &DatabaseEnv) -> eyre::Result<()> {
    let write = env.tx_mut()?;
    write.put::<Metadata>(
        MetadataKey::DBSchemaVersion,
        SUBMODULE_SCHEMA.as_bytes().to_vec(),
    )?;
    write.commit()?;
    Ok(())
}

fn touch_elapsed(progress: &mut Progress, base_ms: u64, started: Instant) {
    let extra = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    progress.elapsed_ms = base_ms.saturating_add(extra);
}

fn stats_from(progress: &Progress) -> MigrationStats {
    MigrationStats {
        detected: DetectedSubmoduleSchema::V1,
        migrated: true,
        path_hash_rows: progress.scanned,
        data_paths_copied: progress.copied,
        intervals_written: progress.intervals,
        legacy_data_paths_deleted: progress.deleted,
        elapsed: Duration::from_millis(progress.elapsed_ms),
    }
}

fn legacy_row_count<T: DbTx>(tx: &T) -> eyre::Result<u64> {
    match tx.entries::<ChunkDataPathByPathHash>() {
        Ok(count) => Ok(count_u64(count)),
        Err(err) if missing_table(&err) => Ok(0),
        Err(err) => Err(err.into()),
    }
}

pub(super) fn missing_table(err: &DatabaseError) -> bool {
    let DatabaseError::Open(info) = err else {
        return false;
    };
    *info == MdbxError::NotFound.into()
}

fn count_u64(count: usize) -> u64 {
    u64::try_from(count).unwrap_or(u64::MAX)
}

/// `true` when open must copy schema v1 rows or resume a progress marker.
///
/// An empty directory, a current schema row, a foreign row, and an incomplete
/// conversion are not migrations. Those stay on the caller's open args.
pub(super) fn migration_required(env: &DatabaseEnv) -> eyre::Result<bool> {
    let read = env.tx()?;
    let row = read.get::<Metadata>(MetadataKey::DBSchemaVersion)?;
    let path_hash_rows = count_u64(read.entries::<ChunkPathHashesByOffset>()?);
    let legacy_rows = legacy_row_count(&read)?;
    Ok(match classify(&row) {
        SchemaRow::Progress(_) => true,
        SchemaRow::Absent => path_hash_rows > 0 || legacy_rows > 0,
        SchemaRow::Current | SchemaRow::Foreign | SchemaRow::ConvertIncomplete => false,
    })
}

fn log_open(path: &Path, stats: &MigrationStats) {
    let elapsed_ms = u64::try_from(stats.elapsed.as_millis()).unwrap_or(u64::MAX);
    tracing::info!(
        path = %path.display(),
        detected = stats.detected.as_str(),
        migrated = stats.migrated,
        path_hash_rows = stats.path_hash_rows,
        data_paths_copied = stats.data_paths_copied,
        intervals = stats.intervals_written,
        legacy_data_paths_deleted = stats.legacy_data_paths_deleted,
        elapsed_ms,
        "submodule index schema"
    );
}

fn log_started(path: &Path, progress: &Progress) {
    let resumed = progress.scanned > 0 || progress.phase == Phase::Drop || progress.deleted > 0;
    tracing::info!(
        path = %path.display(),
        detected = "v1",
        resumed,
        path_hash_rows = progress.scanned,
        "submodule index migration started"
    );
}

fn log_progress(path: &Path, progress: &Progress) {
    tracing::info!(
        path = %path.display(),
        path_hash_rows = progress.scanned,
        data_paths_copied = progress.copied,
        intervals = progress.intervals,
        legacy_data_paths_deleted = progress.deleted,
        elapsed_ms = progress.elapsed_ms,
        "submodule index migration progress"
    );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Copy,
    Drop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct OpenRun {
    pub(super) start: u32,
    pub(super) end: u32,
    pub(super) tx_hash: H256,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Progress {
    phase: Phase,
    next_offset: u64,
    open: Option<OpenRun>,
    drop_next: Option<H256>,
    scanned: u64,
    copied: u64,
    intervals: u64,
    deleted: u64,
    elapsed_ms: u64,
}

impl Progress {
    fn new() -> Self {
        Self {
            phase: Phase::Copy,
            next_offset: 0,
            open: None,
            drop_next: None,
            scanned: 0,
            copied: 0,
            intervals: 0,
            deleted: 0,
            elapsed_ms: 0,
        }
    }
}

enum SchemaRow {
    Absent,
    Current,
    Foreign,
    /// `submodule-index-convert` is copying into this directory.
    ConvertIncomplete,
    Progress(Progress),
}

fn classify(bytes: &Option<Vec<u8>>) -> SchemaRow {
    let Some(bytes) = bytes else {
        return SchemaRow::Absent;
    };
    if bytes.as_slice() == SUBMODULE_SCHEMA.as_bytes() {
        return SchemaRow::Current;
    }
    if bytes.as_slice() == SUBMODULE_CONVERT_INCOMPLETE.as_bytes() {
        return SchemaRow::ConvertIncomplete;
    }
    match decode_progress(bytes) {
        Some(progress) => SchemaRow::Progress(progress),
        None => SchemaRow::Foreign,
    }
}

fn encode_progress(progress: &Progress) -> Vec<u8> {
    let body = match progress.phase {
        Phase::Copy => format!(
            "{PROGRESS_PREFIX}copy {} {} {} {} {} {}\n",
            progress.next_offset,
            encode_open(progress.open),
            progress.scanned,
            progress.copied,
            progress.intervals,
            progress.elapsed_ms
        ),
        Phase::Drop => format!(
            "{PROGRESS_PREFIX}drop {} {} {} {} {} {}\n",
            encode_key(progress.drop_next),
            progress.scanned,
            progress.copied,
            progress.intervals,
            progress.deleted,
            progress.elapsed_ms
        ),
    };
    body.into_bytes()
}

fn encode_open(open: Option<OpenRun>) -> String {
    match open {
        None => "-".to_owned(),
        Some(run) => format!("{},{},{}", run.start, run.end, hex_hash(run.tx_hash)),
    }
}

fn encode_key(key: Option<H256>) -> String {
    match key {
        None => "-".to_owned(),
        Some(hash) => hex_hash(hash),
    }
}

fn decode_progress(bytes: &[u8]) -> Option<Progress> {
    if bytes.len() > MAX_MARKER_BYTES {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let text = text.strip_suffix('\n')?;
    let rest = text.strip_prefix(PROGRESS_PREFIX)?;
    let mut parts = rest.split(' ');
    let progress = match parts.next()? {
        "copy" => decode_copy(&mut parts)?,
        "drop" => decode_drop(&mut parts)?,
        _ => return None,
    };
    if parts.next().is_some() {
        return None;
    }
    Some(progress)
}

fn decode_copy(parts: &mut std::str::Split<'_, char>) -> Option<Progress> {
    let next_offset = parts.next()?.parse().ok()?;
    let open = decode_open(parts.next()?)?;
    let scanned = parts.next()?.parse().ok()?;
    let copied = parts.next()?.parse().ok()?;
    let intervals = parts.next()?.parse().ok()?;
    let elapsed_ms = parts.next()?.parse().ok()?;
    Some(Progress {
        phase: Phase::Copy,
        next_offset,
        open,
        drop_next: None,
        scanned,
        copied,
        intervals,
        deleted: 0,
        elapsed_ms,
    })
}

fn decode_drop(parts: &mut std::str::Split<'_, char>) -> Option<Progress> {
    let drop_next = decode_key(parts.next()?)?;
    let scanned = parts.next()?.parse().ok()?;
    let copied = parts.next()?.parse().ok()?;
    let intervals = parts.next()?.parse().ok()?;
    let deleted = parts.next()?.parse().ok()?;
    let elapsed_ms = parts.next()?.parse().ok()?;
    Some(Progress {
        phase: Phase::Drop,
        next_offset: 0,
        open: None,
        drop_next,
        scanned,
        copied,
        intervals,
        deleted,
        elapsed_ms,
    })
}

fn decode_open(text: &str) -> Option<Option<OpenRun>> {
    if text == "-" {
        return Some(None);
    }
    let (start, rest) = text.split_once(',')?;
    let (end, hash) = rest.split_once(',')?;
    let start = start.parse().ok()?;
    let end = end.parse().ok()?;
    if end < start {
        return None;
    }
    Some(Some(OpenRun {
        start,
        end,
        tx_hash: parse_hash(hash)?,
    }))
}

fn decode_key(text: &str) -> Option<Option<H256>> {
    if text == "-" {
        return Some(None);
    }
    Some(Some(parse_hash(text)?))
}

fn hex_hash(hash: H256) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in hash.0 {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

fn parse_hash(text: &str) -> Option<H256> {
    if text.len() != 64 {
        return None;
    }
    let mut bytes = [0_u8; 32];
    for (index, slot) in bytes.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(H256(bytes))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::{H256, PartitionChunkOffset, RelativeChunkOffset};
    use reth_db::Database as _;
    use reth_db::cursor::DbCursorRO as _;
    use reth_db::mdbx::DatabaseArguments;
    use reth_db::transaction::{DbTx as _, DbTxMut as _};

    use super::super::db::{
        add_tx_path_hash_to_offset_range, get_data_path_by_offset,
        get_data_root_infos_for_data_root, get_path_hashes_by_offset, get_pending_body_migration,
        get_tx_leaf_binding, get_tx_path_by_offset,
    };
    use super::super::tables::v1::ChunkDataPathByPathHash;
    use super::super::tables::v1_fixture::{mainnet, master};
    use super::super::tables::{
        ChunkDataPathByOffset, ChunkPathHashes, ChunkPathHashesByOffset, DataRootInfo,
        DataRootInfos, Metadata, PendingBodyMigration, SUBMODULE_SCHEMA, SubmoduleTables,
        TxLeafBinding, TxPathIntervalByStart,
    };
    use super::{
        DetectedSubmoduleSchema, Phase, Progress, SubmoduleMigrationOptions, decode_progress,
        encode_progress, require_submodule_index_copy,
    };
    use crate::metadata::MetadataKey;
    use crate::submodule::{create_or_open_submodule_db, open_submodule_index};
    use crate::submodule::{open_pristine_v1_read_only, verify_submodule_read_equivalence};
    use crate::{IrysDatabaseArgs as _, open_or_create_db};

    fn h(byte: u8) -> H256 {
        H256::repeat_byte(byte)
    }

    fn testing_args() -> eyre::Result<DatabaseArguments> {
        DatabaseArguments::irys_testing()
    }

    fn opts(batch_rows: usize, fail_after_commits: Option<u32>) -> SubmoduleMigrationOptions {
        SubmoduleMigrationOptions {
            fail_after_commits,
            batch_rows,
            batch_value_bytes: 8 * 1024 * 1024,
        }
    }

    fn temp(prefix: &str) -> tempfile::TempDir {
        TempDirBuilder::new().prefix(prefix).build()
    }

    struct Row {
        offset: u32,
        data: Option<(H256, Vec<u8>)>,
        tx: Option<H256>,
    }

    struct Fixture {
        rows: Vec<Row>,
        tx_paths: Vec<(H256, Vec<u8>)>,
        data_root: Option<(H256, DataRootInfos)>,
        binding: Option<(H256, TxLeafBinding)>,
        pending: Option<(u32, PendingBodyMigration)>,
    }

    fn write_master(path: &Path, fixture: &Fixture) -> eyre::Result<()> {
        let db = open_or_create_db(path, master::MasterV1Tables::ALL, testing_args()?)?;
        put_fixture(&db, fixture, true)
    }

    fn write_mainnet(path: &Path, fixture: &Fixture) -> eyre::Result<()> {
        let db = open_or_create_db(path, mainnet::MainnetV1Tables::ALL, testing_args()?)?;
        put_fixture(&db, fixture, false)
    }

    fn put_fixture(
        db: &reth_db::DatabaseEnv,
        fixture: &Fixture,
        pending: bool,
    ) -> eyre::Result<()> {
        let tx = db.tx_mut()?;
        for row in &fixture.rows {
            tx.put::<master::ChunkPathHashesByOffset>(
                PartitionChunkOffset(row.offset),
                ChunkPathHashes {
                    data_path_hash: row.data.as_ref().map(|(hash, _)| *hash),
                    tx_path_hash: row.tx,
                },
            )?;
            if let Some((hash, bytes)) = &row.data {
                tx.put::<master::ChunkDataPathByPathHash>(*hash, bytes.clone())?;
            }
        }
        for (hash, bytes) in &fixture.tx_paths {
            tx.put::<master::TxPathByTxPathHash>(*hash, bytes.clone())?;
        }
        if let Some((root, infos)) = &fixture.data_root {
            tx.put::<master::DataRootInfosByDataRoot>(*root, infos.clone())?;
        }
        if let Some((hash, binding)) = &fixture.binding {
            tx.put::<master::TxLeafBindingByTxPathHash>(*hash, binding.clone())?;
        }
        if pending && let Some((offset, job)) = &fixture.pending {
            tx.put::<master::PendingBodyMigrationsByOffset>(
                PartitionChunkOffset(*offset),
                job.clone(),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn write_contiguous(path: &Path, count: u32) -> eyre::Result<()> {
        let tx_hash = h(1);
        let mut rows = Vec::new();
        for offset in 0..count {
            let data_hash = h(u8::try_from(offset).unwrap_or(0).saturating_add(10));
            rows.push(Row {
                offset,
                data: Some((data_hash, vec![u8::try_from(offset).unwrap_or(0)])),
                tx: Some(tx_hash),
            });
        }
        write_master(
            path,
            &Fixture {
                rows,
                tx_paths: vec![(tx_hash, b"tx".to_vec())],
                data_root: None,
                binding: None,
                pending: None,
            },
        )
    }

    fn master_fixture() -> Fixture {
        let tx_a = h(1);
        let tx_b = h(2);
        let tx_c = h(3);
        let shared = h(19);
        let row = |offset, data_byte: Option<u8>, tx| Row {
            offset,
            data: data_byte.map(|byte| (h(byte), vec![byte])),
            tx,
        };
        Fixture {
            rows: vec![
                row(0, Some(10), Some(tx_a)),
                row(1, Some(11), Some(tx_a)),
                row(2, Some(12), Some(tx_a)),
                Row {
                    offset: 3,
                    data: None,
                    tx: None,
                },
                row(4, Some(14), None),
                row(5, None, Some(tx_b)),
                row(6, Some(16), Some(tx_a)),
                row(8, Some(18), Some(tx_a)),
                Row {
                    offset: 9,
                    data: Some((shared, b"shared".to_vec())),
                    tx: Some(tx_c),
                },
                Row {
                    offset: 10,
                    data: Some((shared, b"shared".to_vec())),
                    tx: Some(tx_c),
                },
            ],
            tx_paths: vec![
                (tx_a, b"tx-a".to_vec()),
                (tx_b, b"tx-b".to_vec()),
                (tx_c, b"tx-c".to_vec()),
            ],
            data_root: Some((
                h(40),
                DataRootInfos(vec![DataRootInfo {
                    start_offset: RelativeChunkOffset(3),
                    data_size: 64,
                }]),
            )),
            binding: Some((
                tx_a,
                TxLeafBinding {
                    data_root: h(40),
                    prefix_hash: h(41),
                },
            )),
            pending: Some((
                0,
                PendingBodyMigration {
                    data_root: h(40),
                    data_size: 64,
                    start_offset: RelativeChunkOffset(3),
                    block_height: 9,
                    attempts: 1,
                },
            )),
        }
    }

    fn schema_of(env: &reth_db::DatabaseEnv) -> eyre::Result<Option<Vec<u8>>> {
        let tx = env.tx()?;
        Ok(tx.get::<Metadata>(MetadataKey::DBSchemaVersion)?)
    }

    fn legacy_of(env: &reth_db::DatabaseEnv) -> eyre::Result<u64> {
        let tx = env.tx()?;
        super::legacy_row_count(&tx)
    }

    fn intervals(env: &reth_db::DatabaseEnv) -> eyre::Result<Vec<(u32, u32, H256)>> {
        let tx = env.tx()?;
        let mut cursor = tx.cursor_read::<TxPathIntervalByStart>()?;
        let mut rows = Vec::new();
        for row in cursor.walk(None)? {
            let (start, value) = row?;
            rows.push((start.0, value.end.0, value.tx_path_hash));
        }
        Ok(rows)
    }

    fn assert_master_answers(env: &reth_db::DatabaseEnv) -> eyre::Result<()> {
        let tx_a = h(1);
        let tx_b = h(2);
        let tx_c = h(3);
        let tx = env.tx()?;
        let hashes = |offset: u32| get_path_hashes_by_offset(&tx, PartitionChunkOffset(offset));
        assert_eq!(hashes(0)?.unwrap().data_path_hash, Some(h(10)));
        assert_eq!(hashes(0)?.unwrap().tx_path_hash, Some(tx_a));
        assert_eq!(hashes(2)?.unwrap().tx_path_hash, Some(tx_a));
        assert_eq!(hashes(3)?, Some(ChunkPathHashes::default()));
        assert_eq!(hashes(4)?.unwrap().data_path_hash, Some(h(14)));
        assert_eq!(hashes(4)?.unwrap().tx_path_hash, None);
        assert_eq!(hashes(5)?.unwrap().data_path_hash, None);
        assert_eq!(hashes(5)?.unwrap().tx_path_hash, Some(tx_b));
        assert_eq!(hashes(6)?.unwrap().tx_path_hash, Some(tx_a));
        assert_eq!(hashes(7)?, None);
        assert_eq!(hashes(8)?.unwrap().tx_path_hash, Some(tx_a));
        assert_eq!(hashes(9)?.unwrap().tx_path_hash, Some(tx_c));
        assert_eq!(
            get_data_path_by_offset(&tx, PartitionChunkOffset(0))?,
            Some(vec![10])
        );
        assert_eq!(get_data_path_by_offset(&tx, PartitionChunkOffset(3))?, None);
        assert_eq!(get_data_path_by_offset(&tx, PartitionChunkOffset(5))?, None);
        assert_eq!(
            get_data_path_by_offset(&tx, PartitionChunkOffset(9))?,
            Some(b"shared".to_vec())
        );
        assert_eq!(
            get_data_path_by_offset(&tx, PartitionChunkOffset(10))?,
            Some(b"shared".to_vec())
        );
        assert_eq!(
            get_tx_path_by_offset(&tx, PartitionChunkOffset(0))?,
            Some(b"tx-a".to_vec())
        );
        assert_eq!(get_tx_path_by_offset(&tx, PartitionChunkOffset(3))?, None);
        assert_eq!(get_tx_path_by_offset(&tx, PartitionChunkOffset(4))?, None);
        assert_eq!(
            get_tx_path_by_offset(&tx, PartitionChunkOffset(5))?,
            Some(b"tx-b".to_vec())
        );
        assert_eq!(
            get_tx_path_by_offset(&tx, PartitionChunkOffset(6))?,
            Some(b"tx-a".to_vec())
        );
        assert_eq!(
            get_tx_path_by_offset(&tx, PartitionChunkOffset(9))?,
            Some(b"tx-c".to_vec())
        );
        let stored = tx
            .get::<ChunkPathHashesByOffset>(PartitionChunkOffset(0))?
            .unwrap();
        assert_eq!(stored.tx_path_hash, Some(tx_a));
        assert_eq!(
            get_data_root_infos_for_data_root(&tx, h(40))?.unwrap().0,
            vec![DataRootInfo {
                start_offset: RelativeChunkOffset(3),
                data_size: 64,
            }]
        );
        assert_eq!(
            get_tx_leaf_binding(&tx, tx_a)?,
            Some(TxLeafBinding {
                data_root: h(40),
                prefix_hash: h(41),
            })
        );
        assert_eq!(
            get_pending_body_migration(&tx, PartitionChunkOffset(0))?
                .unwrap()
                .block_height,
            9
        );
        assert_eq!(
            schema_of(env)?.as_deref(),
            Some(SUBMODULE_SCHEMA.as_bytes())
        );
        assert_eq!(legacy_of(env)?, 0);
        Ok(())
    }

    #[test]
    fn progress_marker_roundtrip() {
        let copy = Progress {
            phase: Phase::Copy,
            next_offset: 12,
            open: Some(super::OpenRun {
                start: 4,
                end: 11,
                tx_hash: h(7),
            }),
            drop_next: None,
            scanned: 12,
            copied: 9,
            intervals: 2,
            deleted: 0,
            elapsed_ms: 15,
        };
        assert_eq!(decode_progress(&encode_progress(&copy)), Some(copy));

        let drop_at = Progress {
            phase: Phase::Drop,
            next_offset: 0,
            open: None,
            drop_next: Some(h(9)),
            scanned: 12,
            copied: 9,
            intervals: 2,
            deleted: 3,
            elapsed_ms: 20,
        };
        assert_eq!(decode_progress(&encode_progress(&drop_at)), Some(drop_at));

        let drop_start = Progress {
            drop_next: None,
            deleted: 0,
            ..drop_at
        };
        assert_eq!(
            decode_progress(&encode_progress(&drop_start)),
            Some(drop_start)
        );
        assert!(decode_progress(SUBMODULE_SCHEMA.as_bytes()).is_none());
        assert!(decode_progress(b"irys-submodule-index migrate-v1 copy nope\n").is_none());
    }

    #[test]
    fn copy_guard_rejects_a_live_name_and_a_symlink_to_one() -> eyre::Result<()> {
        assert!(require_submodule_index_copy(Path::new("db")).is_err());
        assert!(require_submodule_index_copy(Path::new("db.copy")).is_ok());
        assert!(require_submodule_index_copy(Path::new("DB-COPY")).is_ok());

        let dir = temp("copy-guard");
        let live = dir.path().join("db");
        std::fs::create_dir(&live)?;
        let link = dir.path().join("db.copy");
        std::os::unix::fs::symlink(&live, &link)?;
        assert!(require_submodule_index_copy(&link).is_err());
        let real = dir.path().join("real-db.copy");
        std::fs::create_dir(&real)?;
        assert!(require_submodule_index_copy(&real).is_ok());
        Ok(())
    }

    #[test]
    fn empty_db_is_stamped_and_a_second_open_leaves_it() -> eyre::Result<()> {
        let dir = temp("sm-v1-empty");
        let args = testing_args()?;
        let (env, stats) = open_submodule_index(
            dir.path(),
            args.clone(),
            SubmoduleMigrationOptions::default(),
        )?;
        assert_eq!(stats.detected, DetectedSubmoduleSchema::Empty);
        assert!(!stats.migrated);
        assert_eq!(
            schema_of(&env)?.as_deref(),
            Some(SUBMODULE_SCHEMA.as_bytes())
        );
        let tx = env.tx()?;
        assert!(matches!(
            tx.entries::<ChunkDataPathByPathHash>(),
            Err(err) if super::missing_table(&err)
        ));
        drop(tx);
        drop(env);

        let (env, stats) =
            open_submodule_index(dir.path(), args, SubmoduleMigrationOptions::default())?;
        assert_eq!(stats.detected, DetectedSubmoduleSchema::V2);
        assert!(!stats.migrated);
        assert_eq!(
            schema_of(&env)?.as_deref(),
            Some(SUBMODULE_SCHEMA.as_bytes())
        );
        Ok(())
    }

    /// `CONVERTING` is written before the schema row. A short file is the same
    /// unfinished conversion: open must not stamp the directory.
    #[test]
    fn converting_sentinel_is_not_stamped() -> eyre::Result<()> {
        for (label, bytes) in [
            ("full", super::SUBMODULE_CONVERT_INCOMPLETE.as_bytes()),
            ("torn", &b""[..]),
        ] {
            let dir = temp(&format!("sm-converting-{label}"));
            std::fs::write(dir.path().join("CONVERTING"), bytes)?;
            let args = testing_args()?;
            let err = open_submodule_index(
                dir.path(),
                args.clone(),
                SubmoduleMigrationOptions::default(),
            )
            .unwrap_err();
            let text = format!("{err:#}");
            assert!(
                text.contains("incomplete MDBX conversion"),
                "{label}: {text}"
            );
            let env = open_or_create_db(dir.path(), SubmoduleTables::ALL, args.clone())?;
            assert_eq!(schema_of(&env)?, None, "{label}");
            drop(env);
            if label == "full" {
                assert_eq!(
                    std::fs::read(dir.path().join("CONVERTING"))?,
                    super::SUBMODULE_CONVERT_INCOMPLETE.as_bytes()
                );
                let err =
                    open_submodule_index(dir.path(), args, SubmoduleMigrationOptions::default())
                        .unwrap_err();
                assert!(
                    format!("{err:#}").contains("incomplete MDBX conversion"),
                    "{err:#}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn stamped_db_is_not_rebuilt_from_old_tx_hashes() -> eyre::Result<()> {
        let dir = temp("sm-v2-stamp");
        let args = testing_args()?;
        let env = create_or_open_submodule_db(dir.path(), args.clone())?;
        {
            let tx = env.tx_mut()?;
            add_tx_path_hash_to_offset_range(
                &tx,
                PartitionChunkOffset(0),
                PartitionChunkOffset(4),
                Some(h(1)),
            )?;
            tx.put::<ChunkPathHashesByOffset>(
                PartitionChunkOffset(0),
                ChunkPathHashes {
                    data_path_hash: Some(h(2)),
                    tx_path_hash: Some(h(9)),
                },
            )?;
            tx.commit()?;
        }
        drop(env);

        let (env, stats) =
            open_submodule_index(dir.path(), args, SubmoduleMigrationOptions::default())?;
        assert_eq!(stats.detected, DetectedSubmoduleSchema::V2);
        assert!(!stats.migrated);
        assert_eq!(stats.data_paths_copied, 0);
        let tx = env.tx()?;
        let hashes = get_path_hashes_by_offset(&tx, PartitionChunkOffset(0))?.unwrap();
        assert_eq!(hashes.tx_path_hash, Some(h(1)));
        let stored = tx
            .get::<ChunkPathHashesByOffset>(PartitionChunkOffset(0))?
            .unwrap();
        assert_eq!(stored.tx_path_hash, Some(h(9)));
        drop(tx);
        assert_eq!(intervals(&env)?, vec![(0, 4, h(1))]);
        Ok(())
    }

    #[test]
    fn foreign_schema_row_is_refused() -> eyre::Result<()> {
        let dir = temp("sm-v1-foreign");
        let args = testing_args()?;
        let db = open_or_create_db(dir.path(), SubmoduleTables::ALL, args.clone())?;
        {
            let tx = db.tx_mut()?;
            tx.put::<Metadata>(
                MetadataKey::DBSchemaVersion,
                b"irys-submodule-index 9\n".to_vec(),
            )?;
            tx.put::<ChunkPathHashesByOffset>(
                PartitionChunkOffset(1),
                ChunkPathHashes {
                    data_path_hash: Some(h(4)),
                    tx_path_hash: Some(h(5)),
                },
            )?;
            tx.commit()?;
        }
        drop(db);

        let err = create_or_open_submodule_db(dir.path(), args.clone()).unwrap_err();
        assert!(err.to_string().contains("does not match this binary"));
        let db = open_or_create_db(dir.path(), SubmoduleTables::ALL, args)?;
        let tx = db.tx()?;
        assert_eq!(
            tx.get::<Metadata>(MetadataKey::DBSchemaVersion)?,
            Some(b"irys-submodule-index 9\n".to_vec())
        );
        assert!(
            tx.get::<ChunkPathHashesByOffset>(PartitionChunkOffset(1))?
                .is_some()
        );
        Ok(())
    }

    #[test]
    fn v1_rows_open_with_the_same_answers_and_a_second_open_is_current() -> eyre::Result<()> {
        let dir = temp("sm-v1-read");
        let args = testing_args()?;
        write_master(dir.path(), &master_fixture())?;
        let env = create_or_open_submodule_db(dir.path(), args.clone())?;
        assert_master_answers(&env)?;
        assert_eq!(
            intervals(&env)?,
            vec![
                (0, 2, h(1)),
                (5, 5, h(2)),
                (6, 6, h(1)),
                (8, 8, h(1)),
                (9, 10, h(3)),
            ]
        );
        drop(env);

        let (env, stats) =
            open_submodule_index(dir.path(), args, SubmoduleMigrationOptions::default())?;
        assert_eq!(stats.detected, DetectedSubmoduleSchema::V2);
        assert!(!stats.migrated);
        assert_master_answers(&env)?;
        Ok(())
    }

    #[test]
    fn mainnet_v1_without_pending_table_migrates() -> eyre::Result<()> {
        let dir = temp("sm-v1-mainnet");
        let args = testing_args()?;
        let mut fixture = master_fixture();
        fixture.pending = None;
        write_mainnet(dir.path(), &fixture)?;
        let (env, stats) =
            open_submodule_index(dir.path(), args, SubmoduleMigrationOptions::default())?;
        assert!(stats.migrated);
        assert_eq!(stats.detected, DetectedSubmoduleSchema::V1);
        let tx = env.tx()?;
        assert_eq!(
            get_tx_path_by_offset(&tx, PartitionChunkOffset(0))?,
            Some(b"tx-a".to_vec())
        );
        assert_eq!(
            get_data_path_by_offset(&tx, PartitionChunkOffset(0))?,
            Some(vec![10])
        );
        assert_eq!(
            get_pending_body_migration(&tx, PartitionChunkOffset(0))?,
            None
        );
        assert_eq!(
            tx.entries::<super::super::tables::PendingBodyMigrationsByOffset>()?,
            0
        );
        drop(tx);
        assert_eq!(legacy_of(&env)?, 0);
        Ok(())
    }

    #[test]
    fn interrupt_before_any_commit_leaves_the_v1_index() -> eyre::Result<()> {
        let dir = temp("sm-v1-fail0");
        let args = testing_args()?;
        write_contiguous(dir.path(), 5)?;
        let err = open_submodule_index(dir.path(), args.clone(), opts(2, Some(0))).unwrap_err();
        assert!(err.to_string().contains("after 0 commits"));

        let env = open_or_create_db(dir.path(), SubmoduleTables::ALL, args.clone())?;
        assert!(schema_of(&env)?.is_none());
        assert_eq!(legacy_of(&env)?, 5);
        let tx = env.tx()?;
        let stored = tx
            .get::<ChunkPathHashesByOffset>(PartitionChunkOffset(4))?
            .unwrap();
        assert_eq!(stored.tx_path_hash, Some(h(1)));
        assert!(get_data_path_by_offset(&tx, PartitionChunkOffset(0))?.is_none());
        drop(tx);
        drop(env);

        let env = create_or_open_submodule_db(dir.path(), args)?;
        let tx = env.tx()?;
        assert_eq!(
            get_data_path_by_offset(&tx, PartitionChunkOffset(4))?,
            Some(vec![4])
        );
        assert_eq!(
            get_tx_path_by_offset(&tx, PartitionChunkOffset(4))?,
            Some(b"tx".to_vec())
        );
        drop(tx);
        assert_eq!(intervals(&env)?, vec![(0, 4, h(1))]);
        Ok(())
    }

    #[test]
    fn interrupt_mid_copy_resumes_one_interval_and_does_not_publish() -> eyre::Result<()> {
        let dir = temp("sm-v1-mid-copy");
        let args = testing_args()?;
        write_contiguous(dir.path(), 5)?;
        let err = open_submodule_index(dir.path(), args.clone(), opts(2, Some(1))).unwrap_err();
        assert!(err.to_string().contains("after 1 commits"));

        let env = open_or_create_db(dir.path(), SubmoduleTables::ALL, args.clone())?;
        let raw = schema_of(&env)?.expect("progress marker");
        assert_ne!(raw.as_slice(), SUBMODULE_SCHEMA.as_bytes());
        let progress = decode_progress(&raw).expect("copy marker");
        assert_eq!(progress.phase, Phase::Copy);
        assert_eq!(progress.next_offset, 2);
        assert_eq!(progress.copied, 2);
        assert_eq!(progress.intervals, 0);
        assert_eq!(
            progress.open,
            Some(super::OpenRun {
                start: 0,
                end: 1,
                tx_hash: h(1),
            })
        );
        assert_eq!(legacy_of(&env)?, 5);
        assert!(intervals(&env)?.is_empty());
        let tx = env.tx()?;
        assert!(get_data_path_by_offset(&tx, PartitionChunkOffset(0))?.is_some());
        assert!(get_data_path_by_offset(&tx, PartitionChunkOffset(1))?.is_some());
        assert!(get_data_path_by_offset(&tx, PartitionChunkOffset(2))?.is_none());
        drop(tx);
        drop(env);

        let (env, stats) =
            open_submodule_index(dir.path(), args, SubmoduleMigrationOptions::default())?;
        assert!(stats.migrated);
        assert_eq!(stats.path_hash_rows, 5);
        assert_eq!(stats.data_paths_copied, 5);
        assert_eq!(stats.intervals_written, 1);
        assert_eq!(stats.legacy_data_paths_deleted, 5);
        let tx = env.tx()?;
        assert_eq!(
            get_path_hashes_by_offset(&tx, PartitionChunkOffset(4))?
                .unwrap()
                .tx_path_hash,
            Some(h(1))
        );
        drop(tx);
        assert_eq!(intervals(&env)?, vec![(0, 4, h(1))]);
        assert_eq!(legacy_of(&env)?, 0);
        Ok(())
    }

    /// A copy batch read before the drop phase must not put a progress marker
    /// on top of the current schema row. The next open still serves the rows.
    #[test]
    fn stale_copy_commit_after_finish_keeps_the_schema() -> eyre::Result<()> {
        let dir = temp("sm-stale-copy");
        let args = testing_args()?;
        write_contiguous(dir.path(), 4)?;
        let preview = open_or_create_db(dir.path(), SubmoduleTables::ALL, args.clone())?;
        let batch = {
            let read = preview.tx()?;
            super::read_copy_batch(&read, 0, 2, 8 * 1024 * 1024)?
        };
        assert_eq!(batch.rows.len(), 2);
        assert!(!batch.exhausted);
        drop(preview);

        let (env, stats) = open_submodule_index(
            dir.path(),
            args.clone(),
            SubmoduleMigrationOptions::default(),
        )?;
        assert!(stats.migrated);
        assert_eq!(stats.detected, DetectedSubmoduleSchema::V1);
        assert_eq!(
            schema_of(&env)?.as_deref(),
            Some(SUBMODULE_SCHEMA.as_bytes())
        );
        assert_eq!(legacy_of(&env)?, 0);
        let tx = env.tx()?;
        assert_eq!(
            get_data_path_by_offset(&tx, PartitionChunkOffset(0))?,
            Some(vec![0])
        );
        drop(tx);
        drop(env);

        let env = open_or_create_db(dir.path(), SubmoduleTables::ALL, args.clone())?;
        let ctx = super::MigrateCtx {
            env: &env,
            path: dir.path(),
            batch_rows: 2,
            batch_value_bytes: 8 * 1024 * 1024,
            base_ms: 0,
            started: std::time::Instant::now(),
        };
        let mut progress = super::Progress::new();
        let err = super::apply_copy(&ctx, batch, &mut progress).unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("current schema"),
            "stale copy commit must be refused, got {text}"
        );
        assert_eq!(
            schema_of(&env)?.as_deref(),
            Some(SUBMODULE_SCHEMA.as_bytes())
        );
        drop(env);

        let (env, stats) =
            open_submodule_index(dir.path(), args, SubmoduleMigrationOptions::default())?;
        assert_eq!(stats.detected, DetectedSubmoduleSchema::V2);
        assert!(!stats.migrated);
        let tx = env.tx()?;
        assert_eq!(
            get_data_path_by_offset(&tx, PartitionChunkOffset(3))?,
            Some(vec![3])
        );
        assert_eq!(
            get_tx_path_by_offset(&tx, PartitionChunkOffset(0))?,
            Some(b"tx".to_vec())
        );
        Ok(())
    }

    /// A drop batch must not replace the current schema row either.
    #[test]
    fn stale_drop_commit_after_finish_keeps_the_schema() -> eyre::Result<()> {
        let dir = temp("sm-stale-drop");
        let args = testing_args()?;
        write_contiguous(dir.path(), 4)?;
        let (env, stats) = open_submodule_index(
            dir.path(),
            args.clone(),
            SubmoduleMigrationOptions::default(),
        )?;
        assert!(stats.migrated);
        assert_eq!(legacy_of(&env)?, 0);
        drop(env);

        let env = open_or_create_db(dir.path(), SubmoduleTables::ALL, args.clone())?;
        let ctx = super::MigrateCtx {
            env: &env,
            path: dir.path(),
            batch_rows: 2,
            batch_value_bytes: 8 * 1024 * 1024,
            base_ms: 0,
            started: std::time::Instant::now(),
        };
        let mut progress = super::Progress {
            phase: super::Phase::Drop,
            ..super::Progress::new()
        };
        let batch = super::DropBatch {
            keys: vec![h(10)],
            next_key: Some(h(11)),
            exhausted: false,
        };
        let err = super::apply_drop(&ctx, batch, &mut progress).unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("current schema"),
            "stale drop commit must be refused, got {text}"
        );
        assert_eq!(
            schema_of(&env)?.as_deref(),
            Some(SUBMODULE_SCHEMA.as_bytes())
        );
        drop(env);

        let (env, stats) =
            open_submodule_index(dir.path(), args, SubmoduleMigrationOptions::default())?;
        assert_eq!(stats.detected, DetectedSubmoduleSchema::V2);
        assert!(!stats.migrated);
        assert_eq!(
            schema_of(&env)?.as_deref(),
            Some(SUBMODULE_SCHEMA.as_bytes())
        );
        Ok(())
    }

    /// The migration env is exclusive for the run. A reader cannot open it
    /// then. After the run returns, the caller's args are in force again and
    /// a reader can open.
    #[test]
    fn migration_exclusive_blocks_a_reader_until_the_run_returns() -> eyre::Result<()> {
        if std::env::var_os("IRYS_MIGRATION_EXCLUSIVE_CHILD").is_some() {
            return reader_during_migration();
        }
        let dir = temp("sm-v1-exclusive");
        let args = testing_args()?;
        write_contiguous(dir.path(), 4)?;
        let path = dir.path().to_path_buf();
        let probe = path.clone();
        super::DURING_MIGRATION.with(|cell| {
            *cell.borrow_mut() = Some(Box::new(move |open_path: &Path| {
                assert_eq!(open_path, probe.as_path());
                let status = spawn_reader(&probe).expect("reader");
                let text = status.text();
                assert_ne!(
                    status.code,
                    Some(0),
                    "a reader opened the index during migration: {text}"
                );
                let lower = text.to_ascii_lowercase();
                assert!(
                    lower.contains("busy") || lower.contains("exclusive") || lower.contains("lock"),
                    "reader error was not a held database: {text}"
                );
            }));
        });
        let (env, stats) =
            open_submodule_index(&path, args.clone(), SubmoduleMigrationOptions::default())?;
        assert!(stats.migrated);
        assert_eq!(
            schema_of(&env)?.as_deref(),
            Some(SUBMODULE_SCHEMA.as_bytes())
        );
        let status = spawn_reader(&path)?;
        let text = status.text();
        assert_eq!(
            status.code,
            Some(0),
            "a reader must open after migration returns: {text}"
        );
        drop(env);
        let (env, stats) = open_submodule_index(&path, args, SubmoduleMigrationOptions::default())?;
        assert_eq!(stats.detected, DetectedSubmoduleSchema::V2);
        assert!(!stats.migrated);
        drop(env);
        Ok(())
    }

    struct ReaderOutput {
        code: Option<i32>,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    }

    impl ReaderOutput {
        fn text(&self) -> String {
            format!(
                "code {:?} stdout {} stderr {}",
                self.code,
                String::from_utf8_lossy(&self.stdout),
                String::from_utf8_lossy(&self.stderr)
            )
        }
    }

    fn spawn_reader(path: &Path) -> eyre::Result<ReaderOutput> {
        let output = std::process::Command::new(std::env::current_exe()?)
            .arg("migration_exclusive_blocks_a_reader_until_the_run_returns")
            .env("IRYS_MIGRATION_EXCLUSIVE_CHILD", "1")
            .env("IRYS_MIGRATION_DIR", path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()?;
        Ok(ReaderOutput {
            code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    fn reader_during_migration() -> eyre::Result<()> {
        let path = std::path::PathBuf::from(std::env::var("IRYS_MIGRATION_DIR")?);
        let args = testing_args()?.with_exclusive(Some(false));
        reth_db::DatabaseEnv::open(&path, reth_db::DatabaseEnvKind::RO, args)
            .map(|_| ())
            .map_err(|err| eyre::eyre!("reader open failed: {err}"))
    }

    #[test]
    fn interrupt_mid_delete_keeps_the_new_index_unpublished() -> eyre::Result<()> {
        let dir = temp("sm-v1-mid-drop");
        let args = testing_args()?;
        write_contiguous(dir.path(), 5)?;
        let err = open_submodule_index(dir.path(), args.clone(), opts(2, Some(4))).unwrap_err();
        assert!(err.to_string().contains("after 4 commits"));

        let env = open_or_create_db(dir.path(), SubmoduleTables::ALL, args.clone())?;
        let raw = schema_of(&env)?.expect("progress marker");
        assert_ne!(raw.as_slice(), SUBMODULE_SCHEMA.as_bytes());
        let progress = decode_progress(&raw).expect("drop marker");
        assert_eq!(progress.phase, Phase::Drop);
        assert_eq!(progress.copied, 5);
        assert_eq!(progress.intervals, 1);
        assert_eq!(progress.deleted, 2);
        assert_eq!(legacy_of(&env)?, 3);
        let tx = env.tx()?;
        for offset in 0..5 {
            assert!(get_data_path_by_offset(&tx, PartitionChunkOffset(offset))?.is_some());
        }
        drop(tx);
        assert_eq!(intervals(&env)?, vec![(0, 4, h(1))]);
        drop(env);

        let env = create_or_open_submodule_db(dir.path(), args)?;
        let tx = env.tx()?;
        assert_eq!(
            get_tx_path_by_offset(&tx, PartitionChunkOffset(0))?,
            Some(b"tx".to_vec())
        );
        drop(tx);
        assert_eq!(legacy_of(&env)?, 0);
        assert_eq!(
            schema_of(&env)?.as_deref(),
            Some(SUBMODULE_SCHEMA.as_bytes())
        );
        Ok(())
    }

    #[test]
    fn corrupt_progress_marker_is_refused_without_deleting_v1_rows() -> eyre::Result<()> {
        let dir = temp("sm-v1-corrupt");
        let args = testing_args()?;
        write_contiguous(dir.path(), 5)?;
        {
            let db = open_or_create_db(dir.path(), SubmoduleTables::ALL, args.clone())?;
            let tx = db.tx_mut()?;
            tx.put::<Metadata>(
                MetadataKey::DBSchemaVersion,
                b"irys-submodule-index migrate-v1 copy nope\n".to_vec(),
            )?;
            tx.commit()?;
        }
        let err = create_or_open_submodule_db(dir.path(), args.clone()).unwrap_err();
        assert!(err.to_string().contains("does not match this binary"));
        let env = open_or_create_db(dir.path(), SubmoduleTables::ALL, args)?;
        assert_eq!(legacy_of(&env)?, 5);
        Ok(())
    }

    #[test]
    fn verify_against_pristine_v1_matches_and_a_corrupt_row_fails() -> eyre::Result<()> {
        let args = testing_args()?;
        assert!(open_pristine_v1_read_only(Path::new("db"), args.clone()).is_err());

        let parent = temp("sm-verify-copy");
        let pristine = parent.path().join("pristine.copy");
        let migrated = parent.path().join("migrated.copy");
        std::fs::create_dir(&pristine)?;
        write_master(&pristine, &master_fixture())?;
        copy_dir(&pristine, &migrated)?;
        let (env, stats) = open_submodule_index(
            &migrated,
            args.clone(),
            SubmoduleMigrationOptions::default(),
        )?;
        assert!(stats.migrated);
        assert_eq!(stats.detected, DetectedSubmoduleSchema::V1);
        drop(env);

        let digest = files_digest(&pristine)?;
        let report = compare_copy(&pristine, &migrated, &args)?;
        assert_eq!(report.offsets_checked, 10);
        assert_eq!(report.matches, 10);
        assert!(!report.has_mismatch(), "{report}");
        assert!(report.samples.is_empty());
        assert_eq!(report.data_root_infos_checked, 1);
        assert_eq!(report.tx_path_rows_checked, 3);
        assert_eq!(report.tx_leaf_bindings_checked, 1);
        assert_eq!(report.pending_body_checked, 1);
        assert_eq!(
            report.to_string().lines().next(),
            Some("offsets_checked: 10")
        );
        assert_eq!(files_digest(&pristine)?, digest);
        {
            let v1 = open_pristine_v1_read_only(&pristine, args.clone())?;
            let tx = v1.tx()?;
            assert_eq!(tx.get::<Metadata>(MetadataKey::DBSchemaVersion)?, None);
            assert_eq!(
                tx.get::<master::ChunkDataPathByPathHash>(h(10))?,
                Some(vec![10])
            );
        }

        {
            let env = open_or_create_db(&migrated, SubmoduleTables::ALL, args.clone())?;
            let tx = env.tx_mut()?;
            tx.put::<ChunkDataPathByOffset>(PartitionChunkOffset(0), b"corrupt".to_vec())?;
            tx.commit()?;
        }
        let report = compare_copy(&pristine, &migrated, &args)?;
        assert!(report.has_mismatch());
        assert_eq!(report.offsets_checked, 10);
        assert_eq!(report.matches, 9);
        assert_eq!(report.mismatch_data_path, 1);
        assert_eq!(report.mismatch_tx_path, 0);
        assert_eq!(report.mismatch_data_path_hash, 0);
        assert_eq!(report.mismatch_tx_path_hash, 0);
        assert_eq!(report.data_root_infos_mismatches, 0);
        assert_eq!(report.tx_path_rows_mismatches, 0);
        assert_eq!(report.tx_leaf_bindings_mismatches, 0);
        assert_eq!(report.pending_body_mismatches, 0);
        let text = report.to_string();
        assert!(text.contains("mismatch_data_path: 1"), "{text}");
        assert!(text.contains("mismatch: data_path offset 0"), "{text}");
        assert_eq!(files_digest(&pristine)?, digest);
        Ok(())
    }

    #[test]
    fn verify_mainnet_v1_without_pending_table_matches() -> eyre::Result<()> {
        let args = testing_args()?;
        let parent = temp("sm-mainnet-verify-copy");
        let pristine = parent.path().join("pristine.copy");
        let migrated = parent.path().join("migrated.copy");
        std::fs::create_dir(&pristine)?;
        let mut fixture = master_fixture();
        fixture.pending = None;
        write_mainnet(&pristine, &fixture)?;
        copy_dir(&pristine, &migrated)?;
        let (env, stats) = open_submodule_index(
            &migrated,
            args.clone(),
            SubmoduleMigrationOptions::default(),
        )?;
        assert!(stats.migrated);
        drop(env);

        let digest = files_digest(&pristine)?;
        let report = compare_copy(&pristine, &migrated, &args)?;
        assert_eq!(report.offsets_checked, 10);
        assert_eq!(report.matches, 10);
        assert!(!report.has_mismatch(), "{report}");
        assert_eq!(report.pending_body_checked, 0);
        assert_eq!(report.pending_body_mismatches, 0);
        assert_eq!(files_digest(&pristine)?, digest);
        Ok(())
    }

    fn compare_copy(
        pristine: &Path,
        migrated: &Path,
        args: &reth_db::mdbx::DatabaseArguments,
    ) -> eyre::Result<crate::submodule::ReadEquivalenceReport> {
        let v1 = open_pristine_v1_read_only(pristine, args.clone())?;
        let (v2, _) =
            open_submodule_index(migrated, args.clone(), SubmoduleMigrationOptions::default())?;
        let left = v1.tx()?;
        let right = v2.tx()?;
        verify_submodule_read_equivalence(&left, &right)
    }

    fn files_digest(root: &Path) -> eyre::Result<Vec<(String, [u8; 32])>> {
        let mut out = Vec::new();
        digest_dir(root, "", &mut out)?;
        out.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(out)
    }

    fn digest_dir(dir: &Path, prefix: &str, out: &mut Vec<(String, [u8; 32])>) -> eyre::Result<()> {
        let mut entries = std::fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let rel = if prefix.is_empty() {
                name.to_string()
            } else {
                format!("{prefix}/{name}")
            };
            let kind = entry.file_type()?;
            if kind.is_dir() {
                digest_dir(&entry.path(), &rel, out)?;
            } else if kind.is_file() {
                let bytes = std::fs::read(entry.path())?;
                out.push((rel, irys_types::hash_sha256(&bytes)));
            }
        }
        Ok(())
    }

    /// Copy `IRYS_SUBMODULE_V1_DB` and migrate the copy. The source path is
    /// never opened.
    #[test]
    #[ignore = "set IRYS_SUBMODULE_V1_DB to a submodule db directory"]
    fn migrate_v1_directory_from_env() -> eyre::Result<()> {
        let Ok(source) = std::env::var("IRYS_SUBMODULE_V1_DB") else {
            eyre::bail!(
                "set IRYS_SUBMODULE_V1_DB to the source directory; this test copies it and does not open the source"
            );
        };
        let source = Path::new(&source).to_path_buf();
        let parent = temp("sm-v1-real");
        let dest = parent.path().join("db.copy");
        copy_dir(&source, &dest)?;
        require_submodule_index_copy(&dest)?;
        let args = crate::submodule_db_args(&irys_types::config::DatabaseConfig::default())?;
        let (_env, stats) =
            open_submodule_index(&dest, args, SubmoduleMigrationOptions::default())?;
        println!("{stats}");
        Ok(())
    }

    fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
        std::fs::create_dir(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let to = dst.join(entry.file_name());
            let kind = entry.file_type()?;
            if kind.is_dir() {
                copy_dir(&entry.path(), &to)?;
            } else if kind.is_file() {
                std::fs::copy(entry.path(), &to)?;
            }
        }
        Ok(())
    }
}
