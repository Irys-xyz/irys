//! CPU implementations of the two entropy-packing variants under comparison.
//!
//! Variant A is the current algorithm (`irys_c::capacity_single::compute_entropy_chunk`).
//! Variant B keeps the seed, Phase 1, the SHA count and the 64-byte input per step, but in
//! Phase 2 hashes `prev || buf[j]` where `j` is derived from the previous hash output, so the
//! read index is only known once the previous hash has completed.

use openssl::sha;

pub const SEGMENT_SIZE: usize = 32;
pub const SEGMENTS: usize = 8192;
pub const CHUNK_SIZE: usize = SEGMENTS * SEGMENT_SIZE;
pub const DEFAULT_ITERATIONS: u32 = 1_000_000;

pub type Chunk = [u8; CHUNK_SIZE];

pub mod autotune;
pub mod cpu;
pub mod cpu_avx2;
pub mod cpu_avx512;
pub mod cpu_fast;
#[cfg(feature = "cuda")]
pub mod gpu;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, clap::ValueEnum)]
pub enum Variant {
    A,
    B,
}

impl Variant {
    pub const ALL: [Self; 2] = [Self::A, Self::B];

    pub fn name(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::B => "B",
        }
    }
}

/// Inputs identifying one entropy chunk.
#[derive(Clone, Copy, Debug)]
pub struct ChunkId {
    pub address: [u8; 20],
    pub partition_hash: [u8; 32],
    pub chain_id: u64,
    pub partition_offset: u64,
}

/// seed = SHA(address(20) || partition_hash(32) || chain_id u64 LE || partition_offset u64 LE)
#[inline]
pub fn seed_hash(id: &ChunkId) -> [u8; 32] {
    let mut h = sha::Sha256::new();
    h.update(&id.address);
    h.update(&id.partition_hash);
    h.update(&id.chain_id.to_le_bytes());
    h.update(&id.partition_offset.to_le_bytes());
    h.finish()
}

#[inline(always)]
fn seg(buf: &Chunk, i: usize) -> &[u8] {
    &buf[i * SEGMENT_SIZE..(i + 1) * SEGMENT_SIZE]
}

/// Phase 1 (shared by A and B): prev = seed; for i in 0..8192 { prev = SHA(prev); buf[i] = prev }
#[inline]
fn phase1(id: &ChunkId, buf: &mut Chunk) {
    let mut prev = seed_hash(id);
    for i in 0..SEGMENTS {
        // Not `sha::sha256`: in OpenSSL 3 that one-shot call is `EVP_Q_digest(.., "SHA256", ..)`,
        // which looks the algorithm up by name in the shared provider store on every call and
        // stops scaling past a few threads. `Sha256::new()` (SHA256_Init) has no shared state.
        let mut h = sha::Sha256::new();
        h.update(&prev);
        prev = h.finish();
        buf[i * SEGMENT_SIZE..(i + 1) * SEGMENT_SIZE].copy_from_slice(&prev);
    }
}

/// Variant A (current): buf[i] = SHA(buf[i-1 mod 8192] || buf[i]).
pub fn entropy_a(id: &ChunkId, iterations: u32, buf: &mut Chunk) {
    phase1(id, buf);
    let mut hash_count = SEGMENTS;
    while hash_count < iterations as usize {
        let i = hash_count % SEGMENTS;
        let left = if i == 0 { SEGMENTS - 1 } else { i - 1 };
        let mut h = sha::Sha256::new();
        h.update(seg(buf, left));
        h.update(seg(buf, i));
        let out = h.finish();
        buf[i * SEGMENT_SIZE..(i + 1) * SEGMENT_SIZE].copy_from_slice(&out);
        hash_count += 1;
    }
}

/// Variant B (candidate): j = LE u32(prev[0..4]) & 8191; buf[i] = SHA(prev || buf[j]); prev = buf[i].
pub fn entropy_b(id: &ChunkId, iterations: u32, buf: &mut Chunk) {
    phase1(id, buf);
    let mut prev: [u8; 32] = seg(buf, SEGMENTS - 1).try_into().unwrap();
    let mut hash_count = SEGMENTS;
    while hash_count < iterations as usize {
        let i = hash_count % SEGMENTS;
        let j = (u32::from_le_bytes(prev[0..4].try_into().unwrap()) as usize) & (SEGMENTS - 1);
        let mut h = sha::Sha256::new();
        h.update(&prev);
        h.update(seg(buf, j));
        prev = h.finish();
        buf[i * SEGMENT_SIZE..(i + 1) * SEGMENT_SIZE].copy_from_slice(&prev);
        hash_count += 1;
    }
}

pub fn entropy(variant: Variant, id: &ChunkId, iterations: u32, buf: &mut Chunk) {
    match variant {
        Variant::A => entropy_a(id, iterations, buf),
        Variant::B => entropy_b(id, iterations, buf),
    }
}

/// The unmodified reference implementation of variant A from `irys-c`.
pub fn reference_a(id: &ChunkId, iterations: u32, out: &mut Vec<u8>) {
    irys_c::capacity_single::compute_entropy_chunk(
        irys_types::IrysAddress::from(id.address),
        id.partition_offset as std::ffi::c_ulong,
        id.partition_hash,
        iterations,
        CHUNK_SIZE,
        out,
        id.chain_id,
    );
}

pub fn new_chunk() -> Box<Chunk> {
    vec![0_u8; CHUNK_SIZE]
        .into_boxed_slice()
        .try_into()
        .unwrap()
}

/// Fixed benchmark/test inputs so runs are reproducible.
pub fn fixed_id(partition_offset: u64) -> ChunkId {
    let mut address = [0_u8; 20];
    let mut partition_hash = [0_u8; 32];
    for (k, b) in address.iter_mut().enumerate() {
        *b = 0xA0 ^ k as u8;
    }
    for (k, b) in partition_hash.iter_mut().enumerate() {
        *b = (k as u8).wrapping_mul(7).wrapping_add(3);
    }
    ChunkId {
        address,
        partition_hash,
        chain_id: 1270,
        partition_offset,
    }
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
