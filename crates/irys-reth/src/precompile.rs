use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::{Address, Bytes};
use reth_evm::precompiles::PrecompilesMap;
use revm::primitives::hardfork::SpecId;
use revm::precompile::{PrecompileOutput, PrecompileResult};

/// Address for a trivial custom precompile that returns a constant vector of zero bytes.
///
/// Custom Irys precompiles are placed starting at offset `0x500` (1280).
/// We reserve `0x500` for Programmable Data; `0x501` (1281) is used for this helper.
pub const ZERO_BYTES_PRECOMPILE_ADDRESS: Address = Address::new([
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // top 12 zero bytes
    0, 0, 0, 0, 0, 0, 0x05, 0x01, // 0x0000000000000501 (1281)
]);

fn zero_bytes_precompile() -> DynPrecompile {
    const LEN: usize = 32;

    move |data: &[u8], _gas_limit: u64| -> PrecompileResult {
        // todo - in the future the `data` will contain the
        // actual chunk addresses to reference, now just assume it's the total
        // amount of chunks that must be read
        let chunks_to_read = u64::from_le_bytes(data.try_into().expect("invalid len data"));

        // todo compute how much gas was used for consuming this precompile
        let gas_used = 99999;
        Ok(PrecompileOutput { gas_used, bytes: Bytes::from(vec![0u8; LEN]) })
    }
    .into()
}

/// Registers all Irys custom precompiles into the provided precompile map.
///
/// Currently registers a single helper precompile at `ZERO_BYTES_PRECOMPILE_ADDRESS` that
/// returns 32 zero bytes regardless of input.
pub fn register_irys_precompiles_if_active(precompiles: &mut PrecompilesMap, spec: SpecId) {
    // Only install when Frontier or later is active as per hardfork schedule.
    if spec >= SpecId::FRONTIER {
        precompiles.apply_precompile(&ZERO_BYTES_PRECOMPILE_ADDRESS, |_current| {
            Some(zero_bytes_precompile())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evm::IrysEvmFactory;
    use alloy_evm::{Evm, EvmFactory};
    use alloy_primitives::Address;
    use reth_evm::EvmEnv;
    use revm::context::{BlockEnv, CfgEnv};
    use revm::database_interface::EmptyDB;
    use revm::primitives::hardfork::SpecId;

    /// Ensure the custom zero-bytes precompile is reachable and returns 32 zero bytes.
    #[test]
    fn evm_zero_bytes_precompile_returns_zeros() {
        let factory = IrysEvmFactory::new();

        let mut cfg_env = CfgEnv::default();
        cfg_env.spec = SpecId::CANCUN; // Frontier+ ensures precompile registration
        cfg_env.chain_id = 1;
        let block_env = BlockEnv { gas_limit: 30_000_000, basefee: 0, ..Default::default() };

        let mut evm = factory.create_evm(EmptyDB::default(), EvmEnv { cfg_env, block_env });

        let caller = Address::random();
        let res = evm.transact_system_call(caller, ZERO_BYTES_PRECOMPILE_ADDRESS, Bytes::new());
        assert!(res.is_ok(), "expected Ok, got: {:?}", res);

        let out = res.unwrap().result.into_output().expect("should have output");
        assert_eq!(out.len(), 32, "expected 32 bytes");
        assert!(out.iter().all(|&b| b == 0), "all bytes should be zero");
    }
}
