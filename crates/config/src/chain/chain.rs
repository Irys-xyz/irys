use alloy_core::hex;
use alloy_core::primitives::U256;
use alloy_eips::eip1559::ETHEREUM_BLOCK_GAS_LIMIT_30M;
use alloy_eips::BlobScheduleBlobParams;
use alloy_genesis::{Genesis, GenesisAccount};
use irys_types::{Address, IrysBlockHeader};
use reth_chainspec::EthereumHardfork::{
    ArrowGlacier, Berlin, Byzantium, Cancun, Constantinople, Dao, Frontier, GrayGlacier, Homestead,
    Istanbul, London, MuirGlacier, Paris, Petersburg, Prague, Shanghai, SpuriousDragon, Tangerine,
};
use reth_chainspec::{
    make_genesis_header, BaseFeeParams, BaseFeeParamsKind, Chain, ChainSpec, ForkCondition,
    MAINNET_PRUNE_DELETE_LIMIT,
};
use reth_primitives_traits::SealedHeader;
use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};

pub const IRYS_TESTNET_CHAIN_ID: u64 = 1270;

pub static IRYS_TESTNET: LazyLock<Arc<ChainSpec>> = LazyLock::new(|| {
    // TODO: A proper genesis block

    let genesis = Genesis {
        gas_limit: ETHEREUM_BLOCK_GAS_LIMIT_30M,
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
            map.insert(
                Address::from_slice(
                    hex::decode("A93225CBf141438629f1bd906A31a1c5401CE924")
                        .unwrap()
                        .as_slice(),
                ),
                GenesisAccount {
                    balance: U256::from(1_000_000_000_000_000_000_000_000_u128),
                    ..Default::default()
                },
            );
            map
        },
        ..Default::default()
    };

    let hardforks = [
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
                activation_block_number: 0,
                fork_block: Some(0),
                total_difficulty: U256::ZERO,
            },
        ),
        // make sure we use the same fork condition types as EthereumHardfork! if you use Block for a timestamp fork, things will go sideways
        (Shanghai, ForkCondition::Timestamp(0)),
        (Cancun, ForkCondition::Timestamp(0)),
        (Prague, ForkCondition::Timestamp(0)),
    ]
    .into();

    let mut spec = ChainSpec {
        chain: Chain::from_id(IRYS_TESTNET_CHAIN_ID),
        genesis_header: SealedHeader::new_unhashed(make_genesis_header(&genesis, &hardforks)),
        // or: (for overriding the hash)
        // SealedHeader::new(
        //    make_genesis_header(&genesis, &hardforks),
        //    MAINNET_GENESIS_HASH,
        //),
        genesis: genesis,

        paris_block_and_final_difficulty: None,
        hardforks,
        deposit_contract: None,
        base_fee_params: BaseFeeParamsKind::Constant(BaseFeeParams::ethereum()),
        prune_delete_limit: MAINNET_PRUNE_DELETE_LIMIT,
        blob_params: BlobScheduleBlobParams::default(),
    };
    spec.genesis.config.dao_fork_support = false;
    spec.into()
});

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IrysChainSpec {
    irys_genesis: IrysBlockHeader,
}
