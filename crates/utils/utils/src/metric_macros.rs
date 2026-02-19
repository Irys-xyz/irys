/// Describes a single OpenTelemetry metric for registry/introspection purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetricDescriptor {
    pub name: &'static str,
    pub description: &'static str,
    pub metric_type: &'static str,
}

/// Generates `LazyLock` statics for OpenTelemetry metric instruments and a
/// `METRICS` descriptor array.
///
/// # Syntax
///
/// ```ignore
/// irys_utils::define_metrics! {
///     meter: "meter-name";
///
///     counter COUNTER_NAME("metric.name", "Description");
///     gauge   GAUGE_NAME("metric.name", "Description");
///     histogram HIST_NAME("metric.name", "Description");
///     histogram HIST_NAME("metric.name", "Description", [1.0, 5.0, 10.0, 50.0]);
/// }
/// ```
///
/// Histograms accept an optional third argument — a slice of `f64` bucket
/// boundaries.  When omitted the OTel SDK default boundaries are used.
///
/// For each entry a `static` with [`std::sync::LazyLock`] is generated that
/// auto-initialises the metric instrument on first access.  A `METRICS` const
/// array of [`MetricDescriptor`] is also emitted for introspection/netwatch.
#[macro_export]
macro_rules! define_metrics {
    // Top-level: dispatch each metric line to its @single arm.
    // Histogram-with-boundaries must be matched first (3-arg form).
    (
        meter: $meter_name:expr;

        $( $kind:ident $static_name:ident($metric_name:expr, $desc:expr $(, $bounds:expr)?); )*
    ) => {
        $(
            $crate::define_metrics!(@single $kind, $static_name, $meter_name, $metric_name, $desc $(, $bounds)?);
        )*

        pub(crate) const METRICS: &[$crate::MetricDescriptor] = &[
            $(
                $crate::MetricDescriptor {
                    name: $metric_name,
                    description: $desc,
                    metric_type: $crate::define_metrics!(@type_str $kind),
                },
            )*
        ];
    };

    // -- Counter (always u64) --
    (@single counter, $name:ident, $meter:expr, $mname:expr, $desc:expr) => {
        static $name: std::sync::LazyLock<opentelemetry::metrics::Counter<u64>> =
            std::sync::LazyLock::new(|| {
                opentelemetry::global::meter($meter)
                    .u64_counter($mname)
                    .with_description($desc)
                    .build()
            });
    };

    // -- Gauge (always u64) --
    (@single gauge, $name:ident, $meter:expr, $mname:expr, $desc:expr) => {
        static $name: std::sync::LazyLock<opentelemetry::metrics::Gauge<u64>> =
            std::sync::LazyLock::new(|| {
                opentelemetry::global::meter($meter)
                    .u64_gauge($mname)
                    .with_description($desc)
                    .build()
            });
    };

    // -- Histogram with custom bucket boundaries --
    (@single histogram, $name:ident, $meter:expr, $mname:expr, $desc:expr, $bounds:expr) => {
        static $name: std::sync::LazyLock<opentelemetry::metrics::Histogram<f64>> =
            std::sync::LazyLock::new(|| {
                opentelemetry::global::meter($meter)
                    .f64_histogram($mname)
                    .with_description($desc)
                    .with_boundaries($bounds)
                    .build()
            });
    };

    // -- Histogram with default boundaries --
    (@single histogram, $name:ident, $meter:expr, $mname:expr, $desc:expr) => {
        static $name: std::sync::LazyLock<opentelemetry::metrics::Histogram<f64>> =
            std::sync::LazyLock::new(|| {
                opentelemetry::global::meter($meter)
                    .f64_histogram($mname)
                    .with_description($desc)
                    .build()
            });
    };

    // -- Type string helpers for MetricDescriptor --
    (@type_str counter) => { "counter" };
    (@type_str gauge) => { "gauge" };
    (@type_str histogram) => { "histogram" };
}
