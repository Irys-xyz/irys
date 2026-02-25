use std::{
    env,
    path::{Path, PathBuf},
};

pub(crate) fn build_capacity(c_src: &Path, _ssl_inc_dir: &Path) {
    let mut cc = cc::Build::new();
    // Optimization flags
    cc.flag("-O3")
        .flag("-ffast-math")
        .flag("-finline-functions")
        .flag("-fPIC")
        .flag("-g0");

    // TODO: enable below for debug
    // cc.flag("-O0").flag("-g");

    cc.flag("-std=c99")
        .flag("-Wall")
        .flag("-Wmissing-prototypes");

    // Add library include paths
    for library in &["openssl", "gmp"] {
        let lib = pkg_config::probe_library(library)
            .unwrap_or_else(|_| panic!("unable to find {}", library));

        for inc_path in lib.include_paths {
            cc.flag(format!("-I{}", inc_path.display()));
        }
    }

    let nix_disables_native = env::var("NIX_ENFORCE_NO_NATIVE")
        .map(|v| v != "0")
        .unwrap_or(false);
    if !nix_disables_native {
        cc.flag_if_supported("-march=native");
    }
    cc.file(c_src.join("capacity_single.c"));
    cc.compile("capacity_single");
}

pub(crate) fn bind_capacity(c_src: &Path) {
    let bindings = bindgen::Builder::default()
        .header(c_src.join("capacity.h").to_string_lossy())
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .wrap_unsafe_ops(true)
        .generate()
        .expect("Unable to generate bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("capacity_bindings.rs"))
        .expect("Couldn't write bindings!");
}

pub(crate) fn build_capacity_cuda(c_src: &Path, _ssl_inc_dir: &Path) {
    let mut cc = cc::Build::new();
    cc.cuda(true)
        .cudart("static")
        .opt_level(3)
        .cpp(true)
        .std("c++17")
        .pic(true)
        .define("CAP_IMPL_CUDA", None)
        .flag("-O3")
        .flag("--use_fast_math")
        .flag("--gpu-architecture=native") // optimise for local GPU
        // .flag("--ptxas-options=-v") // enable to see register usage
        .flag("--extra-device-vectorization")
        .flag("--fmad=true")
        .flag("--maxrregcount=0")
        .flag("-Xcompiler=-O3")
        .flag("-Xcompiler=-march=native")
        .flag("-Xcompiler=-mtune=native")
        .flag("-Xcompiler=-ffast-math")
        .flag("-dlto")
        .debug(false);

    let ossl = pkg_config::probe_library("openssl").expect("unable to find openssl");
    for inc_path in ossl.include_paths {
        cc.flag(format!("-I{}", inc_path.to_string_lossy()));
    }

    let gmp = pkg_config::probe_library("gmp").expect("unable to find gmp");
    for inc_path in gmp.include_paths {
        cc.flag(format!("-I{}", inc_path.to_string_lossy()));
    }

    cc.file(c_src.join("capacity_cuda.cu"));
    cc.compile("capacity_cuda");
}

pub(crate) fn bind_capacity_cuda(c_src: &Path) {
    let bindings = bindgen::Builder::default()
        .header(c_src.join("capacity_cuda.h").to_string_lossy())
        .rustified_enum("entropy_chunk_errors")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .wrap_unsafe_ops(true)
        .generate()
        .expect("Unable to generate bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("capacity_bindings_cuda.rs"))
        .expect("Couldn't write bindings!");
}
