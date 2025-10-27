use alloy_primitives::Address;

/// Const function for making an address by concatenating the bytes from two given numbers.
///
/// Note that 32 + 128 = 160 = 20 bytes (the length of an address). This function is used
/// as a convenience for specifying the addresses of the various precompiles.
#[inline]
pub const fn u64_to_address(x: u64) -> Address {
    let x = x.to_be_bytes();
    Address::new([
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, x[0], x[1], x[2], x[3], x[4], x[5], x[6], x[7],
    ])
}

// reserve space for any future eth precompiles
// 0x500
const BASE_PRECOMPILE_OFFSET: u64 = 1280;

#[repr(u64)]
pub enum IrysPrecompileOffsets {
    ProgrammableData = BASE_PRECOMPILE_OFFSET,
}

impl IrysPrecompileOffsets {
    pub const fn to_address(self) -> Address {
        u64_to_address(self as u64)
    }
}

impl From<IrysPrecompileOffsets> for Address {
    fn from(val: IrysPrecompileOffsets) -> Self {
        val.to_address()
    }
}

pub const PD_PRECOMPILE_ADDRESS: Address = IrysPrecompileOffsets::ProgrammableData.to_address();

pub const PD_COST_PER_CHUNK: u64 = 5_000;

pub const ACCESS_KEY_NOP: u64 = 16 * 32; // 16 gas per byte, 32 bytes - same pricing as calldata (as it bloats the block about as much)
