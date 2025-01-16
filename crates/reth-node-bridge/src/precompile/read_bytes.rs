// use irys_primitives::range_specifier::RangeSpecifier;
// use reth_db::transaction::DbTx;
// use reth_db::Database;

// use revm_primitives::{
//     Bytes, Env, PrecompileError, PrecompileErrors, PrecompileOutput, PrecompileResult,
// };

// use super::{
//     irys_executor::IrysPrecompileOffsets,
//     irys_executor::{CustomPrecompileWithAddress, PrecompileStateProvider},
// };

// const PRECOMPILE_ADDRESS: irys_types::Address =
//     IrysPrecompileOffsets::ProgrammableDataReadBytes.to_address();

// pub const PROGRAMMABLE_DATA_READ_BYTES_PRECOMPILE: CustomPrecompileWithAddress =
//     CustomPrecompileWithAddress(PRECOMPILE_ADDRESS, programmable_data_read_bytes_precompile);

// // const CALLDATA_LENGTH: usize = U8_BYTES /* flag */ + U32_BYTES + U32_BYTES + U16_BYTES;
// // just the bytes read range index
// const CALLDATA_LENGTH: usize = U8_BYTES;
// // const U32_BYTES: usize = size_of::<u32>();
// // const U16_BYTES: usize = size_of::<u16>();
// const U8_BYTES: usize = size_of::<u8>();

// /// programmable data precompile
// // TODO: Gas pricing
// fn programmable_data_read_bytes_precompile(
//     call_data: &Bytes,
//     _gas_limit: u64,
//     env: &Env,
//     state_provider: &PrecompileStateProvider,
// ) -> PrecompileResult {
//     // check flag

//     if call_data.len() != CALLDATA_LENGTH {
//         return Err(PrecompileError::Other(format!(
//             "Invalid calldata length, got {} expected {}",
//             call_data.len(),
//             CALLDATA_LENGTH,
//         ))
//         .into());
//     }

//     let access_list = &env.tx.access_list;
//     if access_list.is_empty() {
//         return Err(PrecompileError::Other("Transaction has no access list".to_string()).into());
//     }

//     // now we decompose the call data into it's parts
//     // we want to return as fast as possible for bad input, so we do cheap checks first

//     let call_data_vec = call_data.to_vec();
//     let invalid_input = PrecompileErrors::Error(PrecompileError::Other(
//         "Transaction has no access list entries for this precompile".to_string(),
//     ));

//     // TODO: I don't like this, but it behaves as expected but surely there's a nicer way
//     // TODO: tell the compiler that we will only ever move invalid_input once as it'll short circuit the function
//     // let range_index = u32::from_be_bytes(
//     //     call_data_vec[0..U32_BYTES]
//     //         .try_into()
//     //         .map_err(|_| invalid_input.clone())?,
//     // );
//     // let _start_offset = u32::from_be_bytes(
//     //     call_data_vec[U32_BYTES..U32_BYTES * 2]
//     //         .try_into()
//     //         .map_err(|_| invalid_input.clone())?,
//     // );
//     // let _to_read = u16::from_be_bytes(
//     //     call_data_vec[U32_BYTES * 2..U32_BYTES * 2 + U16_BYTES]
//     //         .try_into()
//     //         .map_err(|_| invalid_input.clone())?,
//     // );

//     return Err(invalid_input);

//     // let byte_read_index =

//     // // find access_list entry for the address of this precompile

//     // // TODO: evaluate if we should check for every entry that is addressed to this precompile, and collate them
//     // let range_specifiers: Vec<RangeSpecifier> = access_list
//     //     .iter()
//     //     .find(|item| item.address == PRECOMPILE_ADDRESS)
//     //     .ok_or(PrecompileErrors::Error(PrecompileError::Other(
//     //         "Transaction has no access list entries for this precompile".to_string(),
//     //     )))?
//     //     .storage_keys
//     //     .iter()
//     //     .map(|sk| RangeSpecifier::from_slice(&sk.0))
//     //     .collect();

//     // // find the requested range specifier
//     // let range_index_usize: usize = range_index.try_into().map_err(|_| invalid_input.clone())?;
//     // let range_specifier =
//     //     range_specifiers
//     //         .get(range_index_usize)
//     //         .ok_or(PrecompileErrors::Error(PrecompileError::Other(format!(
//     //             "range specifier index {} is out of range {}",
//     //             range_index_usize,
//     //             range_specifiers.len()
//     //         ))))?;

//     // // we have the range specifier, now we need to load the data from the node

//     // #[allow(unused_variables)]
//     // let RangeSpecifier {
//     //     partition_index,
//     //     offset,
//     //     chunk_count,
//     // } = range_specifier;

//     // let o: u32 = partition_index.try_into().unwrap();
//     // // // TODO FIXME: THIS IS FOR THE DEMO ONLY! ONCE WE HAVE THE FULL DATA MODEL THIS SHOULD BE CHANGED
//     // let key: u32 = (10 * o) + offset;

//     // let ro_tx = state_provider.provider.get().unwrap().db.tx().unwrap();

//     // let chunk = ro_tx
//     //     .get::<ProgrammableDataChunkCache>(key)
//     //     .unwrap()
//     //     .unwrap();

//     // // TODO use a proper gas calc
//     // Ok(PrecompileOutput::new(100, chunk.into()))
// }

use irys_packing::unpack;
use irys_primitives::range_specifier::{BytesRangeSpecifier, RangeSpecifier};
use irys_storage::reth_provider::IrysRethProviderInner;
use revm_primitives::{
    bytes::Buf, Bytes, Env, FixedBytes, PrecompileError, PrecompileErrors, PrecompileOutput,
    PrecompileResult,
};

use super::utils::ParsedAccessLists;

pub fn read_bytes_first_range(
    call_data: &Bytes,
    gas_limit: u64,
    env: &Env,
    state_provider: &IrysRethProviderInner,
    access_lists: ParsedAccessLists,
) -> PrecompileResult {
    // read the first bytes range
    let bytes_range = access_lists
        .byte_reads
        .get(0)
        .ok_or(PrecompileErrors::Error(PrecompileError::Other(
            "Internal error - unable to parse access list".to_owned(),
        )))?;

    let BytesRangeSpecifier {
        index,
        chunk_offset,
        byte_offset,
        len,
    } = bytes_range;

    // get chunk read specified by the byte read req
    let chunk_read_range = access_lists
        .chunk_reads
        // safe as usize should never be < u8
        .get(*index as usize)
        .ok_or(PrecompileErrors::Error(PrecompileError::Other(
            "Invalid byte read range chunk range index".to_owned(),
        )))?;

    let RangeSpecifier {
        partition_index,
        offset,
        chunk_count,
    } = chunk_read_range;

    // coordinate translation time!

    let storage_config = &state_provider.chunk_provider.storage_config;

    // TODO: this will error if the partition_index > u64::MAX
    // this is fine for testnet, but will need fixing later.

    let translated_base_offset =
        storage_config
            .num_chunks_in_partition
            .saturating_mul(partition_index.try_into().map_err(|_| {
                PrecompileErrors::Error(PrecompileError::Other(format!(
                    "partition_index {} is out of range (u64)",
                    partition_index,
                )))
            })?);

    let translated_start_offset = translated_base_offset.saturating_add(*offset as u64);
    let translated_end_offset = translated_start_offset.saturating_add(*chunk_count as u64);
    // TODO: make safer
    let mut bytes = Vec::with_capacity((*chunk_count as u64 * storage_config.chunk_size) as usize);
    for i in translated_start_offset..translated_end_offset {
        let chunk = state_provider
            .chunk_provider
            .get_chunk_by_ledger_offset(irys_database::Ledger::Publish, i)
            .map_err(|e| {
                PrecompileErrors::Error(PrecompileError::Other(format!(
                    "Error reading chunk with part offset {} - {}",
                    &i, &e
                )))
            })?
            .ok_or(PrecompileErrors::Error(PrecompileError::Other(format!(
                "Unable to read chunk with part offset {}",
                &i,
            ))))?;
        let unpacked_chunk = unpack(
            &chunk,
            storage_config.entropy_packing_iterations,
            storage_config.chunk_size as usize,
        );
        bytes.extend(unpacked_chunk.bytes.0)
    }

    // now we apply the bytes range
    // skip forward `chunk_offset` chunks + `byte_offset` bytes, then read `len` bytes
    let truncated_byte_offset: u64 = byte_offset.try_into().map_err(|_| {
        PrecompileErrors::Error(PrecompileError::Other(format!(
            "byte_offset {} is out of range (u64)",
            byte_offset,
        )))
    })?;

    let offset: usize = ((*chunk_offset as u64) * storage_config.chunk_size
        + truncated_byte_offset)
        .try_into()
        .map_err(|_| {
            PrecompileErrors::Error(PrecompileError::Other(format!(
                "byte_offset {} is out of range (u64)",
                byte_offset,
            )))
        })?;

    let truncated_len: usize = len.try_into().map_err(|_| {
        PrecompileErrors::Error(PrecompileError::Other(format!(
            "len {} is out of range (usize)",
            len,
        )))
    })?;

    let extracted: Bytes = bytes.drain(offset..offset + truncated_len).collect();

    Ok(PrecompileOutput {
        gas_used: 100,
        bytes: extracted,
    })
}

pub fn read_bytes_first_range_with_args(
    call_data: &Bytes,
    gas_limit: u64,
    env: &Env,
    state_provider: &IrysRethProviderInner,
    access_lists: ParsedAccessLists,
) -> PrecompileResult {
    Ok(PrecompileOutput {
        gas_used: todo!(),
        bytes: todo!(),
    })
}
