//! Time a tx-path rebuild against the stored submodule read.
//!
//! This binary is not on the mining path. It opens a consensus database
//! read-only and, when asked, a submodule index.

use std::fs::{self, File};
#[cfg(target_os = "linux")]
use std::io::Error as IoError;
#[cfg(target_os = "linux")]
use std::os::unix::io::AsRawFd as _;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use eyre::eyre;
use irys_database::db::IrysDatabaseExt as _;
use irys_database::metadata::MetadataKey;
use irys_database::migration::ensure_db_version_compatible;
use irys_database::reth_db::cursor::DbCursorRO as _;
use irys_database::reth_db::mdbx::DatabaseArguments;
use irys_database::reth_db::transaction::DbTx as _;
use irys_database::reth_db::{Database as _, DatabaseEnv, DatabaseEnvKind};
use irys_database::submodule::tables::{
    Metadata, SUBMODULE_CONVERT_INCOMPLETE, SUBMODULE_SCHEMA, SUBMODULE_SCHEMA_ROCKS_V1,
    TxPathIntervalByStart, unrecognised_rocks_layout,
};
use irys_database::submodule::{SubmoduleIndex, SubmoduleStore as _};
use irys_database::tables::{IrysDataTxHeaders, IrysTables, MigratedBlockHashes};
use irys_database::tx_path_derive::{
    LedgerSpan, collect_data_spans, derive_tx_path_at_offset_with, derive_tx_path_synthetic_with,
    read_stored_tx_data_path, tx_header_schema, tx_path_hash,
};
use irys_database::{
    IrysDatabaseArgs as _, block_index_latest_height, insert_block_header,
    insert_block_index_items_for_block, insert_tx_header, open_or_create_db,
    set_database_schema_version,
};
use irys_types::{
    DataLedger, DataTransactionHeader, DataTransactionLedger, DatabaseVersion, DbSyncMode, H256,
    H256List, IrysBlockHeader, PartitionChunkOffset, hash_sha256,
};

const USAGE: &str = "\
usage: tx-path-derive-bench --db <irys_consensus_data> [options]
       tx-path-derive-bench --prepare <dir>

Rebuild one ledger tx path from consensus tables, and optionally time today's
stored read. Mining does not call this binary.

--db PATH                 Main consensus DB, opened read-only.
--index PATH              Submodule index directory (MDBX db/ or RocksDB rocks/).
--engine auto|mdbx|rocks  Default auto. mdbx.dat selects MDBX. SCHEMA selects RocksDB.
--mode MODE               all (default), derive-real, derive-synthetic, baseline-stored.
                          all runs baseline-stored only when --index is set.
--samples K               Samples per phase. Default 1000.
--threads T               Concurrent samples. Default 1.
--prefetch LIST           Tx-header read threads per derive (rayon pool), e.g. 1,4,16,32. Default 1 (serial).
                          T>1 cold numbers are contention, not isolated cold reads.
--chunk-size N            Ledger chunk size in bytes. Default 262144.
--ledgers LIST            publish,submit,one_year,thirty_day. Aliases: 0,1,10,20,one_month.
--synthetic LIST          Tree widths. Default 10,100,1000,10000.
--cold-only | --warm-only Default is both. Passing both flags is an error.
--index-span START:END    Exclusive end. RocksDB harvest window. Default 0:10000000.
--migrate-copy            Rewrite a consensus copy up to schema V3. The directory
                          name must contain \"copy\". Not timed.
--migrate-index           Migrate a v1 MDBX submodule copy in place. The directory
                          name must contain \"copy\". Not timed.
--prepare DIR             Write a small V3 fixture and exit. DIR's name must
                          contain \"copy\". Writes DIR/consensus and
                          DIR/submodule-db.copy.

Cold drops file cache with posix_fadvise(DONTNEED) before each sample. Pages
this process still has mapped can stay resident. A RocksDB block cache is not
dropped. Device read counters show whether the disk was read.

baseline-stored offsets are partition offsets. derive-real offsets are ledger
offsets. They name the same chunk only when the submodule stores that ledger
from offset 0. Compare one stored read with one derive.

Release 4.0.8 stamps consensus schema V3 on startup. A database last opened by
that release reads here. An older stamp does not: tx headers still use
bundle_format or an inline promoted_height, and this binary's decoder expects
metadata_format. A read-only open does not migrate. On a copy:
  cp -a irys_consensus_data /tmp/irys_consensus_data.copy
  tx-path-derive-bench --db /tmp/irys_consensus_data.copy --migrate-copy --samples 1000

Release 4.0.8 submodule indexes are schema v1 (one tx hash per offset, no
interval table, no RocksDB). Baseline numbers are the v2 interval read.
  cp -a path/to/submodule/db /tmp/submodule-db.copy
  cargo run -p irys-database --bin submodule-index-migrate -- /tmp/submodule-db.copy
submodule-index-convert writes RocksDB v1 and checks each stored tx
path against a derive. See `submodule-index-convert --help`. 4.0.8 has no
RocksDB index. A rocks SCHEMA that is not RocksDB v1 is refused.
RocksDB v1 stores no tx path, so baseline-stored does not apply.
derive-real and derive-synthetic call the production derive.
";

const DEFAULT_CHUNK: u64 = 262_144;
const DEFAULT_SAMPLES: usize = 1000;
const DEFAULT_SPAN_END: u32 = 10_000_000;
const BUCKETS: [&str; 6] = ["1", "2-10", "11-100", "101-1000", "1001-10000", ">10000"];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Engine {
    Auto,
    Mdbx,
    Rocks,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    All,
    Real,
    Synthetic,
    Baseline,
}

struct Args {
    db: Option<PathBuf>,
    index: Option<PathBuf>,
    engine: Engine,
    mode: Mode,
    samples: usize,
    threads: usize,
    chunk_size: u64,
    ledgers: Vec<DataLedger>,
    synthetic: Vec<usize>,
    /// Tx-header read threads per derive. 1 reads serially.
    prefetch: Vec<usize>,
    warm: bool,
    cold: bool,
    index_span: Option<(u32, u32)>,
    migrate_copy: bool,
    migrate_index: bool,
    prepare: Option<PathBuf>,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            db: None,
            index: None,
            engine: Engine::Auto,
            mode: Mode::All,
            samples: DEFAULT_SAMPLES,
            threads: 1,
            chunk_size: DEFAULT_CHUNK,
            ledgers: vec![
                DataLedger::Publish,
                DataLedger::Submit,
                DataLedger::OneYear,
                DataLedger::ThirtyDay,
            ],
            synthetic: vec![10, 100, 1000, 10_000],
            prefetch: vec![1],
            warm: true,
            cold: true,
            index_span: None,
            migrate_copy: false,
            migrate_index: false,
            prepare: None,
        }
    }
}

fn main() {
    let code = match entry() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:?}");
            1
        }
    };
    std::process::exit(code);
}

fn entry() -> eyre::Result<i32> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let Some(args) = parse_args(&raw)? else {
        println!("{USAGE}");
        return Ok(0);
    };
    if let Some(dir) = &args.prepare {
        prepare_fixture(dir)?;
        return Ok(0);
    }
    run(&args)
}

fn run(args: &Args) -> eyre::Result<i32> {
    let db_path = args.db.as_ref().ok_or_else(|| eyre!("--db is required"))?;
    if args.migrate_copy {
        migrate_consensus(db_path)?;
        println!("consensus migration finished (not timed)");
    }
    let db = open_consensus_ro(db_path)?;
    let version = read_schema(&db)?;
    println!(
        "consensus schema stamp: {}",
        version.map_or_else(|| "absent".to_owned(), |value| value.to_string())
    );
    let schema = tx_header_schema(version);
    println!("{}", schema.message);
    if !schema.readable {
        return Ok(2);
    }
    println!(
        "samples={} threads={} chunk_size={} cold={} warm={}",
        args.samples,
        args.threads,
        args.chunk_size,
        u8::from(args.cold),
        u8::from(args.warm)
    );
    println!(
        "cold closes every environment, uses posix_fadvise(DONTNEED), then reopens untimed for each sample. The RocksDB baseline reopens with a private block cache. Device counters show real reads."
    );
    if args.threads > 1 {
        println!(
            "threads={}: cold samples evict and read with no lock. Those numbers are contention. Isolated cold reads are --threads 1.",
            args.threads
        );
    }

    // Every environment that maps the file must be closed before a cold
    // sample, or fadvise keeps the mapped pages. Phases reopen as needed.
    drop(db);
    if args.cold && args.threads > 1 {
        eyre::bail!(
            "cold samples reopen the environment so no mapping pins pages; run cold with --threads 1 (warm may use more)"
        );
    }
    let mut attempted = 0_usize;
    let mut succeeded = 0_usize;
    if wants_real(args.mode) {
        let (tried, ok) = run_real(db_path, args)?;
        attempted += tried;
        succeeded += ok;
    }
    if wants_synthetic(args.mode) {
        let (tried, ok) = run_synthetic(db_path, args)?;
        attempted += tried;
        succeeded += ok;
    }
    if wants_baseline(args.mode, args.index.is_some())? {
        let index_path = args.index.as_ref().expect("index checked");
        println!(
            "baseline offsets are partition offsets. derive-real offsets are ledger offsets. They match only when the submodule covers that ledger from 0."
        );
        let (tried, ok) = run_baseline(index_path, args)?;
        attempted += tried;
        succeeded += ok;
    } else if matches!(args.mode, Mode::All) {
        println!("baseline-stored skipped: no --index");
    }
    if attempted > 0 && succeeded == 0 {
        return Ok(1);
    }
    Ok(0)
}

fn wants_real(mode: Mode) -> bool {
    matches!(mode, Mode::All | Mode::Real)
}

fn wants_synthetic(mode: Mode) -> bool {
    matches!(mode, Mode::All | Mode::Synthetic)
}

fn wants_baseline(mode: Mode, has_index: bool) -> eyre::Result<bool> {
    match mode {
        Mode::Baseline if !has_index => {
            eyre::bail!("baseline-stored needs --index")
        }
        Mode::Baseline => Ok(true),
        Mode::All => Ok(has_index),
        Mode::Real | Mode::Synthetic => Ok(false),
    }
}

fn run_real(db_path: &Path, args: &Args) -> eyre::Result<(usize, usize)> {
    println!("collecting migrated data spans");
    let spans = {
        let db = open_consensus_ro(db_path)?;
        collect_data_spans(&db)?
    };
    let selected = select_spans(&spans, &args.ledgers);
    if selected.is_empty() {
        eyre::bail!("no migrated data txs in the selected ledgers");
    }
    let disk = device_for(db_path);
    let mut attempted = 0_usize;
    let mut succeeded = 0_usize;
    for &prefetch in &args.prefetch {
        let pool = prefetch_pool(prefetch)?;
        let pool = pool.as_ref();
        let label = format!("derive-real prefetch={prefetch}");
        let phase = |cold: bool, shared: Option<&DatabaseEnv>| {
            let selected = &selected;
            let chunk_size = args.chunk_size;
            measure(
                &label,
                cold,
                args.threads,
                args.samples,
                Some(db_path),
                disk.as_ref(),
                true,
                move |rng| {
                    let (ledger, offset) = pick_offset(rng, selected)?;
                    let fresh;
                    let db = match shared {
                        Some(db) => db,
                        None => {
                            fresh = open_consensus_ro(db_path)?;
                            &fresh
                        }
                    };
                    let derived =
                        derive_tx_path_at_offset_with(db, ledger, offset, chunk_size, pool)?;
                    std::hint::black_box(derived.proof.proof.len());
                    Ok(core_from_derive(&derived))
                },
            )
        };
        if args.cold {
            let stats = phase(true, None);
            attempted += stats.attempted;
            succeeded += stats.succeeded;
        }
        if args.warm {
            let db = open_consensus_ro(db_path)?;
            let stats = phase(false, Some(&db));
            attempted += stats.attempted;
            succeeded += stats.succeeded;
        }
    }
    Ok((attempted, succeeded))
}

/// `None` for 1 (serial reads); otherwise a pool with that many threads.
fn prefetch_pool(threads: usize) -> eyre::Result<Option<rayon::ThreadPool>> {
    eyre::ensure!(threads > 0, "--prefetch values must be > 0");
    if threads == 1 {
        return Ok(None);
    }
    Ok(Some(
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("txpath-prefetch-{i}"))
            .build()?,
    ))
}

fn select_spans(spans: &[LedgerSpan], ledgers: &[DataLedger]) -> Vec<LedgerSpan> {
    let mut selected = Vec::new();
    for ledger in ledgers {
        let rows: Vec<LedgerSpan> = spans
            .iter()
            .copied()
            .filter(|span| span.ledger == *ledger)
            .collect();
        if rows.is_empty() {
            println!("ledger {ledger}: no migrated data spans");
            continue;
        }
        let chunks: u64 = rows
            .iter()
            .map(|span| span.end.saturating_sub(span.start))
            .sum();
        println!("ledger {ledger}: {} spans, {} chunks", rows.len(), chunks);
        selected.extend(rows);
    }
    selected
}

fn run_synthetic(db_path: &Path, args: &Args) -> eyre::Result<(usize, usize)> {
    println!("scanning tx header ids");
    let (ids, block_hash) = {
        let db = open_consensus_ro(db_path)?;
        (load_tx_ids(&db)?, one_block_hash(&db)?)
    };
    if ids.is_empty() {
        eyre::bail!("consensus DB has no tx headers");
    }
    println!("tx headers available: {}", ids.len());
    let disk = device_for(db_path);
    let mut attempted = 0_usize;
    let mut succeeded = 0_usize;
    for width in &args.synthetic {
        let (chosen, cycled) = choose_ids(&ids, *width);
        if cycled {
            println!(
                "derive-synthetic N={width}: cycled ids (unique {}). Cold cost is the unique pages. Repeats in one sample hit the cache just filled.",
                ids.len()
            );
        } else {
            println!("derive-synthetic N={width}: {width} distinct tx headers");
        }
        let leaf_index = width / 2;
        println!("derive-synthetic N={width}: leaf_index={leaf_index} (fixed)");
        for &prefetch in &args.prefetch {
            let pool = prefetch_pool(prefetch)?;
            let pool = pool.as_ref();
            let phase = |cold: bool, shared: Option<&DatabaseEnv>| {
                let chosen = &chosen;
                measure(
                    &format!("derive-synthetic-N={width} prefetch={prefetch}"),
                    cold,
                    args.threads,
                    args.samples,
                    Some(db_path),
                    disk.as_ref(),
                    true,
                    move |_rng| {
                        let fresh;
                        let db = match shared {
                            Some(db) => db,
                            None => {
                                fresh = open_consensus_ro(db_path)?;
                                &fresh
                            }
                        };
                        let derived = derive_tx_path_synthetic_with(
                            db,
                            chosen,
                            &block_hash,
                            leaf_index,
                            pool,
                        )?;
                        std::hint::black_box(derived.proof.proof.len());
                        Ok(core_from_derive(&derived))
                    },
                )
            };
            if args.cold {
                let stats = phase(true, None);
                attempted += stats.attempted;
                succeeded += stats.succeeded;
            }
            if args.warm {
                let db = open_consensus_ro(db_path)?;
                let stats = phase(false, Some(&db));
                attempted += stats.attempted;
                succeeded += stats.succeeded;
            }
        }
    }
    Ok((attempted, succeeded))
}

fn run_baseline(index_path: &Path, args: &Args) -> eyre::Result<(usize, usize)> {
    let engine = detect_engine(index_path, args.engine)?;
    println!("index engine: {}", engine_name(engine));
    // MDBX returns EAGAIN if a second environment opens the same files.
    // Walk the intervals before `SubmoduleIndex` holds the environment.
    let mdbx_intervals = if matches!(engine, Engine::Mdbx) {
        let rows = mdbx_intervals(index_path)?;
        println!("mdbx tx-path intervals: {}", rows.len());
        rows
    } else {
        Vec::new()
    };
    let offsets = {
        let index = open_index(index_path, engine, args.migrate_index)?;
        if !index.stores_tx_paths() {
            eyre::bail!(
                "RocksDB v1 does not store a tx path. baseline-stored times that read. Use --mode derive-real or --mode derive-synthetic."
            );
        }
        harvest_offsets(&index, engine, args, &mdbx_intervals)?
    };
    if offsets.is_empty() {
        eyre::bail!(
            "no offset has both a tx path and a data path. Migration writes the tx path first. The body worker writes the data path later. Baseline needs both."
        );
    }
    println!("baseline usable offsets: {}", offsets.len());
    if offsets.len() < args.samples {
        println!(
            "baseline cycles {} offsets across {} samples",
            offsets.len(),
            args.samples
        );
    }
    let disk = device_for(index_path);
    let mut attempted = 0_usize;
    let mut succeeded = 0_usize;
    let phase = |cold: bool, shared: Option<&SubmoduleIndex>| {
        let offsets = &offsets;
        measure(
            "baseline-stored",
            cold,
            args.threads,
            args.samples,
            Some(index_path),
            disk.as_ref(),
            false,
            move |rng| {
                let offset = offsets[rng.below(offsets.len())];
                let fresh;
                let index = match shared {
                    Some(index) => index,
                    None => {
                        fresh = open_index_cold(index_path, engine)?;
                        &fresh
                    }
                };
                let started = Instant::now();
                let (tx_path, data_path) =
                    read_stored_tx_data_path(index, PartitionChunkOffset(offset))?;
                let total = fit_nanos(started.elapsed())?;
                let tx_len = tx_path.as_ref().map(Vec::len).unwrap_or(0);
                let data_len = data_path.as_ref().map(Vec::len).unwrap_or(0);
                std::hint::black_box(tx_len);
                std::hint::black_box(data_len);
                if tx_path.is_none() || data_path.is_none() {
                    eyre::bail!("stored path missing at offset {offset}");
                }
                Ok(SampleCore {
                    total_ns: total,
                    db_ns: total,
                    cpu_ns: 0,
                    tx_count: 0,
                    proof_len: u64::try_from(tx_len).unwrap_or(u64::MAX),
                })
            },
        )
    };
    if args.cold {
        let stats = phase(true, None);
        attempted += stats.attempted;
        succeeded += stats.succeeded;
    }
    if args.warm {
        let index = open_index(index_path, engine, false)?;
        let stats = phase(false, Some(&index));
        attempted += stats.attempted;
        succeeded += stats.succeeded;
    }
    Ok((attempted, succeeded))
}

fn core_from_derive(derived: &irys_database::tx_path_derive::DerivedTxPath) -> SampleCore {
    SampleCore {
        total_ns: derived.db_nanos.saturating_add(derived.cpu_nanos),
        db_ns: derived.db_nanos,
        cpu_ns: derived.cpu_nanos,
        tx_count: u64::try_from(derived.tx_count).unwrap_or(u64::MAX),
        proof_len: u64::try_from(derived.proof.proof.len()).unwrap_or(u64::MAX),
    }
}

struct SampleCore {
    total_ns: u64,
    db_ns: u64,
    cpu_ns: u64,
    tx_count: u64,
    proof_len: u64,
}

struct Sample {
    core: SampleCore,
    reads: u64,
    sectors: u64,
}

struct PhaseStats {
    attempted: usize,
    succeeded: usize,
}

fn measure<F>(
    name: &str,
    cold: bool,
    threads: usize,
    samples: usize,
    evict_dir: Option<&Path>,
    disk: Option<&DiskId>,
    show_widths: bool,
    sample_one: F,
) -> PhaseStats
where
    F: Fn(&mut XorShift) -> eyre::Result<SampleCore> + Sync,
{
    let kind = if cold { "cold" } else { "warm" };
    if !cold {
        let mut rng = XorShift::new(1);
        if let Err(err) = sample_one(&mut rng) {
            println!("{name} {kind} warmup failed: {err}");
        }
    }
    let counts = split_counts(samples, threads);
    let mut ok = Vec::new();
    let mut failures = FailureLog::default();
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for (index, count) in counts.iter().copied().enumerate() {
            if count == 0 {
                continue;
            }
            let sample_one = &sample_one;
            handles
                .push(scope.spawn(move || worker(index, count, cold, evict_dir, disk, sample_one)));
        }
        for handle in handles {
            match handle.join() {
                Ok((samples, errors)) => {
                    ok.extend(samples);
                    failures.count += errors.count;
                    failures.samples.extend(errors.samples);
                }
                Err(_) => {
                    failures.count += 1;
                    failures.samples.push("worker panicked".to_owned());
                }
            }
        }
    });
    let attempted = ok.len() + failures.count;
    print_phase(name, kind, &ok, &failures, threads, disk, show_widths);
    PhaseStats {
        attempted,
        succeeded: ok.len(),
    }
}

fn worker<F>(
    thread_index: usize,
    count: usize,
    cold: bool,
    evict_dir: Option<&Path>,
    disk: Option<&DiskId>,
    sample_one: &F,
) -> (Vec<Sample>, FailureLog)
where
    F: Fn(&mut XorShift) -> eyre::Result<SampleCore>,
{
    let mut rng = XorShift::new(thread_seed(thread_index));
    let mut ok = Vec::with_capacity(count);
    let mut failures = FailureLog::default();
    for _ in 0..count {
        if cold
            && let Some(dir) = evict_dir
            && let Err(err) = evict_cache(dir)
        {
            push_fail(&mut failures, &err);
            continue;
        }
        let before = disk_counters(disk);
        match sample_one(&mut rng) {
            Ok(core) => {
                let (reads, sectors) = disk_delta(before, disk_counters(disk));
                ok.push(Sample {
                    core,
                    reads,
                    sectors,
                });
            }
            Err(err) => push_fail(&mut failures, &err),
        }
    }
    (ok, failures)
}

/// How many sample errors to keep for the log. Later errors still increment
/// the failure count.
const FAIL_SAMPLE_CAP: usize = 5;

#[derive(Default)]
struct FailureLog {
    count: usize,
    samples: Vec<String>,
}

fn push_fail(failures: &mut FailureLog, err: &eyre::Report) {
    failures.count += 1;
    if failures.samples.len() >= FAIL_SAMPLE_CAP {
        return;
    }
    let text = err.to_string();
    let end = text.len().min(240);
    failures.samples.push(text[..end].to_owned());
}

fn print_phase(
    name: &str,
    kind: &str,
    ok: &[Sample],
    failures: &FailureLog,
    threads: usize,
    disk: Option<&DiskId>,
    show_widths: bool,
) {
    for line in phase_lines(name, kind, ok, failures, threads, disk, show_widths) {
        println!("{line}");
    }
}

fn phase_lines(
    name: &str,
    kind: &str,
    ok: &[Sample],
    failures: &FailureLog,
    threads: usize,
    disk: Option<&DiskId>,
    show_widths: bool,
) -> Vec<String> {
    let mut lines = vec![format!(
        "== {name} {kind} ok={} fail={} threads={threads}",
        ok.len(),
        failures.count
    )];
    if ok.is_empty() {
        lines.push(format!("{name} {kind}: no successful samples"));
    } else {
        lines.extend(duration_lines(
            "db",
            &column(ok, |sample| sample.core.db_ns),
        ));
        lines.extend(duration_lines(
            "cpu",
            &column(ok, |sample| sample.core.cpu_ns),
        ));
        lines.extend(duration_lines(
            "total",
            &column(ok, |sample| sample.core.total_ns),
        ));
        lines.push(count_line(
            "proof_len_bytes",
            &column(ok, |sample| sample.core.proof_len),
        ));
        if show_widths {
            lines.push(count_line(
                "tx_count",
                &column(ok, |sample| sample.core.tx_count),
            ));
            lines.push(bucket_line(ok));
        }
        lines.push(disk_line(ok, disk));
    }
    for failure in &failures.samples {
        lines.push(format!("{name} {kind} failure: {failure}"));
    }
    lines
}

fn column(ok: &[Sample], pick: impl Fn(&Sample) -> u64) -> Vec<u64> {
    let mut values: Vec<u64> = ok.iter().map(pick).collect();
    values.sort_unstable();
    values
}

fn duration_lines(label: &str, sorted: &[u64]) -> [String; 2] {
    [
        format!(
            "{label}_ns min={} p50={} p99={} max={}",
            rank_min(sorted),
            percentile(sorted, 50),
            percentile(sorted, 99),
            rank_max(sorted)
        ),
        format!(
            "{label}_us min={} p50={} p99={} max={}",
            rank_min(sorted) / 1000,
            percentile(sorted, 50) / 1000,
            percentile(sorted, 99) / 1000,
            rank_max(sorted) / 1000
        ),
    ]
}

fn count_line(label: &str, sorted: &[u64]) -> String {
    format!(
        "{label} min={} p50={} p99={} max={}",
        rank_min(sorted),
        percentile(sorted, 50),
        percentile(sorted, 99),
        rank_max(sorted)
    )
}

fn bucket_line(ok: &[Sample]) -> String {
    let mut counts = [0_u64; 6];
    for sample in ok {
        counts[width_bucket(sample.core.tx_count)] += 1;
    }
    let mut line = String::from("n");
    for (label, count) in BUCKETS.iter().zip(counts) {
        line.push_str(&format!(" {label}={count}"));
    }
    line
}

fn width_bucket(count: u64) -> usize {
    match count {
        1 => 0,
        2..=10 => 1,
        11..=100 => 2,
        101..=1000 => 3,
        1001..=10_000 => 4,
        _ => 5,
    }
}

fn disk_line(ok: &[Sample], disk: Option<&DiskId>) -> String {
    let Some(disk) = disk else {
        return "disk unavailable".to_owned();
    };
    let reads = column(ok, |sample| sample.reads);
    let sectors: u64 = ok.iter().map(|sample| sample.sectors).sum();
    let read_sum: u64 = ok.iter().map(|sample| sample.reads).sum();
    format!(
        "disk device={} reads_p50={} reads_sum={read_sum} sectors_sum={sectors}",
        disk.name,
        percentile(&reads, 50)
    )
}

fn rank_min(sorted: &[u64]) -> u64 {
    sorted.first().copied().unwrap_or(0)
}

fn rank_max(sorted: &[u64]) -> u64 {
    sorted.last().copied().unwrap_or(0)
}

/// Nearest-rank percentile. `sorted` is ascending. `pct` is 0..=100.
fn percentile(sorted: &[u64], pct: u64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let len = u64::try_from(sorted.len()).unwrap_or(u64::MAX);
    let rank = pct.saturating_mul(len).div_ceil(100);
    let idx = usize::try_from(rank.saturating_sub(1)).unwrap_or(0);
    sorted[idx.min(sorted.len() - 1)]
}

fn split_counts(samples: usize, threads: usize) -> Vec<usize> {
    let threads = threads.max(1);
    let base = samples / threads;
    let extra = samples % threads;
    (0..threads)
        .map(|index| base + usize::from(index < extra))
        .collect()
}

fn pick_offset(rng: &mut XorShift, spans: &[LedgerSpan]) -> eyre::Result<(DataLedger, u64)> {
    let total: u64 = spans
        .iter()
        .map(|span| span.end.saturating_sub(span.start))
        .sum();
    eyre::ensure!(total > 0, "selected spans contain no chunks");
    let mut pick = rng.below_u64(total);
    for span in spans {
        let len = span.end.saturating_sub(span.start);
        if pick < len {
            return Ok((span.ledger, span.start + pick));
        }
        pick -= len;
    }
    eyre::bail!("span pick missed every ledger")
}

fn choose_ids(all: &[H256], width: usize) -> (Vec<H256>, bool) {
    if all.is_empty() {
        return (Vec::new(), true);
    }
    let mut order: Vec<usize> = (0..all.len()).collect();
    let mut rng = XorShift::new(0xC0FF_EE00 ^ u64::try_from(width).unwrap_or(0));
    if order.len() > 1 {
        let mut index = order.len() - 1;
        while index > 0 {
            let span = u64::try_from(index + 1).unwrap_or(1);
            let pick = usize::try_from(rng.next_u64() % span).unwrap_or(0);
            order.swap(index, pick);
            index -= 1;
        }
    }
    let mut chosen = Vec::with_capacity(width);
    while chosen.len() < width {
        for index in &order {
            chosen.push(all[*index]);
            if chosen.len() == width {
                break;
            }
        }
    }
    (chosen, all.len() < width)
}

fn load_tx_ids(db: &DatabaseEnv) -> eyre::Result<Vec<H256>> {
    db.view_eyre(|tx| {
        let mut cursor = tx.cursor_read::<IrysDataTxHeaders>()?;
        let mut ids = Vec::new();
        for row in cursor.walk(None)? {
            let (id, _) = row?;
            ids.push(id);
            if ids.len().is_multiple_of(100_000) {
                println!("tx headers scanned {}", ids.len());
            }
        }
        Ok(ids)
    })
}

fn one_block_hash(db: &DatabaseEnv) -> eyre::Result<H256> {
    db.view_eyre(|tx| {
        let height = block_index_latest_height(tx)?.ok_or_else(|| eyre!("block index is empty"))?;
        tx.get::<MigratedBlockHashes>(height)?
            .ok_or_else(|| eyre!("migrated hash missing at height {height}"))
    })
}

fn migrate_consensus(path: &Path) -> eyre::Result<()> {
    require_copy_dir(path, "consensus database")?;
    let db = open_or_create_db(
        path,
        IrysTables::ALL,
        DatabaseArguments::irys_default(DbSyncMode::Durable)?.with_exclusive(Some(true)),
    )?;
    ensure_db_version_compatible(&db)?;
    Ok(())
}

fn open_consensus_ro(path: &Path) -> eyre::Result<DatabaseEnv> {
    DatabaseEnv::open(
        path,
        DatabaseEnvKind::RO,
        DatabaseArguments::irys_default(DbSyncMode::Durable)?
            .with_log_level(None)
            .with_exclusive(Some(false)),
    )
    .map_err(|err| eyre!("read-only open {}: {err}", path.display()))
}

fn read_schema(db: &DatabaseEnv) -> eyre::Result<Option<u32>> {
    db.view(irys_database::database_schema_version)
        .map_err(|err| eyre!("schema txn failed: {err}"))?
        .map_err(|err| eyre!("schema decode failed: {err}"))
}

fn submodule_args() -> eyre::Result<DatabaseArguments> {
    Ok(DatabaseArguments::irys_default(DbSyncMode::Durable)?
        .with_log_level(None)
        .with_exclusive(Some(false))
        .with_geometry_max_size(Some(2 * irys_types::TERABYTE)))
}

fn open_mdbx_ro(path: &Path) -> eyre::Result<DatabaseEnv> {
    DatabaseEnv::open(path, DatabaseEnvKind::RO, submodule_args()?)
        .map_err(|err| eyre!("read-only submodule open {}: {err}", path.display()))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MdbxSchema {
    V2,
    V1,
    Incomplete,
    Partial,
    Foreign,
}

fn probe_mdbx(path: &Path) -> eyre::Result<MdbxSchema> {
    let env = open_mdbx_ro(path)?;
    let row = env
        .view(|tx| tx.get::<Metadata>(MetadataKey::DBSchemaVersion))
        .map_err(|err| eyre!("submodule schema txn failed: {err}"))?
        .map_err(|err| eyre!("submodule schema row read failed: {err}"))?;
    Ok(classify_mdbx(row))
}

fn classify_mdbx(row: Option<Vec<u8>>) -> MdbxSchema {
    match row {
        Some(bytes) if bytes.as_slice() == SUBMODULE_SCHEMA.as_bytes() => MdbxSchema::V2,
        Some(bytes) if bytes.as_slice() == SUBMODULE_CONVERT_INCOMPLETE.as_bytes() => {
            MdbxSchema::Incomplete
        }
        Some(bytes) if bytes.starts_with(b"irys-submodule-index migrate-v1 ") => {
            MdbxSchema::Partial
        }
        Some(_) => MdbxSchema::Foreign,
        None => MdbxSchema::V1,
    }
}

/// Fresh open for one cold sample. RocksDB gets a private 64 MiB block cache
/// that dies with the handle, so the process-wide cache cannot serve it.
fn open_index_cold(path: &Path, engine: Engine) -> eyre::Result<SubmoduleIndex> {
    match engine {
        #[cfg(feature = "rocksdb")]
        Engine::Rocks => SubmoduleIndex::open_rocks_with_block_cache(path, 64 * 1024 * 1024),
        _ => open_index(path, engine, false),
    }
}

fn open_index(path: &Path, engine: Engine, migrate: bool) -> eyre::Result<SubmoduleIndex> {
    match engine {
        Engine::Auto => eyre::bail!("engine was not resolved"),
        Engine::Mdbx => open_mdbx_index(path, migrate),
        Engine::Rocks => open_rocks_index(path),
    }
}

fn open_mdbx_index(path: &Path, migrate: bool) -> eyre::Result<SubmoduleIndex> {
    match probe_mdbx(path)? {
        MdbxSchema::V2 => {}
        MdbxSchema::V1 if migrate => require_copy_dir(path, "submodule index")?,
        MdbxSchema::V1 => eyre::bail!("{}", v1_index_message(path)),
        MdbxSchema::Incomplete => eyre::bail!(
            "submodule index {} is an incomplete conversion. Delete it and rerun submodule-index-convert.",
            path.display()
        ),
        MdbxSchema::Partial => eyre::bail!(
            "submodule index {} has an unfinished v1 migration. Rerun submodule-index-migrate on this copy.",
            path.display()
        ),
        MdbxSchema::Foreign => eyre::bail!(
            "submodule index {} schema row is not this binary's v2 marker",
            path.display()
        ),
    }
    SubmoduleIndex::open_mdbx(path, submodule_args()?)
}

fn v1_index_message(path: &Path) -> String {
    format!(
        "submodule index {} has no v2 schema row. Release 4.0.8 stores one tx-path hash per offset and has no interval table. Baseline times the v2 interval read. Do not migrate a live directory.\n\
cp -a {} /tmp/submodule-db.copy\n\
cargo run -p irys-database --bin submodule-index-migrate -- /tmp/submodule-db.copy\n\
submodule-index-convert mdbx-to-rocks writes RocksDB v1. It needs the consensus database and the partition geometry. See `submodule-index-convert --help`.\n\
Or pass --migrate-index when the directory name contains \"copy\". 4.0.8 did not ship a RocksDB index.",
        path.display(),
        path.display()
    )
}

fn accept_rocks_schema(text: &str) -> eyre::Result<()> {
    if text == SUBMODULE_SCHEMA_ROCKS_V1 {
        return Ok(());
    }
    eyre::bail!("{}", unrecognised_rocks_layout(text))
}

fn open_rocks_index(path: &Path) -> eyre::Result<SubmoduleIndex> {
    let text = fs::read_to_string(path.join("SCHEMA")).unwrap_or_default();
    accept_rocks_schema(&text)?;
    open_rocks_checked(path)
}

#[cfg(feature = "rocksdb")]
fn open_rocks_checked(path: &Path) -> eyre::Result<SubmoduleIndex> {
    SubmoduleIndex::open_rocks(path)
}

#[cfg(not(feature = "rocksdb"))]
fn open_rocks_checked(path: &Path) -> eyre::Result<SubmoduleIndex> {
    let _ = path;
    eyre::bail!(
        "this index is RocksDB. Rebuild with --features rocksdb: cargo build -p irys-database --features rocksdb --release --bin tx-path-derive-bench"
    )
}

fn harvest_offsets(
    index: &SubmoduleIndex,
    engine: Engine,
    args: &Args,
    mdbx_intervals: &[(u32, u32)],
) -> eyre::Result<Vec<u32>> {
    let want = args.samples.saturating_mul(4).clamp(1, 20_000);
    let candidates = match engine {
        Engine::Mdbx => spread_offsets(mdbx_intervals, want, args.index_span),
        Engine::Rocks => {
            let span = args.index_span.unwrap_or((0, DEFAULT_SPAN_END));
            if args.index_span.is_none() {
                println!(
                    "index span default 0:{DEFAULT_SPAN_END} (exclusive end). Pass --index-span START:END if offsets live higher."
                );
            }
            harvest_rocks(index, span, want)?
        }
        Engine::Auto => eyre::bail!("engine was not resolved"),
    };
    let mut usable = Vec::new();
    for offset in candidates {
        if let (Some(_), Some(_)) = read_stored_tx_data_path(index, PartitionChunkOffset(offset))? {
            usable.push(offset);
        }
    }
    Ok(usable)
}

fn mdbx_intervals(path: &Path) -> eyre::Result<Vec<(u32, u32)>> {
    let env = open_mdbx_ro(path)?;
    env.view(|tx| -> eyre::Result<Vec<(u32, u32)>> {
        let mut cursor = tx.cursor_read::<TxPathIntervalByStart>()?;
        let mut spans = Vec::new();
        for row in cursor.walk(None)? {
            let (start, interval) = row?;
            if interval.end.0 >= start.0 {
                spans.push((start.0, interval.end.0));
            }
        }
        Ok(spans)
    })
    .map_err(|err| eyre!("interval scan failed: {err}"))?
}

fn spread_offsets(intervals: &[(u32, u32)], want: usize, span: Option<(u32, u32)>) -> Vec<u32> {
    let clipped = clip_intervals(intervals, span);
    if clipped.is_empty() || want == 0 {
        return Vec::new();
    }
    let total: u64 = clipped
        .iter()
        .map(|&(start, end)| u64::from(end - start) + 1)
        .sum();
    let want_u = u64::try_from(want).unwrap_or(1).max(1);
    let stride = (total / want_u).max(1);
    let mut out = Vec::new();
    let mut next = 0_u64;
    let mut base = 0_u64;
    for (start, end) in clipped {
        let len = u64::from(end - start) + 1;
        while next < base + len {
            let delta = u32::try_from(next - base).unwrap_or(0);
            out.push(start.saturating_add(delta));
            if out.len() >= want {
                return out;
            }
            next = next.saturating_add(stride);
        }
        base += len;
    }
    out
}

fn clip_intervals(intervals: &[(u32, u32)], span: Option<(u32, u32)>) -> Vec<(u32, u32)> {
    let mut clipped = Vec::new();
    for &(start, end) in intervals {
        let (start, end) = match span {
            Some((low, high)) => {
                let start = start.max(low);
                let end = end.min(high.saturating_sub(1));
                (start, end)
            }
            None => (start, end),
        };
        if start <= end {
            clipped.push((start, end));
        }
    }
    clipped
}

fn harvest_rocks(index: &SubmoduleIndex, span: (u32, u32), want: usize) -> eyre::Result<Vec<u32>> {
    let mut cursor = span.0;
    let mut stride = 256_u32;
    let mut found = Vec::new();
    while cursor < span.1 && found.len() < want {
        let reach = stride.saturating_mul(64).max(stride);
        let window_last = cursor
            .saturating_add(reach)
            .saturating_sub(1)
            .min(span.1.saturating_sub(1));
        if window_last < cursor {
            break;
        }
        let rows = index.view(|read| {
            read.path_hashes_in_inclusive_range(
                PartitionChunkOffset(cursor),
                PartitionChunkOffset(window_last),
            )
        })?;
        if rows.is_empty() {
            stride = stride.saturating_mul(2).min(1_048_576);
            cursor = window_last.saturating_add(1);
            continue;
        }
        for (offset, hashes) in rows {
            if hashes.tx_path_hash.is_some() {
                found.push(offset.0);
                if found.len() >= want {
                    break;
                }
            }
        }
        cursor = window_last.saturating_add(1);
    }
    Ok(found)
}

fn detect_engine(path: &Path, requested: Engine) -> eyre::Result<Engine> {
    if !matches!(requested, Engine::Auto) {
        return Ok(requested);
    }
    let mdbx = path.join("mdbx.dat").is_file();
    let rocks = path.join("SCHEMA").is_file();
    match (mdbx, rocks) {
        (true, false) => Ok(Engine::Mdbx),
        (false, true) => Ok(Engine::Rocks),
        (true, true) => {
            eyre::bail!(
                "both mdbx.dat and SCHEMA exist at {}. Pass --engine.",
                path.display()
            )
        }
        (false, false) => eyre::bail!(
            "cannot tell the engine at {}. MDBX has mdbx.dat. RocksDB has SCHEMA. Pass --engine.",
            path.display()
        ),
    }
}

fn engine_name(engine: Engine) -> &'static str {
    match engine {
        Engine::Auto => "auto",
        Engine::Mdbx => "mdbx",
        Engine::Rocks => "rocks",
    }
}

fn name_has_copy(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.to_ascii_lowercase().contains("copy"))
}

enum MissingPart {
    Normal(std::ffi::OsString),
    CurDir,
    ParentDir,
}

/// Canonicalize the deepest existing ancestor, then apply the missing tail.
/// `..` in that tail is applied, so `copy/../../outside` does not keep the name.
fn resolve_with_ancestor(path: &Path) -> eyre::Result<PathBuf> {
    let mut missing = Vec::new();
    let mut cursor = path.to_path_buf();
    loop {
        if cursor.as_os_str().is_empty() {
            cursor = std::env::current_dir()?;
        }
        if cursor.exists() || is_symlink(&cursor) {
            let mut resolved = cursor.canonicalize()?;
            apply_missing(&mut resolved, &missing)?;
            return Ok(resolved);
        }
        let popped = match cursor.components().next_back() {
            Some(Component::Normal(name)) => MissingPart::Normal(name.to_os_string()),
            Some(Component::CurDir) => MissingPart::CurDir,
            Some(Component::ParentDir) => MissingPart::ParentDir,
            _ => {
                eyre::bail!("{} has no existing ancestor", path.display())
            }
        };
        let Some(parent) = cursor.parent() else {
            eyre::bail!("{} has no existing ancestor", path.display())
        };
        if parent == cursor {
            eyre::bail!("{} has no existing ancestor", path.display())
        }
        missing.push(popped);
        if parent.as_os_str().is_empty() {
            cursor = std::env::current_dir()?;
            continue;
        }
        cursor = parent.to_path_buf();
    }
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
}

fn apply_missing(base: &mut PathBuf, missing: &[MissingPart]) -> eyre::Result<()> {
    for part in missing.iter().rev() {
        match part {
            MissingPart::Normal(name) => base.push(name),
            MissingPart::CurDir => {}
            MissingPart::ParentDir => {
                if !base.pop() {
                    eyre::bail!("path escapes the filesystem root");
                }
            }
        }
    }
    Ok(())
}

/// Absolute path with `.` and `..` collapsed and symlinks left in place.
/// That is the directory the operator named, not the one a symlink reaches.
fn lexical_absolute(path: &Path) -> eyre::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    Ok(out)
}

fn lexical_parent(path: &Path) -> eyre::Result<PathBuf> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    lexical_absolute(parent)
}

/// Resolve `path`, then refuse when that parent is not the directory named
/// on the command line. A missing final component makes `canonicalize` return
/// `NotFound`, and `create_dir_all` would follow a symlink parent.
fn resolve_operator_path(path: &Path) -> eyre::Result<PathBuf> {
    let resolved = resolve_with_ancestor(path)?;
    let named_parent = lexical_parent(path)?;
    let resolved_parent = match resolved.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("/"),
    };
    if named_parent != resolved_parent {
        eyre::bail!(
            "refusing {}: it resolves under {}, not {}",
            path.display(),
            resolved_parent.display(),
            named_parent.display()
        );
    }
    Ok(resolved)
}

fn require_copy_dir(path: &Path, kind: &str) -> eyre::Result<()> {
    if !name_has_copy(path) {
        eyre::bail!(
            "refusing to migrate {kind} {}: directory name must contain \"copy\"",
            path.display()
        );
    }
    let resolved = resolve_operator_path(path)?;
    if !name_has_copy(&resolved) {
        eyre::bail!(
            "refusing to migrate {kind} {}: it resolves to {}, whose name does not contain \"copy\"",
            path.display(),
            resolved.display()
        );
    }
    Ok(())
}

fn prepare_fixture(dir: &Path) -> eyre::Result<()> {
    require_copy_dir(dir, "fixture directory")?;
    let consensus = dir.join("consensus");
    let index_dir = dir.join("submodule-db.copy");
    if consensus.join("mdbx.dat").is_file() {
        eyre::bail!("fixture already exists at {}", consensus.display());
    }
    fs::create_dir_all(&consensus)?;
    fs::create_dir_all(&index_dir)?;
    let mut next_id = 1_000_u64;
    let publish_10 = make_txs(&mut next_id, 10);
    let publish_100 = make_txs(&mut next_id, 100);
    let publish_1000 = make_txs(&mut next_id, 1000);
    let submit_10 = make_txs(&mut next_id, 10);
    let blocks = [
        prepared_block(0, &[], 0, &[], 0),
        prepared_block(1, &publish_10, 10, &[], 0),
        prepared_block(2, &publish_100, 110, &[], 0),
        prepared_block(3, &publish_1000, 1110, &[], 0),
        prepared_block(4, &[], 1110, &submit_10, 10),
    ];
    let db = open_or_create_db(
        &consensus,
        IrysTables::ALL,
        DatabaseArguments::irys_default(DbSyncMode::Durable)?.with_log_level(None),
    )?;
    db.update_eyre(|tx| {
        for group in [&publish_10, &publish_100, &publish_1000, &submit_10] {
            for header in group {
                insert_tx_header(tx, header)?;
            }
        }
        for block in &blocks {
            insert_block_header(tx, block)?;
            insert_block_index_items_for_block(tx, block)?;
        }
        set_database_schema_version(tx, DatabaseVersion::V3)?;
        Ok(())
    })?;
    drop(db);

    let index = SubmoduleIndex::open_mdbx(&index_dir, submodule_args()?)?;
    store_paths(&index, &publish_100, 10)?;
    store_paths(&index, &publish_1000, 110)?;
    println!("prepared consensus {}", consensus.display());
    println!("prepared submodule {}", index_dir.display());
    println!("schema V3. publish spans at heights 1, 2, 3. submit span at height 4.");
    println!(
        "stored tx paths for the 100-tx and 1000-tx publish blocks (partition offset = ledger offset)."
    );
    println!(
        "run: tx-path-derive-bench --db {} --index {} --samples 30",
        consensus.display(),
        index_dir.display()
    );
    Ok(())
}

fn make_txs(next_id: &mut u64, count: usize) -> Vec<DataTransactionHeader> {
    let mut headers = Vec::with_capacity(count);
    for _ in 0..count {
        headers.push(fixture_tx(*next_id, DEFAULT_CHUNK));
        *next_id += 1;
    }
    headers
}

fn fixture_tx(id: u64, size: u64) -> DataTransactionHeader {
    let mut header = DataTransactionHeader::default();
    header.id = hid(id);
    header.data_root = hid(id.saturating_add(100_000));
    header.prefix_hash = hid(id.saturating_add(200_000));
    header.data_size = size;
    header
}

fn hid(id: u64) -> H256 {
    let mut bytes = [0_u8; 32];
    bytes[..8].copy_from_slice(&id.to_be_bytes());
    H256::from(bytes)
}

fn prepared_block(
    height: u64,
    publish: &[DataTransactionHeader],
    publish_total: u64,
    submit: &[DataTransactionHeader],
    submit_total: u64,
) -> IrysBlockHeader {
    let mut block = IrysBlockHeader::new_mock_header();
    block.height = height;
    block.block_hash = hid(height.saturating_add(1));
    set_fixture_ledger(&mut block, DataLedger::Publish, publish, publish_total);
    set_fixture_ledger(&mut block, DataLedger::Submit, submit, submit_total);
    block
}

fn set_fixture_ledger(
    block: &mut IrysBlockHeader,
    ledger: DataLedger,
    txs: &[DataTransactionHeader],
    total: u64,
) {
    let (root, _) = DataTransactionLedger::merklize_tx_root(txs);
    let slot = block
        .data_ledgers
        .iter_mut()
        .find(|item| item.ledger_id == ledger.get_id())
        .expect("mock header has the ledger");
    slot.tx_ids = H256List(txs.iter().map(|header| header.id).collect());
    slot.tx_root = root;
    slot.total_chunks = total;
}

fn store_paths(
    index: &SubmoduleIndex,
    headers: &[DataTransactionHeader],
    start: u64,
) -> eyre::Result<()> {
    let (_, proofs) = DataTransactionLedger::merklize_tx_root(headers);
    index.update(|write| {
        let mut cursor = start;
        for (header, proof) in headers.iter().zip(&proofs) {
            let chunks = header.data_size.div_ceil(DEFAULT_CHUNK);
            eyre::ensure!(chunks > 0, "fixture tx has no chunks");
            let end = cursor + chunks - 1;
            let path_hash = tx_path_hash(&proof.proof);
            write.add_full_tx_path(path_hash, proof.proof.clone())?;
            write.add_tx_path_hash_to_offset_range(
                PartitionChunkOffset(u32_offset(cursor)?),
                PartitionChunkOffset(u32_offset(end)?),
                Some(path_hash),
            )?;
            let mut offset = cursor;
            while offset <= end {
                let data = offset.to_le_bytes().to_vec();
                let data_hash = H256::from(hash_sha256(&data));
                let chunk = PartitionChunkOffset(u32_offset(offset)?);
                write.add_full_data_path(chunk, data)?;
                write.add_data_path_hash_to_offset_index(chunk, Some(data_hash))?;
                offset += 1;
            }
            cursor = end + 1;
        }
        Ok(())
    })
}

fn u32_offset(value: u64) -> eyre::Result<u32> {
    u32::try_from(value).map_err(|_| eyre!("offset {value} exceeds u32"))
}

struct XorShift(u64);

impl XorShift {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, len: usize) -> usize {
        if len <= 1 {
            return 0;
        }
        usize::try_from(self.next_u64() % u64::try_from(len).unwrap_or(1)).unwrap_or(0)
    }

    fn below_u64(&mut self, span: u64) -> u64 {
        if span <= 1 {
            return 0;
        }
        self.next_u64() % span
    }
}

fn thread_seed(index: usize) -> u64 {
    0xA5A5_5A5A_1234_0000
        ^ u64::try_from(index)
            .unwrap_or(0)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

fn fit_nanos(duration: std::time::Duration) -> eyre::Result<u64> {
    u64::try_from(duration.as_nanos()).map_err(|_| eyre!("nanoseconds exceed u64"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DiskId {
    major: u32,
    minor: u32,
    name: String,
}

#[derive(Clone, Copy)]
struct DiskSnap {
    reads: u64,
    read_sectors: u64,
}

fn device_for(dir: &Path) -> Option<DiskId> {
    disk_id(dir).ok().flatten()
}

fn disk_id(dir: &Path) -> eyre::Result<Option<DiskId>> {
    let canon = dir.canonicalize()?;
    let text = fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    Ok(mount_disk(&text, &canon))
}

fn disk_counters(disk: Option<&DiskId>) -> Option<DiskSnap> {
    let disk = disk?;
    let text = fs::read_to_string("/proc/diskstats").ok()?;
    parse_diskstats(&text, disk.major, disk.minor)
}

fn disk_delta(before: Option<DiskSnap>, after: Option<DiskSnap>) -> (u64, u64) {
    match (before, after) {
        (Some(before), Some(after)) => (
            after.reads.saturating_sub(before.reads),
            after.read_sectors.saturating_sub(before.read_sectors),
        ),
        _ => (0, 0),
    }
}

fn mount_disk(mountinfo: &str, path: &Path) -> Option<DiskId> {
    let mut best: Option<(usize, u32, u32)> = None;
    for line in mountinfo.lines() {
        let Some((mount, major, minor)) = parse_mount_line(line) else {
            continue;
        };
        let mount_path = Path::new(&mount);
        if path != mount_path && !path.starts_with(mount_path) {
            continue;
        }
        if best
            .as_ref()
            .is_some_and(|(best_len, _, _)| *best_len >= mount.len())
        {
            continue;
        }
        best = Some((mount.len(), major, minor));
    }
    let (_, major, minor) = best?;
    let stats = fs::read_to_string("/proc/diskstats").ok()?;
    let name = disk_name(&stats, major, minor)?;
    Some(DiskId { major, minor, name })
}

fn parse_mount_line(line: &str) -> Option<(String, u32, u32)> {
    let mut fields = line.split_whitespace();
    let _id = fields.next()?;
    let _parent = fields.next()?;
    let dev = fields.next()?;
    let _root = fields.next()?;
    let mount = unescape_mount(fields.next()?);
    let (major, minor) = dev.split_once(':')?;
    Some((mount, major.parse().ok()?, minor.parse().ok()?))
}

fn unescape_mount(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 3 < bytes.len() {
            let oct = &text[index + 1..index + 4];
            if let Ok(value) = u8::from_str_radix(oct, 8) {
                out.push(char::from(value));
                index += 4;
                continue;
            }
        }
        out.push(char::from(bytes[index]));
        index += 1;
    }
    out
}

fn parse_diskstats(text: &str, major: u32, minor: u32) -> Option<DiskSnap> {
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let line_major: u32 = fields.next()?.parse().ok()?;
        let line_minor: u32 = fields.next()?.parse().ok()?;
        if line_major != major || line_minor != minor {
            continue;
        }
        let _name = fields.next()?;
        let reads = fields.next()?.parse().ok()?;
        let _merged = fields.next()?;
        let read_sectors = fields.next()?.parse().ok()?;
        return Some(DiskSnap {
            reads,
            read_sectors,
        });
    }
    None
}

fn disk_name(text: &str, major: u32, minor: u32) -> Option<String> {
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let line_major: u32 = fields.next()?.parse().ok()?;
        let line_minor: u32 = fields.next()?.parse().ok()?;
        let name = fields.next()?;
        if line_major == major && line_minor == minor {
            return Some(name.to_owned());
        }
    }
    None
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn posix_fadvise(fd: i32, offset: i64, len: i64, advice: i32) -> i32;
}

#[cfg(target_os = "linux")]
const POSIX_FADV_DONTNEED: i32 = 4;

fn evict_cache(dir: &Path) -> eyre::Result<u32> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = dir;
        eyre::bail!("cold reads need linux posix_fadvise");
    }
    #[cfg(target_os = "linux")]
    {
        let mut files = 0_u32;
        let mut stack = vec![dir.to_path_buf()];
        while let Some(path) = stack.pop() {
            let meta = fs::symlink_metadata(&path)?;
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                for entry in fs::read_dir(&path)? {
                    stack.push(entry?.path());
                }
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            let file = File::open(&path)?;
            let rc = unsafe { posix_fadvise(file.as_raw_fd(), 0, 0, POSIX_FADV_DONTNEED) };
            if rc != 0 {
                eyre::bail!(
                    "fadvise {} failed: {}",
                    path.display(),
                    IoError::from_raw_os_error(rc)
                );
            }
            files += 1;
        }
        Ok(files)
    }
}

fn parse_args(raw: &[String]) -> eyre::Result<Option<Args>> {
    if raw.is_empty() || raw.iter().any(|arg| arg == "--help" || arg == "-h") {
        return Ok(None);
    }
    let mut args = Args::default();
    let mut seen = Vec::<&'static str>::new();
    let mut index_at = 0;
    while index_at < raw.len() {
        let flag = raw[index_at].as_str();
        index_at += 1;
        apply_flag(&mut args, &mut seen, raw, &mut index_at, flag)?;
    }
    validate(&args)?;
    Ok(Some(args))
}

fn mark_once(seen: &mut Vec<&'static str>, flag: &'static str) -> eyre::Result<()> {
    if seen.contains(&flag) {
        eyre::bail!("{flag} given twice");
    }
    seen.push(flag);
    Ok(())
}

fn apply_flag(
    args: &mut Args,
    seen: &mut Vec<&'static str>,
    raw: &[String],
    index_at: &mut usize,
    flag: &str,
) -> eyre::Result<()> {
    match flag {
        "--db" => {
            mark_once(seen, "--db")?;
            args.db = Some(PathBuf::from(next_value(raw, index_at, flag)?));
        }
        "--index" => {
            mark_once(seen, "--index")?;
            args.index = Some(PathBuf::from(next_value(raw, index_at, flag)?));
        }
        "--engine" => {
            mark_once(seen, "--engine")?;
            args.engine = parse_engine(next_value(raw, index_at, flag)?)?;
        }
        "--mode" => {
            mark_once(seen, "--mode")?;
            args.mode = parse_mode(next_value(raw, index_at, flag)?)?;
        }
        "--samples" => {
            mark_once(seen, "--samples")?;
            args.samples = parse_int(next_value(raw, index_at, flag)?, flag)?;
        }
        "--threads" => {
            mark_once(seen, "--threads")?;
            args.threads = parse_int(next_value(raw, index_at, flag)?, flag)?;
        }
        "--chunk-size" => {
            mark_once(seen, "--chunk-size")?;
            args.chunk_size = parse_int(next_value(raw, index_at, flag)?, flag)?;
        }
        "--ledgers" => {
            mark_once(seen, "--ledgers")?;
            args.ledgers = parse_ledgers(next_value(raw, index_at, flag)?)?;
        }
        "--synthetic" => {
            mark_once(seen, "--synthetic")?;
            args.synthetic = parse_synthetic(next_value(raw, index_at, flag)?)?;
        }
        "--prefetch" => {
            mark_once(seen, "--prefetch")?;
            args.prefetch = parse_synthetic(next_value(raw, index_at, flag)?)?;
        }
        "--index-span" => {
            mark_once(seen, "--index-span")?;
            args.index_span = Some(parse_span(next_value(raw, index_at, flag)?)?);
        }
        "--prepare" => {
            mark_once(seen, "--prepare")?;
            args.prepare = Some(PathBuf::from(next_value(raw, index_at, flag)?));
        }
        "--cold-only" | "--warm-only" => {
            if seen
                .iter()
                .any(|item| *item == "--cold-only" || *item == "--warm-only")
            {
                eyre::bail!("pass only one of --cold-only and --warm-only");
            }
            let warm = flag == "--warm-only";
            seen.push(if warm { "--warm-only" } else { "--cold-only" });
            args.warm = warm;
            args.cold = !warm;
        }
        "--migrate-copy" => {
            mark_once(seen, "--migrate-copy")?;
            args.migrate_copy = true;
        }
        "--migrate-index" => {
            mark_once(seen, "--migrate-index")?;
            args.migrate_index = true;
        }
        other => eyre::bail!("unknown argument {other}"),
    }
    Ok(())
}

fn next_value<'a>(raw: &'a [String], index: &mut usize, flag: &str) -> eyre::Result<&'a str> {
    let Some(value) = raw.get(*index) else {
        eyre::bail!("{flag} needs a value");
    };
    if value.starts_with("--") {
        eyre::bail!("{flag} needs a value");
    }
    *index += 1;
    Ok(value)
}

fn parse_int<T: std::str::FromStr>(text: &str, flag: &str) -> eyre::Result<T> {
    text.parse::<T>()
        .map_err(|_| eyre!("{flag} expects an integer, got {text}"))
}

fn parse_engine(text: &str) -> eyre::Result<Engine> {
    match text {
        "auto" => Ok(Engine::Auto),
        "mdbx" => Ok(Engine::Mdbx),
        "rocks" => Ok(Engine::Rocks),
        other => eyre::bail!("unknown engine {other}"),
    }
}

fn parse_mode(text: &str) -> eyre::Result<Mode> {
    match text {
        "all" => Ok(Mode::All),
        "derive-real" => Ok(Mode::Real),
        "derive-synthetic" => Ok(Mode::Synthetic),
        "baseline-stored" => Ok(Mode::Baseline),
        other => eyre::bail!("unknown mode {other}"),
    }
}

fn parse_ledgers(text: &str) -> eyre::Result<Vec<DataLedger>> {
    let mut ledgers = Vec::new();
    for part in text.split(',') {
        if part.is_empty() {
            eyre::bail!("empty ledger in {text}");
        }
        ledgers.push(parse_ledger(part)?);
    }
    Ok(ledgers)
}

fn parse_ledger(text: &str) -> eyre::Result<DataLedger> {
    match text {
        "publish" | "0" => Ok(DataLedger::Publish),
        "submit" | "1" => Ok(DataLedger::Submit),
        "one_year" | "10" => Ok(DataLedger::OneYear),
        "thirty_day" | "one_month" | "20" => Ok(DataLedger::ThirtyDay),
        other => eyre::bail!("unknown ledger {other}"),
    }
}

fn parse_synthetic(text: &str) -> eyre::Result<Vec<usize>> {
    let mut widths = Vec::new();
    for part in text.split(',') {
        if part.is_empty() {
            eyre::bail!("empty synthetic width in {text}");
        }
        let width = parse_int(part, "--synthetic")?;
        if width == 0 {
            eyre::bail!("synthetic width must be >= 1");
        }
        widths.push(width);
    }
    Ok(widths)
}

fn parse_span(text: &str) -> eyre::Result<(u32, u32)> {
    let Some((left, right)) = text.split_once(':') else {
        eyre::bail!("index span must be START:END");
    };
    let start: u32 = left.parse().map_err(|_| eyre!("bad span start {left}"))?;
    let end: u32 = right.parse().map_err(|_| eyre!("bad span end {right}"))?;
    if end <= start {
        eyre::bail!("index span end must be greater than start");
    }
    Ok((start, end))
}

fn validate(args: &Args) -> eyre::Result<()> {
    if args.prepare.is_some() && args.db.is_some() {
        eyre::bail!("--prepare does not take --db");
    }
    if args.prepare.is_some() {
        return Ok(());
    }
    if args.db.is_none() {
        eyre::bail!("--db is required");
    }
    if args.samples == 0 {
        eyre::bail!("--samples must be >= 1");
    }
    if args.threads == 0 {
        eyre::bail!("--threads must be >= 1");
    }
    if args.chunk_size == 0 {
        eyre::bail!("--chunk-size must be >= 1");
    }
    if matches!(args.mode, Mode::Baseline) && args.index.is_none() {
        eyre::bail!("baseline-stored needs --index");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Engine, Mode, parse_args, percentile};

    fn temp() -> irys_testing_utils::tempfile::TempDir {
        irys_testing_utils::TempDirBuilder::new()
            .prefix("tx-copy-guard-")
            .build()
    }

    fn live_link(root: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let real = root.join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("LIVE"), b"live").unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        (real, link)
    }

    #[test]
    fn require_copy_dir_refuses_a_missing_name_through_a_symlink() {
        let tmp = temp();
        let (real, link) = live_link(tmp.path());
        let path = link.join("new.copy");
        let err = super::require_copy_dir(&path, "consensus database").unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("resolves under"),
            "missing final name through a symlink was accepted: {text}"
        );
        assert!(!real.join("new.copy").exists());
        assert_eq!(std::fs::read(real.join("LIVE")).unwrap(), b"live");
    }

    #[test]
    fn require_copy_dir_applies_dotdot_across_a_symlink() {
        let tmp = temp();
        let real = tmp.path().join("other").join("live");
        std::fs::create_dir_all(&real).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let path = link.join("..").join("escaped.copy");
        let err = super::require_copy_dir(&path, "consensus database").unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("resolves under"),
            "dotdot through a symlink was accepted: {text}"
        );
        assert!(!real.parent().unwrap().join("escaped.copy").exists());
    }

    #[test]
    fn require_copy_dir_allows_a_missing_child_of_the_named_directory() {
        let tmp = temp();
        let parent = tmp.path().join("bench");
        std::fs::create_dir(&parent).unwrap();
        let path = parent.join("db.copy");
        super::require_copy_dir(&path, "consensus database").unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn prepare_fixture_does_not_create_through_a_symlink() {
        let tmp = temp();
        let (real, link) = live_link(tmp.path());
        let path = link.join("fixture.copy");
        let err = super::prepare_fixture(&path).unwrap_err();
        assert!(
            !real.join("fixture.copy").exists(),
            "prepare created a directory through the symlink"
        );
        assert!(!real.join("consensus").exists());
        assert_eq!(std::fs::read(real.join("LIVE")).unwrap(), b"live");
        let text = format!("{err:#}");
        assert!(text.contains("resolves under"), "{text}");
    }

    #[test]
    fn prepare_fixture_refuses_a_name_without_copy() {
        let tmp = temp();
        let path = tmp.path().join("fixture");
        let err = super::prepare_fixture(&path).unwrap_err();
        assert!(!path.exists());
        assert!(!path.join("consensus").exists());
        let text = format!("{err:#}");
        assert!(text.contains("copy"), "{text}");
    }

    fn args(parts: &[&str]) -> super::Args {
        let raw: Vec<String> = parts.iter().map(|part| (*part).to_owned()).collect();
        parse_args(&raw).expect("parse").expect("not help")
    }

    #[test]
    fn help_and_empty_args() {
        assert!(parse_args(&[]).expect("parse").is_none());
        let help = ["--help".to_owned()];
        assert!(parse_args(&help).expect("parse").is_none());
    }

    #[test]
    fn parses_ledgers_span_and_prepare() {
        let parsed = args(&[
            "--prepare",
            "/tmp/fixture",
            "--ledgers",
            "one_month,10,submit",
            "--synthetic",
            "10,100",
            "--index-span",
            "5:90",
            "--engine",
            "rocks",
            "--threads",
            "4",
        ]);
        assert_eq!(
            parsed.prepare.as_deref(),
            Some(std::path::Path::new("/tmp/fixture"))
        );
        assert_eq!(parsed.ledgers.len(), 3);
        assert!(matches!(
            parsed.ledgers[0],
            irys_types::DataLedger::ThirtyDay
        ));
        assert!(matches!(parsed.ledgers[1], irys_types::DataLedger::OneYear));
        assert_eq!(parsed.synthetic, vec![10, 100]);
        assert_eq!(parsed.index_span, Some((5, 90)));
        assert!(matches!(parsed.engine, Engine::Rocks));
        assert_eq!(parsed.threads, 4);
    }

    #[test]
    fn rejects_bad_combinations() {
        let cases = [
            vec!["--db", "/tmp/db", "--prepare", "/tmp/out"],
            vec!["--mode", "baseline-stored", "--db", "/tmp/db"],
            vec!["--db", "/tmp/db", "--cold-only", "--warm-only"],
            vec!["--db", "/tmp/db", "--threads", "0"],
            vec!["--db", "/tmp/db", "--samples", "0"],
            vec!["--db", "/tmp/db", "--chunk-size", "0"],
            vec!["--db", "/tmp/db", "--engine", "tape"],
            vec!["--db", "/tmp/a", "--db", "/tmp/b"],
            vec!["--not-a-flag"],
        ];
        for case in cases {
            let raw: Vec<String> = case.iter().map(|part| (*part).to_owned()).collect();
            assert!(parse_args(&raw).is_err(), "{case:?}");
        }
    }

    #[test]
    fn baseline_mode_accepts_an_index() {
        let parsed = args(&[
            "--db",
            "/tmp/db.copy",
            "--index",
            "/tmp/submodule-db.copy",
            "--mode",
            "baseline-stored",
            "--warm-only",
        ]);
        assert!(matches!(parsed.mode, Mode::Baseline));
        assert!(parsed.warm);
        assert!(!parsed.cold);
        assert!(parsed.index.is_some());
    }

    #[test]
    fn public_rocks_marker_opens_and_a_foreign_marker_is_refused() {
        use irys_database::submodule::tables::SUBMODULE_SCHEMA_ROCKS_V1;

        super::accept_rocks_schema(SUBMODULE_SCHEMA_ROCKS_V1).unwrap();
        let foreign = super::accept_rocks_schema("foreign-layout\n").unwrap_err();
        let message = foreign.to_string();
        assert!(
            message.contains("unrecognised RocksDB index layout"),
            "{message}"
        );
        assert!(message.contains("RocksDB v1 only"), "{message}");
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let sorted = [10_u64, 20, 30, 40];
        assert_eq!(percentile(&sorted, 50), 20);
        assert_eq!(percentile(&sorted, 99), 40);
        assert_eq!(percentile(&sorted, 0), 10);
        assert_eq!(percentile(&[], 50), 0);
    }

    fn sample(db_ns: u64, cpu_ns: u64, tx_count: u64, proof_len: u64) -> super::Sample {
        super::Sample {
            core: super::SampleCore {
                total_ns: db_ns.saturating_add(cpu_ns),
                db_ns,
                cpu_ns,
                tx_count,
                proof_len,
            },
            reads: 0,
            sectors: 0,
        }
    }

    #[test]
    fn proof_len_and_tx_count_are_not_durations() {
        let ok = vec![
            sample(1_500, 2_500, 3, 1_500),
            sample(4_500, 500, 2_500, 9_000),
        ];
        let lines = super::phase_lines(
            "derive",
            "warm",
            &ok,
            &super::FailureLog::default(),
            1,
            None,
            true,
        );
        let proof: Vec<_> = lines
            .iter()
            .filter(|line| line.contains("proof_len"))
            .cloned()
            .collect();
        assert!(
            proof
                .iter()
                .all(|line| !line.contains("_ns") && !line.contains("_us")),
            "proof length printed as time: {proof:?}"
        );
        assert_eq!(proof.len(), 1, "proof length printed as time: {proof:?}");
        assert!(
            proof[0].contains("min=1500") && proof[0].contains("max=9000"),
            "proof length bytes missing: {proof:?}"
        );
        let widths: Vec<_> = lines
            .iter()
            .filter(|line| line.starts_with("tx_count"))
            .cloned()
            .collect();
        assert_eq!(widths.len(), 1, "tx count printed as time: {widths:?}");
        assert!(
            widths[0].contains("min=3") && widths[0].contains("max=2500"),
            "{widths:?}"
        );
        assert!(
            !widths[0].contains("_ns") && !widths[0].contains("_us"),
            "tx count printed as time: {widths:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("db_ns ") && line.contains("min=1500")),
            "db time lost its nanosecond rank: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("db_us ") && line.contains("min=1")),
            "db time lost its microsecond rank: {lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.starts_with("cpu_ns "))
                && lines.iter().any(|line| line.starts_with("total_ns ")),
            "cpu or total left the duration format: {lines:?}"
        );
        let baseline = super::phase_lines(
            "baseline",
            "warm",
            &ok,
            &super::FailureLog::default(),
            1,
            None,
            false,
        );
        let baseline_proof: Vec<_> = baseline
            .iter()
            .filter(|line| line.contains("proof_len"))
            .cloned()
            .collect();
        assert_eq!(baseline_proof.len(), 1, "{baseline_proof:?}");
        assert!(
            baseline_proof[0].contains("min=1500") && !baseline_proof[0].contains("_ns"),
            "stored proof length printed as time: {baseline_proof:?}"
        );
        assert!(
            !baseline.iter().any(|line| line.starts_with("tx_count")),
            "baseline printed tx widths: {baseline:?}"
        );
    }

    fn core_ok() -> super::SampleCore {
        super::SampleCore {
            total_ns: 1,
            db_ns: 1,
            cpu_ns: 0,
            tx_count: 1,
            proof_len: 4,
        }
    }

    #[test]
    fn worker_counts_every_failed_sample() {
        let next = std::sync::atomic::AtomicUsize::new(0);
        let (ok, failures) = super::worker(0, 8, false, None, None, &|_rng| {
            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Err(eyre::eyre!("boom-{i}"))
        });
        assert!(ok.is_empty());
        assert_eq!(failures.count, 8);
        assert_eq!(
            failures.samples,
            (0..5).map(|i| format!("boom-{i}")).collect::<Vec<_>>()
        );
        let lines = super::phase_lines("phase", "cold", &ok, &failures, 1, None, false);
        assert!(
            lines[0].contains("ok=0 fail=8 "),
            "fail count used the sample cap: {lines:?}"
        );
        let printed: Vec<_> = lines
            .iter()
            .filter(|line| line.contains("failure:"))
            .collect();
        assert_eq!(printed.len(), 5, "{printed:?}");
        assert!(printed.iter().any(|line| line.contains("boom-4")));
        assert!(!lines.iter().any(|line| line.contains("boom-7")));
    }

    #[test]
    fn worker_keeps_every_message_under_the_cap() {
        let next = std::sync::atomic::AtomicUsize::new(0);
        let (_ok, failures) = super::worker(0, 3, false, None, None, &|_rng| {
            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Err(eyre::eyre!("boom-{i}"))
        });
        assert_eq!(failures.count, 3);
        assert_eq!(
            failures.samples,
            vec![
                "boom-0".to_owned(),
                "boom-1".to_owned(),
                "boom-2".to_owned()
            ]
        );
        let lines = super::phase_lines("phase", "cold", &[], &failures, 1, None, false);
        assert!(lines[0].contains("fail=3"), "{lines:?}");
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains("failure:"))
                .count(),
            3
        );
    }

    #[test]
    fn measure_counts_failures_past_the_sample_cap() {
        for (threads, samples) in [(1_usize, 8_usize), (1, 40), (2, 20)] {
            let stats = super::measure("phase", true, threads, samples, None, None, false, |_| {
                Err(eyre::eyre!("boom"))
            });
            assert_eq!(
                stats.attempted, samples,
                "threads={threads} samples={samples}"
            );
            assert_eq!(stats.succeeded, 0, "threads={threads}");
        }
    }

    #[test]
    fn measure_counts_failures_when_some_samples_succeed() {
        let next = std::sync::atomic::AtomicUsize::new(0);
        let stats = super::measure("phase", true, 2, 20, None, None, true, |_| {
            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if i.is_multiple_of(5) {
                Ok(core_ok())
            } else {
                Err(eyre::eyre!("boom-{i}"))
            }
        });
        assert_eq!(stats.succeeded, 4);
        assert_eq!(stats.attempted, 20);
    }

    #[test]
    fn measure_counts_a_clean_run_as_zero_failures() {
        let stats = super::measure("phase", true, 2, 6, None, None, false, |_| Ok(core_ok()));
        assert_eq!(stats.attempted, 6);
        assert_eq!(stats.succeeded, 6);
    }

    #[test]
    fn warmup_failure_is_not_a_sample_failure() {
        let next = std::sync::atomic::AtomicUsize::new(0);
        let stats = super::measure("phase", false, 1, 4, None, None, false, |_| {
            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if i == 0 {
                Err(eyre::eyre!("warmup"))
            } else {
                Ok(core_ok())
            }
        });
        assert_eq!(stats.succeeded, 4);
        assert_eq!(stats.attempted, 4);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn evict_failures_are_counted_past_the_sample_cap() {
        let next = std::sync::atomic::AtomicUsize::new(0);
        let missing = std::path::Path::new("/no/such/tx-path-evict");
        let stats = super::measure("phase", true, 1, 8, Some(missing), None, false, |_| {
            next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(core_ok())
        });
        assert_eq!(next.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert_eq!(stats.attempted, 8);
        assert_eq!(stats.succeeded, 0);
    }
}
