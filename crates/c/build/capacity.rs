use std::{
    env,
    path::{Path, PathBuf},
};

pub(crate) fn build_capacity(c_src: &Path, _ssl_inc_dir: &Path) {
    let mut cc = cc::Build::new();
    cc.flag("-O3");
    // TODO: enable below for debug
    // cc.flag("-O0 -g")
    cc.flag("-std=c99");
    cc.flag("-finline-functions");
    cc.flag("-Wall");
    cc.flag("-Wmissing-prototypes");

    let ossl = pkg_config::probe_library("openssl").expect("unable to find openssl");
    for inc_path in ossl.include_paths {
        cc.flag(format!("-I{}", inc_path.to_string_lossy()));
    }

    let gmp = pkg_config::probe_library("gmp").expect("unable to find gmp");
    for inc_path in gmp.include_paths {
        cc.flag(format!("-I{}", inc_path.to_string_lossy()));
    }

    cc.flag("-fPIC");
    cc.flag("-Ofast");
    cc.flag("-g0");
    cc.flag("-march=native");
    cc.file(c_src.join("capacity_single.c"));
    cc.compile("capacity_single");
}

pub(crate) fn bind_capacity(c_src: &Path) {
    let bindings = bindgen::Builder::default()
        .header(c_src.join("capacity.h").to_string_lossy())
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("Unable to generate bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("capacity_bindings.rs"))
        .expect("Couldn't write bindings!");
}

pub(crate) fn build_capacity_cuda(c_src: &Path, _ssl_inc_dir: &Path) {
    let mut cc = cc::Build::new();

    cc.cuda(true);
    cc.cudart("static");

    cc.flag("-O3");
    // TODO: enable below for debug
    //cc.flag("-O0");
    //cc.flag("-g");
    cc.flag("-std=c++17");
    cc.flag("-Xcompiler");
    cc.flag("-fPIC");
    cc.flag("-ccbin=/usr/bin/gcc-13");
    cc.flag("-DCAP_IMPL_CUDA");

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
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("Unable to generate bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("capacity_bindings_cuda.rs"))
        .expect("Couldn't write bindings!");
}
