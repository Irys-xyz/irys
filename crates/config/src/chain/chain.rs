use irys_primitives::{Genesis, GenesisAccount, U256};
use irys_types::{Address, IrysBlockHeader, CONFIG};
use once_cell::sync::{Lazy, OnceCell};
use reth_chainspec::EthereumHardfork::{
    ArrowGlacier, Berlin, Byzantium, Cancun, Constantinople, Dao, Frontier, GrayGlacier, Homestead,
    Istanbul, London, MuirGlacier, Paris, Petersburg, Shanghai, SpuriousDragon, Tangerine,
};
use reth_chainspec::{BaseFeeParams, BaseFeeParamsKind, Chain, ChainSpec, ForkCondition};
use reth_primitives::constants::ETHEREUM_BLOCK_GAS_LIMIT;
use reth_primitives::revm_primitives::hex;
use std::collections::BTreeMap;
use std::sync::Arc;

pub const SUPPORTED_CHAINS: &[&str] = &["mainnet" /* , "devnet", "testnet" */];

/// note: for testing this is overriden
pub static IRYS_MAINNET: Lazy<Arc<ChainSpec>> = Lazy::new(|| {
    let mut spec = ChainSpec {
        chain: Chain::from_id(CONFIG.irys_chain_id),
        // TODO: A proper genesis block
        genesis: Genesis {
            gas_limit: ETHEREUM_BLOCK_GAS_LIMIT,
            alloc: {
                let mut map = BTreeMap::new();
                map.insert(
                    Address::from_slice(
                        hex::decode("64f1a2829e0e698c18e7792d6e74f67d89aa0a32")
                            .unwrap()
                            .as_slice(),
                    ),
                    GenesisAccount {
                        balance: U256::from(690000000000000000_u128),
                        ..Default::default()
                    },
                );
                map
            },
            ..Default::default()
        },
        genesis_hash: OnceCell::new(),
        genesis_header: Default::default(),
        paris_block_and_final_difficulty: None,
        hardforks: [
            (Frontier, ForkCondition::Block(0)),
            (Homestead, ForkCondition::Block(0)),
            (Dao, ForkCondition::Block(0)),
            (Tangerine, ForkCondition::Block(0)),
            (SpuriousDragon, ForkCondition::Block(0)),
            (Byzantium, ForkCondition::Block(0)),
            (Constantinople, ForkCondition::Block(0)),
            (Petersburg, ForkCondition::Block(0)),
            (Istanbul, ForkCondition::Block(0)),
            (MuirGlacier, ForkCondition::Block(0)),
            (Berlin, ForkCondition::Block(0)),
            (London, ForkCondition::Block(0)),
            (ArrowGlacier, ForkCondition::Block(0)),
            (GrayGlacier, ForkCondition::Block(0)),
            (
                Paris,
                ForkCondition::TTD {
                    fork_block: Some(0),
                    total_difficulty: U256::ZERO,
                },
            ),
            (Shanghai, ForkCondition::Block(0)),
            (Cancun, ForkCondition::Block(0)),
        ]
        .into(),
        deposit_contract: None,
        base_fee_params: BaseFeeParamsKind::Constant(BaseFeeParams::ethereum()),
        max_gas_limit: ETHEREUM_BLOCK_GAS_LIMIT,
        prune_delete_limit: 20000,
    };
    spec.genesis.config.dao_fork_support = false;
    spec.into()
});

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IrysChainSpec {
    irys_genesis: IrysBlockHeader,
}

impl IrysChainSpec {
    fn new() -> Self {
        Self {
            irys_genesis: IrysBlockHeader::new(),
        }
    }
}
