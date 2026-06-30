irys_utils::define_metrics! {
    meter: "irys-vdf";

    gauge VDF_GLOBAL_STEP("irys.vdf.global_step_number", "Current VDF global step number");
}

pub fn record_vdf_global_step(step: u64) {
    VDF_GLOBAL_STEP.record(step, &[]);
}

// VDF_MINING_ENABLED keeps its original "irys-chain" instrumentation scope (an
// operator-facing OTEL contract) even though its definition now lives here,
// co-located with the Plan 1 mining-enable flag owner. define_metrics! bakes the
// meter name into each instrument, so this MUST stay a separate block from the
// "irys-vdf" gauge above.
irys_utils::define_metrics! {
    meter: "irys-chain";

    gauge VDF_MINING_ENABLED("irys.vdf.mining_enabled", "Whether VDF mining is enabled (1=yes, 0=no)");
}

pub fn record_vdf_mining_enabled(enabled: bool) {
    VDF_MINING_ENABLED.record(u64::from(enabled), &[]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::global;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

    // Must run under nextest (process-per-test isolation): the metric statics bind
    // to the global meter provider on first record (LazyLock), so the provider
    // must be installed before this instrument is first recorded.
    #[test]
    fn vdf_mining_enabled_preserves_name_scope_type_and_description() {
        let exporter = InMemoryMetricExporter::default();
        let reader = PeriodicReader::builder(exporter.clone()).build();
        let provider = SdkMeterProvider::builder().with_reader(reader).build();
        global::set_meter_provider(provider.clone());

        record_vdf_mining_enabled(true);
        provider.force_flush().expect("flush metrics");

        let resource_metrics = exporter.get_finished_metrics().expect("collected metrics");

        let mut found = false;
        for rm in &resource_metrics {
            for scope in rm.scope_metrics() {
                for metric in scope.metrics() {
                    if metric.name() == "irys.vdf.mining_enabled" {
                        found = true;
                        // Scope preserved as "irys-chain" even though the
                        // definition now lives in irys-vdf.
                        assert_eq!(scope.scope().name(), "irys-chain");
                        assert_eq!(
                            metric.description(),
                            "Whether VDF mining is enabled (1=yes, 0=no)"
                        );
                        assert!(matches!(
                            metric.data(),
                            AggregatedMetrics::U64(MetricData::Gauge(_))
                        ));
                    }
                }
            }
        }
        assert!(found, "irys.vdf.mining_enabled must be exported");
    }
}
