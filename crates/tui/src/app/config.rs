use eyre::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize)]
pub struct TuiConfig {
    #[serde(default = "default_refresh_interval")]
    pub refresh_interval_secs: u64,
    #[serde(default = "default_connection_timeout")]
    pub connection_timeout_secs: u64,

    #[serde(default)]
    pub nodes: Vec<NodeConfig>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct NodeConfig {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
}

fn default_refresh_interval() -> u64 {
    10
}

fn default_connection_timeout() -> u64 {
    5
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            refresh_interval_secs: default_refresh_interval(),
            connection_timeout_secs: default_connection_timeout(),
            nodes: Vec::new(),
        }
    }
}

impl TuiConfig {
    pub fn load(config_path: Option<String>) -> Result<Self> {
        let config_file = match config_path {
            Some(path) => PathBuf::from(path),
            None => {
                // Try current directory first
                let current_dir_path = PathBuf::from("tui.toml");
                if current_dir_path.exists() {
                    current_dir_path
                } else if let Some(config_dir) = dirs::config_dir() {
                    config_dir.join("irys-tui").join("config.toml")
                } else {
                    return Ok(Self::default());
                }
            }
        };

        if config_file.exists() {
            let contents = std::fs::read_to_string(&config_file)?;
            Ok(toml::from_str(&contents)?)
        } else {
            Ok(Self::default())
        }
    }
}
