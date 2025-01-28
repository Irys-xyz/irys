//! Crate dedicated to the `IrysNodeConfig` to avoid depdendency cycles
use std::{
    env::{self},
    fs,
    num::ParseIntError,
    path::{Path, PathBuf},
};

use chain::chainspec::IrysChainSpecBuilder;
use irys_primitives::GenesisAccount;
use irys_types::{irys::IrysSigner, Address, CONFIG};
use serde::{Deserialize, Serialize};

pub mod chain;

// TODO: convert this into a set of clap args

#[derive(Debug, Clone)]
/// Top level configuration struct for the node
pub struct IrysNodeConfig {
    /// Signer instance used for mining
    pub mining_signer: IrysSigner,
    /// Node ID/instance number: used for testing
    pub instance_number: u32,

    /// base data directory, i.e `./.tmp`
    /// should not be used directly, instead use the appropriate methods, i.e `instance_directory`
    pub base_directory: PathBuf,
    /// `ChainSpec` builder - used to generate `ChainSpec`, which defines most of the chain-related parameters
    pub chainspec_builder: IrysChainSpecBuilder,
}

/// "sane" default configuration
impl Default for IrysNodeConfig {
    fn default() -> Self {
        let base_dir = env::current_dir()
            .expect("Unable to determine working dir, aborting")
            .join(".irys");

        Self {
            chainspec_builder: IrysChainSpecBuilder::mainnet(),
            mining_signer: IrysSigner::random_signer(),
            instance_number: 1,
            base_directory: base_dir,
        }
    }
}

pub fn decode_hex(s: &str) -> Result<Vec<u8>, ParseIntError> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16))
        .collect()
}

impl IrysNodeConfig {
    pub fn mainnet() -> Self {
        Self {
            mining_signer: IrysSigner::mainnet_from_slice(&decode_hex(CONFIG.mining_key).unwrap()),
            instance_number: 1,
            base_directory: env::current_dir()
                .expect("Unable to determine working dir, aborting")
                .join(".irys"),
            chainspec_builder: IrysChainSpecBuilder::mainnet(),
        }
    }

    /// get the instance-specific directory path
    pub fn instance_directory(&self) -> PathBuf {
        self.base_directory.join(self.instance_number.to_string())
    }
    /// get the instance-specific storage module directory path
    pub fn storage_module_dir(&self) -> PathBuf {
        self.instance_directory().join("storage_modules")
    }
    /// get the instance-specific reth data directory path
    pub fn reth_data_dir(&self) -> PathBuf {
        self.instance_directory().join("reth")
    }
    /// get the instance-specific reth log directory path
    pub fn reth_log_dir(&self) -> PathBuf {
        self.reth_data_dir().join("logs")
    }
    /// get the instance-specific `block_index` directory path  
    pub fn block_index_dir(&self) -> PathBuf {
        self.instance_directory().join("block_index")
    }

    /// get the instance-specific `vdf_steps` directory path  
    pub fn vdf_steps_dir(&self) -> PathBuf {
        self.instance_directory().join("vdf_steps")
    }

    /// Extend the configured genesis accounts
    /// These accounts are used as the genesis state for the chain
    pub fn extend_genesis_accounts(
        &mut self,
        accounts: impl IntoIterator<Item = (Address, GenesisAccount)>,
    ) -> &mut Self {
        self.chainspec_builder.extend_accounts(accounts);
        self
    }
}

// pub struct IrysConfigBuilder {
//     /// Signer instance used for mining
//     pub mining_signer: IrysSigner,
//     /// Node ID/instance number: used for testing
//     pub instance_number: u32,
//     /// configuration of partitions and their associated storage providers

//     /// base data directory, i.e `./.tmp`
//     /// should not be used directly, instead use the appropriate methods, i.e `instance_directory`
//     pub base_directory: PathBuf,
//     /// ChainSpec builder - used to generate ChainSpec, which defines most of the chain-related parameters
//     pub chainspec_builder: IrysChainSpecBuilder,
// }

// impl Default for IrysConfigBuilder {
//     fn default() -> Self {
//         Self {
//             instance_number: 0,
//             base_directory:absolute(PathBuf::from_str("../../.tmp").unwrap()).unwrap(),
//             chainspec_builder: IrysChainSpecBuilder::mainnet(),
//             mining_signer: IrysSigner::random_signer(),
//         }
//     }
// }

// impl IrysConfigBuilder {
//     pub fn new() -> Self {
//         return IrysConfigBuilder::default();
//     }
//     pub fn instance_number(mut self, number: u32) -> Self {
//         self.instance_number = number;
//         self
//     }
//     pub fn base_directory(mut self, path: PathBuf) -> Self {
//         self.base_directory = path;
//         self
//     }
//     // pub fn base_directory(mut self, path: PathBuf) -> Self {
//     //     self.base_directory = path;
//     //     self
//     // }
//     // pub fn add_partition_and_sm(mut self, partition: Partition, storage_module: )
//     pub fn mainnet() -> Self {
//         return IrysConfigBuilder::new()
//             .base_directory(absolute(PathBuf::from_str("../../.irys").unwrap()).unwrap());
//     }

//     pub fn build(mut self) -> IrysNodeConfig {
//
//         return self.config;
//     }
// }

pub const PRICE_PER_CHUNK_PERM: u128 = 10000;
pub const PRICE_PER_CHUNK_5_EPOCH: u128 = 10;

/// Subsystem allowing for the configuration of storage submodules via a handy TOML file
///
/// Storage submodule path mappings are now governed by a `~/.irys_storage_modules.toml` file.
/// This file is automatically created if it does not exist when the node starts, and is
/// populated with `submodule_paths` set to an empty array by default.
///
/// If `submodule_paths` is empty, everything works the way it normally would with submodules
/// stored inline within the `.irys` `storage_modules` directory.
///
/// If `submodule_paths` has items, they are expected to be paths to directories that can be
/// mounted as submodules. If these are specified, the number of paths in `submodule_paths`
/// must exactly match the number of expected submodules based on the current storage config,
/// or an error will be thrown and the process will abort. During storage initialization,
/// symlinks will be created within the `storage_modules` directory mapping the regular storage
/// location for each submodule to the `submodule_paths` in the order they are specified.
///
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct StorageSubmodulesConfig {
    #[serde(default)]
    pub is_using_hardcoded_paths: bool, // Defaults to false with default trait
    pub submodule_paths: Vec<PathBuf>,
}

impl StorageSubmodulesConfig {
    /// Loads the [`StorageSubmodulesConfig`] from a TOML file at the given path
    pub fn from_toml(path: impl AsRef<Path>) -> eyre::Result<Self> {
        let contents = fs::read_to_string(path)?;
        let config: Self = toml::from_str(&contents)?;
        Ok(config)
    }

    /// Forces the lazy loading of the [`STORAGE_SUBMODULES_CONFIG`]
    pub fn load() {
        let _ = STORAGE_SUBMODULES_CONFIG.submodule_paths.len();
    }
}

/// Configure storage submodule paths:
/// - In deployed envs (IRYS_ENV set): Try HOME/.irys_storage_submodules.toml first
/// - Otherwise: Use/create .irys/<instance id>/.irys_storage_submodules.toml
/// - Default: Create config with hardcoded submodule paths in .irys
pub static STORAGE_SUBMODULES_CONFIG: once_cell::sync::Lazy<StorageSubmodulesConfig> =
    once_cell::sync::Lazy::new(|| {
        const FILENAME: &str = ".irys_storage_submodules.toml";
        let node_config = IrysNodeConfig::default();
        let instance_dir = node_config.instance_directory();
        let home_dir = env::var("HOME").expect("Failed to get home directory");

        let is_deployed = env::var("IRYS_ENV").is_ok();
        let config_path_local = Path::new(&instance_dir).join(FILENAME);
        let config_path_home = Path::new(&home_dir).join(FILENAME);

        // Try HOME directory config in deployed environments
        if is_deployed {
            if config_path_home.exists() {
                // Remove the .irys directory config so there's no confusion
                if config_path_local.exists() {
                    fs::remove_file(config_path_local).expect("able to delete file");
                }
                tracing::info!("Loading config from {:?}", config_path_home);
                return StorageSubmodulesConfig::from_toml(config_path_home).unwrap();
            }
        }

        // Clear out any leftover storage module infos from a non default config
        // (but leave the intervals)
        let sm_path = instance_dir.join("storage_modules");
        if sm_path.exists() {
            fs::read_dir(instance_dir.join("storage_modules"))
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    let binding = e.file_name();
                    let name = binding.to_string_lossy();
                    name.starts_with("StorageModule_")
                        && name.ends_with(".json")
                        && !name.contains("_intervals")
                        && name[14..name.len() - 5].parse::<u32>().is_ok()
                })
                .for_each(|e| fs::remove_file(e.path()).unwrap());
        }

        // Try .irys directory config in dev environment
        if config_path_local.exists() {
            return StorageSubmodulesConfig::from_toml(config_path_local).unwrap();
        }

        // Create default config with hardcoded paths in dev if none exists
        tracing::info!("Creating default config at {:?}", config_path_local);
        let config = StorageSubmodulesConfig {
            is_using_hardcoded_paths: true,
            submodule_paths: vec![
                Path::new(&instance_dir).join("storage_modules/submodule_0"),
                Path::new(&instance_dir).join("storage_modules/submodule_1"),
                Path::new(&instance_dir).join("storage_modules/submodule_2"),
            ],
        };

        // Write and verify config
        fs::create_dir_all(instance_dir).expect(".irys config dir can be created");
        let toml = toml::to_string(&config).expect("Able to serialize config");
        fs::write(&config_path_local, toml).unwrap_or_else(|_| {
            panic!("Failed to write config to {}", config_path_local.display())
        });

        // Load the config to verify it parses
        StorageSubmodulesConfig::from_toml(config_path_local).unwrap()
    });
