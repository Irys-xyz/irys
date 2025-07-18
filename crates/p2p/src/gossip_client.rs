#![allow(
    clippy::module_name_repetitions,
    reason = "I have no idea how to name this module to satisfy this lint"
)]
use crate::peer_list::{PeerList, ScoreDecreaseReason, ScoreIncreaseReason};
use crate::types::{GossipDataRequest, GossipError, GossipResult};
use core::time::Duration;
use irys_types::{Address, GossipData, GossipRequest, PeerAddress, PeerListItem};
use reqwest::Client;
use reqwest::Response;
use serde::Serialize;
use std::sync::Arc;
use tracing::error;

#[derive(Debug, Clone, thiserror::Error)]
#[error("Gossip client error: {0}")]
pub struct GossipClientError(String);

#[derive(Debug, Clone)]
pub struct GossipClient {
    pub mining_address: Address,
    client: Client,
}

// TODO: Remove this when PeerList is no longer an actix service
impl Default for GossipClient {
    fn default() -> Self {
        panic!("GossipClient must be initialized with a timeout and mining address. Default is implemented only to satisfy actix trait bounds.");
    }
}

impl GossipClient {
    #[must_use]
    pub fn new(timeout: Duration, mining_address: Address) -> Self {
        Self {
            mining_address,
            client: Client::builder()
                .timeout(timeout)
                .build()
                .expect("Failed to create reqwest client"),
        }
    }

    pub fn internal_client(&self) -> &Client {
        &self.client
    }

    /// Send data to a peer and update their score based on the result
    ///
    /// # Errors
    ///
    /// If the peer is offline or the request fails, an error is returned.
    async fn send_data_and_update_score_internal<P>(
        &self,
        peer: (&Address, &PeerListItem),
        data: &GossipData,
        peer_list: &P,
    ) -> GossipResult<()>
    where
        P: PeerList,
    {
        let peer_miner_address = peer.0;
        let peer = peer.1;

        let res = self.send_data(peer, data).await;
        Self::handle_score(peer_list, &res, peer_miner_address).await;
        res
    }

    /// Request a specific data to be gossiped. Returns true if the peer has the data,
    /// and false if it doesn't.
    pub async fn make_get_data_request_and_update_the_score(
        &self,
        peer: &(Address, PeerListItem),
        requested_data: GossipDataRequest,
        peer_list: &impl PeerList,
    ) -> GossipResult<bool> {
        let url = format!("http://{}/gossip/get_data", peer.1.address.gossip);
        let get_data_request = self.create_request(requested_data);

        let res = self
            .client
            .post(&url)
            .json(&get_data_request)
            .send()
            .await
            .map_err(|error| GossipError::Network(error.to_string()))?
            .json()
            .await
            .map_err(|error| GossipError::Network(error.to_string()));
        Self::handle_score(peer_list, &res, &peer.0).await;
        res
    }

    pub async fn check_health(&self, peer: PeerAddress) -> Result<bool, GossipClientError> {
        let url = format!("http://{}/gossip/health", peer.gossip);

        let response = self
            .internal_client()
            .get(&url)
            .send()
            .await
            .map_err(|error| GossipClientError(error.to_string()))?;

        if !response.status().is_success() {
            return Err(GossipClientError(format!(
                "Health check failed with status: {}",
                response.status()
            )));
        }

        response
            .json()
            .await
            .map_err(|error| GossipClientError(error.to_string()))
    }

    /// Send data to a peer
    ///
    /// # Errors
    ///
    /// If the peer is offline or the request fails, an error is returned.
    async fn send_data(&self, peer: &PeerListItem, data: &GossipData) -> GossipResult<()> {
        Self::check_if_peer_is_online(peer)?;
        match data {
            GossipData::Chunk(unpacked_chunk) => {
                self.send_data_internal(
                    format!("http://{}/gossip/chunk", peer.address.gossip),
                    unpacked_chunk,
                )
                .await?;
            }
            GossipData::Transaction(irys_transaction_header) => {
                self.send_data_internal(
                    format!("http://{}/gossip/transaction", peer.address.gossip),
                    irys_transaction_header,
                )
                .await?;
            }
            GossipData::CommitmentTransaction(commitment_tx) => {
                self.send_data_internal(
                    format!("http://{}/gossip/commitment_tx", peer.address.gossip),
                    commitment_tx,
                )
                .await?;
            }
            GossipData::Block(irys_block_header) => {
                self.send_data_internal(
                    format!("http://{}/gossip/block", peer.address.gossip),
                    &irys_block_header,
                )
                .await?;
            }
            GossipData::ExecutionPayload(execution_payload) => {
                self.send_data_internal(
                    format!("http://{}/gossip/execution_payload", peer.address.gossip),
                    &execution_payload,
                )
                .await?;
            }
        };

        Ok(())
    }

    fn check_if_peer_is_online(peer: &PeerListItem) -> GossipResult<()> {
        if !peer.is_online {
            return Err(GossipError::InvalidPeer("Peer is offline".into()));
        }
        Ok(())
    }

    async fn send_data_internal<T: Serialize + ?Sized>(
        &self,
        url: String,
        data: &T,
    ) -> Result<Response, GossipError> {
        let req = self.create_request(data);
        self.client
            .post(&url)
            .json(&req)
            .send()
            .await
            .map_err(|error| GossipError::Network(error.to_string()))
    }

    async fn handle_score<P: PeerList, T>(
        peer_list: &P,
        result: &GossipResult<T>,
        peer_miner_address: &Address,
    ) {
        match &result {
            Ok(_) => {
                // Successful send, increase score
                if let Err(e) = peer_list
                    .increase_peer_score(peer_miner_address, ScoreIncreaseReason::Online)
                    .await
                {
                    error!("Failed to increase peer score: {}", e);
                }
            }
            Err(_) => {
                // Failed to send, decrease score
                if let Err(e) = peer_list
                    .decrease_peer_score(peer_miner_address, ScoreDecreaseReason::Offline)
                    .await
                {
                    error!("Failed to decrease peer score: {}", e);
                };
            }
        }
    }

    /// Sends data to a peer and update their score in a detached task
    pub fn send_data_and_update_the_score_detached<P: PeerList>(
        &self,
        peer: (&Address, &PeerListItem),
        data: Arc<GossipData>,
        peer_list: &P,
    ) {
        let client = self.clone();
        let peer_list = peer_list.clone();
        let peer_miner_address = *peer.0;
        let peer = peer.1.clone();

        tokio::spawn(async move {
            if let Err(e) = client
                .send_data_and_update_score_internal(
                    (&peer_miner_address, &peer),
                    &data,
                    &peer_list,
                )
                .await
            {
                error!("Error sending data to peer: {}", e);
            }
        });
    }

    fn create_request<T>(&self, data: T) -> GossipRequest<T> {
        GossipRequest {
            miner_address: self.mining_address,
            data,
        }
    }
}
