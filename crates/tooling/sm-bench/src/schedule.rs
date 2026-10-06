//! Step schedule, presets, and the pass/fail judge.
//!
//! Rates are integer ratios. `1.8/s` is `9/5`, which is a 555_555_555 ns period.

use std::path::Path;

use eyre::{WrapErr as _, eyre};
use serde::Deserialize;

pub(crate) const RECALL_SKIP_LAG_STEPS: u128 = 3;
pub(crate) const RECALL_START_P99_LIMIT_US: u64 = 200_000;

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
            "unknown preset {other}; expected recall-1, recall-1.8, or smoke"
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
    pub chunk_size: u64,
    pub recall_ops: u64,
    pub recall_skips: u64,
    pub recall_start_p99_us: u64,
    pub overloaded_secs: u64,
    pub pending_samples: Vec<u64>,
    pub pending_after_drain: u64,
    pub migration_jobs_after_drain: u64,
    pub ceiling: u64,
    pub flush_batch: u64,
    pub aborted: Option<String>,
}

pub(crate) fn judge(measured: &StepMeasured) -> Vec<String> {
    let mut reasons = Vec::new();
    if let Some(reason) = &measured.aborted {
        reasons.push(format!("aborted: {reason}"));
    }
    if measured.recall_skips > 0 {
        reasons.push(format!(
            "skipped {} recall(s) (start lag >= {RECALL_SKIP_LAG_STEPS} steps)",
            measured.recall_skips
        ));
    }
    if measured.recall_ops > 0 && measured.recall_start_p99_us > RECALL_START_P99_LIMIT_US {
        reasons.push(format!(
            "recall start-delay p99 {} us > {RECALL_START_P99_LIMIT_US} us",
            measured.recall_start_p99_us
        ));
    }
    short_durable(
        "ingest",
        measured.scheduled_ingest,
        measured.enqueued_ingest,
        measured.durable_ingest,
        measured.chunk_size,
        &mut reasons,
    );
    short_durable(
        "migration",
        measured.scheduled_migration,
        measured.enqueued_migration,
        measured.durable_migration,
        measured.chunk_size,
        &mut reasons,
    );
    if measured.secs >= 5 && measured.overloaded_secs.saturating_mul(5) > measured.secs {
        reasons.push(format!(
            "overloaded for {}s of {}s",
            measured.overloaded_secs, measured.secs
        ));
    }
    if measured.pending_after_drain > measured.ceiling {
        reasons.push(format!(
            "pending {} bytes after drain is above the ceiling {}",
            measured.pending_after_drain, measured.ceiling
        ));
    }
    if backlog_grew(&measured.pending_samples, measured.flush_batch)
        && measured.pending_after_drain > measured.flush_batch
    {
        reasons.push(
            "pending-write backlog grew through the step and was still high after drain"
                .to_string(),
        );
    }
    if measured.migration_jobs_after_drain > 0 {
        reasons.push(format!(
            "{} body-migration row(s) remained after drain",
            measured.migration_jobs_after_drain
        ));
    }
    reasons
}

fn short_durable(
    name: &str,
    scheduled: u64,
    enqueued: u64,
    durable: u64,
    chunk_size: u64,
    reasons: &mut Vec<String>,
) {
    if scheduled > 0 && durable.saturating_add(chunk_size) < scheduled {
        reasons.push(format!(
            "{name} durable {durable} bytes is short of offered {scheduled} bytes"
        ));
    }
    if enqueued > durable.saturating_add(chunk_size) {
        reasons.push(format!(
            "{name} durable {durable} bytes is short of enqueued {enqueued} bytes"
        ));
    }
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
            chunk_size: 262_144,
            recall_ops: 10,
            recall_skips: 0,
            recall_start_p99_us: 1_000,
            overloaded_secs: 0,
            pending_samples: vec![0; 40],
            pending_after_drain: 0,
            migration_jobs_after_drain: 0,
            ceiling: 1_000_000,
            flush_batch: 100,
            aborted: None,
        }
    }

    #[test]
    fn judge_flags_each_failure() {
        assert!(judge(&base()).is_empty());
        let mut skipped = base();
        skipped.recall_skips = 1;
        assert!(!judge(&skipped).is_empty());
        let mut late = base();
        late.recall_start_p99_us = 200_001;
        assert!(!judge(&late).is_empty());
        let mut short = base();
        short.scheduled_ingest = 1_000_000;
        short.durable_ingest = 100;
        short.enqueued_ingest = 100;
        assert!(!judge(&short).is_empty());
        let mut overloaded = base();
        overloaded.secs = 10;
        overloaded.overloaded_secs = 3;
        assert!(!judge(&overloaded).is_empty());
        let mut held = base();
        held.pending_after_drain = held.ceiling + 1;
        assert!(!judge(&held).is_empty());
    }
}
