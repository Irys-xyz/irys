use std::fmt;

use tracing::{
    Event, Level, Subscriber,
    field::{Field, Visit},
    span::{Attributes, Id},
};
use tracing_subscriber::{Layer, layer::Context, registry::LookupSpan};

pub const MDBX_RW_TX_LOCK_STALLS_TOTAL: &str = "libmdbx.rw_tx_lock_stalls_total";

/// Histogram name for libmdbx writer-tx acquisition latency (seconds),
/// labelled by `scope`. Recorded by `IrysDatabaseExt::update_eyre` for both
/// the consensus and EVM databases.
///
/// Bucket boundaries come from the global prometheus recorder's defaults;
/// per-metric tuning would require configuring the recorder at install time,
/// not via the [`metrics::describe_histogram!`] macro.
pub const DB_TX_MUT_ACQUIRE_DURATION_SECONDS: &str = "db.tx_mut_acquire_duration_seconds";

/// Span field name read by [`MdbxLockMetricsLayer`] to attribute a stall to a
/// particular database. Callers should set this field on a parent span using
/// one of the `DB_SCOPE_*` constants below.
pub const DB_SCOPE_FIELD: &str = "db_scope";

/// Canonical scope label for the Irys consensus database.
pub const DB_SCOPE_IRYS_CONSENSUS: &str = "irys-consensus";

/// Canonical scope label for the Irys cache database (sibling MDBX env
/// holding cache tables such as `CachedChunks` and `IngressProofs`).
pub const DB_SCOPE_IRYS_CACHE: &str = "irys-cache";

/// Canonical scope label for the Reth EVM database.
pub const DB_SCOPE_RETH_EVM: &str = "reth-evm";

/// Fallback scope used when no parent span carries [`DB_SCOPE_FIELD`].
pub const DB_SCOPE_UNKNOWN: &str = "unknown";

/// Span name used by callers to wrap rw-tx acquisitions for stall attribution.
/// Must be paired with a `db_scope` field set to one of the `DB_SCOPE_*` constants.
pub const MDBX_RW_TX_SPAN: &str = "mdbx_rw_tx";

const LIBMDBX_TARGET: &str = "libmdbx";
/// Prefix matched against `target="libmdbx"` WARN messages to recognise
/// writer-lock stalls. Matched with `starts_with` so trailing punctuation,
/// retry counters, or future suffix changes in the upstream wording don't
/// silently drop the counter to zero.
const RW_TX_LOCK_STALL_MESSAGE: &str = "Process stalled, awaiting read-write transaction lock";

/// Tracing [`Layer`] that converts upstream `target="libmdbx"` writer-lock
/// stall warnings into a [`metrics`] counter, attributed to a database scope
/// pulled from the active span context.
///
/// MDBX raises `MDBX_BUSY` from the C library boundary, so the warn event
/// itself carries no DB identity. Callers wrap their `Database::tx_mut` /
/// `Database::update` entrypoints in a span carrying the [`DB_SCOPE_FIELD`]
/// field; this layer reads that field on span creation, stashes it in the
/// span's extensions, and looks it up when the warn event fires. Stalls
/// emitted outside any such span are tagged [`DB_SCOPE_UNKNOWN`].
#[derive(Debug)]
pub struct MdbxLockMetricsLayer;

#[must_use]
pub const fn mdbx_lock_metrics_layer() -> MdbxLockMetricsLayer {
    MdbxLockMetricsLayer
}

pub(crate) fn describe_mdbx_metrics() {
    metrics::describe_counter!(
        MDBX_RW_TX_LOCK_STALLS_TOTAL,
        "libmdbx read-write transaction lock stall warnings, attributed by db_scope"
    );
    metrics::describe_histogram!(
        DB_TX_MUT_ACQUIRE_DURATION_SECONDS,
        metrics::Unit::Seconds,
        "Time spent acquiring a libmdbx writer transaction via begin_rw_txn, attributed by scope. Both successful and failed acquires are recorded so genuinely-slow-then-failed waits are visible."
    );
}

#[derive(Debug, Clone, Copy)]
struct DbScopeExt(&'static str);

impl<S> Layer<S> for MdbxLockMetricsLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut visitor = DbScopeFieldVisitor::default();
        attrs.record(&mut visitor);
        if let Some(scope) = visitor.scope
            && let Some(span) = ctx.span(id)
        {
            span.extensions_mut().insert(DbScopeExt(scope));
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        if !is_mdbx_rw_tx_lock_stall(event) {
            return;
        }
        let scope = lookup_db_scope(event, &ctx).unwrap_or(DB_SCOPE_UNKNOWN);
        record_mdbx_rw_tx_lock_stall(scope);
    }
}

fn lookup_db_scope<S>(event: &Event<'_>, ctx: &Context<'_, S>) -> Option<&'static str>
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    // Iterate leaf-to-root so the innermost db_scope wins on nested spans.
    ctx.event_scope(event)?
        .find_map(|span| span.extensions().get::<DbScopeExt>().map(|ext| ext.0))
}

fn record_mdbx_rw_tx_lock_stall(scope: &'static str) {
    metrics::counter!(MDBX_RW_TX_LOCK_STALLS_TOTAL, "scope" => scope).increment(1);
}

fn is_mdbx_rw_tx_lock_stall(event: &Event<'_>) -> bool {
    let metadata = event.metadata();
    if metadata.target() != LIBMDBX_TARGET || *metadata.level() != Level::WARN {
        return false;
    }

    let mut visitor = LockStallMessageVisitor::default();
    event.record(&mut visitor);
    visitor.matched
}

#[derive(Default)]
struct LockStallMessageVisitor {
    matched: bool,
}

impl Visit for LockStallMessageVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.matched = is_lock_stall_message(value);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.matched = debug_message_matches(value);
        }
    }
}

fn is_lock_stall_message(value: &str) -> bool {
    value.starts_with(RW_TX_LOCK_STALL_MESSAGE)
}

fn debug_message_matches(value: &dyn fmt::Debug) -> bool {
    let formatted = format!("{value:?}");
    // Some recording paths route &str through Debug, which surrounds the
    // value with quotes. Strip a single matching pair before matching.
    let trimmed = formatted
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(&formatted);
    is_lock_stall_message(trimmed)
}

#[derive(Default)]
struct DbScopeFieldVisitor {
    scope: Option<&'static str>,
}

impl Visit for DbScopeFieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == DB_SCOPE_FIELD {
            self.scope = canonical_scope(value);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() != DB_SCOPE_FIELD {
            return;
        }
        // Some recording paths route &str through Debug, which surrounds the
        // value with quotes. Strip a single matching pair before canonicalising.
        let formatted = format!("{value:?}");
        let trimmed = formatted
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(&formatted);
        self.scope = canonical_scope(trimmed);
    }
}

fn canonical_scope(value: &str) -> Option<&'static str> {
    match value {
        DB_SCOPE_IRYS_CONSENSUS => Some(DB_SCOPE_IRYS_CONSENSUS),
        DB_SCOPE_IRYS_CACHE => Some(DB_SCOPE_IRYS_CACHE),
        DB_SCOPE_RETH_EVM => Some(DB_SCOPE_RETH_EVM),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    };

    use metrics::{
        Counter, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString, Unit,
    };
    use tracing::info_span;
    use tracing_subscriber::{Registry, layer::SubscriberExt as _};

    use super::*;

    #[test]
    fn layer_counts_only_libmdbx_rw_lock_stall_warnings() {
        let recorder = StallCounterRecorder::default();
        let subscriber = Registry::default().with(mdbx_lock_metrics_layer());

        metrics::with_local_recorder(&recorder, || {
            tracing::subscriber::with_default(subscriber, || {
                tracing::warn!(
                    target: "libmdbx",
                    "Process stalled, awaiting read-write transaction lock."
                );
                tracing::warn!(
                    target: "libmdbx",
                    "Process stalled, awaiting read-write transaction lock"
                );
                // Future suffix variants — prefix match keeps these counted.
                tracing::warn!(
                    target: "libmdbx",
                    "Process stalled, awaiting read-write transaction lock!"
                );
                tracing::warn!(
                    target: "libmdbx",
                    "Process stalled, awaiting read-write transaction lock (retry 1)"
                );
                tracing::warn!(target: "libmdbx", "unrelated warning");
                tracing::info!(
                    target: "libmdbx",
                    "Process stalled, awaiting read-write transaction lock."
                );
                tracing::warn!(
                    target: "irys",
                    "Process stalled, awaiting read-write transaction lock."
                );
            });
        });

        // No parent span carries db_scope → counter increments under "unknown".
        assert_eq!(recorder.scope_count(DB_SCOPE_UNKNOWN), 4);
        assert_eq!(recorder.scope_count(DB_SCOPE_IRYS_CONSENSUS), 0);
        assert_eq!(recorder.scope_count(DB_SCOPE_RETH_EVM), 0);
    }

    #[test]
    fn layer_attributes_consensus_db_stalls_via_span_field() {
        let recorder = StallCounterRecorder::default();
        let subscriber = Registry::default().with(mdbx_lock_metrics_layer());

        metrics::with_local_recorder(&recorder, || {
            tracing::subscriber::with_default(subscriber, || {
                let span = info_span!("op", db_scope = DB_SCOPE_IRYS_CONSENSUS);
                let _enter = span.enter();
                tracing::warn!(
                    target: "libmdbx",
                    "Process stalled, awaiting read-write transaction lock."
                );
            });
        });

        assert_eq!(recorder.scope_count(DB_SCOPE_IRYS_CONSENSUS), 1);
        assert_eq!(recorder.scope_count(DB_SCOPE_UNKNOWN), 0);
    }

    #[test]
    fn layer_attributes_cache_db_stalls_via_span_field() {
        let recorder = StallCounterRecorder::default();
        let subscriber = Registry::default().with(mdbx_lock_metrics_layer());

        metrics::with_local_recorder(&recorder, || {
            tracing::subscriber::with_default(subscriber, || {
                let span = info_span!("op", db_scope = DB_SCOPE_IRYS_CACHE);
                let _enter = span.enter();
                tracing::warn!(
                    target: "libmdbx",
                    "Process stalled, awaiting read-write transaction lock."
                );
            });
        });

        assert_eq!(recorder.scope_count(DB_SCOPE_IRYS_CACHE), 1);
        assert_eq!(recorder.scope_count(DB_SCOPE_UNKNOWN), 0);
    }

    #[test]
    fn layer_attributes_evm_db_stalls_via_span_field() {
        let recorder = StallCounterRecorder::default();
        let subscriber = Registry::default().with(mdbx_lock_metrics_layer());

        metrics::with_local_recorder(&recorder, || {
            tracing::subscriber::with_default(subscriber, || {
                let span = info_span!("op", db_scope = DB_SCOPE_RETH_EVM);
                let _enter = span.enter();
                tracing::warn!(
                    target: "libmdbx",
                    "Process stalled, awaiting read-write transaction lock."
                );
            });
        });

        assert_eq!(recorder.scope_count(DB_SCOPE_RETH_EVM), 1);
        assert_eq!(recorder.scope_count(DB_SCOPE_UNKNOWN), 0);
    }

    #[test]
    fn layer_innermost_span_wins_when_nested() {
        let recorder = StallCounterRecorder::default();
        let subscriber = Registry::default().with(mdbx_lock_metrics_layer());

        metrics::with_local_recorder(&recorder, || {
            tracing::subscriber::with_default(subscriber, || {
                let outer = info_span!("outer", db_scope = DB_SCOPE_IRYS_CONSENSUS);
                let _outer = outer.enter();
                let inner = info_span!("inner", db_scope = DB_SCOPE_RETH_EVM);
                let _inner = inner.enter();
                tracing::warn!(
                    target: "libmdbx",
                    "Process stalled, awaiting read-write transaction lock."
                );
            });
        });

        assert_eq!(recorder.scope_count(DB_SCOPE_RETH_EVM), 1);
        assert_eq!(recorder.scope_count(DB_SCOPE_IRYS_CONSENSUS), 0);
    }

    #[test]
    fn layer_ignores_unrecognised_scope_values() {
        let recorder = StallCounterRecorder::default();
        let subscriber = Registry::default().with(mdbx_lock_metrics_layer());

        metrics::with_local_recorder(&recorder, || {
            tracing::subscriber::with_default(subscriber, || {
                let span = info_span!("op", db_scope = "made-up-scope");
                let _enter = span.enter();
                tracing::warn!(
                    target: "libmdbx",
                    "Process stalled, awaiting read-write transaction lock."
                );
            });
        });

        // Unknown scope values are not stored, so the event falls back to UNKNOWN.
        assert_eq!(recorder.scope_count(DB_SCOPE_UNKNOWN), 1);
    }

    #[derive(Default, Clone)]
    struct StallCounterRecorder {
        counters: Arc<Mutex<HashMap<String, Arc<AtomicU64>>>>,
    }

    impl StallCounterRecorder {
        fn scope_count(&self, scope: &str) -> u64 {
            let counters = self.counters.lock().expect("counters mutex poisoned");
            counters
                .get(scope)
                .map(|c| c.load(Ordering::Acquire))
                .unwrap_or(0)
        }

        fn counter_for(&self, scope: &str) -> Arc<AtomicU64> {
            let mut counters = self.counters.lock().expect("counters mutex poisoned");
            Arc::clone(
                // clone: Arc handle to the per-scope counter storage; cheaply cloned to share between recorder and assertions
                counters
                    .entry(scope.to_string())
                    .or_insert_with(|| Arc::new(AtomicU64::new(0))),
            )
        }
    }

    impl Recorder for StallCounterRecorder {
        fn describe_counter(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {
        }

        fn describe_gauge(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {}

        fn describe_histogram(
            &self,
            _key: KeyName,
            _unit: Option<Unit>,
            _description: SharedString,
        ) {
        }

        fn register_counter(&self, key: &Key, _metadata: &Metadata<'_>) -> Counter {
            if key.name() != MDBX_RW_TX_LOCK_STALLS_TOTAL {
                return Counter::noop();
            }
            let scope = key
                .labels()
                .find(|l| l.key() == "scope")
                .map(|l| l.value().to_string())
                .unwrap_or_default();
            Counter::from_arc(self.counter_for(&scope))
        }

        fn register_gauge(&self, _key: &Key, _metadata: &Metadata<'_>) -> Gauge {
            Gauge::noop()
        }

        fn register_histogram(&self, _key: &Key, _metadata: &Metadata<'_>) -> Histogram {
            Histogram::noop()
        }
    }
}
