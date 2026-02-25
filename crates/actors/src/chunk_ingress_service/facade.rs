use crate::chunk_ingress_service::{
    ChunkIngressError, ChunkIngressMessage, CriticalChunkIngressError, IngressProofError,
};
use crate::services::ServiceSenders;
use irys_types::{IngressProof, SendTraced as _, Traced, chunk::UnpackedChunk};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Clone, Debug)]
pub struct ChunkIngressFacadeImpl {
    service: UnboundedSender<Traced<ChunkIngressMessage>>,
}

impl ChunkIngressFacadeImpl {
    pub fn new(service: UnboundedSender<Traced<ChunkIngressMessage>>) -> Self {
        Self { service }
    }

    pub async fn handle_chunk_ingress(
        &self,
        chunk: UnpackedChunk,
    ) -> Result<(), ChunkIngressError> {
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        let chunk_data_root = chunk.data_root;
        let chunk_tx_offset = chunk.tx_offset;
        self.service
            .send_traced(ChunkIngressMessage::IngestChunk(chunk, Some(oneshot_tx)))
            .map_err(|_| {
                CriticalChunkIngressError::Other(format!(
                    "Error sending ChunkIngressMessage for chunk data_root {:?} tx_offset {}",
                    chunk_data_root, chunk_tx_offset
                ))
            })?;
        oneshot_rx.await.map_err(|_| {
            ChunkIngressError::Critical(CriticalChunkIngressError::Other(format!(
                "ChunkIngressService dropped response channel for chunk data_root {:?} tx_offset {}",
                chunk_data_root, chunk_tx_offset
            )))
        })?
    }

    pub async fn handle_ingest_ingress_proof(
        &self,
        ingress_proof: IngressProof,
    ) -> Result<(), IngressProofError> {
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        let data_root = ingress_proof.data_root;
        self.service
            .send_traced(ChunkIngressMessage::IngestIngressProof(
                ingress_proof,
                oneshot_tx,
            ))
            .map_err(|_| {
                IngressProofError::Other(format!(
                    "Error sending IngestIngressProof message for data_root {:?}",
                    data_root
                ))
            })?;
        oneshot_rx.await.map_err(|_| {
            IngressProofError::Other(format!(
                "ChunkIngressService dropped response channel for ingress proof data_root {:?}",
                data_root
            ))
        })?
    }
}

impl From<&ServiceSenders> for ChunkIngressFacadeImpl {
    fn from(value: &ServiceSenders) -> Self {
        Self {
            service: value.chunk_ingress.clone(),
        }
    }
}
