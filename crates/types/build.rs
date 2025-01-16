use build_print::{info, warn};
use std::env;

fn main() {
    let Ok(profile) = env::var("CARGO_PROFILE") else {
        info!("irys-types using default config");
        return;
    };
    println!("cargo:rustc-cfg=profile=\"{}\"", profile);
    let config_toml_path = match profile.as_str() {
        "testnet" => "configs/testnet.toml",
        "mainnet" => "configs/mainnet.toml",
        _ => {
            warn!(
                "irys-types using default config, no config defined for profile: {}",
                profile
            );
            return;
        }
    };
    println!("cargo:rustc-env=CONFIG_TOML_PATH={}", config_toml_path);
    info!("irys-types using config: {}", config_toml_path);
}
