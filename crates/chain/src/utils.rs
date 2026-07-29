use irys_types::{NodeConfig, NodeMode};
use std::path::Path;
use std::path::PathBuf;
use tracing::debug;

/// Parse a node config, annotating failures so an operator can act on them.
///
/// toml already reports the line, column, and offending source line. Keep its
/// rendering intact and add only the file path and a pointer to the setup docs.
/// This is a separate function so the error text is testable without touching
/// the environment or the filesystem.
pub fn parse_config(contents: &str, path: &Path) -> eyre::Result<NodeConfig> {
    toml::from_str::<NodeConfig>(contents).map_err(|err| {
        eyre::eyre!(
            "Invalid config file at {}:\n{err}\nHave you followed the setup steps in SETUP.md?",
            path.display(),
        )
    })
}

#[tracing::instrument(level = "trace", skip_all)]
pub fn load_config() -> eyre::Result<NodeConfig> {
    // load the config
    let config_path = std::env::var("CONFIG")
        .unwrap_or_else(|_| "config.toml".to_owned())
        .parse::<PathBuf>()
        .expect("file path to be valid");

    debug!("Loading config from {:?}", &config_path);
    let contents = match std::fs::read_to_string(&config_path) {
        Ok(contents) => contents,
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
                "Unable to read config file at {:?} - {err}\nHave you followed the setup steps in SETUP.md?",
                &config_path,
            );
        }
    };

    let mut config = parse_config(&contents, &config_path)?;

    let is_genesis = std::env::var("GENESIS")
        .map(|_| true)
        .unwrap_or(matches!(config.node_mode, NodeMode::Genesis));

    if is_genesis {
        config.node_mode = NodeMode::Genesis;
    }

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The testnet template with `node_mode` forced back to the retired spelling.
    fn template_with_retired_node_mode() -> String {
        let template = include_str!("../../config/templates/testnet_config.toml");
        template
            .lines()
            .map(|line| {
                if line.trim_start().starts_with("node_mode") {
                    "node_mode = \"Peer\""
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn parse_config_surfaces_the_node_mode_migration_error() {
        let err = parse_config(
            &template_with_retired_node_mode(),
            Path::new("/etc/irys/config.toml"),
        )
        .expect_err("a retired node_mode must not parse")
        .to_string();

        // The migration guidance from the hand-written Deserialize survives.
        assert!(err.contains("no longer exists"), "{err}");
        assert!(err.contains("Miner"), "{err}");
        assert!(err.contains("Observer"), "{err}");
        // toml's positional rendering survives, so the operator can find the
        // offending line: the header, the echoed source line, and the caret.
        // Asserted structurally rather than against a fixed line number, which
        // shifts whenever the template's leading comments change.
        assert!(err.contains("TOML parse error at line "), "{err}");
        assert!(err.contains("node_mode = \"Peer\""), "{err}");
        assert!(err.contains('^'), "{err}");
        // The operator is pointed at the setup docs.
        assert!(err.contains("SETUP.md"), "{err}");
        // The offending path is named.
        assert!(err.contains("/etc/irys/config.toml"), "{err}");
    }

    #[test]
    fn parse_config_reports_a_syntax_error_with_position() {
        let err = parse_config("this is not = = valid toml", Path::new("bad.toml"))
            .expect_err("malformed TOML must not parse")
            .to_string();

        assert!(err.contains("line 1"), "{err}");
        assert!(err.contains("SETUP.md"), "{err}");
    }

    #[test]
    fn parse_config_accepts_the_testnet_template() {
        let template = include_str!("../../config/templates/testnet_config.toml");
        parse_config(template, Path::new("testnet_config.toml"))
            .expect("the shipped testnet template must parse");
    }
}
