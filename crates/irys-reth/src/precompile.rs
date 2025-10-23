use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::Bytes;
use irys_primitives::precompile::PD_PRECOMPILE_ADDRESS;
use reth_evm::precompiles::PrecompilesMap;
use revm::precompile::{PrecompileOutput, PrecompileResult};
use revm::primitives::hardfork::SpecId;

/// Programmable Data precompile implementation.
///
/// Gas cost:
/// - Constant cost: PD_COST_PER_CHUNK (5,000 gas) regardless of data size
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

        // Parse operation and calculate result length

        // Use constant gas cost regardless of data size returned.
        // This is similar to how simple precompiles like IDENTITY work.
        const GAS_COST: u64 = 5000; // todo take a single sane price cost

        // Check if we have sufficient gas
        if GAS_COST > gas_limit {
            return Err(revm::precompile::PrecompileError::OutOfGas);
        }

        // TODO: Fetch actual chunk data from Irys data layer.
        // For now, return zeroed placeholder bytes
        let result_bytes = vec![0_u8; 8];

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
        let input = Bytes::from(vec![42, 42]);

        let res = evm.transact_system_call(caller, PD_PRECOMPILE_ADDRESS, input);
        assert!(res.is_ok(), "expected Ok, got: {:?}", res);

        let result = res.unwrap();
        let out = result.result.into_output().expect("should have output");

        assert!(
            out.iter().all(|&b| b == 0),
            "placeholder bytes should be zero"
        );
    }
}
