//! Data tree v2.
//!
//! One tree commits to uploaded bytes and to `data_size`: chunk proofs, segment
//! proofs, and the size witness (spec §1–§5). This crate has no L1 types.

#![forbid(unsafe_code)]

pub mod error;
pub mod geometry;
pub mod hash;
pub mod types;

pub use error::{BuildError, ChunkLengthError, GeometryError, ProofError, SizeError, VerifyError};
pub use geometry::Geometry;
pub use hash::{
    TAG_CHUNK_KEY, TAG_LEAF, TAG_NODE, TAG_SIZE, TAG_TICKET, chunk_key, data_commitment, empty,
    leaf, node, size_witness,
};
pub use types::{ChunkKey, DataCommitment, DataTreeRoot, NodeHash, SizeWitness};

#[cfg(test)]
mod tests;
