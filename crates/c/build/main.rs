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

    if std::env::var("CARGO_FEATURE_NVIDIA").is_ok() {
        capacity::build_capacity_cuda(&c_src, &include_dir);
        capacity::bind_capacity_cuda(&c_src);
    }
}

// build from src
// add openssl-src = "300.3.2" to cargo toml

fn env_inner(name: &str) -> Option<OsString> {
    let var = env::var_os(name);
    println!("cargo:rerun-if-env-changed={name}");

    match var {
        Some(ref v) => println!("{} = {}", name, v.to_string_lossy()),
        None => println!("{name} unset"),
    }

    var
}

fn env(name: &str) -> Option<OsString> {
    let prefix = env::var("TARGET").unwrap().to_uppercase().replace('-', "_");
    let prefixed = format!("{prefix}_{name}");
    env_inner(&prefixed).or_else(|| env_inner(name))
}

#[must_use] pub fn build_openssl() -> (PathBuf, PathBuf) {
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
