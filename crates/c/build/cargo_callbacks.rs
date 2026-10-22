use std::path::{Path, PathBuf};

/// bindgen's `CargoCallbacks`, except paths inside this package are emitted relative to
/// the package root (cargo resolves them there). Absolute paths make cargo's fingerprint
/// differ per checkout, so a target/ copied between worktrees would rerun this script and
/// rebuild everything that depends on irys-c. Paths outside the package (system / toolchain
/// headers) stay absolute: they are the same in every checkout.
#[derive(Debug)]
pub(crate) struct RelativeCargoCallbacks {
    manifest_dir: PathBuf,
}

impl RelativeCargoCallbacks {
    pub(crate) fn new() -> Self {
        Self {
            manifest_dir: PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()),
        }
    }

    fn rerun_if_changed(&self, filename: &str) {
        let path = Path::new(filename);
        let path = path.strip_prefix(&self.manifest_dir).unwrap_or(path);
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

impl bindgen::callbacks::ParseCallbacks for RelativeCargoCallbacks {
    fn header_file(&self, filename: &str) {
        self.rerun_if_changed(filename);
    }

    fn include_file(&self, filename: &str) {
        self.rerun_if_changed(filename);
    }

    fn read_env_var(&self, key: &str) {
        println!("cargo:rerun-if-env-changed={key}");
    }
}
