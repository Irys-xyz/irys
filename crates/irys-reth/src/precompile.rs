use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::Bytes;
use irys_primitives::precompile::{PD_COST_PER_CHUNK, PD_PRECOMPILE_ADDRESS};
use reth_evm::precompiles::PrecompilesMap;
use revm::precompile::{PrecompileOutput, PrecompileResult};
use revm::primitives::hardfork::SpecId;

/// Programmable Data precompile operation types.
/// These match the constants defined in Precompiles.sol
const READ_FULL_BYTE_RANGE: u8 = 0;
const READ_PARTIAL_BYTE_RANGE: u8 = 1;

/// Chunk size for gas calculation.
/// TODO: Make this configurable via constructor argument instead of hardcoding.
/// This should match the consensus chunk_size configuration.
const CHUNK_SIZE: usize = 32;

/// Programmable Data precompile implementation.
///
/// This precompile enables smart contracts to read data from Irys's data layer.
/// Supports two operations:
/// - READ_FULL_BYTE_RANGE (0): Reads entire byte range by index
/// - READ_PARTIAL_BYTE_RANGE (1): Reads partial byte range with offset and length
///
/// Input format:
/// - READ_FULL_BYTE_RANGE: [op_id: u8][byte_range_index: u8]
/// - READ_PARTIAL_BYTE_RANGE: [op_id: u8][byte_range_index: u8][start_offset: u32][length: u32]
///
/// Gas cost:
/// - Constant cost: PD_COST_PER_CHUNK (5,000 gas) regardless of data size
/// - TODO: Use dynamic pricing based on chunks accessed once access_list is available
///
/// TODO: Investigate how to access tx access_list from within precompile context.
/// The standard precompile interface `|data: &[u8], gas_limit: u64| -> PrecompileResult`
/// does not expose the transaction's access_list, even in newer versions of reth/alloy_evm.
/// We need the access_list to parse ByteRangeSpecifier/ChunkRangeSpecifier storage keys
/// and determine which chunks to fetch from the Irys data layer.
fn pd_precompile() -> DynPrecompile {
    move |data: &[u8], gas_limit: u64| -> PrecompileResult {
        // Validate minimum input length (at least operation type + byte_range_index)
        if data.len() < 2 {
            return Err(revm::precompile::PrecompileError::Other(
                "PD precompile: insufficient input data".into(),
            ));
        }

        let operation = data[0];
        let _byte_range_index = data[1];

        // Parse operation and calculate result length
        let result_length = match operation {
            READ_FULL_BYTE_RANGE => {
                // For full range read, we'll return placeholder data
                // TODO: Determine actual byte range length from access list
                // For now, return CHUNK_SIZE bytes as placeholder
                CHUNK_SIZE
            }
            READ_PARTIAL_BYTE_RANGE => {
                // Validate input length for partial read
                if data.len() < 10 {
                    // 1 (op) + 1 (index) + 4 (offset) + 4 (length)
                    return Err(revm::precompile::PrecompileError::Other(
                        "PD precompile: insufficient input for partial byte range".into(),
                    ));
                }

                // Extract start_offset (bytes 2-5) and length (bytes 6-9)
                let _start_offset = u32::from_be_bytes([data[2], data[3], data[4], data[5]]);
                let length = u32::from_be_bytes([data[6], data[7], data[8], data[9]]);

                length as usize
            }
            _ => {
                return Err(revm::precompile::PrecompileError::Other(
                    format!("PD precompile: unknown operation type: {}", operation),
                ));
            }
        };

        // Use constant gas cost regardless of data size returned.
        // This is similar to how simple precompiles like IDENTITY work.
        // TODO: Once we can access the access_list, calculate actual gas based on:
        // - Number of chunks accessed from the access list
        // - Number of access list keys used
        const GAS_COST: u64 = PD_COST_PER_CHUNK;

        // Check if we have sufficient gas
        if GAS_COST > gas_limit {
            return Err(revm::precompile::PrecompileError::OutOfGas);
        }

        // TODO: Fetch actual chunk data from Irys data layer based on:
        // - byte_range_index: which ByteRangeSpecifier from the access list to use
        // - start_offset/length: which portion of the data to return
        //
        // For now, return zeroed placeholder bytes
        let result_bytes = vec![0_u8; result_length];

        Ok(PrecompileOutput {
            gas_used: GAS_COST,
            bytes: Bytes::from(result_bytes),
        })
    }
    .into()
}

/// Registers all Irys custom precompiles into the provided precompile map.
///
/// Currently registers the Programmable Data precompile at `PD_PRECOMPILE_ADDRESS` (0x500)
/// for reading data chunks from the Irys data layer.
pub fn register_irys_precompiles_if_active(precompiles: &mut PrecompilesMap, spec: SpecId) {
    // Only install when Frontier or later is active as per hardfork schedule.
    if spec >= SpecId::FRONTIER {
        precompiles.apply_precompile(&PD_PRECOMPILE_ADDRESS, |_current| Some(pd_precompile()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evm::IrysEvmFactory;
    use alloy_evm::{Evm as _, EvmFactory as _};
    use alloy_primitives::Address;
    use reth_evm::EvmEnv;
    use revm::context::{BlockEnv, CfgEnv};
    use revm::database_interface::EmptyDB;
    use revm::primitives::hardfork::SpecId;

    /// Test PD precompile with READ_FULL_BYTE_RANGE operation (happy path).
    #[test]
    fn pd_precompile_read_full_byte_range() {
        let factory = IrysEvmFactory::new();

        let mut cfg_env = CfgEnv::default();
        cfg_env.spec = SpecId::CANCUN;
        cfg_env.chain_id = 1;
        let block_env = BlockEnv {
            gas_limit: 30_000_000,
            basefee: 0,
            ..Default::default()
        };

        let mut evm = factory.create_evm(EmptyDB::default(), EvmEnv { cfg_env, block_env });

        let caller = Address::random();
        // Input: [operation: READ_FULL_BYTE_RANGE (0)][byte_range_index: 0]
        let input = Bytes::from(vec![READ_FULL_BYTE_RANGE, 0]);

        let res = evm.transact_system_call(caller, PD_PRECOMPILE_ADDRESS, input);
        assert!(res.is_ok(), "expected Ok, got: {:?}", res);

        let result = res.unwrap();
        let out = result.result.into_output().expect("should have output");

        // Should return CHUNK_SIZE bytes of placeholder data (all zeros)
        assert_eq!(
            out.len(),
            CHUNK_SIZE,
            "expected CHUNK_SIZE bytes for full range read"
        );
        assert!(
            out.iter().all(|&b| b == 0),
            "placeholder bytes should be zero"
        );
    }
}
