use crate::block_tree_service::BlockMigratedEvent;
use crate::mempool_service::{
    ChunkIngressError, MempoolServiceMessage, TxIngressError, TxReadError,
};
use crate::services::ServiceSenders;
use eyre::eyre;
use irys_types::{
    chunk::UnpackedChunk, Base64, CommitmentTransaction, IrysBlockHeader, IrysTransactionHeader,
    H256,
};
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::sync::mpsc::UnboundedSender;

#[async_trait::async_trait]
pub trait MempoolFacade: Clone + Send + Sync + 'static {
    async fn handle_data_transaction_ingress(
        &self,
        tx_header: IrysTransactionHeader,
    ) -> Result<(), TxIngressError>;
    async fn handle_commitment_transaction_ingress(
        &self,
        tx_header: CommitmentTransaction,
    ) -> Result<(), TxIngressError>;
    async fn handle_chunk_ingress(&self, chunk: UnpackedChunk) -> Result<(), ChunkIngressError>;
    async fn is_known_transaction(&self, tx_id: H256) -> Result<bool, TxReadError>;
    async fn get_block_header(
        &self,
        block_hash: H256,
        include_chunk: bool,
    ) -> Result<Option<IrysBlockHeader>, TxReadError>;
    async fn migrate_block(
        &self,
        irys_block_header: Arc<IrysBlockHeader>,
    ) -> Result<usize, TxIngressError>;

    async fn insert_poa_chunk(&self, block_hash: H256, chunk_data: Base64) -> eyre::Result<()>;
}

#[derive(Clone, Debug)]
pub struct MempoolServiceFacadeImpl {
    service: UnboundedSender<MempoolServiceMessage>,
    migration_sender: broadcast::Sender<BlockMigratedEvent>,
}

impl MempoolServiceFacadeImpl {
    pub fn new(
        service: UnboundedSender<MempoolServiceMessage>,
        migration_sender: broadcast::Sender<BlockMigratedEvent>,
    ) -> Self {
        Self {
            service,
            migration_sender,
        }
    }
}

impl From<&ServiceSenders> for MempoolServiceFacadeImpl {
    fn from(value: &ServiceSenders) -> Self {
        Self {
            service: value.mempool.clone(),
            migration_sender: value.block_migrated_events.clone(),
        }
    }
}

#[async_trait::async_trait]
impl MempoolFacade for MempoolServiceFacadeImpl {
    async fn handle_data_transaction_ingress(
        &self,
        tx_header: IrysTransactionHeader,
    ) -> Result<(), TxIngressError> {
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        self.service
            .send(MempoolServiceMessage::IngestDataTx(tx_header, oneshot_tx))
            .map_err(|_| TxIngressError::Other("Error sending TxIngressMessage ".to_owned()))?;

        oneshot_rx.await.expect("to process TxIngressMessage")
    }

    async fn handle_commitment_transaction_ingress(
        &self,
        commitment_tx: CommitmentTransaction,
    ) -> Result<(), TxIngressError> {
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        self.service
            .send(MempoolServiceMessage::IngestCommitmentTx(
                commitment_tx,
                oneshot_tx,
            ))
            .map_err(|_| {
                TxIngressError::Other("Error sending CommitmentTxIngressMessage ".to_owned())
            })?;

        oneshot_rx
            .await
            .expect("to process CommitmentTxIngressMessage")
    }

    async fn handle_chunk_ingress(&self, chunk: UnpackedChunk) -> Result<(), ChunkIngressError> {
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        self.service
            .send(MempoolServiceMessage::IngestChunk(chunk, oneshot_tx))
            .map_err(|_| {
                ChunkIngressError::Other("Error sending ChunkIngressMessage ".to_owned())
            })?;

        oneshot_rx.await.expect("to process ChunkIngressMessage")
    }

    async fn is_known_transaction(&self, tx_id: H256) -> Result<bool, TxReadError> {
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        self.service
            .send(MempoolServiceMessage::DataTxExists(tx_id, oneshot_tx))
            .map_err(|_| TxReadError::Other("Error sending TxExistenceQuery ".to_owned()))?;

        oneshot_rx.await.expect("to process TxExistenceQuery")
    }

    async fn get_block_header(
        &self,
        block_hash: H256,
        include_chunk: bool,
    ) -> Result<Option<IrysBlockHeader>, TxReadError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.service
            .send(MempoolServiceMessage::GetBlockHeader(
                block_hash,
                include_chunk,
                tx,
            ))
            .map_err(|_| TxReadError::Other("Error sending GetBlockHeader message".to_owned()))?;

        rx.await
            .map_err(|_| TxReadError::Other("GetBlockHeader response error".to_owned()))
    }

    async fn migrate_block(
        &self,
        irys_block_header: Arc<IrysBlockHeader>,
    ) -> Result<usize, TxIngressError> {
        self.migration_sender
            .send(BlockMigratedEvent {
                block: irys_block_header,
            })
            .map_err(|e| TxIngressError::Other(format!("Failed to send BlockMigratedEvent: {}", e)))
    }

    async fn insert_poa_chunk(&self, block_hash: H256, chunk_data: Base64) -> eyre::Result<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.service
            .send(MempoolServiceMessage::InsertPoAChunk(
                block_hash, chunk_data, tx,
            ))
            .map_err(|send_error| eyre!("{send_error:?}"))?;

        rx.await.map_err(|recv_error| eyre!("{recv_error:?}"))
    }
}
