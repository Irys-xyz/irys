use std::{env, ffi::OsString, path::PathBuf};

mod capacity;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let c_src = manifest_dir.join("c_src");

    // Watch individual files instead of the directory.
    // Directory-level watches use mtime which can change even when rsync
    // doesn't modify any files, triggering unnecessary full rebuilds.
    for entry in std::fs::read_dir(&c_src).unwrap() {
        let entry = entry.unwrap();
        println!("cargo:rerun-if-changed={}", entry.path().display());
    }

    let (lib_dir, include_dir) = build_openssl();
    let pkgconfig_dir = lib_dir.join("pkgconfig");
    // tell pkgconfig to discover our vendored openssl build
    // SAFETY: build scripts are single-threaded; no other threads observe this env var.
    unsafe { env::set_var("PKG_CONFIG_PATH", pkgconfig_dir) };

    capacity::build_capacity(&c_src, &include_dir);
    capacity::bind_capacity(&c_src);

    if std::env::var("CARGO_FEATURE_NVIDIA").is_ok() {
        capacity::build_capacity_cuda(&c_src, &include_dir);
        capacity::bind_capacity_cuda(&c_src);
        link_cuda()
    }
}

fn link_cuda() {
    {
        // Try to find CUDA in common locations
        let cuda_paths = vec![
            "/usr/local/cuda/lib64",
            "/usr/lib/x86_64-linux-gnu",
            "/opt/cuda/lib64",
        ];

        let mut found = false;
        for path in &cuda_paths {
            if std::path::Path::new(path).exists() {
                println!("cargo:rustc-link-search=native={}", path);
                found = true;
                break;
            }
        }

        // Also check CUDA_PATH environment variable
        if let Ok(cuda_path) = std::env::var("CUDA_PATH") {
            println!("cargo:rustc-link-search=native={}/lib64", cuda_path);
            found = true;
        }

        if !found {
            println!(
                "cargo:warning=CUDA library path not found in standard locations. Set CUDA_PATH or add -L flag manually."
            );
        }

        println!("cargo:rustc-link-lib=cudart");
        println!("cargo:rerun-if-changed=build.rs");
        println!("cargo:rerun-if-env-changed=CUDA_PATH");
    }
}

// build from src
// add openssl-src = "300.3.2" to cargo toml

fn env_inner(name: &str) -> Option<OsString> {
    let var = env::var_os(name);
    println!("cargo:rerun-if-env-changed={name}");

    match var {
        Some(ref v) => println!("{name} = {}", v.to_string_lossy()),
        None => println!("{name} unset"),
    }

    var
}

fn env(name: &str) -> Option<OsString> {
    let prefix = env::var("TARGET").unwrap().to_uppercase().replace('-', "_");
    let prefixed = format!("{prefix}_{name}");
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
