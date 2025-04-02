use irys_types::Config;
use std::fs::File;
use std::io::Write;
use tracing::{error, info};

struct TestingConfigs {
    genesis: String,
    peer1: String,
    peer2: String,
}

fn write_config(config: &Config, path: &str) -> std::io::Result<()> {
    let toml_str = toml::to_string(config).expect("Failed to serialise config");
    let mut file = File::create(path)?;
    file.write_all(toml_str.as_bytes())?;
    Ok(())
}

#[actix_web::test]
async fn heavy_sync_chain_state() -> eyre::Result<()> {
    let config_paths = TestingConfigs {
        genesis: String::from(".tmp/config-genesis.toml"),
        peer1: String::from(".tmp/config-peer1.toml"),
        peer2: String::from(".tmp/config-peer2.toml"),
    };

    let mut test_config = Config::testnet();
    test_config.port = 8080;

    match write_config(&test_config, &config_paths.genesis) {
        Ok(_) => {
            info!("{} config written", config_paths.genesis)
        }
        Err(_) => {
            error!("FAILURE: {} config not written", config_paths.genesis)
        }
    }

    test_config.port = 8081;
    match write_config(&test_config, &config_paths.peer1) {
        Ok(_) => {
            info!("{} config written", config_paths.peer1)
        }
        Err(_) => {
            error!("FAILURE: {} config not written", config_paths.peer1)
        }
    }

    test_config.port = 8082;
    match write_config(&test_config, &config_paths.peer2) {
        Ok(_) => {
            info!("{} config written", config_paths.peer2)
        }
        Err(_) => {
            error!("FAILURE: {} config not written", config_paths.peer2)
        }
    }

    //start genesis

    //start mining

    //start two additional peers, instructing them to use the genesis peer as their trusted peer

    Ok(())
}
