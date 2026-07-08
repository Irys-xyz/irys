use opentelemetry::KeyValue;

irys_utils::define_metrics! {
    meter: "irys-vdf";

    gauge VDF_GLOBAL_STEP("irys.vdf.global_step_number", "Current VDF global step number");
    counter VDF_REANCHOR_APPLIED("irys.vdf.reanchor_applied_total", "In-process VDF re-anchor outcomes on the VDF thread by result (labelled): applied = buffer swapped onto canonical steps; builder_rejected / swap_failed = buffer stays poisoned until the next deep-reorg gate or a node restart");
}

pub fn record_vdf_global_step(step: u64) {
    VDF_GLOBAL_STEP.record(step, &[]);
}

pub fn record_vdf_reanchor_applied(result: &'static str) {
    VDF_REANCHOR_APPLIED.add(1, &[KeyValue::new("result", result)]);
}
