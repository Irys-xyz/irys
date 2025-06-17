#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
// and for some reason, clippy::all... doesn't actually cover all lints??
#![allow(
    clippy::all,
    clippy::pedantic,
    clippy::nursery,
    clippy::unused_trait_names,
    clippy::needless_raw_strings,
    clippy::unseparated_literal_suffix,
    clippy::allow_attributes
)]
include!(concat!(env!("OUT_DIR"), "/capacity_bindings.rs"));
