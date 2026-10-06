//! Run header, per-second samples, and the step summary.
//!
//! Latency values are integer microseconds. Rates in the table are integer MiB,
//! `bytes / (1024 * 1024)`.

use std::fs::{self, File};
use std::io::{BufWriter, Write as _};
use std::path::Path;
use std::process::Command;

use eyre::WrapErr as _;
use serde::Serialize;

use crate::schedule::{Plan, StepMeasured};
use crate::stats::{DriverBucket, summarize};

#[derive(Serialize)]
pub(crate) struct GitInfo {
    pub commit: String,
    pub dirty: bool,
}

pub(crate) fn git_info() -> GitInfo {
    let commit = text_out(["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    let dirty = text_out(["status", "--porcelain"]).is_some_and(|text| !text.is_empty());
    GitInfo { commit, dirty }
}

fn text_out<const N: usize>(args: [&str; N]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub(crate) fn write_json(path: &Path, value: &impl Serialize) -> eyre::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = File::create(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

pub(crate) struct SampleWriter {
    file: BufWriter<File>,
}

impl SampleWriter {
    pub(crate) fn create(path: &Path) -> eyre::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file =
            File::create(path).wrap_err_with(|| format!("failed to create {}", path.display()))?;
        Ok(Self {
            file: BufWriter::new(file),
        })
    }

    pub(crate) fn write_line(&mut self, value: &impl Serialize) -> eyre::Result<()> {
        serde_json::to_writer(&mut self.file, value)?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        Ok(())
    }
}

#[derive(Serialize)]
pub(crate) struct SampleLine<'a> {
    pub step: &'a str,
    pub sec: u64,
    pub driver: &'a str,
    pub ops: u64,
    pub bytes: u64,
    pub skips: u64,
    pub lat_p50_us: u64,
    pub lat_p99_us: u64,
    pub lat_max_us: u64,
    pub start_delay_p50_us: u64,
    pub start_delay_p99_us: u64,
    pub start_delay_max_us: u64,
    pub enqueued: u64,
    pub durable: u64,
    pub pending_bytes: u64,
    pub pending_chunks: u64,
    pub sweep_slots: u64,
    pub migration_jobs: u64,
    pub inflight: u64,
    pub overloaded: bool,
}

pub(crate) fn sample_line<'a>(
    step: &'a str,
    sec: u64,
    driver: &'a str,
    bucket: &DriverBucket,
    pending_bytes: u64,
    pending_chunks: u64,
    sweep_slots: u64,
    migration_jobs: u64,
    inflight: u64,
    overloaded: bool,
) -> SampleLine<'a> {
    let lat = summarize(&bucket.lats_us);
    let delay = summarize(&bucket.delays_us);
    SampleLine {
        step,
        sec,
        driver,
        ops: bucket.ops,
        bytes: bucket.bytes,
        skips: bucket.skips,
        lat_p50_us: lat.p50,
        lat_p99_us: lat.p99,
        lat_max_us: lat.max,
        start_delay_p50_us: delay.p50,
        start_delay_p99_us: delay.p99,
        start_delay_max_us: delay.max,
        enqueued: bucket.enqueued,
        durable: bucket.durable,
        pending_bytes,
        pending_chunks,
        sweep_slots,
        migration_jobs,
        inflight,
        overloaded,
    }
}

#[derive(Serialize)]
struct DriverOut {
    ops: u64,
    bytes: u64,
    skips: u64,
    enqueued: u64,
    durable: u64,
    lat_p50_us: u64,
    lat_p99_us: u64,
    lat_max_us: u64,
    start_delay_p50_us: u64,
    start_delay_p99_us: u64,
    start_delay_max_us: u64,
}

fn driver_out(bucket: &DriverBucket) -> DriverOut {
    let lat = summarize(&bucket.lats_us);
    let delay = summarize(&bucket.delays_us);
    DriverOut {
        ops: bucket.ops,
        bytes: bucket.bytes,
        skips: bucket.skips,
        enqueued: bucket.enqueued,
        durable: bucket.durable,
        lat_p50_us: lat.p50,
        lat_p99_us: lat.p99,
        lat_max_us: lat.max,
        start_delay_p50_us: delay.p50,
        start_delay_p99_us: delay.p99,
        start_delay_max_us: delay.max,
    }
}

#[derive(Serialize)]
struct StepOut {
    name: String,
    passed: bool,
    reasons: Vec<String>,
    secs: u64,
    scheduled_ingest: u64,
    scheduled_migration: u64,
    overloaded_secs: u64,
    pending_after_drain: u64,
    migration_jobs_after_drain: u64,
    recall: DriverOut,
    ingest: DriverOut,
    migration: DriverOut,
    serve: DriverOut,
}

pub(crate) struct StepRow {
    pub name: String,
    pub passed: bool,
    pub reasons: Vec<String>,
    pub measured: StepMeasured,
    pub recall: DriverBucket,
    pub ingest: DriverBucket,
    pub migration: DriverBucket,
    pub serve: DriverBucket,
}

#[derive(Serialize)]
struct Summary<'a> {
    passed: bool,
    exit_code: i32,
    interrupted: bool,
    driver_notes: &'a [String],
    steps: Vec<StepOut>,
}

pub(crate) fn write_summary(
    path: &Path,
    rows: &[StepRow],
    passed: bool,
    exit_code: i32,
    interrupted: bool,
    driver_notes: &[String],
) -> eyre::Result<()> {
    let steps = rows.iter().map(step_out).collect();
    write_json(
        path,
        &Summary {
            passed,
            exit_code,
            interrupted,
            driver_notes,
            steps,
        },
    )
}

fn step_out(row: &StepRow) -> StepOut {
    StepOut {
        name: row.name.clone(),
        passed: row.passed,
        reasons: row.reasons.clone(),
        secs: row.measured.secs,
        scheduled_ingest: row.measured.scheduled_ingest,
        scheduled_migration: row.measured.scheduled_migration,
        overloaded_secs: row.measured.overloaded_secs,
        pending_after_drain: row.measured.pending_after_drain,
        migration_jobs_after_drain: row.measured.migration_jobs_after_drain,
        recall: driver_out(&row.recall),
        ingest: driver_out(&row.ingest),
        migration: driver_out(&row.migration),
        serve: driver_out(&row.serve),
    }
}

pub(crate) fn write_table(path: &Path, rows: &[StepRow]) -> eyre::Result<()> {
    let text = table(rows);
    fs::write(path, &text)?;
    print!("{text}");
    std::io::stdout().flush()?;
    Ok(())
}

fn table(rows: &[StepRow]) -> String {
    let mut out = String::new();
    out.push_str(
        "step            secs  recall ops/skip  start_p99_us  ingest MiB off/dur  mig MiB off/dur  serve ops  result\n",
    );
    for row in rows {
        let result = if row.passed { "PASS" } else { "FAIL" };
        let line = format!(
            "{:<15} {:>4}  {:>8}/{:<4}  {:>12}  {:>8}/{:<8}  {:>6}/{:<6}  {:>9}  {result}\n",
            row.name,
            row.measured.secs,
            row.recall.ops,
            row.recall.skips,
            summarize(&row.recall.delays_us).p99,
            mib(row.measured.scheduled_ingest),
            mib(row.ingest.durable),
            mib(row.measured.scheduled_migration),
            mib(row.migration.durable),
            row.serve.ops,
        );
        out.push_str(&line);
        for reason in &row.reasons {
            out.push_str(&format!("  - {reason}\n"));
        }
    }
    out
}

fn mib(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}

pub(crate) fn plan_steps(plan: &Plan) -> Vec<serde_json::Value> {
    plan.steps
        .iter()
        .map(|step| {
            serde_json::json!({
                "name": step.name,
                "secs": step.secs,
                "recall_num": step.rates.recall_num,
                "recall_den": step.rates.recall_den,
                "ingest_mib_s": step.rates.ingest_mib_s,
                "migration_mib_s": step.rates.migration_mib_s,
                "serve_per_sec": step.rates.serve_per_sec,
                "serve_seq_pct": step.rates.serve_seq_pct,
            })
        })
        .collect()
}
