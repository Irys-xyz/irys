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

const FILENAME: &str = ".irys_submodules.toml";

impl StorageSubmodulesConfig {
    /// Loads the [`StorageSubmodulesConfig`] from a TOML file at the given path
    pub fn from_toml(path: impl AsRef<Path>) -> eyre::Result<Self> {
        let contents = fs::read_to_string(path)?;
        let config: Self = toml::from_str(&contents)?;

        let submodule_count = config.submodule_paths.len();
        if submodule_count < 3 {
            // Eventually this should be based off the genesis config, but
            // hard coded for now to help debug config / env issues.
            panic!(
                "Insufficient submodules: found {}, but minimum of 3 required in .irys_submodules.toml for chain initialization",
                submodule_count
            );
        }

        Ok(config)
    }

    pub fn load(instance_dir: PathBuf) -> eyre::Result<Self> {
        let home_dir = env::var("HOME").expect("Failed to get home directory");

        let is_deployed = env::var("IRYS_ENV").is_ok();
        let config_path_local = Path::new(&instance_dir).join(FILENAME);
        let config_path_home = Path::new(&home_dir).join(FILENAME);

        // Create base `storage_modules` directory if it doesn't exist
        let base_path = instance_dir.join("storage_modules");
        fs::create_dir_all(base_path.clone()).expect("to create storage_modules directory");

        // Start by removing all symlinks from the `storage_modules` dir
        fs::read_dir(base_path.clone())
            .expect("to read storage_modules dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_symlink()).unwrap_or(false))
            .for_each(|e| {
                println!("{:?}", e.path());
                fs::remove_dir_all(e.path()).unwrap()
            });

        // Try HOME directory config in deployed environments
        if is_deployed {
            if config_path_home.exists() {
                // Remove the .irys directory config so there's no confusion
                if config_path_local.exists() {
                    fs::remove_file(config_path_local).expect("able to delete file");
                }
                tracing::info!("Loading config from {:?}", config_path_home);
                let config = StorageSubmodulesConfig::from_toml(config_path_home).unwrap();

                // Create symlinks for each submodule if user provides paths
                let submodule_paths = &config.submodule_paths;
                for idx in 0..submodule_paths.len() {
                    let dest = submodule_paths.get(idx).unwrap();
                    if let Some(filename) = dest.components().last() {
                        let sm_path = base_path.join(filename.as_os_str());

                        // Check if path exists and is a directory (not a symlink)
                        if sm_path.exists() && sm_path.is_dir() && !sm_path.is_symlink() {
                            fs::remove_dir_all(&sm_path).expect("to remove existing directory");
                        }

                        tracing::info!("Creating symlink from {:?} to {:?}", sm_path, dest);
                        debug_assert!(dest.exists());

                        #[cfg(unix)]
                        std::os::unix::fs::symlink(&dest, &sm_path).expect("to create symlink");
                        #[cfg(windows)]
                        std::os::windows::fs::symlink_dir(&dest, &sm_path)
                            .expect("to create symlink");
                    }
                }

                return Ok(config);
            }
        }

        // Try .irys directory config in dev/local environment
        if config_path_local.exists() {
            return StorageSubmodulesConfig::from_toml(config_path_local);
        } else {
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

            // Ensure the submodule paths exist, StorageModule::new() will do the rest
            for path in &config.submodule_paths {
                fs::create_dir_all(path).expect("to create submodule dir");
            }

            // Load the config to verify it parses
            StorageSubmodulesConfig::from_toml(config_path_local)
        }
        // let _ = STORAGE_SUBMODULES_CONFIG.submodule_paths.len();
    }
}
