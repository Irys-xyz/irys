use revm::precompile::{u64_to_address, PrecompileWithAddress};
use revm_primitives::{Bytes, Env, Precompile, PrecompileOutput, PrecompileResult};

use super::irys_executor::IrysPrecompileOffsets;

pub const PROGRAMMABLE_DATA_PRECOMPILE: PrecompileWithAddress = PrecompileWithAddress(
    IrysPrecompileOffsets::ProgrammableData.to_address(),
    Precompile::Env(programmable_data_precompile),
);

fn programmable_data_precompile(call_data: &Bytes, gas_limit: u64, env: &Env) -> PrecompileResult {
    let mut gas_used = 1000;
    Ok(PrecompileOutput::new(gas_used, vec![].into()))
}
