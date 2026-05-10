use std::fmt;

use tracing::{Event, Level, Subscriber, field::Field, field::Visit};
use tracing_subscriber::Layer;

pub const MDBX_RW_TX_LOCK_STALLS_TOTAL: &str = "libmdbx.rw_tx_lock_stalls_total";

const LIBMDBX_TARGET: &str = "libmdbx";
const RW_TX_LOCK_STALL_MESSAGE: &str = "Process stalled, awaiting read-write transaction lock";
const RW_TX_LOCK_STALL_MESSAGE_WITH_PERIOD: &str =
    "Process stalled, awaiting read-write transaction lock.";

#[derive(Debug)]
pub struct MdbxLockMetricsLayer;

#[must_use]
pub const fn mdbx_lock_metrics_layer() -> MdbxLockMetricsLayer {
    MdbxLockMetricsLayer
}

pub(crate) fn describe_mdbx_metrics() {
    metrics::describe_counter!(
        MDBX_RW_TX_LOCK_STALLS_TOTAL,
        "libmdbx read-write transaction lock stall warnings"
    );
}

impl<S> Layer<S> for MdbxLockMetricsLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        if is_mdbx_rw_tx_lock_stall(event) {
            record_mdbx_rw_tx_lock_stall();
        }
    }
}

fn record_mdbx_rw_tx_lock_stall() {
    metrics::counter!(MDBX_RW_TX_LOCK_STALLS_TOTAL).increment(1);
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
    matches!(
        value,
        RW_TX_LOCK_STALL_MESSAGE | RW_TX_LOCK_STALL_MESSAGE_WITH_PERIOD
    )
}

fn debug_message_matches(value: &dyn fmt::Debug) -> bool {
    debug_value_matches(value, RW_TX_LOCK_STALL_MESSAGE)
        || debug_value_matches(value, RW_TX_LOCK_STALL_MESSAGE_WITH_PERIOD)
}

fn debug_value_matches(value: &dyn fmt::Debug, expected: &str) -> bool {
    let mut writer = ExactDebugWriter::new(expected);
    fmt::write(&mut writer, format_args!("{value:?}")).is_ok() && writer.matches()
}

struct ExactDebugWriter<'a> {
    expected: &'a str,
    consumed: usize,
    matched: bool,
}

impl<'a> ExactDebugWriter<'a> {
    const fn new(expected: &'a str) -> Self {
        Self {
            expected,
            consumed: 0,
            matched: true,
        }
    }

    fn matches(&self) -> bool {
        self.matched && self.consumed == self.expected.len()
    }
}

impl fmt::Write for ExactDebugWriter<'_> {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        if !self.matched {
            return Ok(());
        }

        let Some(remaining) = self.expected.get(self.consumed..) else {
            self.matched = false;
            return Ok(());
        };

        if !remaining.starts_with(value) {
            self.matched = false;
            return Ok(());
        }

        self.consumed = match self.consumed.checked_add(value.len()) {
            Some(consumed) => consumed,
            None => {
                self.matched = false;
                return Ok(());
            }
        };

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    use metrics::{
        Counter, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString, Unit,
    };
    use tracing_subscriber::{Registry, layer::SubscriberExt as _};

    use super::*;

    #[test]
    fn layer_counts_only_libmdbx_rw_lock_stall_warnings() {
        let recorder = StallCounterRecorder::default();
        let counter = Arc::clone(&recorder.value);
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

        assert_eq!(counter.load(Ordering::Acquire), 2);
    }

    #[derive(Default)]
    struct StallCounterRecorder {
        value: Arc<AtomicU64>,
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
            if key.name() == MDBX_RW_TX_LOCK_STALLS_TOTAL {
                return Counter::from_arc(Arc::clone(&self.value));
            }

            Counter::noop()
        }

        fn register_gauge(&self, _key: &Key, _metadata: &Metadata<'_>) -> Gauge {
            Gauge::noop()
        }

        fn register_histogram(&self, _key: &Key, _metadata: &Metadata<'_>) -> Histogram {
            Histogram::noop()
        }
    }
}
