use irys_types::{NodeConfig, NodeMode};
use std::path::PathBuf;
use tracing::{debug, warn};

pub fn load_config() -> eyre::Result<NodeConfig> {
    // load the config
    let config_path = std::env::var("CONFIG")
        .map(|s| s.parse::<PathBuf>().expect("file path to be valid"))
        .unwrap_or_else(|_| {
            std::env::current_dir()
                .expect("Unable to determine working dir, aborting")
                .join("config.toml")
        });

    let mut config = std::fs::read_to_string(&config_path)
        .map(|config_file| {
            debug!("Loading config from {:?}", &config_path);
            toml::from_str::<NodeConfig>(&config_file).expect("invalid config file")
        })
        .unwrap_or_else(|err| {
            warn!(
                ?err,
                "config file not provided, defaulting to testnet config"
            );
            NodeConfig::testnet()
        });
    let is_genesis = std::env::var("GENESIS")
        .map(|_| true)
        .unwrap_or(matches!(config.mode, NodeMode::Genesis));
    if is_genesis {
        config.mode = NodeMode::Genesis;
    } else {
        config.mode = NodeMode::PeerSync;
    };

    Ok(config)
}
