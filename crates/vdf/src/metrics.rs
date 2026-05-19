irys_utils::define_metrics! {
    meter: "irys-vdf";

    gauge VDF_GLOBAL_STEP("irys.vdf.global_step_number", "Current VDF global step number");
}

pub fn record_vdf_global_step(step: u64) {
    VDF_GLOBAL_STEP.record(step, &[]);
}
