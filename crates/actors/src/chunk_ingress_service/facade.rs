use crate::chunk_ingress_service::{
    ChunkIngressError, ChunkIngressMessage, CriticalChunkIngressError, IngressProofError,
};
use crate::services::ServiceSenders;
use irys_types::{chunk::UnpackedChunk, IngressProof};
use tokio::sync::mpsc::UnboundedSender;

#[async_trait::async_trait]
pub trait ChunkIngressFacade: Clone + Send + Sync + 'static {
    async fn handle_chunk_ingress(&self, chunk: UnpackedChunk) -> Result<(), ChunkIngressError>;
    async fn handle_ingest_ingress_proof(
        &self,
        ingress_proof: IngressProof,
    ) -> Result<(), IngressProofError>;
}

#[derive(Clone, Debug)]
pub struct ChunkIngressFacadeImpl {
    service: UnboundedSender<ChunkIngressMessage>,
}

impl ChunkIngressFacadeImpl {
    pub fn new(service: UnboundedSender<ChunkIngressMessage>) -> Self {
        Self { service }
    }
}

impl From<&ServiceSenders> for ChunkIngressFacadeImpl {
    fn from(value: &ServiceSenders) -> Self {
        Self {
            service: value.chunk_ingress.clone(),
        }
    }
}

#[async_trait::async_trait]
impl ChunkIngressFacade for ChunkIngressFacadeImpl {
    async fn handle_chunk_ingress(&self, chunk: UnpackedChunk) -> Result<(), ChunkIngressError> {
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        let chunk_data_root = chunk.data_root;
        let chunk_tx_offset = chunk.tx_offset;
        self.service
            .send(ChunkIngressMessage::IngestChunk(chunk, oneshot_tx))
            .map_err(|_| {
                CriticalChunkIngressError::Other(format!(
                    "Error sending ChunkIngressMessage for chunk data_root {:?} tx_offset {}",
                    chunk_data_root, chunk_tx_offset
                ))
            })?;
        oneshot_rx.await.expect("to process ChunkIngressMessage")
    }

    async fn handle_ingest_ingress_proof(
        &self,
        ingress_proof: IngressProof,
    ) -> Result<(), IngressProofError> {
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        let data_root = ingress_proof.data_root;
        self.service
            .send(ChunkIngressMessage::IngestIngressProof(
                ingress_proof,
                oneshot_tx,
            ))
            .map_err(|_| {
                IngressProofError::Other(format!(
                    "Error sending IngestIngressProof message for data_root {:?}",
                    data_root
                ))
            })?;
        oneshot_rx
            .await
            .expect("to process IngestIngressProof message")
    }
}
