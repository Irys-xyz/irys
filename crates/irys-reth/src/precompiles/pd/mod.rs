//! Programmable Data (PD) Precompile
//!
//! The PD precompile enables efficient on-chain access to large data stored in Irys partitions.
//! It provides two main operations for reading byte ranges from chunk-based storage.
//!
//! # Overview
//!
//! The PD precompile is designed for use cases where smart contracts need to access large
//! amounts of data that are too expensive to store directly in EVM state. Data is stored
//! in partitions divided into chunks, and the precompile allows contracts to read specific
//! byte ranges efficiently.
//!
//! # Architecture
//!
//! ```text
//! Transaction → PD Precompile (0x500) → ChunkProvider → Storage
//!   Access List   1. Parse calldata     Fetch/unpack    Partitions
//!   Calldata      2. Validate ranges    chunks          & Chunks
//!                 3. Calculate gas
//!                 4. Return bytes
//! ```

pub mod constants;
pub mod context;
pub mod error;
pub mod functions;
pub mod precompile;
pub mod read_bytes;
pub mod utils;

pub use context::PdContext;
