use std::ffi::c_ulong;

use irys_types::PACKING_SHA_1_5_S;

#[allow(non_upper_case_globals)]
#[allow(non_camel_case_types)]
#[allow(non_snake_case)]
include!(concat!(env!("OUT_DIR"), "/capacity_bindings_cuda.rs"));
