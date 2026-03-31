//! Constants for the Programmable Data precompile.

/// Flat EVM gas cost charged for any PD precompile invocation.
///
/// This is intentionally a small, fixed cost. The real anti-DoS mechanism for PD chunk I/O
/// is the **per-chunk PD fee** deducted at EVM execution time in `IrysEvm::transact_raw()`
/// (see `crates/irys-reth/src/evm.rs`), not EVM gas metering. PD transactions pay
/// `(base_fee + priority_fee) × chunk_count` in IRYS tokens before execution begins,
/// and total chunks per block are capped by `MAX_PD_CHUNKS_PER_BLOCK`.
///
/// TODO: Benchmark to find optimal cost. Consider reporting non-zero `gas_used`
/// proportional to chunks for more accurate block gas accounting, even though
/// PD fees are the primary payment mechanism.
pub const PD_BASE_GAS_COST: u64 = 5000;
