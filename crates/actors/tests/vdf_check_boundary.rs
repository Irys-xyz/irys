//! Boundary guard: the pure VDF check functions live in `irys_vdf::verify`, not
//! redefined inside the actor validation modules. Reintroducing a local copy
//! would re-split the consensus-critical VDF check surface across crates.

#[test]
fn vdf_check_functions_are_centralised_in_irys_vdf() {
    const BLOCK_VALIDATION: &str = include_str!("../src/block_validation.rs");
    const VALIDATION_SERVICE: &str = include_str!("../src/validation_service.rs");
    const BLOCK_VALIDATION_TASK: &str =
        include_str!("../src/validation_service/block_validation_task.rs");

    // Neither check may be re-defined in the actor validation modules.
    for (name, src) in [
        ("block_validation.rs", BLOCK_VALIDATION),
        ("validation_service.rs", VALIDATION_SERVICE),
        ("block_validation_task.rs", BLOCK_VALIDATION_TASK),
    ] {
        assert!(
            !src.contains("fn is_seed_data_valid"),
            "{name} must not define is_seed_data_valid; it now lives in irys_vdf::verify"
        );
        assert!(
            !src.contains("fn prev_output_is_valid"),
            "{name} must not define prev_output_is_valid; it now lives in irys_vdf::verify"
        );
    }

    // Each consumer must reach the checks through the irys_vdf::verify facade.
    // Match the module path and the symbol separately so a grouped import
    // (`use irys_vdf::verify::{is_seed_data_valid, prev_output_is_valid};`) still
    // passes — only a local redefinition or a non-facade path should fail.
    let reaches_via_facade =
        |src: &str, symbol: &str| src.contains("irys_vdf::verify") && src.contains(symbol);
    assert!(
        reaches_via_facade(BLOCK_VALIDATION, "is_seed_data_valid"),
        "block_validation.rs must reach is_seed_data_valid via the irys_vdf::verify facade"
    );
    assert!(
        reaches_via_facade(BLOCK_VALIDATION, "prev_output_is_valid"),
        "block_validation.rs must reach prev_output_is_valid via the irys_vdf::verify facade"
    );
    assert!(
        reaches_via_facade(VALIDATION_SERVICE, "is_seed_data_valid"),
        "validation_service.rs must reach is_seed_data_valid via the irys_vdf::verify facade"
    );
    assert!(
        reaches_via_facade(BLOCK_VALIDATION_TASK, "is_seed_data_valid"),
        "block_validation_task.rs must reach is_seed_data_valid via the irys_vdf::verify facade"
    );
}
