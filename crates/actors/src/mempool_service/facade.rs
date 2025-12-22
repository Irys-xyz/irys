use crate::block_tree_service::BlockMigratedEvent;
use crate::mempool_service::{
    ChunkIngressError, IngressProofError, MempoolServiceMessage, TxIngressError, TxReadError,
};
use crate::services::ServiceSenders;
use crate::{CriticalChunkIngressError, MempoolReadGuard};
use eyre::eyre;
use irys_types::{
    chunk::UnpackedChunk, CommitmentTransaction, DataTransactionHeader, IrysBlockHeader, H256,
};
use irys_types::{IngressProof, IrysAddress, TxKnownStatus};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc::UnboundedSender};

#[async_trait::async_trait]
pub trait MempoolFacade: Clone + Send + Sync + 'static {
    async fn handle_data_transaction_ingress_api(
        &self,
        tx_header: DataTransactionHeader,
    ) -> Result<(), TxIngressError>;
    async fn handle_data_transaction_ingress_gossip(
        &self,
        tx_header: DataTransactionHeader,
    ) -> Result<(), TxIngressError>;
    async fn handle_commitment_transaction_ingress_api(
        &self,
        tx_header: CommitmentTransaction,
    ) -> Result<(), TxIngressError>;
    async fn handle_commitment_transaction_ingress_gossip(
        &self,
        tx_header: CommitmentTransaction,
    ) -> Result<(), TxIngressError>;
    async fn handle_chunk_ingress(&self, chunk: UnpackedChunk) -> Result<(), ChunkIngressError>;
    async fn is_known_data_transaction(&self, tx_id: H256) -> Result<TxKnownStatus, TxReadError>;
    async fn is_known_commitment_transaction(
        &self,
        tx_id: H256,
    ) -> Result<TxKnownStatus, TxReadError>;

    async fn handle_ingest_ingress_proof(
        &self,
        ingress_proof: IngressProof,
    ) -> Result<(), IngressProofError>;
    async fn get_block_header(
        &self,
        block_hash: H256,
        include_chunk: bool,
    ) -> Result<Option<IrysBlockHeader>, TxReadError>;
    async fn migrate_block(
        &self,
        irys_block_header: Arc<IrysBlockHeader>,
    ) -> Result<usize, TxIngressError>;

    async fn remove_from_blacklist(&self, tx_ids: Vec<H256>) -> eyre::Result<()>;

    async fn get_stake_and_pledge_whitelist(&self) -> HashSet<IrysAddress>;

    async fn update_stake_and_pledge_whitelist(
        &self,
        new_whitelist: HashSet<IrysAddress>,
    ) -> eyre::Result<()>;

    async fn get_internal_read_guard(&self) -> MempoolReadGuard;
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
    async fn handle_data_transaction_ingress_api(
        &self,
        tx_header: DataTransactionHeader,
    ) -> Result<(), TxIngressError> {
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        let tx_id = tx_header.id;
        self.service
            .send(MempoolServiceMessage::IngestDataTxFromApi(
                tx_header, oneshot_tx,
            ))
            .map_err(|_| {
                TxIngressError::Other(format!("Error sending TxIngressMessage for tx {}", tx_id))
            })?;

        oneshot_rx.await.expect("to process TxIngressMessage")
    }

    async fn handle_data_transaction_ingress_gossip(
        &self,
        tx_header: DataTransactionHeader,
    ) -> Result<(), TxIngressError> {
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        let tx_id = tx_header.id;
        self.service
            .send(MempoolServiceMessage::IngestDataTxFromGossip(
                tx_header, oneshot_tx,
            ))
            .map_err(|_| {
                TxIngressError::Other(format!("Error sending TxIngressMessage for tx {}", tx_id))
            })?;

        oneshot_rx.await.expect("to process TxIngressMessage")
    }

    async fn handle_commitment_transaction_ingress_api(
        &self,
        commitment_tx: CommitmentTransaction,
    ) -> Result<(), TxIngressError> {
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        let tx_id = commitment_tx.id();
        self.service
            .send(MempoolServiceMessage::IngestCommitmentTxFromApi(
                commitment_tx,
                oneshot_tx,
            ))
            .map_err(|_| {
                TxIngressError::Other(format!(
                    "Error sending CommitmentTxIngressMessage for tx {}",
                    tx_id
                ))
            })?;

        oneshot_rx
            .await
            .expect("to process CommitmentTxIngressMessage")
    }

    async fn handle_commitment_transaction_ingress_gossip(
        &self,
        commitment_tx: CommitmentTransaction,
    ) -> Result<(), TxIngressError> {
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        let tx_id = commitment_tx.id();
        self.service
            .send(MempoolServiceMessage::IngestCommitmentTxFromGossip(
                commitment_tx,
                oneshot_tx,
            ))
            .map_err(|_| {
                TxIngressError::Other(format!(
                    "Error sending CommitmentTxIngressMessage for tx {}",
                    tx_id
                ))
            })?;

        oneshot_rx
            .await
            .expect("to process CommitmentTxIngressMessage")
    }

    async fn handle_chunk_ingress(&self, chunk: UnpackedChunk) -> Result<(), ChunkIngressError> {
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        let chunk_data_root = chunk.data_root;
        let chunk_tx_offset = chunk.tx_offset;
        self.service
            .send(MempoolServiceMessage::IngestChunk(chunk, oneshot_tx))
            .map_err(|_| {
                CriticalChunkIngressError::Other(format!(
                    "Error sending ChunkIngressMessage for chunk data_root {:?} tx_offset {}",
                    chunk_data_root, chunk_tx_offset
                ))
            })?;

        oneshot_rx.await.expect("to process ChunkIngressMessage")
    }

    async fn is_known_data_transaction(&self, tx_id: H256) -> Result<TxKnownStatus, TxReadError> {
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        self.service
            .send(MempoolServiceMessage::DataTxExists(tx_id, oneshot_tx))
            .map_err(|_| {
                TxReadError::Other(format!("Error sending TxExistenceQuery for tx {}", tx_id))
            })?;

        oneshot_rx.await.expect("to process TxExistenceQuery")
    }

    async fn is_known_commitment_transaction(
        &self,
        tx_id: H256,
    ) -> Result<TxKnownStatus, TxReadError> {
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        self.service
            .send(MempoolServiceMessage::CommitmentTxExists(tx_id, oneshot_tx))
            .map_err(|_| {
                TxReadError::Other(format!("Error sending TxExistenceQuery for tx {}", tx_id))
            })?;

        oneshot_rx.await.expect("to process TxExistenceQuery")
    }

    async fn handle_ingest_ingress_proof(
        &self,
        ingress_proof: IngressProof,
    ) -> Result<(), IngressProofError> {
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        let data_root = ingress_proof.data_root;
        self.service
            .send(MempoolServiceMessage::IngestIngressProof(
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
            .map_err(|_| {
                TxReadError::Other(format!(
                    "Error sending GetBlockHeader message for block {}",
                    block_hash
                ))
            })?;

        rx.await.map_err(|_| {
            TxReadError::Other(format!(
                "GetBlockHeader response error for block {}",
                block_hash
            ))
        })
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

    async fn remove_from_blacklist(&self, tx_ids: Vec<H256>) -> eyre::Result<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.service
            .send(MempoolServiceMessage::RemoveFromBlacklist(tx_ids, tx))
            .map_err(|send_error| eyre!("{send_error:?}"))?;

        rx.await.map_err(|recv_error| eyre!("{recv_error:?}"))
    }

    async fn get_stake_and_pledge_whitelist(&self) -> HashSet<IrysAddress> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.service
            .send(MempoolServiceMessage::CloneStakeAndPledgeWhitelist(tx))
            .expect("to send GetStakeAndPledgeWhitelist message");

        rx.await
            .expect("to process GetStakeAndPledgeWhitelist message")
    }

    async fn update_stake_and_pledge_whitelist(
        &self,
        new_whitelist: HashSet<IrysAddress>,
    ) -> eyre::Result<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.service
            .send(MempoolServiceMessage::UpdateStakeAndPledgeWhitelist(
                new_whitelist,
                tx,
            ))
            .map_err(|send_error| eyre!("{send_error:?}"))?;

        rx.await.map_err(|recv_error| eyre!("{recv_error:?}"))
    }

    async fn get_internal_read_guard(&self) -> MempoolReadGuard {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.service
            .send(MempoolServiceMessage::GetReadGuard(tx))
            .expect("to send GetInternalReadGuard message");

        rx.await.expect("to process GetInternalReadGuard message")
    }
}
