use irys_primitives::GenesisAccount;
use irys_types::{Address, IrysBlockHeader};
use reth_chainspec::ChainSpecBuilder;

use super::chain::IRYS_MAINNET;

/// A helper to build custom chain specs
#[derive(Debug, Default, Clone)]
pub struct IrysChainSpecBuilder {
    pub reth_builder: ChainSpecBuilder,
    pub genesis: IrysBlockHeader,
}

impl IrysChainSpecBuilder {
    /// Construct a new builder from the mainnet chain spec.
    pub fn mainnet() -> Self {
        Self {
            reth_builder: ChainSpecBuilder {
                chain: Some(IRYS_MAINNET.chain),
                genesis: Some(IRYS_MAINNET.genesis.clone()),
                hardforks: IRYS_MAINNET.hardforks.clone(),
            },
            genesis: IrysBlockHeader::new(),
        }
    }

    pub fn new() -> Self {
        Self {
            ..Default::default()
        }
    }

    /// extend the genesis accounts
    pub fn extend_accounts(
        &mut self,
        accounts: impl IntoIterator<Item = (Address, GenesisAccount)>,
    ) -> &mut Self {
        let new_genesis = self
            .reth_builder
            .genesis
            .as_ref()
            .unwrap()
            .clone()
            .extend_accounts(accounts);
        self.reth_builder = self.reth_builder.clone().genesis(new_genesis);
        self
    }
}

// impl into

impl IrysChainSpecBuilder {
    // /// Set the chain ID
    // pub const fn chain(mut self, chain: Chain) -> Self {
    //     self.chain = Some(chain);
    //     self
    // }

    // /// Set the genesis block.
    // pub fn genesis(mut self, genesis: Genesis) -> Self {
    //     self.genesis = Some(genesis);
    //     self
    // }

    // /// Add the given fork with the given activation condition to the spec.
    // pub fn with_fork(mut self, fork: EthereumHardfork, condition: ForkCondition) -> Self {
    //     self.hardforks.insert(fork, condition);
    //     self
    // }

    // /// Remove the given fork from the spec.
    // pub fn without_fork(mut self, fork: EthereumHardfork) -> Self {
    //     self.hardforks.remove(fork);
    //     self
    // }

    // /// Enable the Paris hardfork at the given TTD.
    // ///
    // /// Does not set the merge netsplit block.
    // pub fn paris_at_ttd(self, ttd: U256) -> Self {
    //     self.with_fork(
    //         EthereumHardfork::Paris,
    //         ForkCondition::TTD {
    //             total_difficulty: ttd,
    //             fork_block: None,
    //         },
    //     )
    // }

    // /// Enable Frontier at genesis.
    // pub fn frontier_activated(mut self) -> Self {
    //     self.hardforks
    //         .insert(EthereumHardfork::Frontier, ForkCondition::Block(0));
    //     self
    // }

    // /// Enable Homestead at genesis.
    // pub fn homestead_activated(mut self) -> Self {
    //     self = self.frontier_activated();
    //     self.hardforks
    //         .insert(EthereumHardfork::Homestead, ForkCondition::Block(0));
    //     self
    // }

    // /// Enable Tangerine at genesis.
    // pub fn tangerine_whistle_activated(mut self) -> Self {
    //     self = self.homestead_activated();
    //     self.hardforks
    //         .insert(EthereumHardfork::Tangerine, ForkCondition::Block(0));
    //     self
    // }

    // /// Enable Spurious Dragon at genesis.
    // pub fn spurious_dragon_activated(mut self) -> Self {
    //     self = self.tangerine_whistle_activated();
    //     self.hardforks
    //         .insert(EthereumHardfork::SpuriousDragon, ForkCondition::Block(0));
    //     self
    // }

    // /// Enable Byzantium at genesis.
    // pub fn byzantium_activated(mut self) -> Self {
    //     self = self.spurious_dragon_activated();
    //     self.hardforks
    //         .insert(EthereumHardfork::Byzantium, ForkCondition::Block(0));
    //     self
    // }

    // /// Enable Constantinople at genesis.
    // pub fn constantinople_activated(mut self) -> Self {
    //     self = self.byzantium_activated();
    //     self.hardforks
    //         .insert(EthereumHardfork::Constantinople, ForkCondition::Block(0));
    //     self
    // }

    // /// Enable Petersburg at genesis.
    // pub fn petersburg_activated(mut self) -> Self {
    //     self = self.constantinople_activated();
    //     self.hardforks
    //         .insert(EthereumHardfork::Petersburg, ForkCondition::Block(0));
    //     self
    // }

    // /// Enable Istanbul at genesis.
    // pub fn istanbul_activated(mut self) -> Self {
    //     self = self.petersburg_activated();
    //     self.hardforks
    //         .insert(EthereumHardfork::Istanbul, ForkCondition::Block(0));
    //     self
    // }

    // /// Enable Berlin at genesis.
    // pub fn berlin_activated(mut self) -> Self {
    //     self = self.istanbul_activated();
    //     self.hardforks
    //         .insert(EthereumHardfork::Berlin, ForkCondition::Block(0));
    //     self
    // }

    // /// Enable London at genesis.
    // pub fn london_activated(mut self) -> Self {
    //     self = self.berlin_activated();
    //     self.hardforks
    //         .insert(EthereumHardfork::London, ForkCondition::Block(0));
    //     self
    // }

    // /// Enable Paris at genesis.
    // pub fn paris_activated(mut self) -> Self {
    //     self = self.london_activated();
    //     self.hardforks.insert(
    //         EthereumHardfork::Paris,
    //         ForkCondition::TTD {
    //             fork_block: Some(0),
    //             total_difficulty: U256::ZERO,
    //         },
    //     );
    //     self
    // }

    // /// Enable Shanghai at genesis.
    // pub fn shanghai_activated(mut self) -> Self {
    //     self = self.paris_activated();
    //     self.hardforks
    //         .insert(EthereumHardfork::Shanghai, ForkCondition::Timestamp(0));
    //     self
    // }

    // /// Enable Cancun at genesis.
    // pub fn cancun_activated(mut self) -> Self {
    //     self = self.shanghai_activated();
    //     self.hardforks
    //         .insert(EthereumHardfork::Cancun, ForkCondition::Timestamp(0));
    //     self
    // }

    // /// Enable Prague at genesis.
    // pub fn prague_activated(mut self) -> Self {
    //     self = self.cancun_activated();
    //     self.hardforks
    //         .insert(EthereumHardfork::Prague, ForkCondition::Timestamp(0));
    //     self
    // }

    // /// Enable Bedrock at genesis
    // #[cfg(feature = "optimism")]
    // pub fn bedrock_activated(mut self) -> Self {
    //     self = self.paris_activated();
    //     self.hardforks.insert(
    //         reth_optimism_forks::OptimismHardfork::Bedrock,
    //         ForkCondition::Block(0),
    //     );
    //     self
    // }

    // /// Enable Regolith at genesis
    // #[cfg(feature = "optimism")]
    // pub fn regolith_activated(mut self) -> Self {
    //     self = self.bedrock_activated();
    //     self.hardforks.insert(
    //         reth_optimism_forks::OptimismHardfork::Regolith,
    //         ForkCondition::Timestamp(0),
    //     );
    //     self
    // }

    // /// Enable Canyon at genesis
    // #[cfg(feature = "optimism")]
    // pub fn canyon_activated(mut self) -> Self {
    //     self = self.regolith_activated();
    //     // Canyon also activates changes from L1's Shanghai hardfork
    //     self.hardforks
    //         .insert(EthereumHardfork::Shanghai, ForkCondition::Timestamp(0));
    //     self.hardforks.insert(
    //         reth_optimism_forks::OptimismHardfork::Canyon,
    //         ForkCondition::Timestamp(0),
    //     );
    //     self
    // }

    // /// Enable Ecotone at genesis
    // #[cfg(feature = "optimism")]
    // pub fn ecotone_activated(mut self) -> Self {
    //     self = self.canyon_activated();
    //     self.hardforks
    //         .insert(EthereumHardfork::Cancun, ForkCondition::Timestamp(0));
    //     self.hardforks.insert(
    //         reth_optimism_forks::OptimismHardfork::Ecotone,
    //         ForkCondition::Timestamp(0),
    //     );
    //     self
    // }

    // /// Enable Fjord at genesis
    // #[cfg(feature = "optimism")]
    // pub fn fjord_activated(mut self) -> Self {
    //     self = self.ecotone_activated();
    //     self.hardforks.insert(
    //         reth_optimism_forks::OptimismHardfork::Fjord,
    //         ForkCondition::Timestamp(0),
    //     );
    //     self
    // }

    // /// Enable Granite at genesis
    // #[cfg(feature = "optimism")]
    // pub fn granite_activated(mut self) -> Self {
    //     self = self.fjord_activated();
    //     self.hardforks.insert(
    //         reth_optimism_forks::OptimismHardfork::Granite,
    //         ForkCondition::Timestamp(0),
    //     );
    //     self
    // }

    // /// Build the resulting [`ChainSpec`].
    // ///
    // /// # Panics
    // ///
    // /// This function panics if the chain ID and genesis is not set ([`Self::chain`] and
    // /// [`Self::genesis`])
    // pub fn build(self) -> ChainSpec {
    //     let paris_block_and_final_difficulty = {
    //         self.hardforks
    //             .get(EthereumHardfork::Paris)
    //             .and_then(|cond| {
    //                 if let ForkCondition::TTD {
    //                     fork_block,
    //                     total_difficulty,
    //                 } = cond
    //                 {
    //                     fork_block.map(|fork_block| (fork_block, total_difficulty))
    //                 } else {
    //                     None
    //                 }
    //             })
    //     };
    //     ChainSpec {
    //         chain: self.chain.expect("The chain is required"),
    //         genesis: self.genesis.expect("The genesis is required"),
    //         genesis_hash: OnceCell::new(),
    //         hardforks: self.hardforks,
    //         paris_block_and_final_difficulty,
    //         deposit_contract: None,
    //         ..Default::default()
    //     }
    // }
}

// impl From<&Arc<ChainSpec>> for IrysChainSpecBuilder {
//     fn from(value: &Arc<ChainSpec>) -> Self {
//         Self {
//             chain: Some(value.chain),
//             genesis: Some(value.genesis.clone()),
//             hardforks: value.hardforks.clone(),
//         }
//     }
// }
