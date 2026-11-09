use std::{env, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PVB_CUDA_ARCH");
    println!("cargo:rerun-if-env-changed=CUDA_PATH");
    if env::var_os("CARGO_FEATURE_CUDA").is_none() {
        return;
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let kernel = manifest_dir.join("cuda/pvb.cu");
    // cuda/sha256.cuh is the byte-at-a-time device SHA-256 of the production kernel before the
    // port (moved here from crates/c/c_src), used by the `ref` kernel.
    let cuda_src = manifest_dir.join("cuda");
    println!("cargo:rerun-if-changed={}", kernel.display());
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("cuda/pvb_fast.cuh").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        cuda_src.join("sha256.cuh").display()
    );

    let arch = env::var("PVB_CUDA_ARCH").unwrap_or_else(|_| "native".to_string());
    // Experiment switch for the fast kernel (see FAST_FMA_ADDS in cuda/pvb_fast.cuh).
    println!("cargo:rerun-if-env-changed=PVB_FAST_FMA_ADDS");
    let fma_adds = env::var("PVB_FAST_FMA_ADDS").unwrap_or_else(|_| "0".to_string());
    println!("cargo:rerun-if-env-changed=PVB_FAST_CACHE_HINT");
    let cache_hint = env::var("PVB_FAST_CACHE_HINT").unwrap_or_else(|_| "1".to_string());

    // Same nvcc flags as crates/c/build/capacity.rs (build_capacity_cuda) so the variant A
    // baseline matches the production kernel. `.debug(false)` matters: with debug info enabled
    // in the cargo profile, cc passes `-G` to nvcc, which disables device optimisation (~20x slower).
    cc::Build::new()
        .cuda(true)
        .cudart("static")
        .opt_level(3)
        .cpp(true)
        .std("c++17")
        .pic(true)
        .flag("-O3")
        .flag("--use_fast_math")
        .flag(format!("--gpu-architecture={arch}"))
        .flag("--extra-device-vectorization")
        .flag("--fmad=true")
        .flag("--maxrregcount=0")
        .flag("-Xcompiler=-O3")
        .flag("-Xcompiler=-march=native")
        .flag("-Xcompiler=-mtune=native")
        .flag("-Xcompiler=-ffast-math")
        .flag("-dlto")
        // lets the fast kernel use constexpr SHA-256 tables (folded into immediates)
        .flag("--expt-relaxed-constexpr")
        .debug(false)
        .define("FAST_FMA_ADDS", fma_adds.as_str())
        .define("FAST_CACHE_HINT", cache_hint.as_str())
        .include(&cuda_src)
        .file(&kernel)
        .compile("pvb_cuda");

    let cuda_path = env::var("CUDA_PATH").unwrap_or_else(|_| "/usr/local/cuda".to_string());
    println!("cargo:rustc-link-search=native={cuda_path}/lib64");
}
