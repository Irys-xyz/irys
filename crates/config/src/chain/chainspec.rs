use alloy_primitives::B256;
use irys_types::{
    partition::PartitionHash, Config, DataTransactionLedger, H256List, IrysBlockHeader,
    IrysSignature, PoaData, Signature, VDFLimiterInfo, H256, U256,
};
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
            block_hash: H256::zero(),
            signature: IrysSignature::new(Signature::test_signature()),
            height: 0,
            diff: U256::from(0),
            cumulative_diff: U256::from(0),
            solution_hash: H256::zero(),
            last_diff_timestamp: 0,
            previous_solution_hash: H256::zero(),
            last_epoch_hash: config.consensus.genesis.last_epoch_hash,
            chunk_hash: H256::zero(),
            previous_block_hash: H256::zero(),
            previous_cumulative_diff: U256::from(0),
            poa: PoaData {
                partition_chunk_offset: 0,
                partition_hash: PartitionHash::zero(),
                chunk: None,
                ledger_id: None,
                tx_path: None,
                data_path: None,
            },
            reward_address: config.consensus.genesis.reward_address,
            reward_amount: U256::from(0),
            miner_address: config.consensus.genesis.miner_address,
            timestamp: config.consensus.genesis.timestamp_millis,
            system_ledgers: vec![],
            data_ledgers: vec![
                DataTransactionLedger {
                    ledger_id: 0,
                    tx_root: H256::zero(),
                    tx_ids: H256List::new(),
                    max_chunk_offset: 0,
                    expires: None,
                    proofs: None,
                },
                DataTransactionLedger {
                    ledger_id: 1,
                    tx_root: H256::zero(),
                    tx_ids: H256List::new(),
                    max_chunk_offset: 0,
                    expires: None,
                    proofs: None,
                },
            ],
            evm_block_hash: B256::ZERO,
            vdf_limiter_info: VDFLimiterInfo {
                output: H256::zero(),
                global_step_number: 0,
                seed: config.consensus.genesis.vdf_seed,
                next_seed: config
                    .consensus
                    .genesis
                    .vdf_next_seed
                    .unwrap_or(config.consensus.genesis.vdf_seed),
                prev_output: H256::zero(),
                last_step_checkpoints: H256List::new(),
                steps: H256List::new(),
                vdf_difficulty: None,
                next_vdf_difficulty: None,
            },
            oracle_irys_price: config.consensus.genesis_price,
            ema_irys_price: config.consensus.genesis_price,
            treasury: U256::zero(), // Treasury will be set when genesis commitments are added
        };
        Self {
            reth_builder: ChainSpecBuilder::mainnet()
                .chain(config.consensus.reth.chain)
                .genesis(config.consensus.reth.genesis.clone())
                .with_forks(IRYS_TESTNET.hardforks.clone()),
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
}
