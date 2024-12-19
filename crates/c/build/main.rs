use std::{env, ffi::OsString, path::PathBuf};

mod capacity;
mod vdf;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let c_src = manifest_dir.join("c_src");

    println!("cargo:rerun-if-changed={}", c_src.display());

    let (lib_dir, include_dir) = build_openssl();
    let pkgconfig_dir = lib_dir.join("pkgconfig");
    // tell pkgconfig to discover our vendored openssl build
    env::set_var("PKG_CONFIG_PATH", pkgconfig_dir);

    vdf::build_vdf(&c_src, &include_dir);
    vdf::bind_vdf(&c_src);

    capacity::build_capacity(&c_src, &include_dir);
    capacity::bind_capacity(&c_src);
}

// fn build_cuda() {
//     cc::Build::new()
//     // Switch to CUDA C++ library compilation using NVCC.
//     .cuda(true)
//     .cudart("static")
//     // Generate code for Maxwell (GTX 970, 980, 980 Ti, Titan X).
//     .flag("-gencode").flag("arch=compute_52,code=sm_52")
//     // Generate code for Maxwell (Jetson TX1).
//     .flag("-gencode").flag("arch=compute_53,code=sm_53")
//     // Generate code for Pascal (GTX 1070, 1080, 1080 Ti, Titan Xp).
//     .flag("-gencode").flag("arch=compute_61,code=sm_61")
//     // Generate code for Pascal (Tesla P100).
//     .flag("-gencode").flag("arch=compute_60,code=sm_60")
//     // Generate code for Pascal (Jetson TX2).
//     .flag("-gencode").flag("arch=compute_62,code=sm_62")
//     // Generate code in parallel
//     .flag("-t0")
//     .file("bar.cu")
//     .compile("bar");
// }

// build from src
// add openssl-src = "300.3.2" to cargo toml


fn env_inner(name: &str) -> Option<OsString> {
    let var = env::var_os(name);
    println!("cargo:rerun-if-env-changed={}", name);

    match var {
        Some(ref v) => println!("{} = {}", name, v.to_string_lossy()),
        None => println!("{} unset", name),
    }

    var
}

fn env(name: &str) -> Option<OsString> {
    let prefix = env::var("TARGET").unwrap().to_uppercase().replace('-', "_");
    let prefixed = format!("{}_{}", prefix, name);
    env_inner(&prefixed).or_else(|| env_inner(name))
}

pub fn build_openssl() -> (PathBuf, PathBuf) {
    let openssl_config_dir = env("OPENSSL_CONFIG_DIR");

    let mut openssl_src_build = openssl_src::Build::new();
    if let Some(value) = openssl_config_dir {
        openssl_src_build.openssl_dir(PathBuf::from(value));
    }

    let artifacts = openssl_src_build.build();
    let lib_dir = artifacts.lib_dir().to_path_buf();
    let inc_dir = artifacts.include_dir().to_path_buf();
    (lib_dir, inc_dir)
}
