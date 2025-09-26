//! Irys-specific hardfork utilities and ChainSpec helpers.

use alloy_genesis::Genesis;
use alloy_primitives::U256;
use reth_chainspec::{
    hardfork, Chain, ChainHardforks, ChainSpec, ChainSpecBuilder, EthereumHardfork, ForkCondition,
};
use reth_ethereum_forks::Hardfork;

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
    /// Creates a hardfork schedule from an existing [`ChainHardforks`].
    pub fn from_chain_hardforks(inner: ChainHardforks) -> Self {
        Self { inner }
    }

    /// Returns a reference to the underlying [`ChainHardforks`].
    pub fn as_chain_hardforks(&self) -> &ChainHardforks {
        &self.inner
    }

    /// Consumes the wrapper and returns the underlying [`ChainHardforks`].
    pub fn into_chain_hardforks(self) -> ChainHardforks {
        self.inner
    }

    /// Returns a hardfork configuration matching the current Irys mainnet expectations.
    pub fn irys_mainnet() -> Self {
        Self::irys_testnet()
    }

    /// Returns a hardfork configuration for the Irys testnet (all forks active immediately).
    pub fn irys_testnet() -> Self {
        let mut hardforks = ChainHardforks::default();

        for (fork, condition) in [
            // add new hardforks here
            (IrysHardfork::Frontier, ForkCondition::Block(0)),
        ] {
            hardforks.insert(fork, condition);
        }

        Self { inner: hardforks }
    }

    /// Returns a mutable reference to the inner [`ChainHardforks`].
    pub fn inner_mut(&mut self) -> &mut ChainHardforks {
        &mut self.inner
    }
}

impl reth_chainspec::EthereumHardforks for IrysChainHardforks {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        // all Ethereum hardforks are enabled by default
        match fork {
            EthereumHardfork::Frontier => ForkCondition::ZERO_BLOCK,
            EthereumHardfork::Homestead => ForkCondition::ZERO_BLOCK,
            EthereumHardfork::Dao => ForkCondition::ZERO_BLOCK,
            EthereumHardfork::Tangerine => ForkCondition::ZERO_BLOCK,
            EthereumHardfork::SpuriousDragon => ForkCondition::ZERO_BLOCK,
            EthereumHardfork::Byzantium => ForkCondition::ZERO_BLOCK,
            EthereumHardfork::Constantinople => ForkCondition::ZERO_BLOCK,
            EthereumHardfork::Petersburg => ForkCondition::ZERO_BLOCK,
            EthereumHardfork::Istanbul => ForkCondition::ZERO_BLOCK,
            EthereumHardfork::MuirGlacier => ForkCondition::ZERO_BLOCK,
            EthereumHardfork::Berlin => ForkCondition::ZERO_BLOCK,
            EthereumHardfork::London => ForkCondition::ZERO_BLOCK,
            EthereumHardfork::ArrowGlacier => ForkCondition::ZERO_BLOCK,
            EthereumHardfork::GrayGlacier => ForkCondition::ZERO_BLOCK,
            EthereumHardfork::Paris => ForkCondition::ZERO_BLOCK,
            EthereumHardfork::Shanghai => ForkCondition::ZERO_BLOCK,
            EthereumHardfork::Cancun => ForkCondition::ZERO_BLOCK,
            EthereumHardfork::Prague => ForkCondition::ZERO_BLOCK,
            EthereumHardfork::Osaka => ForkCondition::ZERO_BLOCK,
            // as we upgrade reth, we may run into having to enable some of the existing hardforks
        }
    }
}
