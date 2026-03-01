use alloy_primitives::B256;
use irys_types::{
    DataLedger, DataTransactionLedger, GenesisConfig, H256, H256List, IrysBlockHeader,
    IrysBlockHeaderV1, IrysSignature, PoaData, U256, UnixTimestamp, UnixTimestampMs,
    VDFLimiterInfo, hardfork_config::Cascade, partition::PartitionHash,
};

pub fn build_unsigned_irys_genesis_block(
    config: &GenesisConfig,
    evm_block_hash: B256,
    number_of_ingress_proofs_total: u64,
    cascade: Option<&Cascade>,
) -> IrysBlockHeader {
    let mut data_ledgers = vec![
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
    ];
    // Only include OneYear/ThirtyDay ledgers if Cascade activates at genesis (timestamp 0)
    if let Some(cascade) = cascade.filter(|c| c.activation_timestamp == UnixTimestamp::from_secs(0))
    {
        data_ledgers.push(DataTransactionLedger {
            ledger_id: DataLedger::OneYear.into(),
            tx_root: H256::zero(),
            tx_ids: H256List::new(),
            total_chunks: 0,
            expires: Some(cascade.one_year_epoch_length),
            proofs: None,
            required_proof_count: None,
        });
        data_ledgers.push(DataTransactionLedger {
            ledger_id: DataLedger::ThirtyDay.into(),
            tx_root: H256::zero(),
            tx_ids: H256List::new(),
            total_chunks: 0,
            expires: Some(cascade.thirty_day_epoch_length),
            proofs: None,
            required_proof_count: None,
        });
    }
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
        data_ledgers,
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

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::config::consensus::ConsensusConfig;

    fn genesis_config() -> GenesisConfig {
        ConsensusConfig::testing().genesis
    }

    #[test]
    fn test_genesis_block_with_cascade_at_genesis_has_four_ledgers() {
        let cascade = Cascade {
            activation_timestamp: UnixTimestamp::from_secs(0),
            one_year_epoch_length: 365,
            thirty_day_epoch_length: 30,
            annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
        };
        let block =
            build_unsigned_irys_genesis_block(&genesis_config(), B256::ZERO, 2, Some(&cascade));
        assert_eq!(block.data_ledgers.len(), 4);
        assert_eq!(block.data_ledgers[0].ledger_id, DataLedger::Publish as u32);
        assert_eq!(block.data_ledgers[1].ledger_id, DataLedger::Submit as u32);
        assert_eq!(block.data_ledgers[2].ledger_id, DataLedger::OneYear as u32);
        assert_eq!(
            block.data_ledgers[3].ledger_id,
            DataLedger::ThirtyDay as u32
        );
        assert_eq!(block.data_ledgers[2].expires, Some(365));
        assert_eq!(block.data_ledgers[3].expires, Some(30));
        assert!(block.data_ledgers[2].proofs.is_none());
        assert!(block.data_ledgers[3].required_proof_count.is_none());
    }

    #[test]
    fn test_genesis_block_with_cascade_not_at_genesis_has_two_ledgers() {
        let cascade = Cascade {
            activation_timestamp: UnixTimestamp::from_secs(1000),
            one_year_epoch_length: 365,
            thirty_day_epoch_length: 30,
            annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
        };
        let block =
            build_unsigned_irys_genesis_block(&genesis_config(), B256::ZERO, 2, Some(&cascade));
        assert_eq!(block.data_ledgers.len(), 2);
    }

    #[test]
    fn test_genesis_block_without_cascade_has_two_ledgers() {
        let block = build_unsigned_irys_genesis_block(&genesis_config(), B256::ZERO, 2, None);
        assert_eq!(block.data_ledgers.len(), 2);
        assert_eq!(block.data_ledgers[0].ledger_id, DataLedger::Publish as u32);
        assert_eq!(block.data_ledgers[1].ledger_id, DataLedger::Submit as u32);
    }
}
