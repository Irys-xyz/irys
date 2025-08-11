use irys_types::BlockHash;
use tokio::sync::oneshot;

#[derive(Debug)]
pub enum OrphanBlockProcessingMessage {
    ProcessOrphanedAncestor {
        block_hash: BlockHash,
        response: Option<oneshot::Sender<Result<(), OrphanBlockProcessingError>>>,
    },
    RequestParentBlock {
        parent_block_hash: BlockHash,
        response: Option<oneshot::Sender<Result<(), OrphanBlockProcessingError>>>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum OrphanBlockProcessingError {
    #[error("Database error: {0}")]
    DatabaseError(String),
    #[error("Mempool error: {0}")]
    MempoolError(String),
    #[error("Internal error: {0}")]
    OtherInternal(String),
    #[error("Block error: {0}")]
    BlockError(String),
    #[error("Previous block {0:?} not found")]
    PreviousBlockNotFound(BlockHash),
}
