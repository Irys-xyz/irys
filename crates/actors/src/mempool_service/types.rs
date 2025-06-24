use core::fmt::Display;
use irys_types::{
    chunk::UnpackedChunk, Address, Base64, CommitmentTransaction, DataRoot, IrysBlockHeader,
    IrysTransactionHeader, IrysTransactionId, TxChunkOffset, H256,
};
use lru::LruCache;
use reth::rpc::types::BlockId;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};
use tokio::sync::oneshot;
use tokio::sync::RwLock;

#[derive(Debug)]
pub struct MempoolState {
    /// valid submit txs
    pub valid_submit_ledger_tx: BTreeMap<H256, IrysTransactionHeader>,
    pub valid_commitment_tx: BTreeMap<Address, Vec<CommitmentTransaction>>,
    /// The miner's signer instance, used to sign ingress proofs
    pub invalid_tx: Vec<H256>,
    /// Tracks recent valid txids from either data or commitment
    pub recent_valid_tx: HashSet<H256>,
    /// LRU caches for out of order gossip data
    pub pending_chunks: LruCache<DataRoot, LruCache<TxChunkOffset, UnpackedChunk>>,
    pub pending_pledges: LruCache<Address, LruCache<IrysTransactionId, CommitmentTransaction>>,
    /// pre-validated blocks that have passed pre-validation in discovery service
    pub prevalidated_blocks: HashMap<H256, IrysBlockHeader>,
    pub prevalidated_blocks_poa: HashMap<H256, Base64>,
}

pub type AtomicMempoolState = Arc<RwLock<MempoolState>>;

/// Messages that the Mempool Service handler supports
#[derive(Debug)]
pub enum MempoolServiceMessage {
    /// Block Confirmed, read publish txs from block. Overwrite copies in mempool with proof
    BlockConfirmed(Arc<IrysBlockHeader>),
    /// Ingress Chunk, Add to CachedChunks, generate_ingress_proof, gossip chunk
    IngestChunk(
        UnpackedChunk,
        oneshot::Sender<Result<(), ChunkIngressError>>,
    ),
    /// Ingress Pre-validated Block
    IngestBlocks {
        prevalidated_blocks: Vec<Arc<IrysBlockHeader>>,
    },
    /// Confirm commitment tx exists in mempool
    CommitmentTxExists(H256, oneshot::Sender<Result<bool, TxReadError>>),
    /// Ingress CommitmentTransaction into the mempool
    ///
    /// This function performs a series of checks and validations:
    /// - Skips the transaction if it is already known to be invalid or previously processed
    /// - Validates the transaction's anchor and signature
    /// - Inserts the valid transaction into the mempool and database
    /// - Processes any pending pledge transactions that depended on this commitment
    /// - Gossips the transaction to peers if accepted
    /// - Caches the transaction for unstaked signers to be reprocessed later
    IngestCommitmentTx(
        CommitmentTransaction,
        oneshot::Sender<Result<(), TxIngressError>>,
    ),
    /// Confirm data tx exists in mempool or database
    DataTxExists(H256, oneshot::Sender<Result<bool, TxReadError>>),
    /// validate and process an incoming IrysTransactionHeader
    IngestDataTx(
        IrysTransactionHeader,
        oneshot::Sender<Result<(), TxIngressError>>,
    ),
    /// Return filtered list of candidate txns
    /// Filtering based on funding status etc based on the provided EVM block ID
    /// If `None` is provided, the latest canonical block is used
    GetBestMempoolTxs(Option<BlockId>, oneshot::Sender<MempoolTxs>),
    /// Retrieves a list of CommitmentTransactions based on the provided tx ids
    GetCommitmentTxs {
        commitment_tx_ids: Vec<IrysTransactionId>,
        response: oneshot::Sender<HashMap<IrysTransactionId, CommitmentTransaction>>,
    },
    /// Get IrysTransactionHeader from mempool or mdbx
    GetDataTxs(
        Vec<IrysTransactionId>,
        oneshot::Sender<Vec<Option<IrysTransactionHeader>>>,
    ),
    /// Get block header from the mempool cache
    GetBlockHeader(H256, bool, oneshot::Sender<Option<IrysBlockHeader>>),
}

/// Reasons why Transaction Ingress might fail
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxIngressError {
    /// The transaction's signature is invalid
    InvalidSignature,
    /// The account does not have enough tokens to fund this transaction
    Unfunded,
    /// This transaction id is already in the cache
    Skipped,
    /// Invalid anchor value (unknown or too old)
    InvalidAnchor,
    /// Some database error occurred
    DatabaseError,
    /// The service is uninitialized
    ServiceUninitialized,
    /// Catch-all variant for other errors.
    Other(String),
}

impl TxIngressError {
    /// Returns an other error with the given message.
    pub fn other(err: impl Into<String>) -> Self {
        Self::Other(err.into())
    }
    /// Allows converting an error that implements Display into an Other error
    pub fn other_display(err: impl Display) -> Self {
        Self::Other(err.to_string())
    }
}

/// Reasons why Chunk Ingress might fail
#[derive(Debug, Clone)]
pub enum ChunkIngressError {
    /// The `data_path/proof` provided with the chunk data is invalid
    InvalidProof,
    /// The data hash does not match the chunk data
    InvalidDataHash,
    /// This chunk is for an unknown transaction
    UnknownTransaction,
    /// Only the last chunk in a `data_root` tree can be less than `CHUNK_SIZE`
    InvalidChunkSize,
    /// Chunks should have the same data_size field as their parent tx
    InvalidDataSize,
    /// Some database error occurred when reading or writing the chunk
    DatabaseError,
    /// The service is uninitialized
    ServiceUninitialized,
    /// Catch-all variant for other errors.
    Other(String),
}

impl ChunkIngressError {
    /// Returns an other error with the given message.
    pub fn other(err: impl Into<String>) -> Self {
        Self::Other(err.into())
    }
    /// Allows converting an error that implements Display into an Other error
    pub fn other_display(err: impl Display) -> Self {
        Self::Other(err.to_string())
    }
}

/// Reasons why reading a transaction might fail
#[derive(Debug, Clone)]
pub enum TxReadError {
    /// Some database error occurred when reading
    DatabaseError,
    /// The service is uninitialized
    ServiceUninitialized,
    /// The commitment transaction is not found in the mempool
    CommitmentTxNotInMempool,
    /// The transaction is not found in the mempool
    DataTxNotInMempool,
    /// Catch-all variant for other errors.
    Other(String),
}

impl TxReadError {
    /// Returns an other error with the given message.
    pub fn other(err: impl Into<String>) -> Self {
        Self::Other(err.into())
    }
    /// Allows converting an error that implements Display into an Other error
    pub fn other_display(err: impl Display) -> Self {
        Self::Other(err.to_string())
    }
}

#[derive(Debug)]
pub struct MempoolTxs {
    pub commitment_tx: Vec<CommitmentTransaction>,
    pub submit_tx: Vec<IrysTransactionHeader>,
}
