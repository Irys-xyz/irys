use crate::MempoolReadGuard;
use crate::mempool_service::{MempoolServiceMessage, TxIngressError, TxReadError};
use crate::services::ServiceSenders;
use eyre::eyre;
use irys_types::{CommitmentTransaction, DataTransactionHeader, H256};
use irys_types::{IrysAddress, SendTraced as _, Traced, TxKnownStatus};
use std::collections::HashSet;
use tokio::sync::mpsc::UnboundedSender;

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
    async fn is_known_data_transaction(&self, tx_id: H256) -> Result<TxKnownStatus, TxReadError>;
    async fn is_known_commitment_transaction(
        &self,
        tx_id: H256,
    ) -> Result<TxKnownStatus, TxReadError>;

    async fn remove_from_blacklist(&self, tx_ids: Vec<H256>) -> eyre::Result<()>;

    async fn get_stake_and_pledge_whitelist(&self) -> HashSet<IrysAddress>;

    async fn update_stake_and_pledge_whitelist(
        &self,
        new_whitelist: HashSet<IrysAddress>,
    ) -> eyre::Result<()>;

    /// Returns a read guard to the internal mempool state for multi-query flows.
    ///
    /// This method mirrors the facade pattern used by `handle_ingest_ingress_proof` and
    /// `MempoolServiceMessage::GetReadGuard`. It is intended for internal/multi-query
    /// flows where multiple sequential reads are required.
    ///
    /// **Important**: Callers must not hold the guard across long-running work.
    /// Use only for short synchronous reads to avoid blocking other operations.
    async fn get_internal_read_guard(&self) -> MempoolReadGuard;
}

#[derive(Clone, Debug)]
pub struct MempoolServiceFacadeImpl {
    service: UnboundedSender<Traced<MempoolServiceMessage>>,
}

impl MempoolServiceFacadeImpl {
    pub fn new(service: UnboundedSender<Traced<MempoolServiceMessage>>) -> Self {
        Self { service }
    }
}

impl From<&ServiceSenders> for MempoolServiceFacadeImpl {
    fn from(value: &ServiceSenders) -> Self {
        Self {
            service: value.mempool.clone(),
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
            .send_traced(MempoolServiceMessage::IngestDataTxFromApi(
                tx_header, oneshot_tx,
            ))
            .map_err(|_| {
                TxIngressError::Other(format!("Error sending TxIngressMessage for tx {}", tx_id))
            })?;

        oneshot_rx.await.map_err(|_| {
            TxIngressError::Other(
                "Mempool service dropped the request (channel closed)".to_string(),
            )
        })?
    }

    async fn handle_data_transaction_ingress_gossip(
        &self,
        tx_header: DataTransactionHeader,
    ) -> Result<(), TxIngressError> {
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        let tx_id = tx_header.id;
        self.service
            .send_traced(MempoolServiceMessage::IngestDataTxFromGossip(
                tx_header, oneshot_tx,
            ))
            .map_err(|_| {
                TxIngressError::Other(format!("Error sending TxIngressMessage for tx {}", tx_id))
            })?;

        oneshot_rx.await.map_err(|_| {
            TxIngressError::Other(
                "Mempool service dropped the request (channel closed)".to_string(),
            )
        })?
    }

    async fn handle_commitment_transaction_ingress_api(
        &self,
        commitment_tx: CommitmentTransaction,
    ) -> Result<(), TxIngressError> {
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        let tx_id = commitment_tx.id();
        self.service
            .send_traced(MempoolServiceMessage::IngestCommitmentTxFromApi(
                commitment_tx,
                oneshot_tx,
            ))
            .map_err(|_| {
                TxIngressError::Other(format!(
                    "Error sending CommitmentTxIngressMessage for tx {}",
                    tx_id
                ))
            })?;

        oneshot_rx.await.map_err(|_| {
            TxIngressError::Other(
                "Mempool service dropped the request (channel closed)".to_string(),
            )
        })?
    }

    async fn handle_commitment_transaction_ingress_gossip(
        &self,
        commitment_tx: CommitmentTransaction,
    ) -> Result<(), TxIngressError> {
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        let tx_id = commitment_tx.id();
        self.service
            .send_traced(MempoolServiceMessage::IngestCommitmentTxFromGossip(
                commitment_tx,
                oneshot_tx,
            ))
            .map_err(|_| {
                TxIngressError::Other(format!(
                    "Error sending CommitmentTxIngressMessage for tx {}",
                    tx_id
                ))
            })?;

        oneshot_rx.await.map_err(|_| {
            TxIngressError::Other(
                "Mempool service dropped the request (channel closed)".to_string(),
            )
        })?
    }

    async fn is_known_data_transaction(&self, tx_id: H256) -> Result<TxKnownStatus, TxReadError> {
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        self.service
            .send_traced(MempoolServiceMessage::DataTxExists(tx_id, oneshot_tx))
            .map_err(|_| {
                TxReadError::Other(format!("Error sending TxExistenceQuery for tx {}", tx_id))
            })?;

        oneshot_rx.await.map_err(|_| {
            TxReadError::Other("Mempool service dropped the request (channel closed)".to_string())
        })?
    }

    async fn is_known_commitment_transaction(
        &self,
        tx_id: H256,
    ) -> Result<TxKnownStatus, TxReadError> {
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        self.service
            .send_traced(MempoolServiceMessage::CommitmentTxExists(tx_id, oneshot_tx))
            .map_err(|_| {
                TxReadError::Other(format!("Error sending TxExistenceQuery for tx {}", tx_id))
            })?;

        oneshot_rx.await.map_err(|_| {
            TxReadError::Other("Mempool service dropped the request (channel closed)".to_string())
        })?
    }

    async fn remove_from_blacklist(&self, tx_ids: Vec<H256>) -> eyre::Result<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.service
            .send_traced(MempoolServiceMessage::RemoveFromBlacklist(tx_ids, tx))
            .map_err(|send_error| eyre!("{send_error:?}"))?;

        rx.await.map_err(|recv_error| eyre!("{recv_error:?}"))
    }

    async fn get_stake_and_pledge_whitelist(&self) -> HashSet<IrysAddress> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.service
            .send_traced(MempoolServiceMessage::CloneStakeAndPledgeWhitelist(tx))
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
            .send_traced(MempoolServiceMessage::UpdateStakeAndPledgeWhitelist(
                new_whitelist,
                tx,
            ))
            .map_err(|send_error| eyre!("{send_error:?}"))?;

        rx.await.map_err(|recv_error| eyre!("{recv_error:?}"))
    }

    async fn get_internal_read_guard(&self) -> MempoolReadGuard {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.service
            .send_traced(MempoolServiceMessage::GetReadGuard(tx))
            .expect("to send GetInternalReadGuard message");

        // Channel closure here indicates a fatal service failure; this lightweight message
        // doesn't go through the semaphore so timeout is not expected.
        rx.await.expect("to process GetInternalReadGuard message")
    }
}
