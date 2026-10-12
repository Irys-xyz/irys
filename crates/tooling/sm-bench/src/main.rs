//! Storage-module load bench.
//!
//! One submodule, one disk lane. Recall reads come from a read-only entropy
//! file. Writes and serve reads of written data come from the write file.
//! The submodule directory must contain a `loadtest` path component.

mod drivers;
mod guard;
mod module;
mod prepop;
mod report;
mod schedule;
mod stats;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use clap::{ArgAction, Parser};
use eyre::eyre;
use irys_database::submodule::{RocksIngestStats, RocksTuning};
use irys_domain::BenchIndex;
use irys_types::PartitionChunkOffset;
use tokio::sync::watch;

use crate::drivers::{
    BenchIo, driver_error, fatal_reason, ingest_driver, migration_driver, note_fatal,
    purge_migrations, recall_driver, resume_frontier, serve_driver, take_fatal,
};
use crate::guard::{
    CHUNK_SIZE, check_written_extents, create_small, has_loadtest_component, require_loadtest,
};
use crate::module::{OpenRequest, flush_batch, open_module, pending_ceiling, span_limit};
use crate::prepop::prepopulate;
use crate::report::{
    PageCacheSpan, RocksStepStats, SampleWriter, StepLog, StepRow, git_info, index_banner,
    plan_steps, read_page_cache, sample_line, write_json, write_summary, write_table,
};
use crate::schedule::{
    Plan, Rates, RecallBar, RecallRule, Step, StepMeasured, apply_step_secs, judge, load_schedule,
    merge_tail, mib_bytes, parse_mix, preset, recall_bar, stop_for_score, tail_window,
};
use crate::stats::{PermitGauge, StepAccum, fold_permit};

/// One 64 MiB tx paced at 2 MiB/s needs about 32s after the clock stops.
/// The drain waits long enough for that tx to become durable.
const DRAIN: Duration = Duration::from_secs(45);

#[derive(Parser, Debug)]
#[command(name = "irys-sm-bench")]
struct Args {
    /// Submodule directory. The path must contain a `loadtest` component.
    #[arg(long)]
    dir: PathBuf,
    /// Read-only entropy file. Opened `O_RDONLY`.
    #[arg(long)]
    recall_file: PathBuf,
    /// Mining recall reads `--recall-file` with `O_DIRECT`.
    ///
    /// Recall ranges are read once and are not reused. Buffered reads of those
    /// ranges fill the page cache. Serving, the entropy sweep, and packed
    /// writes stay on the buffered descriptors. If the open is rejected the
    /// run header says `fallback` and recall stays buffered.
    #[arg(long, action = ArgAction::SetTrue)]
    recall_direct_io: bool,
    /// Write region. Ingest, migration, and serve reads use this file.
    #[arg(long)]
    write_file: PathBuf,
    /// Partition size in chunks. Default: recall file bytes / 256 KiB.
    #[arg(long)]
    chunks: Option<u64>,
    /// Directory for `header.json`, `samples.jsonl`, `summary.json`, and `steps.jsonl`.
    #[arg(long)]
    out: PathBuf,
    /// Built-in schedule: `recall-1`, `recall-1.8`, or `smoke`.
    #[arg(long, conflicts_with = "schedule")]
    preset: Option<String>,
    /// TOML schedule. See the crate README.
    #[arg(long, conflicts_with = "preset")]
    schedule: Option<PathBuf>,
    /// Chunks in one recall read.
    #[arg(long, default_value_t = 400)]
    recall_chunks: u64,
    /// Partition offset that maps to byte 0 of the write file.
    #[arg(long, default_value_t = 0)]
    write_origin: u64,
    /// First partition offset of the migration window. Default: midpoint of the write window.
    #[arg(long)]
    migrate_origin: Option<u64>,
    /// Synthetic index rows to insert before the run. Idempotent across restarts.
    #[arg(long, default_value_t = 0)]
    prepopulate_index: u64,
    /// Submodule index engine. `mdbx` is the production engine.
    #[arg(long, value_enum, default_value_t = IndexEngine::Mdbx)]
    index_engine: IndexEngine,
    /// RocksDB preset. Same names as `RocksTuning::presets`. Default `baseline`
    /// when `--index-engine rocks`.
    #[arg(long)]
    rocks_preset: Option<String>,
    /// Replace the preset block cache, in bytes. Requires `--index-engine rocks`.
    #[arg(long)]
    rocks_block_cache: Option<u64>,
    /// Replace the preset small-file compact threshold. `0` turns the hook off.
    /// Requires `--index-engine rocks`.
    #[arg(long)]
    rocks_small_file_compact_threshold: Option<u64>,
    /// Arm index group commit. MDBX syncs every N registrations. RocksDB
    /// does not sync on this count: the next synced write or the WAL
    /// backstop does.
    ///
    /// `0` does not call `enable_index_group_commit` (that method rejects 0)
    /// and keeps one durable commit per registration.
    #[arg(long, default_value_t = 8)]
    index_group_commit: u32,
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// Create missing recall and write files at this exact size. A multiple of 256 KiB.
    #[arg(long)]
    create_small: Option<u64>,
    #[arg(long, default_value_t = 100)]
    num_writes_before_sync: u64,
    /// Seconds between completion rewrites of `intervals.json`.
    /// Write-ahead `Interrupted` marks stay immediate. `0` rewrites on every sync.
    #[arg(long, default_value_t = irys_types::DEFAULT_INTERVALS_PERSIST_SECS)]
    intervals_persist_secs: u64,
    /// Minimum milliseconds between durable points. A durable point is the
    /// `chunks.dat` `fdatasync` and the synced presence commit. The default
    /// matches the node: 250, so a run without this flag measures production.
    /// On HDD that cap keeps a 16–28 MiB/s ingest under saturation and adds
    /// about 100 ms to durable p99. `0` runs a point on every gap. Pwrites
    /// are not delayed. Force, shutdown, drop, headroom loss, and the
    /// pending-byte ceiling stay immediate. This is the longest a point
    /// may wait.
    #[arg(long, default_value_t = irys_types::DEFAULT_MIN_DURABLE_INTERVAL_MS)]
    min_durable_interval_ms: u64,
    /// Pending bytes, in parts per thousand of the memory ceiling, above
    /// which a durable point runs at once. `400` is the default line.
    /// Crossing it keeps deferral closed until pending bytes fall to half
    /// the line. `1000` puts the line on the ceiling. `0` runs a point as
    /// soon as any byte is pending.
    #[arg(long, default_value_t = irys_types::DEFAULT_DURABLE_HEADROOM_PERMILLE)]
    durable_headroom_permille: u64,
    #[arg(long, default_value_t = 262_144)]
    entropy_sweep_max_bytes: u64,
    /// Transaction size mix `chunks:weight,...`.
    #[arg(long, default_value = "1:40,4:25,16:20,64:10,256:5")]
    ingest_mix: String,
    /// Override every step duration.
    #[arg(long)]
    step_secs: Option<u64>,
    /// Maximum chunks in one sequential serve read.
    #[arg(long, default_value_t = 8)]
    serve_seq_chunks: u32,
    /// When the generator commits `index_transaction_data`.
    ///
    /// `serial` (default) commits each transaction on the generator before that
    /// transaction's chunks are paced. Production indexes on the chunk
    /// migration service and writes bodies on another task, so a later index
    /// overlaps chunk writes. `overlap` starts the next commit while this
    /// transaction's chunks are paced. The submodule MDBX still has one writer.
    #[arg(long, value_enum, default_value_t = IngestIndex::Serial)]
    ingest_index: IngestIndex,
    /// In-flight ingest chunks. Each permit is held until the chunk is durable.
    ///
    /// 1024 chunks is 256 MiB. At 32 MiB/s the cap binds only when durable
    /// latency exceeds 8 s, so it is not the limit on the preset ladder.
    /// Production has no such cap: `write_data_chunk_queued` only waits for
    /// the index ACK. Public HTTP admission is 256 and ingress handlers
    /// default to 30. Migration has no bench cap; it waits on the
    /// pending-write ceiling. Serve is one read at a time.
    #[arg(long, default_value_t = 1024)]
    ingest_concurrency: usize,
    /// Recall start-delay p99 may exceed the `idle` step by this many milliseconds.
    #[arg(long, default_value_t = 200)]
    recall_delay_margin_ms: u64,
    /// Recall latency p99 may exceed the `idle` step by this many milliseconds.
    #[arg(long, default_value_t = 500)]
    recall_latency_margin_ms: u64,
    /// Stop after a hard step failure. A recall-margin miss or an enqueue
    /// shortfall still continues, so the rest of the ladder is measured.
    #[arg(long, action = ArgAction::SetTrue, overrides_with = "no_stop_on_fail")]
    stop_on_fail: bool,
    /// Keep going after a hard scored failure. A drain timeout or a driver
    /// abort still stops the plan.
    #[arg(long, action = ArgAction::SetTrue, overrides_with = "stop_on_fail")]
    no_stop_on_fail: bool,
    /// Judge the last N scored seconds as well as the whole step.
    ///
    /// A step can meet its mean and still collapse once buffers fill.
    /// `0` turns the second judge off. The drain is not part of the window.
    #[arg(long, default_value_t = DEFAULT_JUDGE_TAIL_SECS)]
    judge_tail_secs: u64,
}

const DEFAULT_JUDGE_TAIL_SECS: u64 = 240;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum IngestIndex {
    /// Commit each tx on the generator, then pace its chunks.
    Serial,
    /// Commit the next tx while the current tx's chunks are paced.
    Overlap,
}

impl std::fmt::Display for IngestIndex {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Serial => "serial",
            Self::Overlap => "overlap",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum IndexEngine {
    Mdbx,
    Rocks,
}

impl std::fmt::Display for IndexEngine {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Mdbx => "mdbx",
            Self::Rocks => "rocks",
        })
    }
}

/// Bench index under `--dir`. Rocks options are refused when the engine is MDBX.
fn resolve_index(
    engine: IndexEngine,
    preset: Option<&str>,
    block_cache: Option<u64>,
    small_file_compact_threshold: Option<u64>,
) -> eyre::Result<BenchIndex> {
    match engine {
        IndexEngine::Mdbx => {
            if preset.is_some() || block_cache.is_some() || small_file_compact_threshold.is_some() {
                return Err(eyre!(
                    "--rocks-preset, --rocks-block-cache, and --rocks-small-file-compact-threshold require --index-engine rocks"
                ));
            }
            Ok(BenchIndex::Mdbx)
        }
        IndexEngine::Rocks => {
            let mut tuning = RocksTuning::preset(preset.unwrap_or("baseline"))?;
            if let Some(bytes) = block_cache {
                if bytes == 0 {
                    return Err(eyre!("--rocks-block-cache must be greater than 0"));
                }
                let bytes = usize::try_from(bytes)
                    .map_err(|_| eyre!("--rocks-block-cache does not fit in usize"))?;
                tuning = tuning.with_block_cache(bytes);
            }
            if let Some(threshold) = small_file_compact_threshold {
                let threshold = usize::try_from(threshold).map_err(|_| {
                    eyre!("--rocks-small-file-compact-threshold does not fit in usize")
                })?;
                tuning = tuning.with_small_file_compact_threshold(threshold);
            }
            Ok(BenchIndex::Rocks(tuning))
        }
    }
}

fn main() {
    let code = match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("irys-sm-bench: {error:?}");
            1
        }
    };
    std::process::exit(code);
}

fn run() -> eyre::Result<i32> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();
    refuse_without_loadtest("dir", &args.dir)?;
    refuse_without_loadtest("recall-file", &args.recall_file)?;
    refuse_without_loadtest("write-file", &args.write_file)?;
    if args.num_writes_before_sync == 0 || args.entropy_sweep_max_bytes == 0 {
        return Err(eyre!("sync and entropy sweep sizes must be greater than 0"));
    }
    if args.serve_seq_chunks == 0 {
        return Err(eyre!("--serve-seq-chunks must be greater than 0"));
    }
    if args.ingest_concurrency == 0 || args.ingest_concurrency > tokio::sync::Semaphore::MAX_PERMITS
    {
        return Err(eyre!(
            "--ingest-concurrency must be in 1..={}",
            tokio::sync::Semaphore::MAX_PERMITS
        ));
    }
    std::fs::create_dir_all(&args.dir)?;
    let write_created = if let Some(bytes) = args.create_small {
        create_small(&args.recall_file, bytes, true)?;
        create_small(&args.write_file, bytes, false)?
    } else {
        false
    };
    let dir = require_loadtest("dir", &args.dir)?;
    let recall_file = require_loadtest("recall-file", &args.recall_file)?;
    let write_file = require_loadtest("write-file", &args.write_file)?;
    if recall_file == write_file {
        return Err(eyre!("recall file and write file are the same path"));
    }
    let chunks_dat = dir.join("chunks.dat");
    if recall_file == chunks_dat || write_file == chunks_dat {
        return Err(eyre!(
            "refusing to use chunks.dat; the bench does not open that name"
        ));
    }
    check_written_extents(&write_file, write_created)?;

    let recall_len = std::fs::metadata(&recall_file)?.len();
    let write_len = std::fs::metadata(&write_file)?.len();
    let partition_chunks = partition_chunks(args.chunks, recall_len)?;
    let write_origin = u32::try_from(args.write_origin)
        .map_err(|_| eyre!("--write-origin does not fit in u32"))?;
    let write_chunks = write_chunks(write_len)?;
    let write_end = write_origin
        .checked_add(write_chunks)
        .ok_or_else(|| eyre!("write window overflows u32"))?;
    if u64::from(write_end) > u64::from(partition_chunks) {
        return Err(eyre!(
            "write window ends at {write_end}, past the partition ({partition_chunks} chunks)"
        ));
    }
    let migrate_origin = migrate_origin(args.migrate_origin, write_origin, write_end)?;
    let recall_chunks = u32::try_from(args.recall_chunks)
        .map_err(|_| eyre!("--recall-chunks does not fit in u32"))?;
    if recall_chunks == 0 || recall_chunks > partition_chunks {
        return Err(eyre!("--recall-chunks must be in 1..={partition_chunks}"));
    }
    let mix = parse_mix(&args.ingest_mix)?;
    let mix_total: u64 = mix.iter().map(|(_, weight)| *weight).sum();
    let mut plan = if let Some(path) = &args.schedule {
        load_schedule(path)?
    } else {
        preset(args.preset.as_deref().unwrap_or("recall-1"))?
    };
    if let Some(secs) = args.step_secs {
        apply_step_secs(&mut plan, secs)?;
    }
    if args.no_stop_on_fail {
        plan.stop_on_fail = false;
    } else if args.stop_on_fail {
        plan.stop_on_fail = true;
    }
    let delay_margin_us = args.recall_delay_margin_ms.saturating_mul(1_000);
    let latency_margin_us = args.recall_latency_margin_ms.saturating_mul(1_000);
    let index = resolve_index(
        args.index_engine,
        args.rocks_preset.as_deref(),
        args.rocks_block_cache,
        args.rocks_small_file_compact_threshold,
    )?;

    let mut opened = open_module(OpenRequest {
        dir: &dir,
        recall_file: &recall_file,
        write_file: &write_file,
        partition_chunks,
        recall_chunks: u64::from(recall_chunks),
        write_origin,
        write_chunks: u64::from(write_chunks),
        num_writes_before_sync: args.num_writes_before_sync,
        entropy_sweep_max_bytes: args.entropy_sweep_max_bytes,
        intervals_persist_secs: args.intervals_persist_secs,
        min_durable_interval_ms: args.min_durable_interval_ms,
        durable_headroom_permille: args.durable_headroom_permille,
        recall_direct_io: args.recall_direct_io,
        index,
        index_group_commit: args.index_group_commit,
    })?;
    let prepopulated = prepopulate(
        &opened.module,
        &dir,
        partition_chunks,
        args.prepopulate_index,
    )?;
    let purged = purge_migrations(&opened.module, partition_chunks)?;
    if purged > 0 {
        eprintln!("irys-sm-bench: purged {purged} leftover body-migration row(s)");
    }
    let ingest_cursor = resume_frontier(&opened.module, write_origin, migrate_origin);
    let migration_cursor = resume_frontier(&opened.module, migrate_origin, write_end);
    preflight(
        &plan,
        &mix,
        span_limit(args.entropy_sweep_max_bytes, CHUNK_SIZE),
        ingest_cursor,
        migrate_origin,
        migration_cursor,
        write_end,
    )?;
    let index_dir = dir.join(opened.index.dir_name());
    let (rocks_preset, rocks_tuning, rocks_block_cache_bytes, rocks_shared_resources) =
        match opened.index {
            BenchIndex::Mdbx => (None, None, None, None),
            BenchIndex::Rocks(tuning) => (
                Some(tuning.name),
                Some(tuning.describe()),
                Some(tuning.block_cache_bytes),
                Some(tuning.share_resources),
            ),
        };
    let index_line = index_banner(
        opened.index.engine_name(),
        &index_dir,
        rocks_preset,
        rocks_tuning.as_deref(),
        rocks_block_cache_bytes,
        args.index_group_commit,
    );
    let recall_io = opened.module.recall_io_mode();
    eprintln!("irys-sm-bench: {}", index_line.trim_end());
    eprintln!("irys-sm-bench: recall_io={recall_io}");
    eprintln!(
        "irys-sm-bench: partition {partition_chunks} chunks, write window {write_origin}..{write_end}, ingest frontier {ingest_cursor}, migration frontier {migration_cursor}"
    );

    let ceiling = pending_ceiling(args.num_writes_before_sync, CHUNK_SIZE);
    let flush = flush_batch(args.num_writes_before_sync, CHUNK_SIZE);
    let span = span_limit(args.entropy_sweep_max_bytes, CHUNK_SIZE);
    let git = git_info();
    let mut header = serde_json::json!({
        "git": {"commit": git.commit, "dirty": git.dirty},
        "rustc": rustc_version(),
        "dir": dir,
        "recall_file": recall_file,
        "write_file": write_file,
        "partition_chunks": partition_chunks,
        "recall_chunks": recall_chunks,
        "write_origin": write_origin,
        "write_chunks": write_chunks,
        "write_end": write_end,
        "migrate_origin": migrate_origin,
        "ingest_frontier": ingest_cursor,
        "migration_frontier": migration_cursor,
        "chunk_size": CHUNK_SIZE,
        "prepopulate_index": prepopulated,
        "seed": args.seed,
        "num_writes_before_sync": args.num_writes_before_sync,
        "intervals_persist_secs": args.intervals_persist_secs,
        "entropy_sweep_max_bytes": args.entropy_sweep_max_bytes,
        "ingest_mix": args.ingest_mix,
        "serve_seq_chunks": args.serve_seq_chunks,
        "ingest_concurrency": args.ingest_concurrency,
        "ingest_index": args.ingest_index.to_string(),
        "recall_delay_margin_ms": args.recall_delay_margin_ms,
        "recall_latency_margin_ms": args.recall_latency_margin_ms,
        "recall_start_p99_limit_us": crate::schedule::RECALL_START_P99_LIMIT_US,
        "recall_lat_p99_limit_us": crate::schedule::RECALL_LAT_P99_LIMIT_US,
        "ceiling_bytes": ceiling,
        "flush_batch_bytes": flush,
        "span_limit_chunks": span,
        "drain_secs": DRAIN.as_secs(),
        "plan": plan.name,
        "stop_on_fail": plan.stop_on_fail,
        "steps": plan_steps(&plan),
        "io_notes": [
            "One disk lane schedules the recall fd and the write fd.",
            "Recall reads always use the entropy file, including ranges that are now Data.",
            "Entropy-for-XOR preads and packed writes use the write file at (offset - write_origin).",
            "Merkle proofs are not checked. Chunk bytes are synthetic.",
            "Ingest and migration use separate halves of the write window.",
            "Startup deletes leftover body-migration rows in this bench database.",
            "Durable latency ends at is_data_chunk_durable_at, after fsync.",
            "intervals.json write-ahead Interrupted marks are immediate. Completions rewrite at most every intervals_persist_secs, or sooner on the next write-ahead.",
            "Ingest concurrency is a bench semaphore held until the chunk is durable. It is not a storage-module admission limit.",
            "The offer deadline advances by the chunk interval from the previous deadline. A late release catches up instead of inserting a fresh gap.",
            "ingest_index serial commits index_transaction_data on the generator before that tx is paced. overlap starts the next commit during the current tx. Production indexes on the chunk migration service while another task writes bodies.",
            "Migration enqueue waits on the pending-write ceiling. Serve issues one read at a time.",
            "A step named idle is the recall baseline. Other steps use the absolute p99 ceilings when no idle step has completed.",
            "judge_tail_secs scores the last N seconds as well as the whole step. Durable lag inside that window is not a failure. The drain check stays on the whole step.",
        ],
    });
    header["min_durable_interval_ms"] = serde_json::json!(args.min_durable_interval_ms);
    header["durable_headroom_permille"] = serde_json::json!(args.durable_headroom_permille);
    header["recall_direct_io"] = serde_json::json!(args.recall_direct_io);
    header["recall_io"] = serde_json::Value::from(recall_io);
    header["io_notes"]
        .as_array_mut()
        .expect("io_notes is an array")
        .push(serde_json::Value::from(
            "recall_direct_io reads the entropy file with O_DIRECT. Serving, entropy-for-XOR, and packed writes stay on the buffered descriptors. fallback means the open was rejected and recall stayed buffered.",
        ));
    header["io_notes"]
        .as_array_mut()
        .expect("io_notes is an array")
        .push(serde_json::Value::from(
            "min_durable_interval_ms defaults to 250, the node default for HDD storage modules. It spaces the chunks.dat fdatasync and the synced presence commit. 0 is a point on every gap. Pwrites are not delayed. Force, shutdown, drop, headroom loss, and the pending-byte ceiling stay immediate. The interval is the longest wait.",
        ));
    header["io_notes"]
        .as_array_mut()
        .expect("io_notes is an array")
        .push(serde_json::Value::from(
            "durable_headroom_permille is the pending-byte line, in parts per thousand of the memory ceiling. Crossing it runs the durable point at once and keeps deferral closed until pending bytes fall to half the line. 1000 puts the line on the ceiling. 0 runs a point as soon as any byte is pending.",
        ));
    header["judge_tail_secs"] = serde_json::json!(args.judge_tail_secs);
    header["index_engine"] = serde_json::Value::from(opened.index.engine_name());
    header["index_group_commit"] = serde_json::json!(args.index_group_commit);
    header["index_dir"] = serde_json::Value::from(index_dir.display().to_string());
    header["rocks_preset"] = rocks_preset
        .map(serde_json::Value::from)
        .unwrap_or(serde_json::Value::Null);
    header["rocks_tuning"] = rocks_tuning
        .map(serde_json::Value::from)
        .unwrap_or(serde_json::Value::Null);
    header["rocks_block_cache_bytes"] = match rocks_block_cache_bytes {
        Some(bytes) => serde_json::json!(bytes),
        None => serde_json::Value::Null,
    };
    header["rocks_shared_resources"] = match rocks_shared_resources {
        Some(shared) => serde_json::json!(shared),
        None => serde_json::Value::Null,
    };
    std::fs::create_dir_all(&args.out)?;
    write_json(&args.out.join("header.json"), &header)?;
    let mut samples = SampleWriter::create(&args.out.join("samples.jsonl"))?;
    let mut step_log = StepLog::create(&args.out.join("steps.jsonl"))?;

    let io = Arc::new(BenchIo {
        module: Arc::clone(&opened.module),
        chunk_size: CHUNK_SIZE,
        partition_chunks,
        recall_chunks,
        write_origin,
        migrate_origin,
        write_end,
        mix,
        mix_total,
        span_limit: span,
        ceiling,
        seed: args.seed,
        serve_seq_chunks: args.serve_seq_chunks,
        buckets: std::sync::Mutex::new(crate::stats::Buckets::default()),
        written: std::sync::Mutex::new(crate::stats::Written::default()),
        fatal: std::sync::Mutex::new(None),
        accept_new: std::sync::atomic::AtomicBool::new(false),
        open_txs: std::sync::atomic::AtomicUsize::new(0),
        ingest_inflight: std::sync::atomic::AtomicUsize::new(0),
        migration_inflight: std::sync::atomic::AtomicUsize::new(0),
        recall_busy: std::sync::atomic::AtomicBool::new(false),
        serve_busy: std::sync::atomic::AtomicBool::new(false),
        index_lock: tokio::sync::Mutex::new(()),
        permits: PermitGauge::new(),
    });
    opened.start_lane()?;

    let out = args.out;
    let stop_on_fail = plan.stop_on_fail;
    let ingest_concurrency = args.ingest_concurrency;
    let overlap_index = args.ingest_index == IngestIndex::Overlap;
    let judge_tail_secs = args.judge_tail_secs;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let code = runtime.block_on(execute(
        io,
        plan,
        &mut samples,
        &mut step_log,
        &out,
        ingest_cursor,
        migration_cursor,
        ceiling,
        flush,
        stop_on_fail,
        ingest_concurrency,
        delay_margin_us,
        latency_margin_us,
        overlap_index,
        &index_line,
        judge_tail_secs,
        recall_io,
    ))?;
    match opened.stop_lane() {
        Ok(()) => Ok(code),
        Err(error) => {
            eprintln!("irys-sm-bench: {error}");
            Ok(if code == 0 { 2 } else { code })
        }
    }
}

fn refuse_without_loadtest(label: &str, path: &Path) -> eyre::Result<()> {
    if has_loadtest_component(path) {
        Ok(())
    } else {
        Err(eyre!(
            "{label} {} has no `loadtest` path component; refusing to run",
            path.display()
        ))
    }
}

fn partition_chunks(requested: Option<u64>, recall_len: u64) -> eyre::Result<u32> {
    let chunks = if let Some(chunks) = requested {
        if chunks == 0 {
            return Err(eyre!("--chunks must be greater than 0"));
        }
        let bytes = chunks
            .checked_mul(CHUNK_SIZE)
            .ok_or_else(|| eyre!("--chunks overflows"))?;
        if recall_len < bytes {
            return Err(eyre!(
                "recall file is {recall_len} bytes, shorter than --chunks ({bytes} bytes)"
            ));
        }
        chunks
    } else {
        if !recall_len.is_multiple_of(CHUNK_SIZE) {
            return Err(eyre!(
                "recall file length {recall_len} is not a multiple of {CHUNK_SIZE}; pass --chunks"
            ));
        }
        let chunks = recall_len / CHUNK_SIZE;
        if chunks == 0 {
            return Err(eyre!("recall file has no chunks"));
        }
        chunks
    };
    u32::try_from(chunks).map_err(|_| eyre!("partition chunk count does not fit in u32"))
}

fn write_chunks(write_len: u64) -> eyre::Result<u32> {
    if !write_len.is_multiple_of(CHUNK_SIZE) {
        return Err(eyre!(
            "write file length {write_len} is not a multiple of {CHUNK_SIZE}"
        ));
    }
    let chunks = write_len / CHUNK_SIZE;
    if chunks == 0 {
        return Err(eyre!("write file has no chunks"));
    }
    u32::try_from(chunks).map_err(|_| eyre!("write chunk count does not fit in u32"))
}

fn migrate_origin(requested: Option<u64>, write_origin: u32, write_end: u32) -> eyre::Result<u32> {
    let origin = if let Some(value) = requested {
        u32::try_from(value).map_err(|_| eyre!("--migrate-origin does not fit in u32"))?
    } else {
        write_origin + (write_end - write_origin) / 2
    };
    if origin < write_origin || origin > write_end {
        return Err(eyre!(
            "--migrate-origin {origin} is outside the write window {write_origin}..{write_end}"
        ));
    }
    Ok(origin)
}

fn preflight(
    plan: &Plan,
    mix: &[(u32, u64)],
    span: usize,
    ingest_cursor: u32,
    migrate_origin: u32,
    migration_cursor: u32,
    write_end: u32,
) -> eyre::Result<()> {
    let max_tx = mix
        .iter()
        .map(|(chunks, _)| u64::from(*chunks))
        .max()
        .unwrap_or(1);
    let ingest_steps = plan
        .steps
        .iter()
        .filter(|step| step.rates.ingest_mib_s > 0)
        .count() as u64;
    let ingest_bytes = plan.steps.iter().fold(0_u64, |sum, step| {
        sum.saturating_add(mib_bytes(step.rates.ingest_mib_s, step.secs))
    });
    let ingest_need = ingest_bytes
        .div_ceil(CHUNK_SIZE)
        .saturating_add(max_tx.saturating_mul(ingest_steps));
    let ingest_room = u64::from(migrate_origin.saturating_sub(ingest_cursor));
    if ingest_bytes > 0 && ingest_need > ingest_room {
        return Err(eyre!(
            "ingest schedule needs {ingest_need} chunks and the window has {ingest_room}"
        ));
    }
    let mig_tx = u64::from(u32::try_from(span).unwrap_or(u32::MAX).max(32));
    let mig_steps = plan
        .steps
        .iter()
        .filter(|step| step.rates.migration_mib_s > 0)
        .count() as u64;
    let mig_bytes = plan.steps.iter().fold(0_u64, |sum, step| {
        sum.saturating_add(mib_bytes(step.rates.migration_mib_s, step.secs))
    });
    let mig_need = mig_bytes
        .div_ceil(CHUNK_SIZE)
        .saturating_add(mig_tx.saturating_mul(mig_steps));
    let mig_room = u64::from(write_end.saturating_sub(migration_cursor));
    if mig_bytes > 0 && mig_need > mig_room {
        return Err(eyre!(
            "migration schedule needs {mig_need} chunks and the window has {mig_room}"
        ));
    }
    Ok(())
}

fn rustc_version() -> String {
    Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

async fn execute(
    io: Arc<BenchIo>,
    plan: Plan,
    samples: &mut SampleWriter,
    step_log: &mut StepLog,
    out: &Path,
    ingest_cursor: u32,
    migration_cursor: u32,
    ceiling: u64,
    flush: u64,
    stop_on_fail: bool,
    ingest_concurrency: usize,
    delay_margin_us: u64,
    latency_margin_us: u64,
    overlap_index: bool,
    index_line: &str,
    judge_tail_secs: u64,
    recall_io: &str,
) -> eyre::Result<i32> {
    let (rates_tx, rates_rx) = watch::channel(Rates::idle());
    let (stop_tx, stop_rx) = watch::channel(false);
    let (interrupt_tx, mut interrupt_rx) = watch::channel(false);
    {
        let stop_tx = stop_tx.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                eprintln!("irys-sm-bench: interrupted");
                let _ = interrupt_tx.send(true);
                let _ = stop_tx.send(true);
            }
        });
    }
    let recall = tokio::spawn(recall_driver(
        Arc::clone(&io),
        rates_rx.clone(),
        stop_rx.clone(),
    ));
    let ingest = tokio::spawn(ingest_driver(
        Arc::clone(&io),
        rates_rx.clone(),
        stop_rx.clone(),
        ingest_cursor,
        ingest_concurrency,
        overlap_index,
    ));
    let migration = tokio::spawn(migration_driver(
        Arc::clone(&io),
        rates_rx.clone(),
        stop_rx.clone(),
        migration_cursor,
    ));
    let serve = tokio::spawn(serve_driver(Arc::clone(&io), rates_rx, stop_rx));

    // Drivers are up. The monotonic stamp matches bpftrace `nsecs`.
    step_log.mark_run()?;

    let mut rows = Vec::new();
    let mut scored_fail = false;
    let mut interrupted = false;
    let mut recall_baseline: Option<(u64, u64)> = None;
    for (index, step) in plan.steps.iter().enumerate() {
        if *interrupt_rx.borrow() {
            interrupted = true;
            break;
        }
        eprintln!(
            "irys-sm-bench: step {} for {}s (recall {}/{}, ingest {} MiB/s, migration {} MiB/s, serve {}/s)",
            step.name,
            step.secs,
            step.rates.recall_num,
            step.rates.recall_den,
            step.rates.ingest_mib_s,
            step.rates.migration_mib_s,
            step.rates.serve_per_sec
        );
        if recall.is_finished()
            || ingest.is_finished()
            || migration.is_finished()
            || serve.is_finished()
        {
            scored_fail = true;
            break;
        }
        let index = u32::try_from(index).map_err(|_| eyre!("too many steps"))?;
        step_log.mark(index, &step.name, "start")?;
        io.accept_new.store(true, Ordering::Release);
        let _ = rates_tx.send(step.rates);
        let bar = recall_bar(step, recall_baseline);
        let outcome = run_step(
            step,
            &io,
            samples,
            step_log,
            index,
            &rates_tx,
            &mut interrupt_rx,
            ceiling,
            flush,
            RecallRule {
                bar,
                delay_margin_us,
                latency_margin_us,
            },
            judge_tail_secs,
        )
        .await?;
        if matches!(bar, RecallBar::Baseline)
            && !outcome.interrupted
            && outcome.row.measured.recall_ops > 0
        {
            recall_baseline = Some((
                outcome.row.measured.recall_start_p99_us,
                outcome.row.measured.recall_lat_p99_us,
            ));
        }
        if outcome.interrupted {
            interrupted = true;
        }
        if !outcome.passed && !outcome.interrupted {
            scored_fail = true;
        }
        let halt = outcome.halt;
        let stop = stop_for_score(&outcome.findings, stop_on_fail);
        if let Some(cache) = &outcome.row.page_cache {
            eprintln!(
                "irys-sm-bench: step {} page cache Cached {}->{} KiB Inactive(file) {}->{} KiB",
                outcome.row.name,
                cache.start.cached_kib,
                cache.end.cached_kib,
                cache.start.inactive_file_kib,
                cache.end.inactive_file_kib,
            );
        }
        rows.push(outcome.row);
        // A drain timeout or a driver abort stops the plan even when
        // `stop_on_fail` is false. Recall-margin and enqueue-shortfall
        // misses do not stop it.
        if interrupted || halt || stop {
            break;
        }
    }

    let _ = stop_tx.send(true);
    let _ = rates_tx.send(Rates::idle());
    let mut driver_notes = Vec::new();
    for (name, task) in [
        ("recall", recall),
        ("ingest", ingest),
        ("migration", migration),
        ("serve", serve),
    ] {
        if let Some(message) = driver_error(name, task.await) {
            eprintln!("irys-sm-bench: {message}");
            driver_notes.push(message);
            if !interrupted {
                scored_fail = true;
            }
        }
    }
    let passed = !scored_fail && !interrupted;
    let code = if interrupted && !scored_fail {
        1
    } else if scored_fail {
        2
    } else {
        0
    };
    write_summary(
        &out.join("summary.json"),
        &rows,
        passed,
        code,
        interrupted,
        &driver_notes,
        recall_io,
    )?;
    let table_head = format!("recall_io={recall_io}\n{index_line}");
    write_table(&out.join("table.txt"), &rows, &table_head)?;
    Ok(code)
}

struct StepOutcome {
    row: StepRow,
    passed: bool,
    interrupted: bool,
    /// Drain timeout or a driver abort. Later steps must not start.
    halt: bool,
    findings: Vec<crate::schedule::Finding>,
}

async fn run_step(
    step: &Step,
    io: &Arc<BenchIo>,
    samples: &mut SampleWriter,
    step_log: &mut StepLog,
    index: u32,
    rates_tx: &watch::Sender<Rates>,
    interrupt: &mut watch::Receiver<bool>,
    ceiling: u64,
    flush: u64,
    rule: RecallRule,
    judge_tail_secs: u64,
) -> eyre::Result<StepOutcome> {
    io.permits.reset_window();
    let page_cache_start = read_page_cache();
    let rocks_before = sample_rocks(&io.module);
    let durable_before = io.module.durable_point_counts();
    let mut accum = StepAccum::default();
    let started = tokio::time::Instant::now();
    let mut interrupted = false;
    for sec in 1..=step.secs {
        if *interrupt.borrow() {
            interrupted = true;
            break;
        }
        let deadline = started + Duration::from_secs(sec);
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {}
            _ = interrupt.changed() => { interrupted = true; }
        }
        record_second(step, sec, io, samples, &mut accum)?;
        if interrupted || fatal_reason(io).is_some() {
            break;
        }
    }
    io.accept_new.store(false, Ordering::Release);
    // Recall and serve stop. The open tx keeps the step's ingest and migration
    // rate: it captured that rate when it started, and `accept_new` blocks a new tx.
    let mut drain_rates = step.rates;
    drain_rates.recall_num = 0;
    drain_rates.serve_per_sec = 0;
    let _ = rates_tx.send(drain_rates);
    step_log.mark(index, &step.name, "drain")?;
    if *interrupt.borrow() {
        interrupted = true;
    }
    let drained = wait_quiet(io, interrupt, DRAIN).await?;
    if *interrupt.borrow() {
        interrupted = true;
    }
    accum.add_quiet(&take_buckets(io));
    accum.add_permit_sample(&io.permits.sample());
    let pending_after = io.module.pending_write_bytes();
    let jobs_after = io.module.pending_body_migrations()?.len();
    if !drained && !interrupted {
        note_fatal(
            io,
            format!(
                "drain timed out after {}s (pending {pending_after} bytes, {jobs_after} migration rows)",
                DRAIN.as_secs()
            ),
        );
    }
    // After the drain's index read, so that I/O stays inside the step window.
    step_log.mark(index, &step.name, "end")?;
    let aborted = if interrupted { None } else { take_fatal(io) };
    let recall_delay = crate::stats::summarize(&accum.recall.delays_us);
    let recall_lat = crate::stats::summarize(&accum.recall.lats_us);
    let measured = StepMeasured {
        secs: accum.seconds,
        scheduled_ingest: mib_bytes(step.rates.ingest_mib_s, accum.seconds),
        durable_ingest: accum.ingest.durable,
        enqueued_ingest: accum.ingest.enqueued,
        scheduled_migration: mib_bytes(step.rates.migration_mib_s, accum.seconds),
        durable_migration: accum.migration.durable,
        enqueued_migration: accum.migration.enqueued,
        serve_ops: accum.serve.ops,
        scheduled_serve: step.rates.serve_per_sec.saturating_mul(accum.seconds),
        chunk_size: CHUNK_SIZE,
        recall_ops: accum.recall.ops,
        recall_skips: accum.recall.skips,
        recall_start_p99_us: recall_delay.p99,
        recall_lat_p99_us: recall_lat.p99,
        ingest_enqueue_blocked_us: accum.ingest.enqueue_blocked_us,
        migration_enqueue_blocked_us: accum.migration.enqueue_blocked_us,
        overloaded_secs: accum.overloaded_secs,
        pending_samples: accum.pending_samples,
        pending_after_drain: pending_after,
        migration_jobs_after_drain: u64::try_from(jobs_after).unwrap_or(u64::MAX),
        ceiling,
        flush_batch: flush,
        aborted,
    };
    let tail = if interrupted {
        None
    } else {
        tail_window(
            &accum.views,
            judge_tail_secs,
            &step.rates,
            CHUNK_SIZE,
            ceiling,
            flush,
            &rule,
        )
    };
    let findings = if interrupted {
        Vec::new()
    } else {
        let whole = judge(&measured, &rule);
        match &tail {
            Some(window) => merge_tail(whole, window, measured.secs),
            None => whole,
        }
    };
    let mut reasons: Vec<String> = findings
        .iter()
        .map(|finding| finding.message.clone())
        .collect();
    if interrupted {
        reasons.insert(0, "interrupted".to_string());
    }
    let passed = reasons.iter().all(|reason| reason == "interrupted");
    let soft_fail = !passed && !interrupted && findings.iter().all(|finding| finding.soft);
    let page_cache = page_cache_start
        .zip(read_page_cache())
        .map(|(start, end)| PageCacheSpan { start, end });
    let rocks = rocks_before
        .and_then(|before| sample_rocks(&io.module).map(|after| rocks_delta(before, after)));
    let durable_points = io
        .module
        .durable_point_counts()
        .saturating_delta(durable_before);
    Ok(StepOutcome {
        passed,
        interrupted,
        halt: measured.aborted.is_some(),
        findings,
        row: StepRow {
            name: step.name.clone(),
            passed,
            soft_fail,
            reasons,
            measured,
            recall: accum.recall,
            ingest: accum.ingest,
            migration: accum.migration,
            serve: accum.serve,
            rocks,
            durable_points,
            tail,
            page_cache,
        },
    })
}

fn sample_rocks(module: &irys_domain::StorageModule) -> Option<RocksIngestStats> {
    let Some(submodule) = module.get_submodule(PartitionChunkOffset::from(0_u32)) else {
        eprintln!("irys-sm-bench: no submodule at offset 0; omitting rocks counters");
        return None;
    };
    match submodule.db.rocks_ingest_stats() {
        Ok(stats) => stats,
        Err(error) => {
            eprintln!("irys-sm-bench: rocks property read failed: {error:#}");
            None
        }
    }
}

fn rocks_delta(before: RocksIngestStats, after: RocksIngestStats) -> RocksStepStats {
    RocksStepStats {
        write_stall_us: after
            .write_stall_micros
            .saturating_sub(before.write_stall_micros),
        pending_compaction_bytes: after.pending_compaction_bytes,
        flushes: after.flushes.saturating_sub(before.flushes),
        running_flushes: after.running_flushes,
        wal_syncs: after.wal_syncs.saturating_sub(before.wal_syncs),
        wal_backstop_syncs: after
            .wal_backstop_syncs
            .saturating_sub(before.wal_backstop_syncs),
        small_file_compactions: after
            .small_file_compactions
            .saturating_sub(before.small_file_compactions),
        small_file_compact_files_before: after
            .small_file_compact_files_before
            .saturating_sub(before.small_file_compact_files_before),
        small_file_compact_files_after: after
            .small_file_compact_files_after
            .saturating_sub(before.small_file_compact_files_after),
    }
}

fn record_second(
    step: &Step,
    sec: u64,
    io: &BenchIo,
    samples: &mut SampleWriter,
    accum: &mut StepAccum,
) -> eyre::Result<()> {
    let mut buckets = take_buckets(io);
    fold_permit(&mut buckets.ingest, &io.permits.sample());
    let snap = io.module.bench_queue_snapshot();
    let jobs = io.module.pending_body_migrations()?.len();
    let inflight =
        io.ingest_inflight.load(Ordering::Acquire) + io.migration_inflight.load(Ordering::Acquire);
    let overloaded = snap.pending_bytes >= io.ceiling;
    let jobs_u64 = u64::try_from(jobs).unwrap_or(u64::MAX);
    let pending_chunks = u64::try_from(snap.pending_chunks).unwrap_or(u64::MAX);
    let sweep_slots = u64::try_from(snap.sweep_slots).unwrap_or(u64::MAX);
    let inflight_u64 = u64::try_from(inflight).unwrap_or(u64::MAX);
    for (name, bucket, scheduled) in [
        ("recall", &buckets.recall, 0),
        (
            "ingest",
            &buckets.ingest,
            mib_bytes(step.rates.ingest_mib_s, 1),
        ),
        (
            "migration",
            &buckets.migration,
            mib_bytes(step.rates.migration_mib_s, 1),
        ),
        ("serve", &buckets.serve, 0),
    ] {
        samples.write_line(&sample_line(
            &step.name,
            sec,
            name,
            bucket,
            snap.pending_bytes,
            pending_chunks,
            sweep_slots,
            jobs_u64,
            inflight_u64,
            overloaded,
            scheduled,
            CHUNK_SIZE,
        ))?;
    }
    accum.add_second(
        &buckets.recall,
        &buckets.ingest,
        &buckets.migration,
        &buckets.serve,
        snap.pending_bytes,
        overloaded,
    );
    Ok(())
}

fn take_buckets(io: &BenchIo) -> crate::stats::Buckets {
    io.buckets
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
}

async fn wait_quiet(
    io: &BenchIo,
    interrupt: &mut watch::Receiver<bool>,
    limit: Duration,
) -> eyre::Result<bool> {
    let started = tokio::time::Instant::now();
    loop {
        if *interrupt.borrow() {
            return Ok(false);
        }
        let pending = io.module.pending_write_bytes();
        let jobs = io.module.pending_body_migrations()?.len();
        let inflight = io.ingest_inflight.load(Ordering::Acquire)
            + io.migration_inflight.load(Ordering::Acquire);
        let open = io.open_txs.load(Ordering::Acquire);
        let busy = io.recall_busy.load(Ordering::Acquire) || io.serve_busy.load(Ordering::Acquire);
        if pending == 0 && jobs == 0 && inflight == 0 && open == 0 && !busy {
            return Ok(true);
        }
        if started.elapsed() >= limit {
            return Ok(false);
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
            _ = interrupt.changed() => return Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(extra: &[&str]) -> Vec<String> {
        let mut args = vec![
            "irys-sm-bench".to_string(),
            "--dir".to_string(),
            "/tmp/sm-bench-local/loadtest/mod".to_string(),
            "--recall-file".to_string(),
            "/tmp/sm-bench-local/loadtest/entropy".to_string(),
            "--write-file".to_string(),
            "/tmp/sm-bench-local/loadtest/write".to_string(),
            "--out".to_string(),
            "/tmp/sm-bench-local/loadtest/out".to_string(),
        ];
        args.extend(extra.iter().map(|item| (*item).to_string()));
        args
    }

    fn parsed(extra: &[&str]) -> Args {
        Args::try_parse_from(argv(extra)).expect("flags parse")
    }

    #[test]
    fn index_engine_defaults_to_mdbx() {
        let args = parsed(&[]);
        assert_eq!(args.index_engine, IndexEngine::Mdbx);
        assert_eq!(args.index_group_commit, 8);
        assert_eq!(
            args.intervals_persist_secs,
            irys_types::DEFAULT_INTERVALS_PERSIST_SECS
        );
        assert_eq!(
            parsed(&["--intervals-persist-secs", "0"]).intervals_persist_secs,
            0
        );
        assert_eq!(
            args.min_durable_interval_ms,
            irys_types::DEFAULT_MIN_DURABLE_INTERVAL_MS
        );
        assert_eq!(
            parsed(&["--min-durable-interval-ms", "0"]).min_durable_interval_ms,
            0
        );
        assert_eq!(
            args.durable_headroom_permille,
            irys_types::DEFAULT_DURABLE_HEADROOM_PERMILLE
        );
        assert_eq!(
            parsed(&["--durable-headroom-permille", "1000"]).durable_headroom_permille,
            1000
        );
        assert!(!args.recall_direct_io);
        assert!(parsed(&["--recall-direct-io"]).recall_direct_io);
        assert!(args.rocks_preset.is_none());
        assert!(args.rocks_block_cache.is_none());
        assert!(args.rocks_small_file_compact_threshold.is_none());
        assert_eq!(
            resolve_index(args.index_engine, None, None, None).unwrap(),
            BenchIndex::Mdbx
        );
    }

    #[test]
    fn rocks_flags_require_the_rocks_engine() {
        let preset = parsed(&["--rocks-preset", "baseline"]);
        let err = resolve_index(
            preset.index_engine,
            preset.rocks_preset.as_deref(),
            preset.rocks_block_cache,
            preset.rocks_small_file_compact_threshold,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("require --index-engine rocks"),
            "{err}"
        );

        let cache = parsed(&["--rocks-block-cache", "4096"]);
        let err = resolve_index(
            cache.index_engine,
            cache.rocks_preset.as_deref(),
            cache.rocks_block_cache,
            cache.rocks_small_file_compact_threshold,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("require --index-engine rocks"),
            "{err}"
        );

        let threshold = parsed(&["--rocks-small-file-compact-threshold", "8"]);
        let err = resolve_index(
            threshold.index_engine,
            threshold.rocks_preset.as_deref(),
            threshold.rocks_block_cache,
            threshold.rocks_small_file_compact_threshold,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("require --index-engine rocks"),
            "{err}"
        );
    }

    #[test]
    fn rocks_defaults_to_baseline_and_takes_a_cache_override() {
        let args = parsed(&["--index-engine", "rocks"]);
        let BenchIndex::Rocks(tuning) = resolve_index(
            args.index_engine,
            args.rocks_preset.as_deref(),
            args.rocks_block_cache,
            args.rocks_small_file_compact_threshold,
        )
        .unwrap() else {
            panic!("expected rocks");
        };
        assert_eq!(tuning, RocksTuning::baseline());
        assert!(tuning.share_resources);

        let args = parsed(&[
            "--index-engine",
            "rocks",
            "--rocks-preset",
            "blobs",
            "--rocks-block-cache",
            "1048576",
        ]);
        let BenchIndex::Rocks(tuning) = resolve_index(
            args.index_engine,
            args.rocks_preset.as_deref(),
            args.rocks_block_cache,
            args.rocks_small_file_compact_threshold,
        )
        .unwrap() else {
            panic!("expected rocks");
        };
        assert_eq!(tuning.name, "blobs");
        assert!(tuning.blobs);
        assert_eq!(tuning.block_cache_bytes, 1_048_576);
        assert!(!tuning.share_resources);
    }

    #[test]
    fn rocks_small_file_compact_threshold_overrides_the_preset() {
        let args = parsed(&[
            "--index-engine",
            "rocks",
            "--rocks-preset",
            "fdatasync-wal-rotate-compact",
            "--rocks-small-file-compact-threshold",
            "4",
        ]);
        assert_eq!(args.rocks_small_file_compact_threshold, Some(4));
        let BenchIndex::Rocks(tuning) = resolve_index(
            args.index_engine,
            args.rocks_preset.as_deref(),
            args.rocks_block_cache,
            args.rocks_small_file_compact_threshold,
        )
        .unwrap() else {
            panic!("expected rocks");
        };
        let preset = RocksTuning::preset("fdatasync-wal-rotate-compact").unwrap();
        assert_eq!(tuning.name, preset.name);
        assert_eq!(tuning.small_file_compact_threshold, 4);
        assert_ne!(preset.small_file_compact_threshold, 4);
        assert_eq!(tuning.use_fsync, preset.use_fsync);
        assert_eq!(tuning.max_total_wal_size, preset.max_total_wal_size);
        assert_eq!(tuning.block_cache_bytes, preset.block_cache_bytes);
        assert!(
            tuning.describe().contains("small_file_compact_threshold=4"),
            "{}",
            tuning.describe()
        );

        let off = parsed(&[
            "--index-engine",
            "rocks",
            "--rocks-preset",
            "fdatasync-wal-rotate-compact",
            "--rocks-small-file-compact-threshold",
            "0",
        ]);
        let BenchIndex::Rocks(tuning) = resolve_index(
            off.index_engine,
            off.rocks_preset.as_deref(),
            off.rocks_block_cache,
            off.rocks_small_file_compact_threshold,
        )
        .unwrap() else {
            panic!("expected rocks");
        };
        assert_eq!(tuning.small_file_compact_threshold, 0);
        assert!(tuning.describe().contains("small_file_compact_threshold=0"));
    }

    #[test]
    fn every_shared_preset_parses_and_bad_values_fail() {
        for name in RocksTuning::preset_names().split(", ") {
            let args = parsed(&["--index-engine", "rocks", "--rocks-preset", name]);
            let BenchIndex::Rocks(tuning) = resolve_index(
                args.index_engine,
                args.rocks_preset.as_deref(),
                None,
                args.rocks_small_file_compact_threshold,
            )
            .unwrap() else {
                panic!("expected rocks");
            };
            assert_eq!(tuning.name, name);
        }

        let args = parsed(&["--index-engine", "rocks", "--rocks-preset", "nope"]);
        let err = resolve_index(
            args.index_engine,
            args.rocks_preset.as_deref(),
            None,
            args.rocks_small_file_compact_threshold,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown rocks preset"), "{err}");

        let args = parsed(&["--index-engine", "rocks", "--rocks-block-cache", "0"]);
        let err = resolve_index(
            args.index_engine,
            args.rocks_preset.as_deref(),
            args.rocks_block_cache,
            args.rocks_small_file_compact_threshold,
        )
        .unwrap_err();
        assert!(err.to_string().contains("greater than 0"), "{err}");
    }

    #[test]
    fn index_group_commit_accepts_zero_for_both_engines() {
        assert_eq!(parsed(&["--index-group-commit", "0"]).index_group_commit, 0);
        let rocks = parsed(&["--index-engine", "rocks", "--index-group-commit", "8"]);
        assert_eq!(rocks.index_engine, IndexEngine::Rocks);
        assert_eq!(rocks.index_group_commit, 8);
    }

    #[test]
    fn judge_tail_secs_defaults_to_four_minutes() {
        assert_eq!(parsed(&[]).judge_tail_secs, DEFAULT_JUDGE_TAIL_SECS);
        assert_eq!(parsed(&["--judge-tail-secs", "0"]).judge_tail_secs, 0);
        assert_eq!(parsed(&["--judge-tail-secs", "90"]).judge_tail_secs, 90);
    }

    #[test]
    fn unknown_index_engine_is_rejected_by_the_parser() {
        let err = Args::try_parse_from(argv(&["--index-engine", "lmdb"])).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("mdbx") && text.contains("rocks"), "{text}");
    }
}
