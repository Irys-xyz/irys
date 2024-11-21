//! Crate dedicated to the IrysNodeConfig to avoid depdendency cycles
use std::{
    path::{absolute, PathBuf},
    str::FromStr as _,
};

use chain::chainspec::IrysChainSpecBuilder;
use irys_primitives::GenesisAccount;
use irys_storage::ii;
use irys_types::{
    block_production::Partition, irys::IrysSigner, Address, PartitionStorageProviderConfig,
    StorageModuleConfig, CHUNK_SIZE,
};

pub mod chain;

// TODO: convert this into a set of clap args

#[derive(Debug, Clone)]
/// Top level configuration struct for the node
pub struct IrysNodeConfig {
    /// Signer instance used for mining
    pub mining_signer: IrysSigner,
    /// Node ID/instance number: used for testing
    pub instance_number: u32,
    /// configuration of partitions and their associated storage providers
    /// TODO: rework
    pub sm_partition_config: Vec<(Partition, PartitionStorageProviderConfig)>,
    /// base data directory, i.e `./.tmp`
    /// should not be used directly, instead use the appropriate methods, i.e `instance_directory`
    pub base_directory: PathBuf,
    /// ChainSpec builder - used to generate ChainSpec, which defines most of the chain-related parameters
    pub chainspec_builder: IrysChainSpecBuilder,
}

/// "sane" default configuration
impl Default for IrysNodeConfig {
    fn default() -> Self {
        let base_dir = absolute(PathBuf::from_str("../../.tmp").unwrap()).unwrap();
        Self {
            chainspec_builder: IrysChainSpecBuilder::mainnet(),
            mining_signer: IrysSigner::random_signer(),
            instance_number: 1,
            sm_partition_config: get_default_partitions_and_storage_providers(
                base_dir.join("0/storage_modules"),
            )
            .unwrap(),
            base_directory: base_dir,
        }
    }
}
impl IrysNodeConfig {
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
    /// get the instance-specific block_index directory path  
    pub fn block_index_dir(&self) -> PathBuf {
        self.instance_directory().join("block_index")
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

/// get a set of preconfigured partitions and storage modules
pub fn get_default_partitions_and_storage_providers(
    storage_module_dir: PathBuf,
) -> eyre::Result<Vec<(Partition, PartitionStorageProviderConfig)>> {
    Ok(vec![
        (
            Partition::default(),
            PartitionStorageProviderConfig {
                sm_paths_offsets: vec![
                    (
                        ii(0, 3),
                        StorageModuleConfig {
                            directory_path: storage_module_dir.join("p1/sm1"),
                            size_bytes: 10 * CHUNK_SIZE,
                        },
                    ),
                    (
                        ii(4, 10),
                        StorageModuleConfig {
                            directory_path: storage_module_dir.join("p1/sm2"),
                            size_bytes: 10 * CHUNK_SIZE,
                        },
                    ),
                ],
            },
        ),
        (
            Partition::default(),
            PartitionStorageProviderConfig {
                sm_paths_offsets: vec![
                    (
                        ii(0, 5),
                        StorageModuleConfig {
                            directory_path: storage_module_dir.join("p2/sm1"),
                            size_bytes: 10 * CHUNK_SIZE,
                        },
                    ),
                    (
                        ii(6, 10),
                        StorageModuleConfig {
                            directory_path: storage_module_dir.join("p2/sm2"),
                            size_bytes: 10 * CHUNK_SIZE,
                        },
                    ),
                ],
            },
        ),
    ])
}

// pub struct IrysConfigBuilder {
//     /// Signer instance used for mining
//     pub mining_signer: IrysSigner,
//     /// Node ID/instance number: used for testing
//     pub instance_number: u32,
//     /// configuration of partitions and their associated storage providers
//     /// TODO: rework
//     pub sm_partition_config_builder: Box<dyn Fn(PathBuf) -> Vec<(Partition, PartitionStorageProviderConfig)>>,
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
//             sm_partition_config_builder: Box::new(|_: PathBuf| { Vec::new()})
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
//         let sm_partition_config = self.sm_partition_config_builder
//         return self.config;
//     }
// }
