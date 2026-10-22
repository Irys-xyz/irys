//! Data tree v2.
//!
//! One tree commits to uploaded bytes and to `data_size`: chunk proofs, segment
//! proofs, and the size witness (spec §1–§5). This crate has no L1 types.
//! Only the normative sections of `docs/spec.md` bind this crate; proposal
//! sections describe planned integration.
//!
//! In this crate, "spec §N" means section N of `../docs/spec.md`.

#![forbid(unsafe_code)]

pub mod builder;
pub mod chunk;
pub mod error;
pub mod geometry;
pub mod hash;
pub mod shape;
pub mod types;
pub mod verify;

pub use builder::{DataTree, DataTreeBuilder};
pub use chunk::chunk_root;
pub use error::{BuildError, ChunkLengthError, GeometryError, ProofError, SizeError, VerifyError};
pub use geometry::{Geometry, MAX_SEGMENTS_PER_CHUNK, MAX_SUBTREE_HEIGHT};
pub use hash::{TreeDomainTag, chunk_key, data_commitment, empty, leaf, node, size_witness};
pub use shape::depth;
pub use types::{ChunkKey, DataCommitment, DataTreeRoot, HASH_SIZE, NodeHash, SizeWitness};
pub use verify::{verify_chunk, verify_segment, verify_size};

#[cfg(test)]
mod tests;
