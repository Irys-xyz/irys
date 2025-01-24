use build_print::{info, warn};
use std::env;

fn main() {
    println!("cargo::rerun-if-env-changed=IRYS_ENV");
    println!("cargo::rerun-if-changed=crates/types/configs");
    let Ok(irys_env) = env::var("IRYS_ENV") else {
        info!("irys-types using default config (set IRYS_ENV to override)");
        return;
    };
    let config_toml_path = match irys_env.as_str() {
        "testnet" => "crates/types/configs/testnet.toml",
        "mainnet" => "crates/types/configs/mainnet.toml",
        _ => {
            warn!(
                "irys-types using default config, no config defined for IRYS_ENV={}",
                irys_env
            );
            return;
        }
    };
    println!("cargo:rustc-env=CONFIG_TOML_PATH={}", config_toml_path);
    info!("irys-types using config: {}", config_toml_path);
}
