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

    async fn get_stake_and_pledge_whitelist(&self) -> eyre::Result<HashSet<IrysAddress>>;

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
    ///
    /// # Errors
    ///
    /// Fails when the mempool service has stopped, for example while the node
    /// shuts down and gossip workers are still answering peers.
    async fn get_internal_read_guard(&self) -> eyre::Result<MempoolReadGuard>;
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

    async fn get_stake_and_pledge_whitelist(&self) -> eyre::Result<HashSet<IrysAddress>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.service
            .send_traced(MempoolServiceMessage::CloneStakeAndPledgeWhitelist(tx))
            .map_err(|send_error| eyre!("{send_error:?}"))?;

        rx.await.map_err(|recv_error| eyre!("{recv_error:?}"))
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

    async fn get_internal_read_guard(&self) -> eyre::Result<MempoolReadGuard> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.service
            .send_traced(MempoolServiceMessage::GetReadGuard(tx))
            .map_err(|send_error| eyre!("{send_error:?}"))?;

        // Channel closure here means the mempool service has stopped; this lightweight
        // message doesn't go through the semaphore so timeout is not expected.
        rx.await.map_err(|recv_error| eyre!("{recv_error:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use tokio::sync::mpsc::unbounded_channel;

    #[derive(Clone, Copy, Debug)]
    enum Query {
        StakeAndPledgeWhitelist,
        InternalReadGuard,
    }

    #[derive(Clone, Copy, Debug)]
    enum Stopped {
        /// The service loop has exited and dropped its receiver.
        BeforeSend,
        /// The service took the message but dropped it without replying.
        BeforeReply,
    }

    async fn run(facade: &MempoolServiceFacadeImpl, query: Query) -> eyre::Result<()> {
        match query {
            Query::StakeAndPledgeWhitelist => {
                facade.get_stake_and_pledge_whitelist().await.map(drop)
            }
            Query::InternalReadGuard => facade.get_internal_read_guard().await.map(drop),
        }
    }

    /// Gossip handlers call these while the node may be shutting down, so a
    /// stopped mempool must surface as an error rather than a panic.
    #[rstest]
    #[tokio::test]
    async fn query_fails_without_panicking_once_the_mempool_has_stopped(
        #[values(Query::StakeAndPledgeWhitelist, Query::InternalReadGuard)] query: Query,
        #[values(Stopped::BeforeSend, Stopped::BeforeReply)] stopped: Stopped,
    ) {
        let (sender, mut receiver) = unbounded_channel::<Traced<MempoolServiceMessage>>();
        let facade = MempoolServiceFacadeImpl::new(sender);
        match stopped {
            Stopped::BeforeSend => drop(receiver),
            Stopped::BeforeReply => {
                tokio::spawn(async move { while receiver.recv().await.is_some() {} });
            }
        }

        assert!(run(&facade, query).await.is_err());
    }
}
