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
use crate::stats::{DriverBucket, Phase, permit_avg_milli, summarize};

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
    pub enqueue_blocked_us: u64,
    pub pending_bytes: u64,
    pub pending_chunks: u64,
    pub sweep_slots: u64,
    pub migration_jobs: u64,
    pub inflight: u64,
    pub overloaded: bool,
    pub phases: PhasesOut,
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
    scheduled_bytes: u64,
    chunk_size: u64,
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
        enqueue_blocked_us: bucket.enqueue_blocked_us,
        pending_bytes,
        pending_chunks,
        sweep_slots,
        migration_jobs,
        inflight,
        overloaded,
        phases: phases_out(
            bucket,
            scheduled_bytes,
            chunk_size,
            1_000_000,
            driver == "migration",
        ),
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
    ingest_offered_bytes: u64,
    ingest_enqueued_bytes: u64,
    ingest_durable_bytes: u64,
    ingest_enqueue_blocked_us: u64,
    ingest_enqueue_blocked_secs: u64,
    migration_offered_bytes: u64,
    migration_enqueued_bytes: u64,
    migration_durable_bytes: u64,
    migration_enqueue_blocked_us: u64,
    migration_enqueue_blocked_secs: u64,
    overloaded_secs: u64,
    pending_after_drain: u64,
    migration_jobs_after_drain: u64,
    soft_fail: bool,
    recall: DriverOut,
    ingest: DriverOut,
    migration: DriverOut,
    serve: DriverOut,
    ingest_phases: PhasesOut,
    migration_phases: PhasesOut,
    rocks: Option<RocksStepStats>,
}

pub(crate) struct StepRow {
    pub name: String,
    pub passed: bool,
    /// Failed only on a recall margin or an enqueue shortfall.
    pub soft_fail: bool,
    pub reasons: Vec<String>,
    pub measured: StepMeasured,
    pub recall: DriverBucket,
    pub ingest: DriverBucket,
    pub migration: DriverBucket,
    pub serve: DriverBucket,
    pub rocks: Option<RocksStepStats>,
}

/// RocksDB counters for one step. Stall time and flush count are deltas.
/// Pending compaction bytes and running flushes are the gauge at step end.
#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct RocksStepStats {
    pub write_stall_us: u64,
    pub pending_compaction_bytes: u64,
    pub flushes: u64,
    pub running_flushes: u64,
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
        ingest_offered_bytes: row.measured.scheduled_ingest,
        ingest_enqueued_bytes: row.measured.enqueued_ingest,
        ingest_durable_bytes: row.measured.durable_ingest,
        ingest_enqueue_blocked_us: row.measured.ingest_enqueue_blocked_us,
        ingest_enqueue_blocked_secs: row.measured.ingest_enqueue_blocked_us / 1_000_000,
        migration_offered_bytes: row.measured.scheduled_migration,
        migration_enqueued_bytes: row.measured.enqueued_migration,
        migration_durable_bytes: row.measured.durable_migration,
        migration_enqueue_blocked_us: row.measured.migration_enqueue_blocked_us,
        migration_enqueue_blocked_secs: row.measured.migration_enqueue_blocked_us / 1_000_000,
        overloaded_secs: row.measured.overloaded_secs,
        soft_fail: row.soft_fail,
        pending_after_drain: row.measured.pending_after_drain,
        migration_jobs_after_drain: row.measured.migration_jobs_after_drain,
        recall: driver_out(&row.recall),
        ingest: driver_out(&row.ingest),
        migration: driver_out(&row.migration),
        serve: driver_out(&row.serve),
        ingest_phases: phases_out(
            &row.ingest,
            row.measured.scheduled_ingest,
            row.measured.chunk_size,
            row.measured.secs.saturating_mul(1_000_000),
            false,
        ),
        migration_phases: phases_out(
            &row.migration,
            row.measured.scheduled_migration,
            row.measured.chunk_size,
            row.measured.secs.saturating_mul(1_000_000),
            true,
        ),
        rocks: row.rocks,
    }
}

pub(crate) fn index_banner(
    engine: &str,
    dir: &Path,
    preset: Option<&str>,
    tuning: Option<&str>,
    block_cache_bytes: Option<usize>,
) -> String {
    match (preset, tuning, block_cache_bytes) {
        (Some(preset), Some(tuning), Some(bytes)) => format!(
            "index_engine={engine} index_dir={} rocks_preset={preset} rocks_block_cache_bytes={bytes} rocks_tuning={tuning}\n",
            dir.display()
        ),
        _ => format!("index_engine={engine} index_dir={}\n", dir.display()),
    }
}

pub(crate) fn write_table(path: &Path, rows: &[StepRow], index_line: &str) -> eyre::Result<()> {
    let text = table(rows, index_line);
    fs::write(path, &text)?;
    print!("{text}");
    std::io::stdout().flush()?;
    Ok(())
}

#[derive(Serialize)]
pub(crate) struct PhasesOut {
    pub limit: &'static str,
    pub pace_behind_us: u64,
    pub pace_sleep_us: u64,
    pub pace_released_bytes: u64,
    pub pace_scheduled_bytes: u64,
    pub txreg_count: u64,
    pub txreg_total_us: u64,
    pub txreg_p50_us: u64,
    pub txreg_p99_us: u64,
    pub txreg_max_us: u64,
    pub txreg_wait_us: u64,
    pub settle_count: u64,
    pub settle_total_us: u64,
    pub settle_p50_us: u64,
    pub settle_p99_us: u64,
    pub settle_max_us: u64,
    pub enqueue_count: u64,
    pub enqueue_total_us: u64,
    pub enqueue_p50_us: u64,
    pub enqueue_p99_us: u64,
    pub enqueue_max_us: u64,
    pub durable_count: u64,
    pub durable_total_us: u64,
    pub durable_p50_us: u64,
    pub durable_p99_us: u64,
    pub durable_max_us: u64,
    pub permit_wait_us: u64,
    pub permit_max_in_use: u64,
    pub permit_avg_in_use_milli: u64,
    pub gen_us: u64,
}

fn phase_stats(phase: &Phase) -> (u64, u64, u64, u64, u64) {
    let summary = summarize(&phase.samples_us);
    (
        phase.count,
        phase.total_us,
        summary.p50,
        summary.p99,
        summary.max,
    )
}

fn phases_out(
    bucket: &DriverBucket,
    scheduled: u64,
    chunk: u64,
    step_us: u64,
    enqueue_blocks: bool,
) -> PhasesOut {
    let (txreg_count, txreg_total_us, txreg_p50_us, txreg_p99_us, txreg_max_us) =
        phase_stats(&bucket.txreg);
    let (settle_count, settle_total_us, settle_p50_us, settle_p99_us, settle_max_us) =
        phase_stats(&bucket.settle);
    let (enqueue_count, enqueue_total_us, enqueue_p50_us, enqueue_p99_us, enqueue_max_us) =
        phase_stats(&bucket.enqueue_lat);
    let (durable_count, durable_total_us, durable_p50_us, durable_p99_us, durable_max_us) =
        phase_stats(&bucket.durable_wait);
    PhasesOut {
        limit: phase_limit(bucket, scheduled, chunk, step_us, enqueue_blocks),
        pace_behind_us: bucket.pace_behind_us,
        pace_sleep_us: bucket.pace_sleep_us,
        pace_released_bytes: bucket.pace_released,
        pace_scheduled_bytes: scheduled,
        txreg_count,
        txreg_total_us,
        txreg_p50_us,
        txreg_p99_us,
        txreg_max_us,
        txreg_wait_us: bucket.txreg_wait_us,
        settle_count,
        settle_total_us,
        settle_p50_us,
        settle_p99_us,
        settle_max_us,
        enqueue_count,
        enqueue_total_us,
        enqueue_p50_us,
        enqueue_p99_us,
        enqueue_max_us,
        durable_count,
        durable_total_us,
        durable_p50_us,
        durable_p99_us,
        durable_max_us,
        permit_wait_us: bucket.permit_wait_us,
        permit_max_in_use: bucket.permit_max_in_use,
        permit_avg_in_use_milli: permit_avg_milli(bucket),
        gen_us: bucket.gen_us,
    }
}

/// Why released or enqueued bytes missed `rate * step`.
///
/// `rate` means the schedule was met. A named stall is the largest generator
/// wait and at least 5% of the step. `pacing` means the bytes were short and
/// none of those waits explain it. Ingest enqueue overlaps, so it is not a
/// generator stall. Migration deposit is.
pub(crate) fn phase_limit(
    bucket: &DriverBucket,
    scheduled: u64,
    chunk: u64,
    step_us: u64,
    enqueue_blocks: bool,
) -> &'static str {
    if scheduled == 0 {
        return "off";
    }
    let short = bucket.pace_released.saturating_add(chunk) < scheduled
        || bucket.enqueued.saturating_add(chunk) < scheduled;
    if !short {
        return "rate";
    }
    let mut stalls = [
        ("txreg", bucket.txreg_wait_us),
        ("gen", bucket.gen_us),
        ("permits", bucket.permit_wait_us),
        ("backpressure", 0),
        ("enqueue", 0),
    ];
    if bucket.enqueue_blocked_us > bucket.permit_wait_us {
        stalls[3].1 = bucket.enqueue_blocked_us;
    }
    if enqueue_blocks {
        stalls[4].1 = bucket.enqueue_lat.total_us;
    }
    let threshold = step_us / 20;
    let mut best_name: &'static str = "pacing";
    let mut best = 0_u64;
    for (name, cost) in stalls {
        if cost > best {
            best = cost;
            best_name = name;
        }
    }
    if best < threshold {
        "pacing"
    } else {
        best_name
    }
}

fn table(rows: &[StepRow], index_line: &str) -> String {
    let mut out = String::new();
    if !index_line.is_empty() {
        out.push_str(index_line);
        if !index_line.ends_with('\n') {
            out.push('\n');
        }
    }
    out.push_str(&format!(
        "{:<15} {:>4}  {:>15}  {:>11}  {:>11}  {:>12}  {:>22}  {:>19}  {:>9}  result\n",
        "step",
        "secs",
        "recall ops/skip",
        "lat_p50_us",
        "lat_p99_us",
        "start_p99_us",
        "ingest MiB off/enq/dur",
        "mig MiB off/enq/dur",
        "serve ops",
    ));
    for row in rows {
        let result = if row.passed { "PASS" } else { "FAIL" };
        let recall_lat = summarize(&row.recall.lats_us);
        let ingest = mib_triple(
            row.measured.scheduled_ingest,
            row.measured.enqueued_ingest,
            row.measured.durable_ingest,
        );
        let migration = mib_triple(
            row.measured.scheduled_migration,
            row.measured.enqueued_migration,
            row.measured.durable_migration,
        );
        let line = format!(
            "{:<15} {:>4}  {:>10}/{:<4}  {:>11}  {:>11}  {:>12}  {ingest:>22}  {migration:>19}  {:>9}  {result}\n",
            row.name,
            row.measured.secs,
            row.recall.ops,
            row.recall.skips,
            recall_lat.p50,
            recall_lat.p99,
            summarize(&row.recall.delays_us).p99,
            row.serve.ops,
        );
        out.push_str(&line);
        out.push_str(&phase_line(
            "ingest",
            &row.ingest,
            row.measured.scheduled_ingest,
            row.measured.chunk_size,
            row.measured.secs,
            false,
        ));
        out.push_str(&phase_line(
            "migration",
            &row.migration,
            row.measured.scheduled_migration,
            row.measured.chunk_size,
            row.measured.secs,
            true,
        ));
        if let Some(rocks) = row.rocks {
            out.push_str(&format!(
                "rocks: stall={} pending_compaction_bytes={} flushes={} running_flushes={}\n",
                fmt_ms(rocks.write_stall_us),
                rocks.pending_compaction_bytes,
                rocks.flushes,
                rocks.running_flushes,
            ));
        }
        for reason in &row.reasons {
            out.push_str(&format!("  - {reason}\n"));
        }
    }
    out
}

fn phase_line(
    label: &str,
    bucket: &DriverBucket,
    scheduled: u64,
    chunk: u64,
    secs: u64,
    enqueue_blocks: bool,
) -> String {
    let phases = phases_out(
        bucket,
        scheduled,
        chunk,
        secs.saturating_mul(1_000_000),
        enqueue_blocks,
    );
    format!(
        "{label} phases: limit={} behind={} sleep={} rel/sched={}/{}MiB txreg n={} p99={} wait={} settle n={} p99={} enqueue n={} p99={} durable p99={} permit_wait={} permits={}/{} gen={}\n",
        phases.limit,
        fmt_secs(phases.pace_behind_us),
        fmt_secs(phases.pace_sleep_us),
        mib(phases.pace_released_bytes),
        mib(phases.pace_scheduled_bytes),
        phases.txreg_count,
        fmt_ms(phases.txreg_p99_us),
        fmt_secs(phases.txreg_wait_us),
        phases.settle_count,
        fmt_ms(phases.settle_p99_us),
        phases.enqueue_count,
        fmt_ms(phases.enqueue_p99_us),
        fmt_ms(phases.durable_p99_us),
        fmt_secs(phases.permit_wait_us),
        phases.permit_max_in_use,
        fmt_milli(phases.permit_avg_in_use_milli),
        fmt_secs(phases.gen_us),
    )
}

fn fmt_secs(micros: u64) -> String {
    let whole = micros / 1_000_000;
    let millis = (micros % 1_000_000) / 1_000;
    format!("{whole}.{millis:03}s")
}

fn fmt_ms(micros: u64) -> String {
    let millis = micros / 1000;
    let tenths = (micros % 1000) / 100;
    format!("{millis}.{tenths}ms")
}

fn fmt_milli(milli: u64) -> String {
    format!("{}.{:03}", milli / 1000, milli % 1000)
}

fn mib(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}

fn mib_triple(offered: u64, enqueued: u64, durable: u64) -> String {
    format!("{}/{}/{}", mib(offered), mib(enqueued), mib(durable))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::{StepMeasured, percentile};

    fn measured(scheduled: u64) -> StepMeasured {
        StepMeasured {
            secs: 600,
            scheduled_ingest: scheduled,
            durable_ingest: 0,
            enqueued_ingest: 0,
            scheduled_migration: 0,
            durable_migration: 0,
            enqueued_migration: 0,
            chunk_size: 262_144,
            recall_ops: 0,
            recall_skips: 0,
            recall_start_p99_us: 0,
            recall_lat_p99_us: 0,
            ingest_enqueue_blocked_us: 0,
            migration_enqueue_blocked_us: 0,
            overloaded_secs: 0,
            pending_samples: Vec::new(),
            pending_after_drain: 0,
            migration_jobs_after_drain: 0,
            ceiling: 1,
            flush_batch: 1,
            aborted: None,
        }
    }

    #[test]
    fn phase_limit_names_the_generator_stall() {
        let mut bucket = DriverBucket::default();
        assert_eq!(phase_limit(&bucket, 0, 262_144, 600_000_000, false), "off");
        bucket.pace_released = 1_000_000;
        bucket.enqueued = 1_000_000;
        bucket.txreg_wait_us = 400_000_000;
        assert_eq!(
            phase_limit(&bucket, 1_000_000, 262_144, 600_000_000, false),
            "rate"
        );
        bucket.pace_released = 0;
        bucket.enqueued = 0;
        assert_eq!(
            phase_limit(&bucket, 1_000_000, 262_144, 600_000_000, false),
            "txreg"
        );
        bucket.txreg_wait_us = 1_000;
        assert_eq!(
            phase_limit(&bucket, 1_000_000, 262_144, 600_000_000, false),
            "pacing"
        );
        bucket.permit_wait_us = 100_000_000;
        bucket.enqueue_blocked_us = 100_000_000;
        assert_eq!(
            phase_limit(&bucket, 1_000_000, 262_144, 600_000_000, false),
            "permits"
        );
        bucket.permit_wait_us = 0;
        bucket.enqueue_blocked_us = 0;
        bucket.enqueue_lat.push(200_000_000);
        assert_eq!(
            phase_limit(&bucket, 1_000_000, 262_144, 600_000_000, true),
            "enqueue"
        );
        assert_eq!(
            phase_limit(&bucket, 1_000_000, 262_144, 600_000_000, false),
            "pacing",
            "ingest enqueue overlaps and is not a generator stall"
        );
    }

    #[test]
    fn the_table_prints_the_phase_limit() {
        let mut ingest = DriverBucket::default();
        ingest.txreg.push(80_000);
        ingest.txreg_wait_us = 400_000_000;
        ingest.pace_behind_us = 400_000_000;
        let row = StepRow {
            name: "ingest-32".to_string(),
            passed: false,
            soft_fail: true,
            reasons: vec!["short".to_string()],
            measured: measured(19_200 * 1024 * 1024),
            recall: DriverBucket::default(),
            ingest,
            migration: DriverBucket::default(),
            serve: DriverBucket::default(),
            rocks: None,
        };
        let text = table(&[row], "index_engine=mdbx index_dir=/loadtest/db\n");
        assert!(text.contains("index_engine=mdbx"));
        assert!(text.contains("ingest phases: limit=txreg"));
        assert!(text.contains("migration phases: limit=off"));
        assert!(text.contains("txreg n=1 p99=80.0ms"));
        assert!(text.contains("wait=400.000s"));
        assert!(percentile(&[80_000], 99) == 80_000);
    }

    #[test]
    fn sample_lines_carry_phase_fields() {
        let mut bucket = DriverBucket::default();
        bucket.txreg.push(80_000);
        bucket.txreg_wait_us = 80_000;
        bucket.pace_released = 100;
        let line = sample_line(
            "ingest-4",
            1,
            "ingest",
            &bucket,
            0,
            0,
            0,
            0,
            0,
            false,
            4 * 1024 * 1024,
            262_144,
        );
        let value = serde_json::to_value(&line).expect("sample json");
        assert_eq!(value["phases"]["limit"], "txreg");
        assert_eq!(value["phases"]["txreg_p99_us"], 80_000);
        assert_eq!(value["phases"]["pace_scheduled_bytes"], 4 * 1024 * 1024);
        assert_eq!(value["phases"]["pace_released_bytes"], 100);
        assert!(value["phases"]["permit_avg_in_use_milli"].is_u64());
    }
}
