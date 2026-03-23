use alloy_evm::precompiles::{DynPrecompile, PrecompileInput};
use irys_types::precompile::PD_PRECOMPILE_ADDRESS;
use reth_evm::precompiles::PrecompilesMap;
use revm::precompile::PrecompileError;
use revm::precompile::{PrecompileOutput, PrecompileResult};
use revm::primitives::hardfork::SpecId;
use std::borrow::Cow;
use tracing::{debug, warn};

use crate::precompiles::pd::constants::{
    INDEX_OFFSET, LENGTH_FIELD_OFFSET, LENGTH_SIZE, OFFSET_FIELD_OFFSET, OFFSET_SIZE,
    PD_BASE_GAS_COST, READ_BYTES_RANGE_BY_INDEX_CALLDATA_LEN, READ_PARTIAL_BYTE_RANGE_CALLDATA_LEN,
};
use crate::precompiles::pd::error::PdPrecompileError;
use crate::precompiles::pd::functions::PdFunctionId;
use crate::precompiles::pd::read_bytes::read_chunk_data;
use crate::precompiles::pd::utils::parse_pd_specifiers;

use super::context::PdContext;

/// Programmable Data precompile implementation.
#[inline]
fn pd_precompile(pd_context: PdContext) -> DynPrecompile {
    (move |input: PrecompileInput<'_>| -> PrecompileResult {
        let data = input.data;
        let gas_limit = input.gas;
        debug!(
            data_len = data.len(),
            gas_limit = gas_limit,
            "PD precompile called"
        );

        if data.len() < 2 {
            warn!("PD precompile: insufficient input data, len={}", data.len());
            return Err(PdPrecompileError::InsufficientInput {
                expected: 2,
                actual: data.len(),
            }
            .into());
        }

        if PD_BASE_GAS_COST > gas_limit {
            warn!(
                base_cost = PD_BASE_GAS_COST,
                gas_limit = gas_limit,
                "PD precompile: insufficient gas for base cost"
            );
            return Err(revm::precompile::PrecompileError::OutOfGas);
        }

        let access_list = pd_context.read_access_list();
        if access_list.is_empty() {
            warn!("PD precompile: transaction missing required access list");
            return Err(PdPrecompileError::MissingAccessList.into());
        }

        let decoded_id = PdFunctionId::try_from(data[0]).map_err(|e| {
            warn!(function_id = data[0], "PD precompile: unknown function ID");
            PrecompileError::Other(Cow::Owned(format!("PD precompile: {}", e)))
        })?;

        debug!(function_id = ?decoded_id, "PD precompile: decoded function ID");

        let specifiers =
            parse_pd_specifiers(&access_list, &pd_context.chunk_config()).map_err(|e| {
                warn!(error = ?e, "PD precompile: failed to parse access list");
                PdPrecompileError::InvalidAccessList {
                    reason: e.to_string(),
                }
            })?;

        // Note: available_gas is passed for interface compatibility but is intentionally unused.
        // PD chunk I/O is metered via per-chunk IRYS token fees (deducted in
        // IrysEvm::transact_raw), not via EVM gas. The only EVM gas charged is
        // the flat PD_BASE_GAS_COST.
        let _available_gas = gas_limit.saturating_sub(PD_BASE_GAS_COST);

        let result_bytes = match decoded_id {
            PdFunctionId::ReadFullByteRange => {
                // Calldata: [function_id(1), index(1)]
                if data.len() != READ_BYTES_RANGE_BY_INDEX_CALLDATA_LEN {
                    return Err(PdPrecompileError::InvalidCalldataLength {
                        function: "ReadFullByteRange",
                        expected: READ_BYTES_RANGE_BY_INDEX_CALLDATA_LEN,
                        actual: data.len(),
                    }
                    .into());
                }
                let index = data[INDEX_OFFSET];
                let spec =
                    specifiers
                        .get(index as usize)
                        .ok_or(PdPrecompileError::SpecifierNotFound {
                            index,
                            available: specifiers.len(),
                        })?;
                read_chunk_data(spec, spec.byte_off, spec.len, &pd_context)?
            }
            PdFunctionId::ReadPartialByteRange => {
                // Calldata: [function_id(1), index(1), offset(4), length(4)]
                if data.len() != READ_PARTIAL_BYTE_RANGE_CALLDATA_LEN {
                    return Err(PdPrecompileError::InvalidCalldataLength {
                        function: "ReadPartialByteRange",
                        expected: READ_PARTIAL_BYTE_RANGE_CALLDATA_LEN,
                        actual: data.len(),
                    }
                    .into());
                }
                let index = data[INDEX_OFFSET];

                let offset_end = OFFSET_FIELD_OFFSET + OFFSET_SIZE;
                let offset =
                    u32::from_be_bytes(data[OFFSET_FIELD_OFFSET..offset_end].try_into().map_err(
                        |_| PdPrecompileError::InvalidCalldataLength {
                            function: "ReadPartialByteRange (offset field)",
                            expected: OFFSET_SIZE,
                            actual: data[OFFSET_FIELD_OFFSET..].len(),
                        },
                    )?);

                let length_end = LENGTH_FIELD_OFFSET + LENGTH_SIZE;
                let length =
                    u32::from_be_bytes(data[LENGTH_FIELD_OFFSET..length_end].try_into().map_err(
                        |_| PdPrecompileError::InvalidCalldataLength {
                            function: "ReadPartialByteRange (length field)",
                            expected: LENGTH_SIZE,
                            actual: data[LENGTH_FIELD_OFFSET..].len(),
                        },
                    )?);

                let spec =
                    specifiers
                        .get(index as usize)
                        .ok_or(PdPrecompileError::SpecifierNotFound {
                            index,
                            available: specifiers.len(),
                        })?;
                read_chunk_data(spec, offset, length, &pd_context)?
            }
        };

        let total_gas = PD_BASE_GAS_COST.checked_add(0).ok_or_else(|| {
            warn!(
                base_gas = PD_BASE_GAS_COST,
                "PD precompile: gas calculation overflow"
            );
            PdPrecompileError::GasOverflow {
                base: PD_BASE_GAS_COST,
                operation: 0,
            }
        })?;

        if total_gas > gas_limit {
            warn!(
                total_gas = total_gas,
                gas_limit = gas_limit,
                "PD precompile: total gas exceeds limit"
            );
            return Err(PrecompileError::OutOfGas);
        }

        debug!(
            gas_used = total_gas,
            bytes_returned = result_bytes.len(),
            "PD precompile: execution successful"
        );

        Ok(PrecompileOutput {
            gas_used: total_gas,
            gas_refunded: 0,
            bytes: result_bytes,
            reverted: false,
        })
    })
    .into()
}

/// Registers the PD precompile at address 0x500.
#[inline]
pub fn register_irys_precompiles_if_active(
    precompiles: &mut PrecompilesMap,
    spec: SpecId,
    pd_context: PdContext,
) {
    // Only install when Frontier or later is active as per hardfork schedule.
    if spec >= SpecId::FRONTIER {
        precompiles.apply_precompile(&PD_PRECOMPILE_ADDRESS, |_current| {
            Some(pd_precompile(pd_context))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evm::{IRYS_USD_PRICE_ACCOUNT, IrysEvmFactory};
    use alloy_eips::eip2930::AccessList;
    use alloy_evm::{Evm as _, EvmFactory as _};
    use alloy_primitives::{Address, B256, Bytes};
    use dashmap::DashMap;
    use irys_types::range_specifier::{PdAccessListArgsTypeId, PdDataRead, encode_pd_fee};
    use reth_evm::EvmEnv;
    use revm::context::{BlockEnv, CfgEnv, TxEnv};
    use revm::database::CacheDB;
    use revm::database_interface::EmptyDB;
    use revm::primitives::{TxKind, U256, hardfork::SpecId};
    use revm::state::AccountInfo;
    use std::sync::Arc;

    /// Fixed test caller address used across all PD precompile tests so the
    /// CacheDB can be pre-seeded with sufficient balance for PD fee deduction.
    const TEST_CALLER: Address = Address::repeat_byte(0x42);

    /// Large fee value that exceeds the minimum PD transaction cost threshold
    /// configured in the testing hardfork config (0.01 USD at 1e18 scale).
    /// Using 1 IRYS (1e18) per chunk ensures the min-cost check always passes.
    const TEST_FEE_PER_CHUNK: u128 = 1_000_000_000_000_000_000;

    /// Appends PD fee keys (PdPriorityFee + PdBaseFeeCap) to an existing access list
    /// that targets `PD_PRECOMPILE_ADDRESS`. Required because the EVM layer validates
    /// PD transactions via `parse_pd_transaction()`, which rejects access lists
    /// missing fee parameters.
    fn with_test_fees(access_list: AccessList) -> AccessList {
        let fee = U256::from(TEST_FEE_PER_CHUNK);
        let mut items = access_list.0;
        if let Some(pd_item) = items
            .iter_mut()
            .find(|i| i.address == PD_PRECOMPILE_ADDRESS)
        {
            pd_item.storage_keys.push(B256::from(
                encode_pd_fee(PdAccessListArgsTypeId::PdPriorityFee as u8, fee)
                    .expect("test fee encoding"),
            ));
            pd_item.storage_keys.push(B256::from(
                encode_pd_fee(PdAccessListArgsTypeId::PdBaseFeeCap as u8, fee)
                    .expect("test fee encoding"),
            ));
        }
        AccessList(items)
    }

    /// Creates a `CacheDB` pre-seeded with accounts required by the PD fee
    /// validation path in `transact_raw`:
    /// - `IRYS_USD_PRICE_ACCOUNT` with a non-zero balance ($1.00 in 1e18 scale)
    /// - `TEST_CALLER` with a large balance to cover PD fee deductions
    fn test_db() -> CacheDB<EmptyDB> {
        let mut db = CacheDB::new(EmptyDB::default());
        // IRYS/USD price = $1.00 (1e18 scale)
        db.insert_account_info(
            *IRYS_USD_PRICE_ACCOUNT,
            AccountInfo {
                balance: U256::from(1_000_000_000_000_000_000_u128),
                ..Default::default()
            },
        );
        // Fund the test caller with a very large balance so PD fee deduction
        // does not fail even for tests with many chunks (e.g., 1000 chunks * 1e18)
        db.insert_account_info(
            TEST_CALLER,
            AccountInfo {
                balance: U256::MAX / U256::from(2),
                ..Default::default()
            },
        );
        db
    }

    /// Creates a ChunkDataIndex pre-populated with zero-filled chunks for offsets 0..num_chunks.
    /// Chunk size matches the testing ConsensusConfig (256_000 bytes).
    fn test_chunk_data_index(num_chunks: u64) -> irys_types::chunk_provider::ChunkDataIndex {
        let index = Arc::new(DashMap::new());
        let chunk = Arc::new(bytes::Bytes::from(vec![0_u8; 256_000]));
        for offset in 0..num_chunks {
            index.insert((0_u32, offset), chunk.clone());
        }
        index
    }

    /// Creates a default TxEnv for testing PD precompile.
    fn tx_env_default(data: Bytes, access_list: AccessList) -> TxEnv {
        TxEnv {
            caller: TEST_CALLER,
            kind: TxKind::Call(PD_PRECOMPILE_ADDRESS),
            nonce: 0,
            gas_limit: 30_000_000,
            value: U256::ZERO,
            data,
            gas_price: 0,
            chain_id: Some(1),
            gas_priority_fee: None,
            access_list,
            blob_hashes: Vec::new(),
            max_fee_per_blob_gas: 0,
            tx_type: 1,
            authorization_list: Default::default(),
        }
    }

    /// Helper to execute PD precompile with given input and access list.
    fn execute_precompile(
        input: Vec<u8>,
        access_list: AccessList,
        num_chunks: u64,
    ) -> revm::context::result::ResultAndState {
        let chunk_data_index = test_chunk_data_index(num_chunks);
        let factory = IrysEvmFactory::new_for_testing(chunk_data_index);

        let mut cfg_env = CfgEnv::default();
        cfg_env.spec = SpecId::CANCUN;
        cfg_env.chain_id = 1;

        let block_env = BlockEnv {
            gas_limit: 30_000_000,
            basefee: 0,
            ..Default::default()
        };

        let mut evm = factory.create_evm(test_db(), EvmEnv { cfg_env, block_env });

        let tx = tx_env_default(input.into(), access_list);

        evm.transact_raw(tx).unwrap()
    }

    /// Build a PD access list with a single data read specifier and test fees.
    fn build_test_access_list(spec: &PdDataRead) -> AccessList {
        use alloy_eips::eip2930::AccessListItem;
        with_test_fees(AccessList(vec![AccessListItem {
            address: PD_PRECOMPILE_ADDRESS,
            storage_keys: vec![B256::from(spec.encode())],
        }]))
    }

    #[test]
    fn pd_precompile_read_full_byte_range() {
        let spec = PdDataRead {
            partition_index: 0,
            start: 0,
            len: 100,
            byte_off: 0,
        };

        let access_list = build_test_access_list(&spec);
        let result = execute_precompile(vec![0, 0], access_list, 1);
        assert!(result.result.is_success(), "transaction should succeed");

        let min_expected_gas = PD_BASE_GAS_COST;
        assert!(
            result.result.gas_used() >= min_expected_gas,
            "Gas used ({}) should be at least {} (base)",
            result.result.gas_used(),
            PD_BASE_GAS_COST
        );

        let out = result
            .result
            .into_output()
            .expect("successful result should have output");

        assert!(
            out.iter().all(|&b| b == 0),
            "placeholder bytes should be zero"
        );
    }

    #[test]
    fn test_insufficient_input_data() {
        let chunk_data_index = test_chunk_data_index(0);
        let factory = IrysEvmFactory::new_for_testing(chunk_data_index);
        let mut cfg_env = CfgEnv::default();
        cfg_env.spec = SpecId::CANCUN;
        let block_env = BlockEnv::default();
        let mut evm = factory.create_evm(test_db(), EvmEnv { cfg_env, block_env });

        let input = Bytes::from(vec![0]); // Only 1 byte (need at least 2)
        let tx = tx_env_default(input, AccessList::default());

        let result = evm.transact_raw(tx).unwrap();
        assert!(
            !result.result.is_success(),
            "should fail with insufficient input data"
        );
    }

    #[test]
    fn test_read_partial_byte_range() {
        let spec = PdDataRead {
            partition_index: 0,
            start: 0,
            len: 500,
            byte_off: 0,
        };

        let access_list = build_test_access_list(&spec);

        // Use function ID 1 (ReadPartialByteRange) with index 0, offset 100, length 200
        let mut input = vec![1, 0]; // function_id=1, index=0
        input.extend_from_slice(&100_u32.to_be_bytes()); // offset=100
        input.extend_from_slice(&200_u32.to_be_bytes()); // length=200

        let result = execute_precompile(input, access_list, 2);
        assert!(
            result.result.is_success(),
            "ReadPartialByteRange should succeed"
        );

        let min_expected_gas = PD_BASE_GAS_COST;
        assert!(
            result.result.gas_used() >= min_expected_gas,
            "Gas used should include precompile costs"
        );
    }

    #[test]
    fn test_no_access_list() {
        let chunk_data_index = test_chunk_data_index(0);
        let factory = IrysEvmFactory::new_for_testing(chunk_data_index);
        let mut cfg_env = CfgEnv::default();
        cfg_env.spec = SpecId::CANCUN;
        let block_env = BlockEnv::default();
        let mut evm = factory.create_evm(test_db(), EvmEnv { cfg_env, block_env });

        let input = Bytes::from(vec![0, 0]);
        let tx = tx_env_default(input, AccessList::default()); // Empty access list

        let result = evm.transact_raw(tx).unwrap();
        assert!(
            !result.result.is_success(),
            "should fail with no access list"
        );
    }

    #[test]
    fn test_invalid_function_id() {
        let spec = PdDataRead {
            partition_index: 0,
            start: 0,
            len: 100,
            byte_off: 0,
        };

        let access_list = build_test_access_list(&spec);

        let chunk_data_index = test_chunk_data_index(1);
        let factory = IrysEvmFactory::new_for_testing(chunk_data_index);
        let mut cfg_env = CfgEnv::default();
        cfg_env.spec = SpecId::CANCUN;
        let block_env = BlockEnv::default();
        let mut evm = factory.create_evm(test_db(), EvmEnv { cfg_env, block_env });

        let input = Bytes::from(vec![99, 0]); // Function ID 99 doesn't exist
        let tx = tx_env_default(input, access_list);

        let result = evm.transact_raw(tx).unwrap();
        assert!(
            !result.result.is_success(),
            "should fail with invalid function ID"
        );
    }

    #[test]
    fn test_moderate_chunks() {
        let spec = PdDataRead {
            partition_index: 0,
            start: 0,
            len: 5000,
            byte_off: 0,
        };

        let access_list = build_test_access_list(&spec);

        let result = execute_precompile(vec![0, 0], access_list, 20);
        assert!(
            result.result.is_success(),
            "transaction should succeed with moderate chunks"
        );

        let min_expected_gas = PD_BASE_GAS_COST;
        assert!(
            result.result.gas_used() >= min_expected_gas,
            "Gas used ({}) should be at least {}",
            result.result.gas_used(),
            min_expected_gas
        );
    }

    #[test]
    fn test_zero_len_rejected() {
        // PdDataRead with len=0 is rejected at decode time, so we can't even
        // construct a valid access list key for it. This tests that the
        // precompile rejects the invalid access list.
        use alloy_eips::eip2930::AccessListItem;

        // Manually construct a key with len=0 (type byte 0x01 but len field = 0)
        let mut key = [0_u8; 32];
        key[0] = 0x01; // type byte
        // partition_index=0 at [1..9] (already zero)
        // start=0 at [9..13] (already zero)
        // len=0 at [13..17] (already zero)
        // byte_off=0 at [17..20] (already zero)

        let access_list = with_test_fees(AccessList(vec![AccessListItem {
            address: PD_PRECOMPILE_ADDRESS,
            storage_keys: vec![B256::from(key)],
        }]));

        let chunk_data_index = test_chunk_data_index(0);
        let factory = IrysEvmFactory::new_for_testing(chunk_data_index);
        let mut cfg_env = CfgEnv::default();
        cfg_env.spec = SpecId::CANCUN;
        cfg_env.chain_id = 1;
        let block_env = BlockEnv::default();
        let mut evm = factory.create_evm(test_db(), EvmEnv { cfg_env, block_env });

        let input = Bytes::from(vec![0, 0]);
        let tx = tx_env_default(input, access_list);

        // Zero-len PdDataRead is rejected at access list parse time
        let result = evm.transact_raw(tx);
        // The transaction should either error or revert
        match result {
            Ok(r) => assert!(
                !r.result.is_success(),
                "zero-len specifier should not succeed"
            ),
            Err(_) => {} // Also acceptable — rejected at EVM layer
        }
    }

    #[test]
    fn test_large_chunks_no_overflow() {
        let spec = PdDataRead {
            partition_index: 0,
            start: 0,
            len: 10000,
            byte_off: 0,
        };

        let access_list = build_test_access_list(&spec);

        let result = execute_precompile(vec![0, 0], access_list, 1000);
        assert!(
            result.result.is_success(),
            "transaction should succeed with large chunk count"
        );

        let min_expected_gas = PD_BASE_GAS_COST;
        assert!(
            result.result.gas_used() >= min_expected_gas,
            "Gas used ({}) should be at least {}",
            result.result.gas_used(),
            min_expected_gas
        );
    }
}
