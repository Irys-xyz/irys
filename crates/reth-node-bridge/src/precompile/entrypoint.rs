use irys_primitives::precompile::IrysPrecompileOffsets;
use revm_primitives::{Bytes, Env, PrecompileError, PrecompileErrors, PrecompileResult};

use super::{
    functions::PdFunctionId,
    irys_executor::{CustomPrecompileWithAddress, PrecompileStateProvider},
    read_bytes::{read_bytes_range_by_index, read_partial_byte_range},
    utils::parse_access_list,
};

pub const PRECOMPILE_ADDRESS: irys_types::Address =
    IrysPrecompileOffsets::ProgrammableData.to_address();

pub const PROGRAMMABLE_DATA_PRECOMPILE: CustomPrecompileWithAddress =
    CustomPrecompileWithAddress(PRECOMPILE_ADDRESS, programmable_data_precompile);

/// programmable data precompile
/// this precompile is an 'actual' smart contract, with multiple subfunctions.
// TODO: Gas pricing
fn programmable_data_precompile(
    call_data: &Bytes,
    gas_limit: u64,
    env: &Env,
    state_provider: &PrecompileStateProvider,
) -> PrecompileResult {
    if call_data.is_empty() {
        return Err(PrecompileError::Other(
            "Invalid empty calldata (function selector required)".to_string(),
        )
        .into());
    }

    let access_list = &env.tx.access_list;
    if access_list.is_empty() {
        // this is a constant requirement across all execution paths, as we always need at least one PD chunk read range, which must be in the access list.
        return Err(PrecompileError::Other("Transaction has no access list".to_string()).into());
    }

    let call_data_vec = call_data.to_vec();

    // decode the first byte of the calldata
    let decoded_id = PdFunctionId::try_from(call_data_vec[0])
        .map_err(|e| PrecompileErrors::Error(PrecompileError::Other(format!("{}", &e))))?;

    let provider_guard = state_provider.provider.read().map_err(|_| {
        PrecompileErrors::Error(PrecompileError::Other(
            "Failed to acquire read lock on IrysRethProvider".to_owned(),
        ))
    })?;
    let state_provider =
        provider_guard
            .as_ref()
            .ok_or(PrecompileErrors::Error(PrecompileError::Other(
                "IrysRethProvider not initialized".to_owned(),
            )))?;

    let parsed = parse_access_list(access_list).map_err(|_| {
        PrecompileErrors::Error(PrecompileError::Other(
            "Internal error - unable to parse access list".to_owned(),
        ))
    })?;

    let res = match decoded_id {
        PdFunctionId::ReadFullByteRange => {
            read_bytes_range_by_index(call_data, gas_limit, env, state_provider, parsed)
        }
        PdFunctionId::ReadPartialByteRange => {
            read_partial_byte_range(call_data, gas_limit, env, state_provider, parsed)
        }
    };
    res
}
