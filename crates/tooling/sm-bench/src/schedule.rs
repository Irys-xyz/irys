//! Step schedule, presets, and the pass/fail judge.
//!
//! Rates are integer ratios. `1.8/s` is `9/5`, which is a 555_555_555 ns period.
//!
//! A step named `idle` or `warm`, and the first recall-only step, is the
//! recall baseline. Later steps fail recall when start-delay or latency p99
//! rises by more than the configured margin. A schedule with no such step
//! uses the absolute ceilings instead.
//! Recall-margin misses, enqueue shortfalls, and serve-rate misses are soft:
//! they fail the step and do not stop the ladder. A durability backlog is not soft.
//!
//! The tail window judges the last N scored seconds on their own. A step
//! whose mean meets the offer can still fail once the backlog fills.

use std::path::Path;

use eyre::{WrapErr as _, eyre};
use serde::Deserialize;

pub(crate) const RECALL_SKIP_LAG_STEPS: u128 = 3;
/// Absolute start-delay ceiling when the schedule has no `idle` step.
pub(crate) const RECALL_START_P99_LIMIT_US: u64 = 200_000;
/// Absolute recall-latency ceiling when the schedule has no `idle` step.
pub(crate) const RECALL_LAT_P99_LIMIT_US: u64 = 500_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rates {
    pub recall_num: u64,
    pub recall_den: u64,
    pub ingest_mib_s: u64,
    pub migration_mib_s: u64,
    pub serve_per_sec: u64,
    pub serve_seq_pct: u64,
}

impl Rates {
    pub(crate) const fn idle() -> Self {
        Self {
            recall_num: 0,
            recall_den: 1,
            ingest_mib_s: 0,
            migration_mib_s: 0,
            serve_per_sec: 0,
            serve_seq_pct: 50,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Step {
    pub name: String,
    pub secs: u64,
    pub rates: Rates,
}

#[derive(Debug, Clone)]
pub(crate) struct Plan {
    pub name: String,
    pub stop_on_fail: bool,
    pub steps: Vec<Step>,
}

#[derive(Debug, Deserialize)]
struct ScheduleFile {
    #[serde(default = "default_true")]
    stop_on_fail: bool,
    step: Vec<StepFile>,
}

#[derive(Debug, Deserialize)]
struct StepFile {
    #[serde(default)]
    name: String,
    secs: u64,
    #[serde(default)]
    recall_num: u64,
    #[serde(default = "default_one")]
    recall_den: u64,
    #[serde(default)]
    ingest_mib_s: u64,
    #[serde(default)]
    migration_mib_s: u64,
    #[serde(default)]
    serve_per_sec: u64,
    #[serde(default = "default_fifty")]
    serve_seq_pct: u64,
}

fn default_true() -> bool {
    true
}
fn default_one() -> u64 {
    1
}
fn default_fifty() -> u64 {
    50
}

pub(crate) fn preset(name: &str) -> eyre::Result<Plan> {
    match name {
        "recall-1" => Ok(recall_plan("recall-1", 1, 1)),
        "recall-1.8" => Ok(recall_plan("recall-1.8", 9, 5)),
        "ingest-ooo" => Ok(Plan {
            name: "ingest-ooo".to_string(),
            stop_on_fail: true,
            steps: vec![
                Step {
                    name: "idle".to_string(),
                    secs: 8,
                    rates: Rates {
                        recall_num: 1,
                        recall_den: 1,
                        ..Rates::idle()
                    },
                },
                Step {
                    name: "ingest".to_string(),
                    secs: 20,
                    rates: Rates {
                        recall_num: 1,
                        recall_den: 1,
                        ingest_mib_s: 8,
                        migration_mib_s: 0,
                        serve_per_sec: 0,
                        serve_seq_pct: 50,
                    },
                },
            ],
        }),
        "smoke" => Ok(Plan {
            name: "smoke".to_string(),
            stop_on_fail: true,
            steps: vec![Step {
                name: "smoke".to_string(),
                secs: 8,
                rates: Rates {
                    recall_num: 1,
                    recall_den: 1,
                    ingest_mib_s: 2,
                    migration_mib_s: 1,
                    serve_per_sec: 4,
                    serve_seq_pct: 50,
                },
            }],
        }),
        other => Err(eyre!(
            "unknown preset {other}; expected recall-1, recall-1.8, smoke, or ingest-ooo"
        )),
    }
}

fn recall_plan(name: &str, recall_num: u64, recall_den: u64) -> Plan {
    let mut steps = vec![Step {
        name: "idle".to_string(),
        secs: 600,
        rates: Rates {
            recall_num,
            recall_den,
            ..Rates::idle()
        },
    }];
    for mib in [2_u64, 4, 8, 16, 32] {
        steps.push(Step {
            name: format!("ingest-{mib}"),
            secs: 600,
            rates: Rates {
                recall_num,
                recall_den,
                ingest_mib_s: mib,
                migration_mib_s: 0,
                serve_per_sec: 0,
                serve_seq_pct: 50,
            },
        });
    }
    Plan {
        name: name.to_string(),
        stop_on_fail: true,
        steps,
    }
}

pub(crate) fn load_schedule(path: &Path) -> eyre::Result<Plan> {
    let text = std::fs::read_to_string(path)
        .wrap_err_with(|| format!("failed to read schedule {}", path.display()))?;
    let file: ScheduleFile = toml::from_str(&text).wrap_err("schedule TOML")?;
    if file.step.is_empty() {
        return Err(eyre!("schedule has no steps"));
    }
    let mut steps = Vec::with_capacity(file.step.len());
    for (index, step) in file.step.into_iter().enumerate() {
        if step.secs == 0 {
            return Err(eyre!("step {index} has zero duration"));
        }
        if step.recall_den == 0 {
            return Err(eyre!("step {index} recall_den is 0"));
        }
        if step.serve_seq_pct > 100 {
            return Err(eyre!("step {index} serve_seq_pct is above 100"));
        }
        let name = if step.name.is_empty() {
            format!("step-{index}")
        } else {
            step.name
        };
        steps.push(Step {
            name,
            secs: step.secs,
            rates: Rates {
                recall_num: step.recall_num,
                recall_den: step.recall_den,
                ingest_mib_s: step.ingest_mib_s,
                migration_mib_s: step.migration_mib_s,
                serve_per_sec: step.serve_per_sec,
                serve_seq_pct: step.serve_seq_pct,
            },
        });
    }
    Ok(Plan {
        name: path.display().to_string(),
        stop_on_fail: file.stop_on_fail,
        steps,
    })
}

pub(crate) fn apply_step_secs(plan: &mut Plan, secs: u64) -> eyre::Result<()> {
    if secs == 0 {
        return Err(eyre!("--step-secs must be greater than 0"));
    }
    for step in &mut plan.steps {
        step.secs = secs;
    }
    Ok(())
}

/// Nanoseconds between recalls. `None` when the driver is off.
pub(crate) fn period_ns(num: u64, den: u64) -> Option<u64> {
    if num == 0 || den == 0 {
        return None;
    }
    Some(1_000_000_000_u64.saturating_mul(den) / num)
}

/// `1:40,4:25` is 40 parts of 1 chunk and 25 parts of 4 chunks.
pub(crate) fn parse_mix(text: &str) -> eyre::Result<Vec<(u32, u64)>> {
    let mut mix = Vec::new();
    if text.is_empty() {
        return Err(eyre!("ingest mix is empty"));
    }
    for part in text.split(',') {
        let (chunks, weight) = part
            .split_once(':')
            .ok_or_else(|| eyre!("ingest mix entry {part} is not chunks:weight"))?;
        let chunks: u32 = chunks
            .trim()
            .parse()
            .wrap_err_with(|| format!("ingest mix chunk count {chunks}"))?;
        let weight: u64 = weight
            .trim()
            .parse()
            .wrap_err_with(|| format!("ingest mix weight {weight}"))?;
        if chunks == 0 || weight == 0 {
            return Err(eyre!("ingest mix entry {part} has a zero"));
        }
        mix.push((chunks, weight));
    }
    Ok(mix)
}

pub(crate) fn mib_bytes(mib_s: u64, secs: u64) -> u64 {
    mib_s.saturating_mul(1024 * 1024).saturating_mul(secs)
}

/// Nearest-rank percentile. `rank = ceil(pct/100 * n) - 1`.
pub(crate) fn percentile(samples: &[u64], pct: u64) -> u64 {
    if samples.is_empty() || pct == 0 {
        return 0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let n = sorted.len() as u64;
    let rank = pct.saturating_mul(n).div_ceil(100).saturating_sub(1);
    let index = usize::try_from(rank).unwrap_or(sorted.len() - 1);
    sorted[index.min(sorted.len() - 1)]
}

pub(crate) fn median(samples: &[u64]) -> u64 {
    percentile(samples, 50)
}

#[derive(Debug, Clone)]
pub(crate) struct StepMeasured {
    pub secs: u64,
    pub scheduled_ingest: u64,
    pub durable_ingest: u64,
    pub enqueued_ingest: u64,
    pub scheduled_migration: u64,
    pub durable_migration: u64,
    pub enqueued_migration: u64,
    /// Serve reads finished during the step, including the drain.
    pub serve_ops: u64,
    /// `serve_per_sec * scored seconds`. Zero means this step has no serve target.
    pub scheduled_serve: u64,
    pub chunk_size: u64,
    pub recall_ops: u64,
    pub recall_skips: u64,
    pub recall_start_p99_us: u64,
    pub recall_lat_p99_us: u64,
    pub ingest_enqueue_blocked_us: u64,
    pub migration_enqueue_blocked_us: u64,
    pub overloaded_secs: u64,
    pub pending_samples: Vec<u64>,
    pub pending_after_drain: u64,
    pub migration_jobs_after_drain: u64,
    pub ceiling: u64,
    pub flush_batch: u64,
    pub aborted: Option<String>,
}

/// How this step's recall p99 is judged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecallBar {
    /// The step named `idle`. It sets the baseline. Skips still fail it.
    Baseline,
    /// Compare with the `idle` step's p99 plus the margins.
    Relative { start_p99_us: u64, lat_p99_us: u64 },
    /// No `idle` baseline yet. Use the absolute ceilings.
    Absolute,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RecallRule {
    pub bar: RecallBar,
    pub delay_margin_us: u64,
    pub latency_margin_us: u64,
}

/// The recall bar for one step.
///
/// `idle` and `warm` are baselines. So is the first recall-only step when no
/// baseline exists yet: it offers no ingest, migration, or serve, so its own
/// p99 is not a miss. A later step uses that baseline. Absolute ceilings apply
/// only when no baseline has been seen and this step is not one.
pub(crate) fn recall_bar(step: &Step, baseline: Option<(u64, u64)>) -> RecallBar {
    let recall_only = step.rates.recall_num > 0
        && step.rates.ingest_mib_s == 0
        && step.rates.migration_mib_s == 0
        && step.rates.serve_per_sec == 0;
    if step.name == "idle" || step.name == "warm" || (baseline.is_none() && recall_only) {
        return RecallBar::Baseline;
    }
    match baseline {
        Some((start_p99_us, lat_p99_us)) => RecallBar::Relative {
            start_p99_us,
            lat_p99_us,
        },
        None => RecallBar::Absolute,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Finding {
    pub message: String,
    /// Recall-margin, enqueue-shortfall, and serve-rate misses. They fail the
    /// step and do not stop the ladder while `--stop-on-fail` is set.
    pub soft: bool,
}

impl Finding {
    fn hard(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            soft: false,
        }
    }

    fn soft(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            soft: true,
        }
    }
}

/// True when a scored miss should end the schedule.
/// Soft findings do not. An empty list does not.
pub(crate) fn stop_for_score(findings: &[Finding], stop_on_fail: bool) -> bool {
    stop_on_fail && findings.iter().any(|finding| !finding.soft)
}

pub(crate) fn judge(measured: &StepMeasured, rule: &RecallRule) -> Vec<Finding> {
    let mut reasons = Vec::new();
    if let Some(reason) = &measured.aborted {
        reasons.push(Finding::hard(format!("aborted: {reason}")));
    }
    if measured.recall_skips > 0 {
        reasons.push(Finding::hard(format!(
            "skipped {} recall(s) (start lag >= {RECALL_SKIP_LAG_STEPS} steps)",
            measured.recall_skips
        )));
    }
    recall_latency(measured, rule, &mut reasons);
    short_of_offered(
        "ingest",
        measured.scheduled_ingest,
        measured.enqueued_ingest,
        measured.chunk_size,
        measured.ingest_enqueue_blocked_us,
        &mut reasons,
    );
    short_of_offered(
        "migration",
        measured.scheduled_migration,
        measured.enqueued_migration,
        measured.chunk_size,
        measured.migration_enqueue_blocked_us,
        &mut reasons,
    );
    short_of_enqueued(
        "ingest",
        measured.enqueued_ingest,
        measured.durable_ingest,
        measured.chunk_size,
        &mut reasons,
    );
    short_of_enqueued(
        "migration",
        measured.enqueued_migration,
        measured.durable_migration,
        measured.chunk_size,
        &mut reasons,
    );
    short_of_serve(measured.serve_ops, measured.scheduled_serve, &mut reasons);
    if measured.secs >= 5 && measured.overloaded_secs.saturating_mul(5) > measured.secs {
        reasons.push(Finding::hard(format!(
            "overloaded for {}s of {}s",
            measured.overloaded_secs, measured.secs
        )));
    }
    if measured.pending_after_drain > measured.ceiling {
        reasons.push(Finding::hard(format!(
            "pending {} bytes after drain is above the ceiling {}",
            measured.pending_after_drain, measured.ceiling
        )));
    }
    if backlog_grew(&measured.pending_samples, measured.flush_batch)
        && measured.pending_after_drain > measured.flush_batch
    {
        reasons.push(Finding::hard(
            "pending-write backlog grew through the step and was still high after drain",
        ));
    }
    if measured.migration_jobs_after_drain > 0 {
        reasons.push(Finding::hard(format!(
            "{} body-migration row(s) remained after drain",
            measured.migration_jobs_after_drain
        )));
    }
    reasons
}

fn recall_latency(measured: &StepMeasured, rule: &RecallRule, reasons: &mut Vec<Finding>) {
    if measured.recall_ops == 0 {
        return;
    }
    match rule.bar {
        RecallBar::Baseline => {}
        RecallBar::Relative {
            start_p99_us,
            lat_p99_us,
        } => {
            let start_limit = start_p99_us.saturating_add(rule.delay_margin_us);
            if measured.recall_start_p99_us > start_limit {
                reasons.push(Finding::soft(format!(
                    "recall start-delay p99 {} us exceeds idle p99 {start_p99_us} us by more than {} us",
                    measured.recall_start_p99_us, rule.delay_margin_us
                )));
            }
            let lat_limit = lat_p99_us.saturating_add(rule.latency_margin_us);
            if measured.recall_lat_p99_us > lat_limit {
                reasons.push(Finding::soft(format!(
                    "recall latency p99 {} us exceeds idle p99 {lat_p99_us} us by more than {} us",
                    measured.recall_lat_p99_us, rule.latency_margin_us
                )));
            }
        }
        RecallBar::Absolute => {
            if measured.recall_start_p99_us > RECALL_START_P99_LIMIT_US {
                reasons.push(Finding::soft(format!(
                    "recall start-delay p99 {} us > {RECALL_START_P99_LIMIT_US} us",
                    measured.recall_start_p99_us
                )));
            }
            if measured.recall_lat_p99_us > RECALL_LAT_P99_LIMIT_US {
                reasons.push(Finding::soft(format!(
                    "recall latency p99 {} us > {RECALL_LAT_P99_LIMIT_US} us",
                    measured.recall_lat_p99_us
                )));
            }
        }
    }
}

/// Offered bytes that never reached `write_data_chunk_queued` or
/// `deposit_data_chunks`. One chunk of slack covers a partial last offer.
fn short_of_offered(
    name: &str,
    offered: u64,
    enqueued: u64,
    chunk_size: u64,
    blocked_us: u64,
    reasons: &mut Vec<Finding>,
) {
    if offered > 0 && enqueued.saturating_add(chunk_size) < offered {
        reasons.push(Finding::soft(format!(
            "{name} enqueued {enqueued} bytes is short of offered {offered} bytes (enqueue blocked {}s)",
            blocked_label(blocked_us)
        )));
    }
}

/// Offered serve ops the driver did not finish.
///
/// One op of slack covers a read that straddles the step boundary. A step
/// with `scheduled_serve == 0` has no serve target.
fn short_of_serve(ops: u64, offered: u64, reasons: &mut Vec<Finding>) {
    if offered > 0 && ops.saturating_add(1) < offered {
        reasons.push(Finding::soft(format!(
            "serve {}/{}",
            fmt_count(ops),
            fmt_count(offered)
        )));
    }
}

/// `9600` is `9.6k`. A multiple of 1000 drops the decimal.
fn fmt_count(n: u64) -> String {
    if n < 1_000 {
        return n.to_string();
    }
    let whole = n / 1_000;
    let tenth = (n % 1_000) / 100;
    if n.is_multiple_of(1_000) {
        format!("{whole}k")
    } else {
        format!("{whole}.{tenth}k")
    }
}

/// Bytes the module accepted that were not durable after the drain.
fn short_of_enqueued(
    name: &str,
    enqueued: u64,
    durable: u64,
    chunk_size: u64,
    reasons: &mut Vec<Finding>,
) {
    if enqueued > durable.saturating_add(chunk_size) {
        reasons.push(Finding::hard(format!(
            "{name} durable {durable} bytes is short of enqueued {enqueued} bytes"
        )));
    }
}

fn blocked_label(micros: u64) -> String {
    let whole = micros / 1_000_000;
    let millis = (micros % 1_000_000) / 1_000;
    format!("{whole}.{millis:03}")
}

/// One scored second. The tail judge folds the last N of these.
/// Drain completions are not in this list.
#[derive(Debug, Clone, Default)]
pub(crate) struct SecondView {
    pub recall_ops: u64,
    pub recall_skips: u64,
    pub recall_start_us: Vec<u64>,
    pub recall_lat_us: Vec<u64>,
    pub ingest_enqueued: u64,
    pub ingest_durable: u64,
    pub ingest_blocked_us: u64,
    pub migration_enqueued: u64,
    pub migration_durable: u64,
    pub migration_blocked_us: u64,
    pub serve_ops: u64,
    pub pending_bytes: u64,
    pub overloaded: bool,
}

/// Last N scored seconds, judged apart from the step mean.
#[derive(Debug, Clone)]
pub(crate) struct TailWindow {
    pub secs: u64,
    pub measured: StepMeasured,
    pub recall_start_max_us: u64,
    pub passed: bool,
    pub findings: Vec<Finding>,
}

/// `tail_secs == 0` or an empty step returns `None`.
/// A window longer than the step uses every scored second.
pub(crate) fn tail_window(
    views: &[SecondView],
    tail_secs: u64,
    rates: &Rates,
    chunk_size: u64,
    ceiling: u64,
    flush_batch: u64,
    rule: &RecallRule,
) -> Option<TailWindow> {
    if tail_secs == 0 || views.is_empty() {
        return None;
    }
    let take = usize::try_from(tail_secs).unwrap_or(usize::MAX);
    let start = views.len().saturating_sub(take);
    let slice = &views[start..];
    let secs = u64::try_from(slice.len()).unwrap_or(u64::MAX);
    let mut recall_ops = 0_u64;
    let mut recall_skips = 0_u64;
    let mut starts = Vec::new();
    let mut lats = Vec::new();
    let mut start_max = 0_u64;
    let mut ingest_enqueued = 0_u64;
    let mut ingest_durable = 0_u64;
    let mut ingest_blocked = 0_u64;
    let mut migration_enqueued = 0_u64;
    let mut migration_durable = 0_u64;
    let mut migration_blocked = 0_u64;
    let mut serve_ops = 0_u64;
    let mut pending = Vec::with_capacity(slice.len());
    let mut overloaded = 0_u64;
    for view in slice {
        recall_ops = recall_ops.saturating_add(view.recall_ops);
        recall_skips = recall_skips.saturating_add(view.recall_skips);
        starts.extend_from_slice(&view.recall_start_us);
        lats.extend_from_slice(&view.recall_lat_us);
        for sample in &view.recall_start_us {
            start_max = start_max.max(*sample);
        }
        ingest_enqueued = ingest_enqueued.saturating_add(view.ingest_enqueued);
        ingest_durable = ingest_durable.saturating_add(view.ingest_durable);
        ingest_blocked = ingest_blocked.saturating_add(view.ingest_blocked_us);
        migration_enqueued = migration_enqueued.saturating_add(view.migration_enqueued);
        migration_durable = migration_durable.saturating_add(view.migration_durable);
        migration_blocked = migration_blocked.saturating_add(view.migration_blocked_us);
        serve_ops = serve_ops.saturating_add(view.serve_ops);
        pending.push(view.pending_bytes);
        if view.overloaded {
            overloaded = overloaded.saturating_add(1);
        }
    }
    let measured = StepMeasured {
        secs,
        scheduled_ingest: mib_bytes(rates.ingest_mib_s, secs),
        durable_ingest: ingest_durable,
        enqueued_ingest: ingest_enqueued,
        scheduled_migration: mib_bytes(rates.migration_mib_s, secs),
        durable_migration: migration_durable,
        enqueued_migration: migration_enqueued,
        serve_ops,
        scheduled_serve: rates.serve_per_sec.saturating_mul(secs),
        chunk_size,
        recall_ops,
        recall_skips,
        recall_start_p99_us: percentile(&starts, 99),
        recall_lat_p99_us: percentile(&lats, 99),
        ingest_enqueue_blocked_us: ingest_blocked,
        migration_enqueue_blocked_us: migration_blocked,
        overloaded_secs: overloaded,
        pending_samples: pending,
        pending_after_drain: 0,
        migration_jobs_after_drain: 0,
        ceiling,
        flush_batch,
        aborted: None,
    };
    let findings = judge_tail(&measured, rule);
    let passed = findings.is_empty();
    Some(TailWindow {
        secs,
        measured,
        recall_start_max_us: start_max,
        passed,
        findings,
    })
}

/// Add tail misses when the window is a proper suffix of the step.
/// A window that already covers every scored second is left out, so the
/// step reasons are not repeated.
pub(crate) fn merge_tail(
    mut findings: Vec<Finding>,
    tail: &TailWindow,
    whole_secs: u64,
) -> Vec<Finding> {
    if tail.secs >= whole_secs {
        return findings;
    }
    for finding in &tail.findings {
        findings.push(Finding {
            message: format!("tail: {}", finding.message),
            soft: finding.soft,
        });
    }
    findings
}

/// Steady-state checks for the tail window.
///
/// Recall skips, recall p99, the enqueue-versus-offer gap, the serve-rate
/// gap, and the overloaded-second rule match `judge`. Durable bytes may lag
/// the enqueue inside the window, because the drain is outside it, so a
/// durable shortfall is not a tail failure. Aborts, leftover migration rows,
/// and bytes still pending after the drain stay on the whole-step judge.
fn judge_tail(measured: &StepMeasured, rule: &RecallRule) -> Vec<Finding> {
    let mut reasons = Vec::new();
    if measured.recall_skips > 0 {
        reasons.push(Finding::hard(format!(
            "skipped {} recall(s) (start lag >= {RECALL_SKIP_LAG_STEPS} steps)",
            measured.recall_skips
        )));
    }
    recall_latency(measured, rule, &mut reasons);
    short_of_offered(
        "ingest",
        measured.scheduled_ingest,
        measured.enqueued_ingest,
        measured.chunk_size,
        measured.ingest_enqueue_blocked_us,
        &mut reasons,
    );
    short_of_offered(
        "migration",
        measured.scheduled_migration,
        measured.enqueued_migration,
        measured.chunk_size,
        measured.migration_enqueue_blocked_us,
        &mut reasons,
    );
    short_of_serve(measured.serve_ops, measured.scheduled_serve, &mut reasons);
    if measured.secs >= 5 && measured.overloaded_secs.saturating_mul(5) > measured.secs {
        reasons.push(Finding::hard(format!(
            "overloaded for {}s of {}s",
            measured.overloaded_secs, measured.secs
        )));
    }
    reasons
}

fn backlog_grew(samples: &[u64], flush_batch: u64) -> bool {
    if samples.len() < 30 {
        return false;
    }
    let quarter = samples.len() / 4;
    if quarter == 0 {
        return false;
    }
    let first = median(&samples[..quarter]);
    let last = median(&samples[samples.len() - quarter..]);
    last > first.saturating_add(flush_batch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_periods_match_the_rates() {
        assert_eq!(period_ns(1, 1), Some(1_000_000_000));
        assert_eq!(period_ns(9, 5), Some(555_555_555));
        assert_eq!(period_ns(0, 1), None);
    }

    #[test]
    fn percentile_uses_nearest_rank() {
        let samples = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100];
        assert_eq!(percentile(&samples, 50), 50);
        assert_eq!(percentile(&samples, 99), 100);
        assert_eq!(percentile(&[], 99), 0);
    }

    fn base() -> StepMeasured {
        StepMeasured {
            secs: 600,
            scheduled_ingest: 0,
            durable_ingest: 0,
            enqueued_ingest: 0,
            scheduled_migration: 0,
            durable_migration: 0,
            enqueued_migration: 0,
            serve_ops: 0,
            scheduled_serve: 0,
            chunk_size: 262_144,
            recall_ops: 10,
            recall_skips: 0,
            recall_start_p99_us: 1_000,
            recall_lat_p99_us: 1_000,
            ingest_enqueue_blocked_us: 0,
            migration_enqueue_blocked_us: 0,
            overloaded_secs: 0,
            pending_samples: vec![0; 40],
            pending_after_drain: 0,
            migration_jobs_after_drain: 0,
            ceiling: 1_000_000,
            flush_batch: 100,
            aborted: None,
        }
    }

    fn absolute() -> RecallRule {
        RecallRule {
            bar: RecallBar::Absolute,
            delay_margin_us: 200_000,
            latency_margin_us: 500_000,
        }
    }

    fn messages(findings: &[Finding]) -> Vec<&str> {
        findings
            .iter()
            .map(|finding| finding.message.as_str())
            .collect()
    }

    #[test]
    fn judge_flags_each_hard_failure() {
        let rule = absolute();
        assert!(judge(&base(), &rule).is_empty());
        let mut skipped = base();
        skipped.recall_skips = 1;
        let skipped = judge(&skipped, &rule);
        assert!(skipped.iter().any(|finding| !finding.soft));
        assert!(messages(&skipped)[0].contains("skipped"));
        let mut overloaded = base();
        overloaded.secs = 10;
        overloaded.overloaded_secs = 3;
        assert!(
            judge(&overloaded, &rule)
                .iter()
                .any(|finding| !finding.soft)
        );
        let mut held = base();
        held.pending_after_drain = held.ceiling + 1;
        assert!(judge(&held, &rule).iter().any(|finding| !finding.soft));
    }

    #[test]
    fn absolute_recall_ceilings_apply_without_a_baseline() {
        let rule = absolute();
        let mut late = base();
        late.recall_start_p99_us = 200_000;
        late.recall_lat_p99_us = 500_000;
        assert!(judge(&late, &rule).is_empty());
        late.recall_start_p99_us = 200_001;
        let start = judge(&late, &rule);
        assert!(start.iter().all(|finding| finding.soft));
        assert!(messages(&start)[0].contains("start-delay"));
        late.recall_start_p99_us = 1_000;
        late.recall_lat_p99_us = 500_001;
        let latency = judge(&late, &rule);
        assert!(latency.iter().all(|finding| finding.soft));
        assert!(messages(&latency)[0].contains("latency"));
    }

    #[test]
    fn idle_baseline_ignores_absolute_recall_ceilings() {
        let rule = RecallRule {
            bar: RecallBar::Baseline,
            delay_margin_us: 200_000,
            latency_margin_us: 500_000,
        };
        let mut idle = base();
        idle.recall_start_p99_us = 1_000_000;
        idle.recall_lat_p99_us = 2_000_000;
        assert!(judge(&idle, &rule).is_empty());
        idle.recall_skips = 2;
        assert!(judge(&idle, &rule).iter().any(|finding| !finding.soft));
    }

    #[test]
    fn relative_recall_uses_the_idle_margin() {
        let rule = RecallRule {
            bar: RecallBar::Relative {
                start_p99_us: 190_000,
                lat_p99_us: 1_146_000,
            },
            delay_margin_us: 200_000,
            latency_margin_us: 500_000,
        };
        let mut step = base();
        step.recall_start_p99_us = 390_000;
        step.recall_lat_p99_us = 1_646_000;
        assert!(
            judge(&step, &rule).is_empty(),
            "equal to idle p99 plus the margin passes"
        );
        step.recall_start_p99_us = 390_001;
        let start = judge(&step, &rule);
        assert_eq!(start.len(), 1);
        assert!(start[0].soft);
        assert!(start[0].message.contains("idle p99 190000"));
        assert!(start[0].message.contains("200000"));
        step.recall_start_p99_us = 190_000;
        step.recall_lat_p99_us = 1_646_001;
        let latency = judge(&step, &rule);
        assert_eq!(latency.len(), 1);
        assert!(latency[0].soft);
        assert!(latency[0].message.contains("idle p99 1146000"));
    }

    #[test]
    fn enqueue_shortfall_is_distinct_from_a_durability_backlog() {
        let rule = absolute();
        let mut short = base();
        short.scheduled_ingest = 1_000_000;
        short.enqueued_ingest = 100;
        short.durable_ingest = 100;
        short.ingest_enqueue_blocked_us = 1_500_000;
        let findings = judge(&short, &rule);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].soft);
        assert!(findings[0].message.contains("short of offered"));
        assert!(findings[0].message.contains("1.500s"));
        assert!(!stop_for_score(&findings, true));

        let mut backlog = base();
        backlog.scheduled_ingest = 1_000_000;
        backlog.enqueued_ingest = 1_000_000;
        backlog.durable_ingest = 100;
        let findings = judge(&backlog, &rule);
        assert_eq!(findings.len(), 1);
        assert!(!findings[0].soft);
        assert!(findings[0].message.contains("short of enqueued"));
        assert!(stop_for_score(&findings, true));
        assert!(!stop_for_score(&findings, false));

        let mut both = base();
        both.scheduled_ingest = 1_000_000;
        both.enqueued_ingest = 400_000;
        both.durable_ingest = 100;
        let findings = judge(&both, &rule);
        assert_eq!(findings.len(), 2);
        assert!(findings[0].soft);
        assert!(!findings[1].soft);
        assert!(stop_for_score(&findings, true));
    }

    #[test]
    fn one_chunk_of_slack_is_not_a_shortfall() {
        let rule = absolute();
        let mut step = base();
        step.scheduled_ingest = 1_000_000;
        step.chunk_size = 262_144;
        step.enqueued_ingest = 1_000_000 - 262_144;
        step.durable_ingest = step.enqueued_ingest;
        assert!(judge(&step, &rule).is_empty());
        step.enqueued_ingest -= 1;
        assert!(judge(&step, &rule).iter().any(|finding| finding.soft));
    }

    fn mib(mib_s: u64) -> u64 {
        mib_s * 1024 * 1024
    }

    fn quiet_second(mib_s: u64, start_us: u64) -> SecondView {
        SecondView {
            recall_ops: 1,
            recall_start_us: vec![start_us],
            recall_lat_us: vec![1_000],
            ingest_enqueued: mib(mib_s),
            ingest_durable: mib(mib_s),
            ..SecondView::default()
        }
    }

    fn rates_at(ingest_mib_s: u64) -> Rates {
        Rates {
            recall_num: 1,
            recall_den: 1,
            ingest_mib_s,
            migration_mib_s: 0,
            serve_per_sec: 0,
            serve_seq_pct: 50,
        }
    }

    #[test]
    fn a_late_collapse_fails_the_tail_when_the_mean_holds() {
        let rule = absolute();
        let rates = rates_at(24);
        let mut views = Vec::new();
        for sec in 0..600 {
            let mib_s = if sec < 360 { 32 } else { 12 };
            views.push(quiet_second(mib_s, 1_000));
        }
        let enqueued: u64 = views.iter().map(|view| view.ingest_enqueued).sum();
        let mut whole = base();
        whole.secs = 600;
        whole.scheduled_ingest = mib_bytes(24, 600);
        whole.enqueued_ingest = enqueued;
        whole.durable_ingest = enqueued;
        whole.recall_ops = 600;
        whole.recall_start_p99_us = 1_000;
        whole.recall_lat_p99_us = 1_000;
        whole.pending_samples = vec![0; 600];
        assert!(
            judge(&whole, &rule).is_empty(),
            "360s at 32 MiB/s and 240s at 12 MiB/s average 24"
        );
        let tail = tail_window(&views, 240, &rates, 262_144, 1_000_000, 100, &rule).unwrap();
        assert_eq!(tail.secs, 240);
        assert!(!tail.passed);
        assert!(tail.findings.iter().all(|finding| finding.soft));
        assert!(tail.findings[0].message.contains("short of offered"));
        let merged = merge_tail(Vec::new(), &tail, 600);
        assert!(merged[0].message.starts_with("tail: "));
        assert!(!stop_for_score(&merged, true));
    }

    #[test]
    fn tail_start_delay_fails_when_the_step_p99_does_not() {
        let rule = RecallRule {
            bar: RecallBar::Relative {
                start_p99_us: 1_000,
                lat_p99_us: 1_000,
            },
            delay_margin_us: 200_000,
            latency_margin_us: 500_000,
        };
        let rates = rates_at(0);
        let mut views = Vec::new();
        for sec in 0..600 {
            let start = if sec >= 596 { 10_000_000 } else { 1_000 };
            views.push(quiet_second(0, start));
        }
        let mut starts = Vec::new();
        for view in &views {
            starts.extend_from_slice(&view.recall_start_us);
        }
        assert_eq!(percentile(&starts, 99), 1_000);
        let tail = tail_window(&views, 240, &rates, 262_144, 1, 1, &rule).unwrap();
        assert!(tail.measured.recall_start_p99_us > 200_000);
        assert_eq!(tail.recall_start_max_us, 10_000_000);
        assert!(!tail.passed);
        assert!(tail.findings[0].message.contains("start-delay"));
    }

    #[test]
    fn tail_durable_lag_is_not_a_failure() {
        let rule = absolute();
        let rates = rates_at(16);
        let mut views = Vec::new();
        for _ in 0..240 {
            let mut view = quiet_second(16, 1_000);
            view.ingest_durable = 0;
            views.push(view);
        }
        let tail = tail_window(&views, 240, &rates, 262_144, 1, 1, &rule).unwrap();
        assert!(tail.passed);
        assert_eq!(tail.measured.durable_ingest, 0);
        assert_eq!(tail.measured.enqueued_ingest, mib_bytes(16, 240));
    }

    #[test]
    fn a_window_that_covers_the_step_is_not_merged_twice() {
        let rule = absolute();
        let rates = rates_at(8);
        let views = vec![quiet_second(1, 1_000); 8];
        let tail = tail_window(&views, 240, &rates, 262_144, 1, 1, &rule).unwrap();
        assert_eq!(tail.secs, 8);
        assert!(!tail.passed);
        let merged = merge_tail(Vec::new(), &tail, 8);
        assert!(merged.is_empty());
    }

    #[test]
    fn tail_skips_are_hard_and_zero_disables_the_window() {
        let rule = absolute();
        let rates = rates_at(0);
        assert!(tail_window(&[], 240, &rates, 1, 1, 1, &rule).is_none());
        let mut view = quiet_second(0, 1_000);
        view.recall_skips = 3;
        let views = vec![view; 300];
        assert!(tail_window(&views, 0, &rates, 1, 1, 1, &rule).is_none());
        let tail = tail_window(&views, 240, &rates, 1, 1, 1, &rule).unwrap();
        assert!(!tail.passed);
        assert!(tail.findings.iter().any(|finding| !finding.soft));
        let merged = merge_tail(Vec::new(), &tail, 300);
        assert!(stop_for_score(&merged, true));
    }

    fn warm_step() -> Step {
        Step {
            name: "warm".to_string(),
            secs: 60,
            rates: Rates {
                recall_num: 1,
                recall_den: 1,
                ingest_mib_s: 0,
                migration_mib_s: 0,
                serve_per_sec: 0,
                serve_seq_pct: 50,
            },
        }
    }

    fn load_step() -> Step {
        Step {
            name: "load".to_string(),
            secs: 900,
            rates: Rates {
                recall_num: 1,
                recall_den: 1,
                ingest_mib_s: 16,
                migration_mib_s: 0,
                serve_per_sec: 20,
                serve_seq_pct: 50,
            },
        }
    }

    #[test]
    fn recall_only_warm_step_passes_its_own_targets() {
        // Breaks if a recall-only warm step is judged on the absolute ceilings.
        let bar = recall_bar(&warm_step(), None);
        assert_eq!(bar, RecallBar::Baseline);
        let rule = RecallRule {
            bar,
            delay_margin_us: 200_000,
            latency_margin_us: 500_000,
        };
        let mut measured = base();
        measured.secs = 60;
        measured.recall_ops = 60;
        measured.recall_skips = 0;
        measured.recall_start_p99_us = 800_000;
        measured.recall_lat_p99_us = 700_000;
        measured.scheduled_serve = 0;
        measured.serve_ops = 0;
        assert!(
            judge(&measured, &rule).is_empty(),
            "skips=0 and no serve target is a pass"
        );
        let later = recall_bar(&load_step(), Some((800_000, 700_000)));
        assert_eq!(
            later,
            RecallBar::Relative {
                start_p99_us: 800_000,
                lat_p99_us: 700_000,
            }
        );
        assert_eq!(recall_bar(&load_step(), None), RecallBar::Absolute);
    }

    #[test]
    fn serve_shortfall_names_the_gap_and_a_zero_target_does_not_fail() {
        // Breaks if serve ops are ignored, or if a step with no serve rate fails.
        let rule = absolute();
        let mut quiet = base();
        quiet.scheduled_serve = 0;
        quiet.serve_ops = 0;
        assert!(judge(&quiet, &rule).is_empty());

        let mut hit = base();
        hit.secs = 900;
        hit.scheduled_serve = 18_000;
        hit.serve_ops = 18_000;
        assert!(judge(&hit, &rule).is_empty());

        let mut one_short = hit.clone();
        one_short.serve_ops = 17_999;
        assert!(
            judge(&one_short, &rule).is_empty(),
            "one op of slack covers the last interval"
        );

        let mut miss = hit;
        miss.serve_ops = 9_600;
        let findings = judge(&miss, &rule);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].soft);
        assert_eq!(findings[0].message, "serve 9.6k/18k");
        assert!(!stop_for_score(&findings, true));

        let mut view = quiet_second(0, 1_000);
        view.serve_ops = 10;
        let views = vec![view; 240];
        let mut rates = rates_at(0);
        rates.serve_per_sec = 20;
        let tail = tail_window(&views, 240, &rates, 262_144, 1, 1, &rule).unwrap();
        assert!(!tail.passed);
        assert_eq!(tail.findings.len(), 1);
        assert_eq!(tail.findings[0].message, "serve 2.4k/4.8k");
    }
}
