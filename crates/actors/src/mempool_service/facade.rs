use crate::mempool_service::{
    ChunkIngressError, MempoolServiceMessage, TxIngressError, TxReadError,
};
use irys_types::{
    chunk::UnpackedChunk, CommitmentTransaction, IrysBlockHeader, IrysTransactionHeader, H256,
};
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
}

#[derive(Clone, Debug)]
pub struct MempoolServiceFacadeImpl {
    service: UnboundedSender<MempoolServiceMessage>,
}

impl From<UnboundedSender<MempoolServiceMessage>> for MempoolServiceFacadeImpl {
    fn from(value: UnboundedSender<MempoolServiceMessage>) -> Self {
        Self { service: value }
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
}
