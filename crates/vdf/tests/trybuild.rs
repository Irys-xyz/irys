//! Compile-fail proof that the writable VDF-state escape is absent when the
//! `test-utils` feature is OFF (the default for this integration-test crate).
//! Pins the sealed read-only contract from production builds.

#[test]
fn writable_escape_absent_without_test_utils_feature() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/escape_absent_without_feature.rs");
}
