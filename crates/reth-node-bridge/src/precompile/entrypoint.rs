use revm_primitives::{Bytes, Env, PrecompileError, PrecompileErrors, PrecompileResult};
use tracing::debug;

use super::{
    functions::PdFunctionId,
    irys_executor::{CustomPrecompileWithAddress, IrysPrecompileOffsets, PrecompileStateProvider},
    read_bytes::{read_bytes_first_range, read_bytes_first_range_with_args},
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
    // make sure we were given the u32 index, the u32 range relative offset, and the u16 number of chunks to read
    if call_data.is_empty() {
        return Err(PrecompileError::Other(format!(
            "Invalid empty calldata (function selector required)",
        ))
        .into());
    }

    let access_list = &env.tx.access_list;
    if access_list.is_empty() {
        // this is a constant requirement across all execution paths, as we always need at least one PD request range, which must be in the access list.
        return Err(PrecompileError::Other("Transaction has no access list".to_string()).into());
    }

    let call_data_vec = call_data.to_vec();

    // decode the first byte of the calldata
    let decoded_id = PdFunctionId::try_from(call_data_vec[0])
        .map_err(|e| PrecompileErrors::Error(PrecompileError::Other(format!("{}", &e))))?;

    let state_provider = state_provider
        .provider
        .get()
        .ok_or(PrecompileErrors::Error(PrecompileError::Other(
            "Internal error - provider uninitialised".to_owned(),
        )))?;

    let parsed = parse_access_list(access_list).map_err(|_| {
        PrecompileErrors::Error(PrecompileError::Other(
            "Internal error - unable to parse access list".to_owned(),
        ))
    })?;

    debug!("Parsed access lists: {:?}", &parsed);

    match decoded_id {
        PdFunctionId::ReadBytesFirstRange => {
            read_bytes_first_range(call_data, gas_limit, env, state_provider, parsed)
        }
        PdFunctionId::ReadBytesFirstRangeWithArgs => {
            read_bytes_first_range_with_args(call_data, gas_limit, env, state_provider, parsed)
        }
    }
}
