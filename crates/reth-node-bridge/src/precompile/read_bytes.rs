use eyre::eyre;
use irys_packing::unpack;
use irys_primitives::range_specifier::{ByteRangeSpecifier, ChunkRangeSpecifier, U34};
use irys_storage::reth_provider::IrysRethProviderInner;
use revm_primitives::{
    Bytes, Env, PrecompileError, PrecompileErrors, PrecompileOutput, PrecompileResult,
};
use tracing::info;

use super::utils::ParsedAccessLists;

const PD_CHUNK_READ_COST: u64 = 500;

struct ReadBytesRangeByIndexArgs {
    index: u8,
}

impl ReadBytesRangeByIndexArgs {
    const CALLDATA_LEN: usize = 1 + 1;

    pub fn decode(bytes: &Bytes) -> Result<Self, PrecompileErrors> {
        if bytes.len() != ReadBytesRangeByIndexArgs::CALLDATA_LEN {
            return Err(PrecompileErrors::Error(PrecompileError::Other(
                "Invalid calldata".to_owned(),
            )));
        }
        Ok(ReadBytesRangeByIndexArgs {
            index: u8::from_be(bytes[1]),
        })
    }
}

pub fn read_bytes_range_by_index(
    call_data: &Bytes,
    gas_limit: u64,
    env: &Env,
    state_provider: &IrysRethProviderInner,
    access_lists: ParsedAccessLists,
) -> PrecompileResult {
    let ReadBytesRangeByIndexArgs { index } = ReadBytesRangeByIndexArgs::decode(call_data)?;
    // read the first bytes range
    let bytes_range =
        *access_lists
            .byte_reads
            .get(index as usize)
            .ok_or(PrecompileErrors::Error(PrecompileError::Other(
                "Internal error - unable to parse access list".to_owned(),
            )))?;
    read_bytes_range(bytes_range, state_provider, access_lists)
}

struct ReadPartialByteRangeArgs {
    index: u8,
    offset: u32,
    length: u32,
}

impl ReadPartialByteRangeArgs {
    const CALLDATA_LEN: usize = 1 + 1 + 4 + 4;

    pub fn decode(bytes: &Bytes) -> eyre::Result<ReadPartialByteRangeArgs> {
        if bytes.len() != ReadPartialByteRangeArgs::CALLDATA_LEN {
            return Err(eyre!("Invalid calldata length"));
        }
        Ok(ReadPartialByteRangeArgs {
            index: u8::from_be(bytes[1]),
            offset: u32::from_be_bytes(bytes[2..=5].try_into()?),
            length: u32::from_be_bytes(bytes[6..=9].try_into()?),
        })
    }
}

// this method overrides the length, and augments the start
pub fn read_partial_byte_range(
    call_data: &Bytes,
    gas_limit: u64,
    env: &Env,
    state_provider: &IrysRethProviderInner,
    access_lists: ParsedAccessLists,
) -> PrecompileResult {
    let ReadPartialByteRangeArgs {
        index,
        offset,
        length,
    } = ReadPartialByteRangeArgs::decode(call_data).map_err(|_| {
        PrecompileErrors::Error(PrecompileError::Other("Invalid calldata".to_owned()))
    })?;

    let mut bytes_range =
        *access_lists
            .byte_reads
            .get(index as usize)
            .ok_or(PrecompileErrors::Error(PrecompileError::Other(
                "Internal error - unable to parse access list".to_owned(),
            )))?;

    // add the provided offset and length to the existing bytes range
    // this seems weird, but the API is envisioned as each byte range being a tx/region of interest, so these calls would allow for efficient relative indexing.
    bytes_range
        .translate_offset(
            state_provider.chunk_provider.storage_config.chunk_size,
            offset as u64,
        )
        .map_err(|_| {
            PrecompileErrors::Error(PrecompileError::Other(
                "Internal error - unable to apply offset to byte range".to_string(),
            ))
        })?;

    bytes_range.length = U34::try_from(length).map_err(|_| {
        PrecompileErrors::Error(PrecompileError::Other(
            "Internal error - unable to use new length".to_string(),
        ))
    })?;

    read_bytes_range(bytes_range, state_provider, access_lists)
}

pub fn read_bytes_range(
    bytes_range: ByteRangeSpecifier,
    state_provider: &IrysRethProviderInner,
    access_lists: ParsedAccessLists,
) -> PrecompileResult {
    let ByteRangeSpecifier {
        index,
        chunk_offset,
        byte_offset,
        length,
    } = bytes_range;

    // get chunk read specified by the byte read req
    let chunk_read_range = access_lists
        .chunk_reads
        // safe as usize should never be < u8
        .get(index as usize)
        .ok_or(PrecompileErrors::Error(PrecompileError::Other(
            "Invalid byte read range chunk range index".to_owned(),
        )))?;

    let ChunkRangeSpecifier {
        partition_index,
        offset,
        chunk_count,
    } = chunk_read_range;

    // coordinate translation time!

    let storage_config = &state_provider.chunk_provider.storage_config;

    // TODO: this will error if the partition_index > u64::MAX
    // this is fine for testnet, but will need fixing later.

    let translated_base_chunks_offset =
        storage_config
            .num_chunks_in_partition
            .saturating_mul(partition_index.try_into().map_err(|_| {
                PrecompileErrors::Error(PrecompileError::Other(format!(
                    "partition_index {} is out of range (u64)",
                    partition_index,
                )))
            })?);

    let translated_chunks_start_offset =
        translated_base_chunks_offset.saturating_add(*offset as u64);
    let translated_chunks_end_offset =
        translated_chunks_start_offset.saturating_add(*chunk_count as u64);

    let mut bytes = Vec::with_capacity((*chunk_count as u64 * storage_config.chunk_size) as usize);
    for i in translated_chunks_start_offset..translated_chunks_end_offset {
        let chunk = state_provider
            .chunk_provider
            .get_chunk_by_ledger_offset(irys_database::Ledger::Publish, i.into())
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
        info!("JESSEDEBUG: Read PD chunk {}", &i);

        // TODO: the mempool should request & unpack PD chunks in advance
        let unpacked_chunk = unpack(
            &chunk,
            storage_config.entropy_packing_iterations,
            storage_config.chunk_size as usize,
            storage_config.chain_id,
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

    let offset: usize = ((chunk_offset as u64) * storage_config.chunk_size + truncated_byte_offset)
        .try_into()
        .map_err(|_| {
            PrecompileErrors::Error(PrecompileError::Other(format!(
                "byte_offset {} is out of range (u64)",
                byte_offset,
            )))
        })?;

    let truncated_len: usize = length.try_into().map_err(|_| {
        PrecompileErrors::Error(PrecompileError::Other(format!(
            "length {} is out of range (usize)",
            length,
        )))
    })?;

    // bounds check - as the impl is basic (i.e reads all chunks regardless of requested bytes range), this automatically accounts for the chunk range bounds
    if offset + truncated_len > bytes.len() {
        return Err(PrecompileErrors::Error(PrecompileError::Other(
            "Offset + len is outside of the requested chunk range".to_string(),
        )));
    }
    let extracted: Bytes = bytes.drain(offset..offset + truncated_len).collect();

    Ok(PrecompileOutput {
        gas_used: (*chunk_count as u64) * PD_CHUNK_READ_COST,
        bytes: extracted,
    })
}
