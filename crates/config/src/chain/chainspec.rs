use alloy_primitives::B256;
use irys_types::{
    DataLedger, DataTransactionLedger, GenesisConfig, H256, H256List, IrysBlockHeader,
    IrysBlockHeaderV1, IrysSignature, PoaData, U256, UnixTimestampMs, VDFLimiterInfo,
    partition::PartitionHash,
};

pub fn build_unsigned_irys_genesis_block(
    config: &GenesisConfig,
    evm_block_hash: B256,
    number_of_ingress_proofs_total: u64,
) -> IrysBlockHeader {
    IrysBlockHeader::V1(IrysBlockHeaderV1 {
        block_hash: H256::zero(),
        signature: IrysSignature::default(), // Empty signature to be replaced by actual signing
        height: 0,
        diff: U256::from(0),
        cumulative_diff: U256::from(0),
        solution_hash: H256::zero(),
        last_diff_timestamp: UnixTimestampMs::from_millis(0),
        previous_solution_hash: H256::zero(),
        last_epoch_hash: config.last_epoch_hash,
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
        reward_address: config.reward_address,
        reward_amount: U256::from(0),
        miner_address: config.miner_address,
        timestamp: UnixTimestampMs::from_millis(config.timestamp_millis),
        system_ledgers: vec![],
        data_ledgers: vec![
            DataTransactionLedger {
                ledger_id: DataLedger::Publish.into(),
                tx_root: H256::zero(),
                tx_ids: H256List::new(),
                total_chunks: 0,
                expires: None,
                proofs: None,
                required_proof_count: Some(number_of_ingress_proofs_total as u8),
            },
            DataTransactionLedger {
                ledger_id: DataLedger::Submit.into(),
                tx_root: H256::zero(),
                tx_ids: H256List::new(),
                total_chunks: 0,
                expires: None,
                proofs: None,
                required_proof_count: None,
            },
        ],
        evm_block_hash,
        vdf_limiter_info: VDFLimiterInfo {
            output: H256::zero(),
            global_step_number: 0,
            seed: config.vdf_seed,
            next_seed: config.vdf_next_seed.unwrap_or(config.vdf_seed),
            prev_output: H256::zero(),
            last_step_checkpoints: H256List::new(),
            steps: H256List::new(),
            vdf_difficulty: None,
            next_vdf_difficulty: None,
        },
        oracle_irys_price: config.genesis_price,
        ema_irys_price: config.genesis_price,
        treasury: U256::zero(), // Treasury will be set when genesis commitments are added
    })
}
