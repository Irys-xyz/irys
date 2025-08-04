use irys_types::{NodeConfig, NodeMode};
use std::path::PathBuf;
use tracing::debug;

pub fn load_config() -> eyre::Result<NodeConfig> {
    // load the config
    let config_path = std::env::var("CONFIG")
        .unwrap_or_else(|_| "config.toml".to_owned())
        .parse::<PathBuf>()
        .expect("file path to be valid");

    debug!("Loading config from {:?}", &config_path);
    let mut config = match std::fs::read_to_string(&config_path)
        .map(|config_file| toml::from_str::<NodeConfig>(&config_file).expect("invalid config file"))
    {
        Ok(cfg) => cfg,
        Err(err) => {
            let generate_config =
                std::env::var("GENERATE_CONFIG").unwrap_or_else(|_| "false".to_owned()) == "true";
            if generate_config {
                let mut config = NodeConfig::testnet();
                let signer = config.new_random_signer();
                config.reward_address = signer.address();
                config.mining_key = signer.signer;
                let mut file = std::fs::File::create(&config_path)?;
                std::io::Write::write_all(&mut file, toml::to_string(&config)?.as_bytes())?;
                eyre::bail!("Config file created - please edit it before restarting (see SETUP.md)")
            }
            eyre::bail!(
                "Unable to load config file at {:?} - {:?}\nHave you followed the setup steps in SETUP.md?",
                &config_path,
                &err
            );
        }
    };

    let is_genesis = std::env::var("GENESIS")
        .map(|_| true)
        .unwrap_or(matches!(config.mode, NodeMode::Genesis));

    if is_genesis {
        config.mode = NodeMode::Genesis;
    }

    Ok(config)
}
