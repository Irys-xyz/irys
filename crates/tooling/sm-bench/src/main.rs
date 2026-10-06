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

use clap::Parser;
use eyre::eyre;
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
    SampleWriter, StepRow, git_info, plan_steps, sample_line, write_json, write_summary,
    write_table,
};
use crate::schedule::{
    Plan, Rates, Step, StepMeasured, apply_step_secs, judge, load_schedule, mib_bytes, parse_mix,
    preset,
};
use crate::stats::StepAccum;

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
    /// Write region. Ingest, migration, and serve reads use this file.
    #[arg(long)]
    write_file: PathBuf,
    /// Partition size in chunks. Default: recall file bytes / 256 KiB.
    #[arg(long)]
    chunks: Option<u64>,
    /// Directory for `header.json`, `samples.jsonl`, and `summary.json`.
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
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// Create missing recall and write files at this exact size. A multiple of 256 KiB.
    #[arg(long)]
    create_small: Option<u64>,
    #[arg(long, default_value_t = 100)]
    num_writes_before_sync: u64,
    #[arg(long, default_value_t = 262_144)]
    entropy_sweep_max_bytes: u64,
    /// Leave recall pages cached. The bench default drops them.
    #[arg(long)]
    keep_recall_cache: bool,
    /// Transaction size mix `chunks:weight,...`.
    #[arg(long, default_value = "1:40,4:25,16:20,64:10,256:5")]
    ingest_mix: String,
    /// Override every step duration.
    #[arg(long)]
    step_secs: Option<u64>,
    /// Maximum chunks in one sequential serve read.
    #[arg(long, default_value_t = 8)]
    serve_seq_chunks: u32,
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
        drop_recall_page_cache: !args.keep_recall_cache,
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
    eprintln!(
        "irys-sm-bench: partition {partition_chunks} chunks, write window {write_origin}..{write_end}, ingest frontier {ingest_cursor}, migration frontier {migration_cursor}"
    );

    let ceiling = pending_ceiling(args.num_writes_before_sync, CHUNK_SIZE);
    let flush = flush_batch(args.num_writes_before_sync, CHUNK_SIZE);
    let span = span_limit(args.entropy_sweep_max_bytes, CHUNK_SIZE);
    let git = git_info();
    let header = serde_json::json!({
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
        "entropy_sweep_max_bytes": args.entropy_sweep_max_bytes,
        "drop_recall_page_cache": !args.keep_recall_cache,
        "ingest_mix": args.ingest_mix,
        "serve_seq_chunks": args.serve_seq_chunks,
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
        ],
    });
    std::fs::create_dir_all(&args.out)?;
    write_json(&args.out.join("header.json"), &header)?;
    let mut samples = SampleWriter::create(&args.out.join("samples.jsonl"))?;

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
    });
    opened.start_lane()?;

    let out = args.out;
    let stop_on_fail = plan.stop_on_fail;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let code = runtime.block_on(execute(
        io,
        plan,
        &mut samples,
        &out,
        ingest_cursor,
        migration_cursor,
        ceiling,
        flush,
        stop_on_fail,
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
    out: &Path,
    ingest_cursor: u32,
    migration_cursor: u32,
    ceiling: u64,
    flush: u64,
    stop_on_fail: bool,
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
    ));
    let migration = tokio::spawn(migration_driver(
        Arc::clone(&io),
        rates_rx.clone(),
        stop_rx.clone(),
        migration_cursor,
    ));
    let serve = tokio::spawn(serve_driver(Arc::clone(&io), rates_rx, stop_rx));

    let mut rows = Vec::new();
    let mut scored_fail = false;
    let mut interrupted = false;
    for step in &plan.steps {
        if *interrupt_rx.borrow() {
            interrupted = true;
            break;
        }
        if scored_fail && stop_on_fail {
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
        io.accept_new.store(true, Ordering::Release);
        let _ = rates_tx.send(step.rates);
        let outcome = run_step(
            step,
            &io,
            samples,
            &rates_tx,
            &mut interrupt_rx,
            ceiling,
            flush,
        )
        .await?;
        if outcome.interrupted {
            interrupted = true;
        }
        if !outcome.passed && !outcome.interrupted {
            scored_fail = true;
        }
        rows.push(outcome.row);
        // A drain timeout or a driver abort stops the plan even when
        // `stop_on_fail` is false. A scored miss stops only when that flag is set.
        if interrupted || outcome.halt || (stop_on_fail && !outcome.passed) {
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
    )?;
    write_table(&out.join("table.txt"), &rows)?;
    Ok(code)
}

struct StepOutcome {
    row: StepRow,
    passed: bool,
    interrupted: bool,
    /// Drain timeout or a driver abort. Later steps must not start.
    halt: bool,
}

async fn run_step(
    step: &Step,
    io: &Arc<BenchIo>,
    samples: &mut SampleWriter,
    rates_tx: &watch::Sender<Rates>,
    interrupt: &mut watch::Receiver<bool>,
    ceiling: u64,
    flush: u64,
) -> eyre::Result<StepOutcome> {
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
    if *interrupt.borrow() {
        interrupted = true;
    }
    let drained = wait_quiet(io, interrupt, DRAIN).await?;
    if *interrupt.borrow() {
        interrupted = true;
    }
    accum.add_quiet(&take_buckets(io));
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
    let aborted = if interrupted { None } else { take_fatal(io) };
    let measured = StepMeasured {
        secs: accum.seconds,
        scheduled_ingest: mib_bytes(step.rates.ingest_mib_s, accum.seconds),
        durable_ingest: accum.ingest.durable,
        enqueued_ingest: accum.ingest.enqueued,
        scheduled_migration: mib_bytes(step.rates.migration_mib_s, accum.seconds),
        durable_migration: accum.migration.durable,
        enqueued_migration: accum.migration.enqueued,
        chunk_size: CHUNK_SIZE,
        recall_ops: accum.recall.ops,
        recall_skips: accum.recall.skips,
        recall_start_p99_us: crate::stats::summarize(&accum.recall.delays_us).p99,
        overloaded_secs: accum.overloaded_secs,
        pending_samples: accum.pending_samples,
        pending_after_drain: pending_after,
        migration_jobs_after_drain: u64::try_from(jobs_after).unwrap_or(u64::MAX),
        ceiling,
        flush_batch: flush,
        aborted,
    };
    let mut reasons = if interrupted {
        Vec::new()
    } else {
        judge(&measured)
    };
    if interrupted {
        reasons.insert(0, "interrupted".to_string());
    }
    let passed = reasons.iter().all(|reason| reason == "interrupted");
    Ok(StepOutcome {
        passed,
        interrupted,
        halt: measured.aborted.is_some(),
        row: StepRow {
            name: step.name.clone(),
            passed,
            reasons,
            measured,
            recall: accum.recall,
            ingest: accum.ingest,
            migration: accum.migration,
            serve: accum.serve,
        },
    })
}

fn record_second(
    step: &Step,
    sec: u64,
    io: &BenchIo,
    samples: &mut SampleWriter,
    accum: &mut StepAccum,
) -> eyre::Result<()> {
    let buckets = take_buckets(io);
    let snap = io.module.bench_queue_snapshot();
    let jobs = io.module.pending_body_migrations()?.len();
    let inflight =
        io.ingest_inflight.load(Ordering::Acquire) + io.migration_inflight.load(Ordering::Acquire);
    let overloaded = snap.pending_bytes >= io.ceiling;
    let jobs_u64 = u64::try_from(jobs).unwrap_or(u64::MAX);
    let pending_chunks = u64::try_from(snap.pending_chunks).unwrap_or(u64::MAX);
    let sweep_slots = u64::try_from(snap.sweep_slots).unwrap_or(u64::MAX);
    let inflight_u64 = u64::try_from(inflight).unwrap_or(u64::MAX);
    for (name, bucket) in [
        ("recall", &buckets.recall),
        ("ingest", &buckets.ingest),
        ("migration", &buckets.migration),
        ("serve", &buckets.serve),
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
