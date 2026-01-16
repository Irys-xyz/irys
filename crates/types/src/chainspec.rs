//! Irys-specific hardfork utilities and ChainSpec helpers.
//!
//! ## Adding new hardforks
//! 1. Add an entry to the `hardfork!()` macro call. This creates an enum entry.
//! 2. Add the hardfork parameters to `IrysHardforkConfig` in `hardfork_config.rs`
//! 3. If this hardfork maps to any Ethereum hardforks, add a corresponding entry to `ethereum_hardfork_mapping()`

use std::sync::Arc;

use alloy_eips::BlobScheduleBlobParams;
use alloy_genesis::{ChainConfig, Genesis};
use alloy_primitives::{Address, B256, U256};
use reth_chainspec::{
    hardfork, make_genesis_header, BaseFeeParams, BaseFeeParamsKind, Chain, ChainHardforks,
    ChainSpec, EthereumHardfork, ForkCondition,
};
use reth_primitives_traits::SealedHeader;

use crate::config::consensus::IrysRethConfig;
use crate::hardfork_config::IrysHardforkConfig;

hardfork!(
    #[derive(serde::Serialize, serde::Deserialize)]
    IrysHardfork {
        Frontier,
        NextNameTBD,
        Sprite,
        Aurora
    }
);

/// Hardfork schedule wrapper used across Irys components.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IrysChainHardforks {
    inner: ChainHardforks,
}

impl IrysChainHardforks {
    /// Create a new hardfork schedule from the Irys hardfork configuration.
    pub fn new(config: &IrysHardforkConfig) -> Self {
        let mut hardforks = ChainHardforks::default();
        let mut forks = vec![
            // frontier always enabled from timestamp 0
            (IrysHardfork::Frontier, ForkCondition::ZERO_TIMESTAMP),
        ];

        // Conditionally add NextNameTBD hardfork if configured
        if let Some(ref fork) = config.next_name_tbd {
            forks.push((
                IrysHardfork::NextNameTBD,
                ForkCondition::Timestamp(fork.activation_timestamp.as_secs()),
            ));
        }

        // Conditionally add Sprite hardfork if configured
        if let Some(ref fork) = config.sprite {
            forks.push((
                IrysHardfork::Sprite,
                ForkCondition::Timestamp(fork.activation_timestamp.as_secs()),
            ));
        }

        // Conditionally add Aurora hardfork if configured
        if let Some(ref fork) = config.aurora {
            forks.push((
                IrysHardfork::Aurora,
                ForkCondition::Timestamp(fork.activation_timestamp.as_secs()),
            ));
        }

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
            Self::NextNameTBD => &[],
            // Sprite doesn't add any new Ethereum hardforks (PD is Irys-specific)
            Self::Sprite => &[],
            // Aurora doesn't add any new Ethereum hardforks
            Self::Aurora => &[],
        }
    }
}

/// Build a reth `ChainSpec` from Irys configuration.
pub fn irys_chain_spec(
    chain_id: u64,
    reth_config: &IrysRethConfig,
    hardforks: &IrysHardforkConfig,
    timestamp: u64,
) -> eyre::Result<Arc<ChainSpec>> {
    /// Prune delete limit (matches Ethereum mainnet)
    const MAINNET_PRUNE_DELETE_LIMIT: usize = 20000;

    // Build hardfork schedule from Irys hardfork config
    let chain_hardforks = IrysChainHardforks::new(hardforks);

    // Construct Genesis internally with sensible defaults
    // All Ethereum hardfork fields default to None (we manage hardforks separately)
    let genesis = Genesis {
        config: ChainConfig {
            chain_id,
            // All hardfork fields default to None - Irys manages hardforks via IrysChainHardforks
            ..Default::default()
        },
        gas_limit: reth_config.gas_limit,
        alloc: reth_config.alloc.clone(),
        timestamp,
        nonce: 0,
        difficulty: U256::ZERO,
        mix_hash: B256::ZERO,
        coinbase: Address::ZERO,
        extra_data: Default::default(),
        // These fields should always be None for genesis
        base_fee_per_gas: None,
        excess_blob_gas: None,
        blob_gas_used: None,
        number: None,
        parent_hash: None,
    };

    let genesis_header = make_genesis_header(&genesis, &chain_hardforks.inner);
    let header_hash = genesis_header.hash_slow();

    let chainspec = ChainSpec {
        chain: Chain::from_id(chain_id),
        genesis,
        paris_block_and_final_difficulty: Some((0, U256::from(0))),
        hardforks: chain_hardforks.inner,
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
