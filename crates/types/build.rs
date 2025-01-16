use std::env;

fn main() {
    let Ok(profile) = env::var("CARGO_PROFILE") else {
        return;
    };
    println!("cargo:rustc-cfg=profile=\"{}\"", profile);
    let config_toml_path = match profile.as_str() {
        "testnet" => "configs/testnet.toml",
        "mainnet" => "configs/mainnet.toml",
        _ => return,
    };
    println!("cargo:rustc-env=CONFIG_TOML_PATH={}", config_toml_path);
}
