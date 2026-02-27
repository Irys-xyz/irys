use alloy_evm::precompiles::{DynPrecompile, PrecompileInput};
use alloy_primitives::Bytes;
use irys_types::precompile::PD_PRECOMPILE_ADDRESS;
use reth_evm::precompiles::PrecompilesMap;
use revm::precompile::PrecompileError;
use revm::precompile::{PrecompileOutput, PrecompileResult};
use revm::primitives::hardfork::SpecId;
use std::borrow::Cow;
use tracing::{debug, warn};

use crate::precompiles::pd::constants::PD_BASE_GAS_COST;
use crate::precompiles::pd::error::PdPrecompileError;
use crate::precompiles::pd::functions::PdFunctionId;
use crate::precompiles::pd::read_bytes::{read_bytes_range_by_index, read_partial_byte_range};
use crate::precompiles::pd::utils::parse_access_list;

use super::context::PdContext;

/// Programmable Data precompile implementation.
#[inline]
fn pd_precompile(pd_context: PdContext) -> DynPrecompile {
    move |input: PrecompileInput<'_>| -> PrecompileResult {
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

        let call_data = Bytes::copy_from_slice(data);

        let decoded_id = PdFunctionId::try_from(data[0]).map_err(|e| {
            warn!(function_id = data[0], "PD precompile: unknown function ID");
            PrecompileError::Other(Cow::Owned(format!("PD precompile: {}", e)))
        })?;

        debug!(function_id = ?decoded_id, "PD precompile: decoded function ID");

        let parsed = parse_access_list(&access_list).map_err(|e| {
            warn!(error = ?e, "PD precompile: failed to parse access list");
            PdPrecompileError::InvalidAccessList {
                reason: e.to_string(),
            }
        })?;

        let available_gas = gas_limit.saturating_sub(PD_BASE_GAS_COST);

        let res = match decoded_id {
            PdFunctionId::ReadFullByteRange => {
                read_bytes_range_by_index(&call_data, available_gas, &pd_context, parsed)?
            }
            PdFunctionId::ReadPartialByteRange => {
                read_partial_byte_range(&call_data, available_gas, &pd_context, parsed)?
            }
        };

        let total_gas = PD_BASE_GAS_COST.checked_add(res.gas_used).ok_or_else(|| {
            warn!(
                base_gas = PD_BASE_GAS_COST,
                operation_gas = res.gas_used,
                "PD precompile: gas calculation overflow"
            );
            PdPrecompileError::GasOverflow {
                base: PD_BASE_GAS_COST,
                operation: res.gas_used,
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
            bytes_returned = res.bytes.len(),
            "PD precompile: execution successful"
        );

        Ok(PrecompileOutput {
            gas_used: total_gas,
            gas_refunded: 0,
            bytes: res.bytes,
            reverted: false,
        })
    }
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
    use crate::evm::IrysEvmFactory;
    use alloy_eips::eip2930::AccessList;
    use alloy_evm::{Evm as _, EvmFactory as _};
    use alloy_primitives::{Address, Bytes};
    use reth_evm::EvmEnv;
    use revm::context::{BlockEnv, CfgEnv, TxEnv};
    use revm::database_interface::EmptyDB;
    use revm::primitives::{TxKind, U256, hardfork::SpecId};
    use std::sync::Arc;

    /// Creates a default TxEnv for testing PD precompile.
    fn tx_env_default(data: Bytes, access_list: AccessList) -> TxEnv {
        TxEnv {
            caller: Address::random(),
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
    ) -> revm::context::result::ResultAndState {
        let mock_chunk_provider = Arc::new(irys_types::chunk_provider::MockChunkProvider::new());
        let factory = IrysEvmFactory::new_for_testing(mock_chunk_provider);

        let mut cfg_env = CfgEnv::default();
        cfg_env.spec = SpecId::CANCUN;
        cfg_env.chain_id = 1;

        let block_env = BlockEnv {
            gas_limit: 30_000_000,
            basefee: 0,
            ..Default::default()
        };

        let mut evm = factory.create_evm(EmptyDB::default(), EvmEnv { cfg_env, block_env });

        let tx = tx_env_default(input.into(), access_list);

        evm.transact_raw(tx).unwrap()
    }

    #[test]
    fn pd_precompile_read_full_byte_range() {
        use alloy_eips::eip2930::AccessListItem;
        use alloy_primitives::{B256, aliases::U200};
        use irys_types::range_specifier::{
            ByteRangeSpecifier, ChunkRangeSpecifier, PdAccessListArg, U18, U34,
        };

        let chunk_range = ChunkRangeSpecifier {
            partition_index: U200::ZERO,
            offset: 0,
            chunk_count: 1,
        };
        let byte_range = ByteRangeSpecifier {
            index: 0,
            chunk_offset: 0,
            byte_offset: U18::ZERO,
            length: U34::try_from(100).unwrap(),
        };

        let access_list = AccessList(vec![AccessListItem {
            address: PD_PRECOMPILE_ADDRESS,
            storage_keys: vec![
                B256::from(PdAccessListArg::ChunkRead(chunk_range).encode()),
                B256::from(PdAccessListArg::ByteRead(byte_range).encode()),
            ],
        }]);

        let result = execute_precompile(vec![0, 0], access_list);
        assert!(result.result.is_success(), "transaction should succeed");

        // Verify gas cost is at least the base cost
        // Note: total gas includes EVM overhead, so we check it's at least the precompile cost
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
        use alloy_eips::eip2930::AccessList;
        use alloy_evm::{Evm as _, EvmFactory as _};

        let mock_chunk_provider = Arc::new(irys_types::chunk_provider::MockChunkProvider::new());
        let factory = IrysEvmFactory::new_for_testing(mock_chunk_provider);
        let mut cfg_env = CfgEnv::default();
        cfg_env.spec = SpecId::CANCUN;
        let block_env = BlockEnv::default();
        let mut evm = factory.create_evm(EmptyDB::default(), EvmEnv { cfg_env, block_env });

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
        use alloy_eips::eip2930::{AccessList, AccessListItem};
        use alloy_evm::{Evm as _, EvmFactory as _};
        use alloy_primitives::{B256, aliases::U200};
        use irys_types::range_specifier::{
            ByteRangeSpecifier, ChunkRangeSpecifier, PdAccessListArg, U18, U34,
        };

        let mock_chunk_provider = Arc::new(irys_types::chunk_provider::MockChunkProvider::new());
        let factory = IrysEvmFactory::new_for_testing(mock_chunk_provider);
        let mut cfg_env = CfgEnv::default();
        cfg_env.spec = SpecId::CANCUN;
        let block_env = BlockEnv::default();
        let mut evm = factory.create_evm(EmptyDB::default(), EvmEnv { cfg_env, block_env });

        // Setup access list
        let chunk_range = ChunkRangeSpecifier {
            partition_index: U200::ZERO,
            offset: 0,
            chunk_count: 2,
        };
        let byte_range = ByteRangeSpecifier {
            index: 0,
            chunk_offset: 0,
            byte_offset: U18::ZERO,
            length: U34::try_from(500).unwrap(),
        };

        let access_list_items = vec![AccessListItem {
            address: PD_PRECOMPILE_ADDRESS,
            storage_keys: vec![
                B256::from(PdAccessListArg::ChunkRead(chunk_range).encode()),
                B256::from(PdAccessListArg::ByteRead(byte_range).encode()),
            ],
        }];

        // Use function ID 1 (ReadPartialByteRange) with index 0, offset 100, length 200
        let mut input = vec![1, 0]; // function_id=1, index=0
        input.extend_from_slice(&100_u32.to_be_bytes()); // offset=100
        input.extend_from_slice(&200_u32.to_be_bytes()); // length=200

        let tx = tx_env_default(Bytes::from(input), AccessList(access_list_items));

        let result = evm.transact_raw(tx).unwrap();
        assert!(
            result.result.is_success(),
            "ReadPartialByteRange should succeed"
        );

        // Verify gas is at least the base cost
        let min_expected_gas = PD_BASE_GAS_COST;
        assert!(
            result.result.gas_used() >= min_expected_gas,
            "Gas used should include precompile costs"
        );
    }

    #[test]
    fn test_no_access_list() {
        use alloy_eips::eip2930::AccessList;
        use alloy_evm::{Evm as _, EvmFactory as _};

        let mock_chunk_provider = Arc::new(irys_types::chunk_provider::MockChunkProvider::new());
        let factory = IrysEvmFactory::new_for_testing(mock_chunk_provider);
        let mut cfg_env = CfgEnv::default();
        cfg_env.spec = SpecId::CANCUN;
        let block_env = BlockEnv::default();
        let mut evm = factory.create_evm(EmptyDB::default(), EvmEnv { cfg_env, block_env });

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
        use alloy_eips::eip2930::{AccessList, AccessListItem};
        use alloy_evm::{Evm as _, EvmFactory as _};
        use alloy_primitives::{B256, aliases::U200};
        use irys_types::range_specifier::{
            ByteRangeSpecifier, ChunkRangeSpecifier, PdAccessListArg, U18, U34,
        };

        let mock_chunk_provider = Arc::new(irys_types::chunk_provider::MockChunkProvider::new());
        let factory = IrysEvmFactory::new_for_testing(mock_chunk_provider);
        let mut cfg_env = CfgEnv::default();
        cfg_env.spec = SpecId::CANCUN;
        let block_env = BlockEnv::default();
        let mut evm = factory.create_evm(EmptyDB::default(), EvmEnv { cfg_env, block_env });

        let chunk_range = ChunkRangeSpecifier {
            partition_index: U200::ZERO,
            offset: 0,
            chunk_count: 1,
        };
        let byte_range = ByteRangeSpecifier {
            index: 0,
            chunk_offset: 0,
            byte_offset: U18::ZERO,
            length: U34::try_from(100).unwrap(),
        };

        let access_list_items = vec![AccessListItem {
            address: PD_PRECOMPILE_ADDRESS,
            storage_keys: vec![
                B256::from(PdAccessListArg::ChunkRead(chunk_range).encode()),
                B256::from(PdAccessListArg::ByteRead(byte_range).encode()),
            ],
        }];

        let input = Bytes::from(vec![99, 0]); // Function ID 99 doesn't exist
        let tx = tx_env_default(input, AccessList(access_list_items));

        let result = evm.transact_raw(tx).unwrap();
        assert!(
            !result.result.is_success(),
            "should fail with invalid function ID"
        );
    }

    #[test]
    fn test_moderate_chunks() {
        use alloy_eips::eip2930::{AccessList, AccessListItem};
        use alloy_evm::{Evm as _, EvmFactory as _};
        use alloy_primitives::{B256, aliases::U200};
        use irys_types::range_specifier::{
            ByteRangeSpecifier, ChunkRangeSpecifier, PdAccessListArg, U18, U34,
        };

        let mock_chunk_provider = Arc::new(irys_types::chunk_provider::MockChunkProvider::new());
        let factory = IrysEvmFactory::new_for_testing(mock_chunk_provider);
        let mut cfg_env = CfgEnv::default();
        cfg_env.spec = SpecId::CANCUN;
        let block_env = BlockEnv::default();
        let mut evm = factory.create_evm(EmptyDB::default(), EvmEnv { cfg_env, block_env });

        // Test with moderate number of chunks
        let chunk_range = ChunkRangeSpecifier {
            partition_index: U200::ZERO,
            offset: 0,
            chunk_count: 20,
        };
        let byte_range = ByteRangeSpecifier {
            index: 0,
            chunk_offset: 0,
            byte_offset: U18::ZERO,
            length: U34::try_from(5000).unwrap(),
        };

        let access_list_items = vec![AccessListItem {
            address: PD_PRECOMPILE_ADDRESS,
            storage_keys: vec![
                B256::from(PdAccessListArg::ChunkRead(chunk_range).encode()),
                B256::from(PdAccessListArg::ByteRead(byte_range).encode()),
            ],
        }];

        let input = Bytes::from(vec![0, 0]);
        let tx = tx_env_default(input, AccessList(access_list_items));

        let result = evm.transact_raw(tx).unwrap();
        assert!(
            result.result.is_success(),
            "transaction should succeed with 20 chunks"
        );

        // Verify gas cost is at least the base cost
        let min_expected_gas = PD_BASE_GAS_COST;
        assert!(
            result.result.gas_used() >= min_expected_gas,
            "Gas used ({}) should be at least {}",
            result.result.gas_used(),
            min_expected_gas
        );
    }

    #[test]
    fn test_zero_chunks() {
        use alloy_eips::eip2930::{AccessList, AccessListItem};
        use alloy_evm::{Evm as _, EvmFactory as _};
        use alloy_primitives::{B256, aliases::U200};
        use irys_types::range_specifier::{
            ByteRangeSpecifier, ChunkRangeSpecifier, PdAccessListArg, U18, U34,
        };

        let mock_chunk_provider = Arc::new(irys_types::chunk_provider::MockChunkProvider::new());
        let factory = IrysEvmFactory::new_for_testing(mock_chunk_provider);
        let mut cfg_env = CfgEnv::default();
        cfg_env.spec = SpecId::CANCUN;
        let block_env = BlockEnv::default();
        let mut evm = factory.create_evm(EmptyDB::default(), EvmEnv { cfg_env, block_env });

        // Test with 0 chunks
        let chunk_range = ChunkRangeSpecifier {
            partition_index: U200::ZERO,
            offset: 0,
            chunk_count: 0,
        };
        let byte_range = ByteRangeSpecifier {
            index: 0,
            chunk_offset: 0,
            byte_offset: U18::ZERO,
            length: U34::try_from(0).unwrap(),
        };

        let access_list_items = vec![AccessListItem {
            address: PD_PRECOMPILE_ADDRESS,
            storage_keys: vec![
                B256::from(PdAccessListArg::ChunkRead(chunk_range).encode()),
                B256::from(PdAccessListArg::ByteRead(byte_range).encode()),
            ],
        }];

        let input = Bytes::from(vec![0, 0]);
        let tx = tx_env_default(input, AccessList(access_list_items));

        let result = evm.transact_raw(tx).unwrap();
        assert!(
            result.result.is_success(),
            "transaction should succeed with zero chunks"
        );

        // Should charge at least base cost for zero chunks
        assert!(
            result.result.gas_used() >= PD_BASE_GAS_COST,
            "Gas used ({}) should be at least base cost ({})",
            result.result.gas_used(),
            PD_BASE_GAS_COST
        );
    }

    #[test]
    fn test_large_chunks_no_overflow() {
        use alloy_eips::eip2930::{AccessList, AccessListItem};
        use alloy_evm::{Evm as _, EvmFactory as _};
        use alloy_primitives::{B256, aliases::U200};
        use irys_types::range_specifier::{
            ByteRangeSpecifier, ChunkRangeSpecifier, PdAccessListArg, U18, U34,
        };

        let mock_chunk_provider = Arc::new(irys_types::chunk_provider::MockChunkProvider::new());
        let factory = IrysEvmFactory::new_for_testing(mock_chunk_provider);
        let mut cfg_env = CfgEnv::default();
        cfg_env.spec = SpecId::CANCUN;
        let block_env = BlockEnv::default();
        let mut evm = factory.create_evm(EmptyDB::default(), EvmEnv { cfg_env, block_env });

        // Test with 1000 chunks to verify no overflow in gas calculation
        let chunk_range = ChunkRangeSpecifier {
            partition_index: U200::ZERO,
            offset: 0,
            chunk_count: 1000,
        };
        let byte_range = ByteRangeSpecifier {
            index: 0,
            chunk_offset: 0,
            byte_offset: U18::ZERO,
            length: U34::try_from(10000).unwrap(),
        };

        let access_list_items = vec![AccessListItem {
            address: PD_PRECOMPILE_ADDRESS,
            storage_keys: vec![
                B256::from(PdAccessListArg::ChunkRead(chunk_range).encode()),
                B256::from(PdAccessListArg::ByteRead(byte_range).encode()),
            ],
        }];

        let input = Bytes::from(vec![0, 0]);
        let tx = tx_env_default(input, AccessList(access_list_items));

        let result = evm.transact_raw(tx).unwrap();
        assert!(
            result.result.is_success(),
            "transaction should succeed with large chunk count"
        );

        // Verify gas cost is at least the base cost
        let min_expected_gas = PD_BASE_GAS_COST;
        assert!(
            result.result.gas_used() >= min_expected_gas,
            "Gas used ({}) should be at least {}",
            result.result.gas_used(),
            min_expected_gas
        );
    }
}
