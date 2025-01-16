use std::{env, path::PathBuf};

pub(crate) fn build_vdf(c_src: &PathBuf, _ssl_inc_dir: &PathBuf) {
    let mut cc = cc::Build::new();
    cc.cpp(true);

    cc.flag("-O3");
    // TODO: enable below for debug
    // cc.flag("-O0 -g")
    cc.flag("-finline-functions");
    cc.flag("-Wall");

    // we use pkg_config as it sets linker flags
    let ossl = pkg_config::Config::new()
        .probe("openssl")
        .expect("unable to find openssl");

    for inc_path in ossl.include_paths {
        cc.flag(format!("-I{}", inc_path.to_string_lossy()));
    }

    let gmp = pkg_config::probe_library("gmp").expect("unable to find gmp");
    for inc_path in gmp.include_paths {
        cc.flag(format!("-I{}", inc_path.to_string_lossy()));
    }

    cc.flag("-fPIC");
    cc.flag("-std=c++17");
    cc.flag("-Ofast");
    cc.flag("-g0");
    cc.flag("-march=native");
    cc.file(c_src.join("vdf.cpp")).compile("vdf");
}

pub(crate) fn bind_vdf(c_src: &PathBuf) {
    let bindings = bindgen::Builder::default()
        .header(c_src.join("vdf.h").to_string_lossy())
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("Unable to generate bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("vdf_bindings.rs"))
        .expect("Couldn't write bindings!");
}
