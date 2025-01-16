// pub fn read_chunks_precompile() {
//     // TODO: I don't like this, but it behaves as expected but surely there's a nicer way
//     // TODO: tell the compiler that we will only ever move invalid_input once as it'll short circuit the function
//     let range_index = u32::from_be_bytes(
//         call_data_vec[0..U32_BYTES]
//             .try_into()
//             .map_err(|_| invalid_input.clone())?,
//     );
//     let _start_offset = u32::from_be_bytes(
//         call_data_vec[U32_BYTES..U32_BYTES * 2]
//             .try_into()
//             .map_err(|_| invalid_input.clone())?,
//     );
//     let _to_read = u16::from_be_bytes(
//         call_data_vec[U32_BYTES * 2..U32_BYTES * 2 + U16_BYTES]
//             .try_into()
//             .map_err(|_| invalid_input.clone())?,
//     );

//     // find access_list entry for the address of this precompile

//     // TODO: evaluate if we should check for every entry that is addressed to this precompile, and collate them
//     let range_specifiers: Vec<&FixedBytes<32>> = access_list
//         .iter()
//         .find(|item| item.address == PRECOMPILE_ADDRESS)
//         .ok_or(PrecompileErrors::Error(PrecompileError::Other(
//             "Transaction has no access list entries for this precompile".to_string(),
//         )))?
//         .storage_keys
//         .iter()
//         // .filter_map(|sk| {
//         // })
//         // .map(|sk| RangeSpecifier::from_slice(&sk.0))
//         .collect();

//     let mut grouped_data: HashMap<PdAccessListArgsTypeId, Vec<[u8; 32]>> = HashMap::new();

//     for key in range_specifiers {
//         if let Ok(id) = PdAccessListArgsTypeId::try_from(key[0]) {
//             grouped_data.entry(id).or_insert_with(Vec::new).push(key.0);
//         }
//     }

//     // find the requested range specifier
//     let range_index_usize: usize = range_index.try_into().map_err(|_| invalid_input.clone())?;
//     let range_specifier =
//         range_specifiers
//             .get(range_index_usize)
//             .ok_or(PrecompileErrors::Error(PrecompileError::Other(format!(
//                 "range specifier index {} is out of range {}",
//                 range_index_usize,
//                 range_specifiers.len()
//             ))))?;

//     // we have the range specifier, now we need to load the data from the node

//     #[allow(unused_variables)]
//     let RangeSpecifier {
//         partition_index,
//         offset,
//         chunk_count,
//     } = range_specifier;

//     let provider_inner = state_provider
//         .provider
//         .get()
//         .ok_or(PrecompileErrors::Error(PrecompileError::Other(
//             "Internal error - provider uninitialised".to_owned(),
//         )))?;

//     let storage_config = &provider_inner.chunk_provider.storage_config;

//     // TODO: this will error if the partition_index > u64::MAX
//     // this is fine for testnet, but will need fixing later.

//     let translated_base_offset =
//         storage_config
//             .num_chunks_in_partition
//             .saturating_mul(partition_index.try_into().map_err(|_| {
//                 PrecompileErrors::Error(PrecompileError::Other(format!(
//                     "partition_index {} is out of range (u64)",
//                     partition_index,
//                 )))
//             })?);
//     let translated_start_offset = translated_base_offset.saturating_add(*offset as u64);
//     let translated_end_offset = translated_start_offset.saturating_add(*chunk_count as u64);
//     // TODO: make safer
//     let mut bytes = Vec::with_capacity((*chunk_count as u64 * storage_config.chunk_size) as usize);
//     for i in translated_start_offset..translated_end_offset {
//         let chunk = provider_inner
//             .chunk_provider
//             .get_chunk_by_ledger_offset(irys_database::Ledger::Publish, i)
//             .map_err(|e| {
//                 PrecompileErrors::Error(PrecompileError::Other(format!(
//                     "Error reading chunk with part offset {} - {}",
//                     &i, &e
//                 )))
//             })?
//             .ok_or(PrecompileErrors::Error(PrecompileError::Other(format!(
//                 "Unable to read chunk with part offset {}",
//                 &i,
//             ))))?;
//         let unpacked_chunk = unpack(
//             &chunk,
//             storage_config.entropy_packing_iterations,
//             storage_config.chunk_size as usize,
//         );
//         bytes.extend(unpacked_chunk.bytes.0)
//     }

//     // TODO use a proper gas calc
//     Ok(PrecompileOutput::new(10_000, bytes.into()))
// }
