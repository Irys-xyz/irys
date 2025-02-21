use irys_primitives::GenesisAccount;
use irys_types::{Address, IrysBlockHeader};
use reth_chainspec::{ChainSpec, ChainSpecBuilder};
use tracing::debug;

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
        let mut genesis = IrysBlockHeader::new_mock_header();
        genesis.height = 0;
        Self {
            reth_builder: ChainSpecBuilder {
                chain: Some(IRYS_MAINNET.chain),
                genesis: Some(IRYS_MAINNET.genesis.clone()),
                hardforks: IRYS_MAINNET.hardforks.clone(),
            },
            genesis,
        }
    }

    // build the chainspec and the Irys genesis block
    pub fn build(&self) -> (ChainSpec, IrysBlockHeader) {
        let cs = self.reth_builder.clone().build();
        let mut genesis = self.genesis.clone();
        genesis.evm_block_hash = cs.genesis_hash();
        debug!("EVM genesis block hash: {}", &genesis.evm_block_hash);
        (cs, genesis)
    }

    pub fn new() -> Self {
        Default::default()
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
