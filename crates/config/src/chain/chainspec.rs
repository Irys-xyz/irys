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
) -> eyre::Result<IrysBlockHeader> {
    let proof_count = u8::try_from(number_of_ingress_proofs_total).map_err(|_| {
        eyre::eyre!(
            "number_of_ingress_proofs_total ({}) exceeds u8::MAX (255)",
            number_of_ingress_proofs_total
        )
    })?;

    let mut data_ledgers = vec![
        DataTransactionLedger {
            ledger_id: DataLedger::Publish.into(),
            tx_root: H256::zero(),
            tx_ids: H256List::new(),
            total_chunks: 0,
            expires: None,
            proofs: None,
            required_proof_count: Some(proof_count),
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
    Ok(IrysBlockHeader::V1(IrysBlockHeaderV1 {
        block_hash: H256::zero(),
        signature: IrysSignature::default(),
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
        treasury: U256::zero(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::{Amount, Decimal, IrysAddress};
    use proptest::prelude::*;

    fn test_genesis_config() -> GenesisConfig {
        GenesisConfig {
            timestamp_millis: 0,
            miner_address: IrysAddress::ZERO,
            reward_address: IrysAddress::ZERO,
            last_epoch_hash: H256::zero(),
            vdf_seed: H256::zero(),
            vdf_next_seed: None,
            genesis_price: Amount::token(Decimal::new(15, 2)).expect("valid token amount"),
            initial_packed_partitions: None,
        }
    }

    proptest! {
        #[test]
        fn ingress_proof_count_preserved_for_valid_values(count in 0_u64..=255) {
            let config = test_genesis_config();
            let block = build_unsigned_irys_genesis_block(
                &config,
                B256::ZERO,
                count,
                None,
            ).map_err(|e| TestCaseError::fail(e.to_string()))?;
            let publish_ledger = block
                .data_ledgers
                .iter()
                .find(|l| l.ledger_id == u32::from(DataLedger::Publish))
                .expect("publish ledger should exist");
            prop_assert_eq!(
                publish_ledger.required_proof_count,
                Some(u8::try_from(count).unwrap())
            );
        }
    }

    #[test]
    fn rejects_ingress_proof_count_over_255() {
        let config = test_genesis_config();
        let result = build_unsigned_irys_genesis_block(&config, B256::ZERO, 256, None);
        let err_msg = result
            .expect_err("expected overflow for ingress proof count > 255")
            .to_string();
        assert!(
            err_msg.contains("exceeds u8::MAX"),
            "expected 'exceeds u8::MAX' in error, got: {err_msg}"
        );
    }

    #[test]
    fn genesis_block_has_two_data_ledgers() -> eyre::Result<()> {
        let config = test_genesis_config();
        let block = build_unsigned_irys_genesis_block(&config, B256::ZERO, 5, None)?;
        assert_eq!(block.data_ledgers.len(), 2);
        let submit_ledger = block
            .data_ledgers
            .iter()
            .find(|l| l.ledger_id == u32::from(DataLedger::Submit))
            .expect("submit ledger should exist");
        assert_eq!(submit_ledger.required_proof_count, None);
        Ok(())
    }

    fn genesis_config() -> GenesisConfig {
        use irys_types::config::consensus::ConsensusConfig;
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
            build_unsigned_irys_genesis_block(&genesis_config(), B256::ZERO, 2, Some(&cascade))
                .unwrap();
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
            build_unsigned_irys_genesis_block(&genesis_config(), B256::ZERO, 2, Some(&cascade))
                .unwrap();
        assert_eq!(block.data_ledgers.len(), 2);
    }

    #[test]
    fn test_genesis_block_without_cascade_has_two_ledgers() {
        let block =
            build_unsigned_irys_genesis_block(&genesis_config(), B256::ZERO, 2, None).unwrap();
        assert_eq!(block.data_ledgers.len(), 2);
        assert_eq!(block.data_ledgers[0].ledger_id, DataLedger::Publish as u32);
        assert_eq!(block.data_ledgers[1].ledger_id, DataLedger::Submit as u32);
    }
}
