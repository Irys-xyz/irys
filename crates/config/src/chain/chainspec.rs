use irys_types::{Config, IrysBlockHeader};
use reth_chainspec::{ChainSpec, ChainSpecBuilder};
use tracing::debug;

use super::chain::IRYS_TESTNET;

/// A helper to build custom chain specs
#[derive(Debug, Default, Clone)]
pub struct IrysChainSpecBuilder {
    pub reth_builder: ChainSpecBuilder,
    pub genesis: IrysBlockHeader,
}

impl IrysChainSpecBuilder {
    /// Construct a new builder from the mainnet chain spec.
    pub fn from_config(config: &Config) -> Self {
        let genesis = IrysBlockHeader {
            oracle_irys_price: config.consensus.genesis_price,
            ema_irys_price: config.consensus.genesis_price,
            miner_address: config.node_config.miner_address(),
            reward_address: config.node_config.miner_address(),
            height: 0,
            system_ledgers: vec![], // Make sure theres no invalid txids in the system ledger
            // todo: we need a proper genesis block in the config rather than re-using a mock header
            ..IrysBlockHeader::new_mock_header()
        };
        Self {
            reth_builder: ChainSpecBuilder {
                chain: Some(config.consensus.reth.chain.clone()),
                genesis: Some(config.consensus.reth.genesis.clone()),
                hardforks: IRYS_TESTNET.hardforks.clone(),
            },
            genesis,
        }
    }

    // build the chainspec and the Irys genesis block
    pub fn build(&self) -> (ChainSpec, IrysBlockHeader) {
        let mut cs = self.reth_builder.clone().build();
        let mut genesis = self.genesis.clone();
        // cs.genesis_hash = once_cell::sync::OnceCell::with_value(B256::from_slice(
        //     &hex::decode("26deb95629271c51e1068e09733d9f71f438088ccc0541bfb7c886e0e4cee35a")
        //         .unwrap(),
        // ));
        cs.genesis_hash = once_cell::sync::OnceCell::with_value(
            reth_primitives::revm_primitives::B256::from_slice(
                &hex::decode("f86f1062e7193b04459defeb16ad26efb492c857eb451aa8a09ea58f50eff501")
                    .unwrap(),
            ),
        );

        genesis.evm_block_hash = cs.genesis_hash();
        debug!("EVM genesis block hash: {}", &genesis.evm_block_hash);
        (cs, genesis)
    }
}
