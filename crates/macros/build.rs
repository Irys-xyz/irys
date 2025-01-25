fn main() {
    println!("cargo::rerun-if-env-changed=IRYS_ENV");
    println!("cargo::rerun-if-env-changed=CONFIG_TOML_PATH");
    println!("cargo::rerun-if-changed=crates/types/configs");
}
