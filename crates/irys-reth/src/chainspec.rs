//! Irys-specific hardfork utilities and ChainSpec helpers.
//!
//! ## Adding new hardforks
//! 1. Add an entry to the `hardfork!()` macro call. This creates an enum entry.
//! 2. Add an entry to the `IrysHardforksInConfig` struct. This is used in the chainspec (part of consensus config toml file, inlined and visible to the users)
//! 3. If this hardfork maps to any Ethereum hardforks, add a corresponding entry to `ethereum_hardfork_mapping()`

use std::sync::Arc;

use alloy_eips::BlobScheduleBlobParams;
use alloy_genesis::Genesis;
use alloy_primitives::U256;
use irys_types::chainspec::IrysHardforksInConfig;
use reth_chainspec::{
    hardfork, make_genesis_header, BaseFeeParams, BaseFeeParamsKind, Chain, ChainHardforks,
    ChainSpec, EthereumHardfork, ForkCondition,
};
use reth_primitives_traits::SealedHeader;

hardfork!(
    #[derive(serde::Serialize, serde::Deserialize)]
    IrysHardfork { Frontier }
);

/// Hardfork schedule wrapper used across Irys components.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IrysChainHardforks {
    inner: ChainHardforks,
}

impl IrysChainHardforks {
    pub fn new(_config: IrysHardforksInConfig) -> Self {
        let mut hardforks = ChainHardforks::default();
        let forks = vec![
            // frontier always enabled
            (IrysHardfork::Frontier, ForkCondition::ZERO_BLOCK),
        ];

        // todo: once we have new hardforks, add them conditionally by reading the `config: IrysHardforksInConfig` variable
        // and append them to the vec of `forks`

        for (fork, condition) in forks {
            hardforks.insert(fork, condition);
            for (corresponding_eth_hf, fork_condition) in fork.ethereum_hardfork_mapping() {
                hardforks.insert(*corresponding_eth_hf, *fork_condition);
            }
        }

        Self { inner: hardforks }
    }

    /// Returns a reference to the underlying [`ChainHardforks`].
    pub fn as_chain_hardforks(&self) -> &ChainHardforks {
        &self.inner
    }

    /// Consumes the wrapper and returns the underlying [`ChainHardforks`].
    pub fn into_chain_hardforks(self) -> ChainHardforks {
        self.inner
    }

    /// Returns a mutable reference to the inner [`ChainHardforks`].
    pub fn inner_mut(&mut self) -> &mut ChainHardforks {
        &mut self.inner
    }
}

impl IrysHardfork {
    /// Find which relevant Ethereum hardforks (for evm compatibility) are enabled by a given irys hardfork
    pub fn ethereum_hardfork_mapping(&self) -> &'static [(EthereumHardfork, ForkCondition)] {
        match self {
            Self::Frontier => &[
                (EthereumHardfork::Frontier, ForkCondition::ZERO_BLOCK),
                (EthereumHardfork::Homestead, ForkCondition::ZERO_BLOCK),
                (EthereumHardfork::Dao, ForkCondition::ZERO_BLOCK),
                (EthereumHardfork::Tangerine, ForkCondition::ZERO_BLOCK),
                (EthereumHardfork::SpuriousDragon, ForkCondition::ZERO_BLOCK),
                (EthereumHardfork::Byzantium, ForkCondition::ZERO_BLOCK),
                (EthereumHardfork::Constantinople, ForkCondition::ZERO_BLOCK),
                (EthereumHardfork::Petersburg, ForkCondition::ZERO_BLOCK),
                (EthereumHardfork::Istanbul, ForkCondition::ZERO_BLOCK),
                (EthereumHardfork::MuirGlacier, ForkCondition::ZERO_BLOCK),
                (EthereumHardfork::Berlin, ForkCondition::ZERO_BLOCK),
                (EthereumHardfork::London, ForkCondition::ZERO_BLOCK),
                // PoW -> PoS hardfork; allows us to properly use the Engine API to drive consensus
                (
                    EthereumHardfork::Paris,
                    ForkCondition::TTD {
                        activation_block_number: 0,
                        fork_block: Some(0),
                        total_difficulty: U256::ZERO,
                    },
                ),
                // all hardforks after Paris use timestamps
                (EthereumHardfork::Shanghai, ForkCondition::ZERO_TIMESTAMP),
                (EthereumHardfork::Cancun, ForkCondition::ZERO_TIMESTAMP),
                (EthereumHardfork::Prague, ForkCondition::ZERO_TIMESTAMP),
            ],
        }
    }
}

pub fn irys_chain_spec(chain: Chain, genesis: Genesis) -> eyre::Result<Arc<ChainSpec>> {
    // ensure that the genesis config does not contain any evm hardforks embedded inside of it.
    // we rely on our own hardforks for managing these
    eyre::ensure!(genesis.config.homestead_block.is_none());
    eyre::ensure!(genesis.config.dao_fork_block.is_none());
    eyre::ensure!(genesis.config.eip150_block.is_none());
    eyre::ensure!(genesis.config.eip155_block.is_none());
    eyre::ensure!(genesis.config.byzantium_block.is_none());
    eyre::ensure!(genesis.config.constantinople_block.is_none());
    eyre::ensure!(genesis.config.petersburg_block.is_none());
    eyre::ensure!(genesis.config.istanbul_block.is_none());
    eyre::ensure!(genesis.config.muir_glacier_block.is_none());
    eyre::ensure!(genesis.config.berlin_block.is_none());
    eyre::ensure!(genesis.config.london_block.is_none());
    eyre::ensure!(genesis.config.arrow_glacier_block.is_none());
    eyre::ensure!(genesis.config.gray_glacier_block.is_none());
    eyre::ensure!(genesis.config.terminal_total_difficulty.is_none());
    eyre::ensure!(genesis.config.merge_netsplit_block.is_none());
    eyre::ensure!(genesis.config.merge_netsplit_block.is_none());
    eyre::ensure!(genesis.config.shanghai_time.is_none());
    eyre::ensure!(genesis.config.cancun_time.is_none());
    eyre::ensure!(genesis.config.prague_time.is_none());

    // as per reth docs on the Genesis struct - these fields should remain as None
    eyre::ensure!(genesis.base_fee_per_gas.is_none());
    eyre::ensure!(genesis.excess_blob_gas.is_none());
    eyre::ensure!(genesis.blob_gas_used.is_none());
    eyre::ensure!(genesis.number.is_none());

    /// prune delete limit (matches Ethereum mainnet)
    const MAINNET_PRUNE_DELETE_LIMIT: usize = 20000;

    let hardforks = {
        let hardforks = genesis
            .config
            .extra_fields
            .deserialize_as::<IrysHardforksInConfig>()?;
        IrysChainHardforks::new(hardforks)
    };
    let genesis_header = make_genesis_header(&genesis, &hardforks.inner);
    let header_hash = genesis_header.hash_slow();
    let chainspec = ChainSpec {
        chain,
        genesis,
        paris_block_and_final_difficulty: Some((0, U256::from(0))),
        hardforks: hardforks.inner,
        deposit_contract: None,
        base_fee_params: BaseFeeParamsKind::Constant(BaseFeeParams::ethereum()),
        prune_delete_limit: MAINNET_PRUNE_DELETE_LIMIT,
        genesis_header: SealedHeader::new(genesis_header, header_hash),
        // Blobs are useful for when other L2s will be built on top of irys, so irys can be treated as a data availability layer
        // But it requires irys to gossip blob sidecars.
        blob_params: BlobScheduleBlobParams::mainnet(),
    };

    Ok(Arc::new(chainspec))
}
