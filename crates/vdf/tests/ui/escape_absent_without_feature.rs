//! Compiles `irys-vdf` WITHOUT the `test-utils` feature (the default for an
//! integration-test crate). The writable escape `into_inner_cloned` has been
//! removed and the mutation handle `test_set_step` is gated behind
//! `test-utils`, so neither is reachable here — this file must fail to compile.
use irys_vdf::state::VdfStateReadonly;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};

fn main() {
    let mining = Arc::new(AtomicBool::new(false));
    let state = Arc::new(RwLock::new(irys_vdf::state::VdfState::new(0, 0, mining)));
    let handle = VdfStateReadonly::new(state);
    // Neither of these may be reachable without `test-utils`:
    let _escape = handle.into_inner_cloned();
    let _mutate = handle.test_set_step(1);
}
