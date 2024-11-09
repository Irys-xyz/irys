use irys_primitives::U256;
// from ext/reth/crates/ethereum/cli/src/chainspec.rs
// TODO @JesseTheRobot - remove this - we just needed this to satisfy the CLI
use irys_types::IrysBlockHeader;
use once_cell::sync::{Lazy, OnceCell};
use reth_chainspec::EthereumHardfork::{
    ArrowGlacier, Berlin, Byzantium, Cancun, Constantinople, Dao, Frontier, GrayGlacier, Homestead,
    Istanbul, London, MuirGlacier, Paris, Petersburg, Shanghai, SpuriousDragon, Tangerine,
};
use reth_chainspec::{
    BaseFeeParams, BaseFeeParamsKind, Chain, ChainSpec, EthereumHardfork, ForkCondition,
};
use reth_cli::chainspec::{parse_genesis, ChainSpecParser};
use reth_primitives::constants::ETHEREUM_BLOCK_GAS_LIMIT;
use std::sync::Arc;
use tracing::error;

/// Irys chain specification parser.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct IrysChainSpecParser;

pub const SUPPORTED_CHAINS: &[&str] = &["mainnet" /* , "devnet", "testnet" */];

impl ChainSpecParser for IrysChainSpecParser {
    type ChainSpec = ChainSpec;

    const SUPPORTED_CHAINS: &'static [&'static str] = SUPPORTED_CHAINS;

    fn parse(s: &str) -> eyre::Result<Arc<ChainSpec>> {
        chain_value_parser(s)
    }
}

pub fn chain_value_parser(s: &str) -> eyre::Result<Arc<ChainSpec>, eyre::Error> {
    Ok(match s {
        "mainnet" => IRYS_MAINNET.clone(),
        // "sepolia" => SEPOLIA.clone(),
        // "holesky" => HOLESKY.clone(),
        // "dev" => DEV.clone(),
        _ => match parse_genesis(s) {
            Ok(genesis) => Arc::new(genesis.into()),
            Err(e) => {
                error!("error parsing chainspec: {}", e);
                IRYS_MAINNET.clone()
            }
        },
    })
}

/// note: for testing this is overriden
pub static IRYS_MAINNET: Lazy<Arc<ChainSpec>> = Lazy::new(|| {
    let mut spec = ChainSpec {
        chain: Chain::mainnet(),
        genesis: Default::default(), /* serde_json::from_str(include_str!("../res/genesis/mainnet.json"))
                                     .expect("Can't deserialize Mainnet genesis json"), */
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

// pub static MAINNET: Lazy<Arc<ChainSpec>> = Lazy::new(|| {
//     let mut spec = ChainSpec {
//         chain: Chain::mainnet(),
//         genesis: serde_json::from_str(include_str!("../res/genesis/mainnet.json"))
//             .expect("Can't deserialize Mainnet genesis json"),
//         genesis_hash: once_cell_set(MAINNET_GENESIS_HASH),
//         genesis_header: Default::default(),
//         // <https://etherscan.io/block/15537394>
//         paris_block_and_final_difficulty: Some((
//             15537394,
//             U256::from(58_750_003_716_598_352_816_469u128),
//         )),
//         hardforks: EthereumHardfork::mainnet().into(),
//         // https://etherscan.io/tx/0xe75fb554e433e03763a1560646ee22dcb74e5274b34c5ad644e7c0f619a7e1d0
//         deposit_contract: Some(DepositContract::new(
//             address!("00000000219ab540356cbb839cbe05303d7705fa"),
//             11052984,
//             b256!("649bbc62d0e31342afea4e5cd82d4049e7e1ee912fc0889aa790803be39038c5"),
//         )),
//         base_fee_params: BaseFeeParamsKind::Constant(BaseFeeParams::ethereum()),
//         max_gas_limit: ETHEREUM_BLOCK_GAS_LIMIT,
//         prune_delete_limit: 20000,
//     };
//     spec.genesis.config.dao_fork_support = true;
//     spec.into()
// });

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
