//! Submodule index lookups, labelled by the caller that opened the view.
//!
//! One `data_path` count is one hash-keyed page get. A cursor walk is one
//! count, not one count per row. The global meter is a no-op until telemetry
//! installs a provider.

use std::{cell::Cell, sync::LazyLock};

use opentelemetry::{KeyValue, global, metrics::Counter};

pub(crate) const SERVE_LEDGER: &str = "serve_ledger";
pub(crate) const SERVE_DATA_ROOT: &str = "serve_data_root";
pub(crate) const SERVE_SPAN: &str = "serve_span";
pub(crate) const RECALL: &str = "recall";
pub const MIGRATION: &str = "migration";
pub(crate) const METADATA: &str = "metadata";
/// Ingress chunk whose data root is missing from the node cache.
pub const INGRESS_SIZE: &str = "ingress_size";
/// Ingress check of the rightmost chunk proof for a claimed data size.
pub const INGRESS_VERIFY: &str = "ingress_verify";
/// Placement of a chunk body onto entropy offsets.
pub const PLACEMENT: &str = "placement";
/// Cache reclamation asking which bodies are durable.
pub const CACHE_RECLAIM: &str = "cache_reclaim";
/// Data sync resolving a data root for a residual hole.
pub const DATA_SYNC: &str = "data_sync";
/// Index heal probing whether a placement is ready.
pub const INDEX_HEAL: &str = "index_heal";

thread_local! {
    static CALLER: Cell<&'static str> = const { Cell::new("unknown") };
}

struct Reset {
    previous: &'static str,
}

impl Drop for Reset {
    fn drop(&mut self) {
        CALLER.with(|cell| cell.set(self.previous));
    }
}

pub(crate) fn with_caller<T>(caller: &'static str, body: impl FnOnce() -> T) -> T {
    let previous = CALLER.with(|cell| cell.replace(caller));
    let _reset = Reset { previous };
    body()
}

/// Set the index caller on this thread. A rayon worker does not see the
/// caller's thread-local, so the worker closure calls this itself.
pub fn with_index_caller<T>(caller: &'static str, body: impl FnOnce() -> T) -> T {
    with_caller(caller, body)
}

/// One span per operation, named `index_read`, with the caller as a field.
/// Page gets under it keep that caller. Do not call this inside a rayon worker:
/// the span would have no parent and the export would be one trace per get.
pub fn trace_index_read<T>(caller: &'static str, body: impl FnOnce() -> T) -> T {
    let span = tracing::info_span!("index_read", caller);
    let _enter = span.enter();
    with_caller(caller, body)
}

fn current() -> &'static str {
    CALLER.with(|cell| cell.get())
}

static READS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter("irys-domain")
        .u64_counter("irys.storage.index_reads_total")
        .with_description(
            "Submodule index lookups by caller and kind. data_path, tx_path, tx_leaf, and data_root are individual gets. offset_walk is one cursor, not one row.",
        )
        .build()
});

static VIEWS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter("irys-domain")
        .u64_counter("irys.storage.index_views_total")
        .with_description("Submodule read views opened for an offset span, by caller")
        .build()
});

static SPAN_OFFSETS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter("irys-domain")
        .u64_counter("irys.storage.index_span_offsets_total")
        .with_description("Partition offsets covered by submodule index views, by caller")
        .build()
});

pub(crate) fn note(kind: &'static str) {
    READS.add(
        1,
        &[
            KeyValue::new("caller", current()),
            KeyValue::new("kind", kind),
        ],
    );
}

pub(crate) fn note_view(offsets: u64) {
    let caller = current();
    VIEWS.add(1, &[KeyValue::new("caller", caller)]);
    SPAN_OFFSETS.add(offsets, &[KeyValue::new("caller", caller)]);
}

#[cfg(test)]
mod tests {
    use super::{DATA_SYNC, INGRESS_SIZE, PLACEMENT, current, with_caller};

    #[test]
    fn caller_restores_after_nesting() {
        assert_eq!(current(), "unknown");
        with_caller(INGRESS_SIZE, || {
            assert_eq!(current(), INGRESS_SIZE);
            with_caller(DATA_SYNC, || assert_eq!(current(), DATA_SYNC));
            assert_eq!(current(), INGRESS_SIZE);
        });
        assert_eq!(current(), "unknown");
    }

    #[test]
    fn caller_does_not_cross_threads() {
        let seen = std::thread::scope(|scope| {
            with_caller(PLACEMENT, || scope.spawn(current).join().unwrap())
        });
        assert_eq!(seen, "unknown");
        let seen = std::thread::scope(|scope| {
            scope
                .spawn(|| with_caller(PLACEMENT, current))
                .join()
                .unwrap()
        });
        assert_eq!(seen, PLACEMENT);
    }
}
