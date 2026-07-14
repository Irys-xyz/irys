irys_utils::define_metrics! {
    meter: "irys-vdf";

    gauge VDF_GLOBAL_STEP("irys.vdf.global_step_number", "Current VDF global step number");
    gauge VDF_BUFFER_SUSPECT(
        "irys.vdf.buffer_suspect",
        "Whether the VDF seed buffer is marked suspect pending a re-anchor heal (1=yes, 0=no)"
    );
    counter VDF_REANCHOR_HEALED(
        "irys.vdf.reanchor_healed_total",
        "Number of in-place VDF seed-buffer heals successfully applied"
    );
    counter VDF_REANCHOR_SKIPPED(
        "irys.vdf.reanchor_skipped_total",
        "Number of VDF re-anchor requests skipped without rewriting the buffer"
    );
}

pub fn record_vdf_global_step(step: u64) {
    VDF_GLOBAL_STEP.record(step, &[]);
}

pub fn record_buffer_suspect(suspect: bool) {
    VDF_BUFFER_SUSPECT.record(u64::from(suspect), &[]);
}

pub fn record_reanchor_healed() {
    VDF_REANCHOR_HEALED.add(1, &[]);
}

pub fn record_reanchor_skipped() {
    VDF_REANCHOR_SKIPPED.add(1, &[]);
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
                        // Type AND value: record_vdf_mining_enabled(true) must
                        // land as a u64 gauge data point of 1.
                        match metric.data() {
                            AggregatedMetrics::U64(MetricData::Gauge(gauge)) => {
                                let point = gauge
                                    .data_points()
                                    .next()
                                    .expect("one recorded gauge data point");
                                assert_eq!(point.value(), 1, "true must record as gauge value 1");
                            }
                            _ => panic!("irys.vdf.mining_enabled must be a u64 gauge"),
                        }
                    }
                }
            }
        }
        assert!(found, "irys.vdf.mining_enabled must be exported");
    }

    /// The re-anchor observability trio must reach the exporter with stable
    /// names, the "irys-vdf" scope, and the right instrument kinds — dashboards
    /// and alerts key on these strings, so a silent rename or kind change is a
    /// monitoring outage, not a compile error. Same nextest isolation caveat as
    /// above.
    #[test]
    fn reanchor_metrics_preserve_name_scope_type_and_description() {
        let exporter = InMemoryMetricExporter::default();
        let reader = PeriodicReader::builder(exporter.clone()).build();
        let provider = SdkMeterProvider::builder().with_reader(reader).build();
        global::set_meter_provider(provider.clone());

        record_buffer_suspect(true);
        record_reanchor_healed();
        record_reanchor_skipped();
        provider.force_flush().expect("flush metrics");

        let resource_metrics = exporter.get_finished_metrics().expect("collected metrics");

        let mut found_suspect = false;
        let mut found_healed = false;
        let mut found_skipped = false;
        for rm in &resource_metrics {
            for scope in rm.scope_metrics() {
                for metric in scope.metrics() {
                    match metric.name() {
                        "irys.vdf.buffer_suspect" => {
                            found_suspect = true;
                            assert_eq!(scope.scope().name(), "irys-vdf");
                            assert_eq!(
                                metric.description(),
                                "Whether the VDF seed buffer is marked suspect pending a re-anchor heal (1=yes, 0=no)"
                            );
                            match metric.data() {
                                AggregatedMetrics::U64(MetricData::Gauge(gauge)) => {
                                    let point = gauge
                                        .data_points()
                                        .next()
                                        .expect("one recorded gauge data point");
                                    assert_eq!(
                                        point.value(),
                                        1,
                                        "suspect=true must record as gauge value 1"
                                    );
                                }
                                _ => panic!("irys.vdf.buffer_suspect must be a u64 gauge"),
                            }
                        }
                        "irys.vdf.reanchor_healed_total" => {
                            found_healed = true;
                            assert_eq!(scope.scope().name(), "irys-vdf");
                            assert_eq!(
                                metric.description(),
                                "Number of in-place VDF seed-buffer heals successfully applied"
                            );
                            match metric.data() {
                                AggregatedMetrics::U64(MetricData::Sum(sum)) => {
                                    let point = sum
                                        .data_points()
                                        .next()
                                        .expect("one recorded counter data point");
                                    assert_eq!(point.value(), 1);
                                }
                                _ => panic!("irys.vdf.reanchor_healed_total must be a u64 counter"),
                            }
                        }
                        "irys.vdf.reanchor_skipped_total" => {
                            found_skipped = true;
                            assert_eq!(scope.scope().name(), "irys-vdf");
                            assert_eq!(
                                metric.description(),
                                "Number of VDF re-anchor requests skipped without rewriting the buffer"
                            );
                            match metric.data() {
                                AggregatedMetrics::U64(MetricData::Sum(sum)) => {
                                    let point = sum
                                        .data_points()
                                        .next()
                                        .expect("one recorded counter data point");
                                    assert_eq!(point.value(), 1);
                                }
                                _ => {
                                    panic!("irys.vdf.reanchor_skipped_total must be a u64 counter")
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        assert!(found_suspect, "irys.vdf.buffer_suspect must be exported");
        assert!(
            found_healed,
            "irys.vdf.reanchor_healed_total must be exported"
        );
        assert!(
            found_skipped,
            "irys.vdf.reanchor_skipped_total must be exported"
        );
    }
}
