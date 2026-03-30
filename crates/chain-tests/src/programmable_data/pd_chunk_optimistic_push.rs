//! PD chunk optimistic push integration tests.
//!
//! Tests the optimistic push feature across a 3-node network:
//! - Genesis (Node A): has partition assignments, stores chunks, source of push
//! - Block Producer (Node B): staked with assignments, no PD data for test offsets
//! - Observer (Node C): not staked, not pledged, validator-only push target
//!
//! Three scenarios:
//! 1. Happy path: push delivers chunk before block validation
//! 2. Cache-hit shortcut: duplicate push exits early without re-verification
//! 3. Pending fetch reconciliation: push wins over in-flight P2P fetch

use irys_types::irys::IrysSigner;

use crate::utils::IrysNodeTest;

/// Context returned by [`setup_pd_push_test`] containing all 3 nodes and metadata.
#[expect(dead_code, reason = "fields available for future test assertions")]
pub(crate) struct PdPushTestContext {
    /// Genesis node: has partition assignments, stores chunks, source of optimistic pushes.
    pub genesis: IrysNodeTest<irys_chain::IrysNodeCtx>,
    /// Block producer peer: staked, has assignments. Does NOT store PD data for test offsets.
    pub block_producer: IrysNodeTest<irys_chain::IrysNodeCtx>,
    /// Observer node: not staked, not pledged, validator-only. Primary push target.
    pub observer: IrysNodeTest<irys_chain::IrysNodeCtx>,
    /// Global ledger offset where the uploaded data starts (Publish ledger).
    pub data_start_offset: u64,
    /// Partition index derived from `data_start_offset / num_chunks_in_partition`.
    pub partition_index: u64,
    /// Local offset within the partition: `(data_start_offset % num_chunks_in_partition) as u32`.
    pub local_offset: u32,
    /// Number of chunks in a partition (from consensus config).
    pub num_chunks_in_partition: u64,
    /// Chunk size in bytes (from consensus config).
    pub chunk_size: u64,
    /// Number of chunks uploaded.
    pub num_chunks_uploaded: u64,
    /// The raw data bytes that were uploaded to Genesis.
    pub data_bytes: Vec<u8>,
    /// Signer/account used to upload data on Genesis.
    pub data_signer: IrysSigner,
    /// Signer/account for the block producer (Node B).
    pub peer_signer: IrysSigner,
    /// Signer/account for the observer (Node C).
    pub observer_signer: IrysSigner,
    /// Signer/account dedicated to submitting PD transactions.
    pub pd_signer: IrysSigner,
    /// Second PD signer for tests needing two independent PD accounts.
    pub pd_signer_2: IrysSigner,
}
