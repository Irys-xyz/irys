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
pub(crate) const MIGRATION: &str = "migration";
pub(crate) const METADATA: &str = "metadata";

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
