use irys_database::tables::ProgrammableDataChunkCache;
use irys_primitives::range_specifier::RangeSpecifier;
use reth_db::transaction::DbTx;
use reth_db::Database;

use revm_primitives::{
    Bytes, Env, PrecompileError, PrecompileErrors, PrecompileOutput, PrecompileResult,
};

use super::{
    irys_executor::IrysPrecompileOffsets,
    irys_executor::{CustomPrecompileWithAddress, PrecompileStateProvider},
};

pub const PROGRAMMABLE_DATA_PRECOMPILE: CustomPrecompileWithAddress = CustomPrecompileWithAddress(
    IrysPrecompileOffsets::ProgrammableData.to_address(),
    programmable_data_precompile,
);

// u32: range specifier index, u32 range relative offset, u16 number of chunks to read
// TODO: make a type for this so we can encapsulate the decode + validation logic
const CALLDATA_LENGTH: usize = U32_BYTES + U32_BYTES + U16_BYTES;
const U32_BYTES: usize = size_of::<u32>();
const U16_BYTES: usize = size_of::<u16>();

/// programmable data precompile
// TODO: Gas pricing
fn programmable_data_precompile(
    call_data: &Bytes,
    gas_limit: u64,
    env: &Env,
    state_provider: &PrecompileStateProvider,
) -> PrecompileResult {
    // make sure we were given the u32 index, the u32 range relative offset, and the u16 number of chunks to read
    if call_data.len() != CALLDATA_LENGTH {
        return Err(PrecompileError::Other(format!(
            "Invalid calldata length, got {} expected {}",
            call_data.len(),
            CALLDATA_LENGTH,
        ))
        .into());
    }

    let access_list = &env.tx.access_list;
    if access_list.is_empty() {
        return Err(PrecompileError::Other("Transaction has no access list".to_string()).into());
    }

    // now we decompose the call data into it's parts
    // we want to return as fast as possible for bad input, so we do cheap checks first

    let call_data_vec = call_data.to_vec();
    let invalid_input = PrecompileErrors::Error(PrecompileError::Other(
        "Transaction has no access list entries for this precompile".to_string(),
    ));

    // TODO: I don't like this, but it behaves as expected but surely there's a nicer way
    // TODO: tell the compiler that we will only ever move invalid_input once as it'll short circuit the function
    let range_index = u32::from_be_bytes(
        call_data_vec[0..U32_BYTES]
            .try_into()
            .map_err(|_| invalid_input.clone())?,
    );
    let start_offset = u32::from_be_bytes(
        call_data_vec[U32_BYTES..U32_BYTES * 2]
            .try_into()
            .map_err(|_| invalid_input.clone())?,
    );
    let to_read = u16::from_be_bytes(
        call_data_vec[U32_BYTES * 2..U32_BYTES * 2 + U16_BYTES]
            .try_into()
            .map_err(|_| invalid_input.clone())?,
    );

    let precompile_address: irys_types::Address =
        IrysPrecompileOffsets::ProgrammableData.to_address();

    // find access_list entry for the address of this precompile

    // TODO: evaluate if we should check for every entry that is addressed to this precompile, and collate them
    let range_specifiers: Vec<RangeSpecifier> = access_list
        .iter()
        .find(|item| item.address == precompile_address)
        .ok_or(PrecompileErrors::Error(PrecompileError::Other(
            "Transaction has no access list entries for this precompile".to_string(),
        )))?
        .storage_keys
        .iter()
        .map(|sk| RangeSpecifier::from_slice(sk.0))
        .collect();

    // find the requested range specifier
    let range_index_usize: usize = range_index.try_into().map_err(|_| invalid_input.clone())?;
    let range_specifier =
        range_specifiers
            .get(range_index_usize)
            .ok_or(PrecompileErrors::Error(PrecompileError::Other(format!(
                "range specifier index {} is out of range {}",
                range_index_usize,
                range_specifiers.len()
            ))))?;

    // we have the range specifier, now we need to load the data from the node

    let RangeSpecifier {
        partition_index,
        offset,
        chunk_count,
    } = range_specifier;

    let o: u32 = partition_index.try_into().unwrap();
    // TODO FIXME: THIS IS FOR THE DEMO ONLY! ONCE WE HAVE THE FULL DATA MODEL THIS SHOULD BE CHANGED
    let key: u32 = (10 * o) + offset;
    let ro_tx = state_provider.provider.db.tx().unwrap();

    let chunk = ro_tx
        .get::<ProgrammableDataChunkCache>(key)
        .unwrap()
        .unwrap();

    // TODO use a proper gas calc
    Ok(PrecompileOutput::new(100, chunk.into()))
}
