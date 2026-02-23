use irys_types::{NodeConfig, NodeMode};
use std::path::{Path, PathBuf};
use tracing::debug;

/// Load a [`NodeConfig`] from a TOML file at `path`.
///
/// Returns an error if the file cannot be read or contains invalid TOML.
/// If `generate` is true and the file doesn't exist, a default testnet
/// config is written and the function returns an error asking the user
/// to edit it before restarting.
pub fn load_config_from_path(path: &Path, generate: bool) -> eyre::Result<NodeConfig> {
    debug!("Loading config from {:?}", path);
    match std::fs::read_to_string(path)
        .map(|config_file| toml::from_str::<NodeConfig>(&config_file).expect("invalid config file"))
    {
        Ok(cfg) => Ok(cfg),
        Err(err) => {
            if generate {
                let mut config = NodeConfig::testnet();
                let signer = config.new_random_signer();
                config.reward_address = signer.address();
                config.mining_key = signer.signer;
                let mut file = std::fs::File::create(path)?;
                std::io::Write::write_all(&mut file, toml::to_string(&config)?.as_bytes())?;
                eyre::bail!("Config file created - please edit it before restarting (see SETUP.md)")
            }
            eyre::bail!(
                "Unable to load config file at {:?} - {:?}\nHave you followed the setup steps in SETUP.md?",
                path,
                &err
            );
        }
    }
}

/// Legacy config loader — reads `CONFIG` and `GENESIS` env vars.
///
/// Kept for backward compatibility with bare `irys` (no subcommand) and `irys-cli`.
#[tracing::instrument(level = "trace", skip_all)]
pub fn load_config() -> eyre::Result<NodeConfig> {
    let config_path = std::env::var("CONFIG")
        .unwrap_or_else(|_| "config.toml".to_owned())
        .parse::<PathBuf>()
        .expect("file path to be valid");

    let generate =
        std::env::var("GENERATE_CONFIG").unwrap_or_else(|_| "false".to_owned()) == "true";
    let mut config = load_config_from_path(&config_path, generate)?;

    let is_genesis = std::env::var("GENESIS")
        .map(|_| true)
        .unwrap_or(matches!(config.node_mode, NodeMode::Genesis));

    if is_genesis {
        config.node_mode = NodeMode::Genesis;
    }

    Ok(config)
}
