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
) -> eyre::Result<IrysBlockHeader> {
    let proof_count = u8::try_from(number_of_ingress_proofs_total).map_err(|_| {
        eyre::eyre!(
            "number_of_ingress_proofs_total ({}) exceeds u8::MAX (255)",
            number_of_ingress_proofs_total
        )
    })?;

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
        data_ledgers: vec![
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
        let result = build_unsigned_irys_genesis_block(&config, B256::ZERO, 256);
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
        let block = build_unsigned_irys_genesis_block(&config, B256::ZERO, 5)?;
        assert_eq!(block.data_ledgers.len(), 2);
        let submit_ledger = block
            .data_ledgers
            .iter()
            .find(|l| l.ledger_id == u32::from(DataLedger::Submit))
            .expect("submit ledger should exist");
        assert_eq!(submit_ledger.required_proof_count, None);
        Ok(())
    }
}
