#![allow(
    clippy::module_name_repetitions,
    reason = "I have no idea how to name this module to satisfy this lint"
)]
use crate::types::{GossipError, GossipResult, RequestedData};
use actix::{Actor, Context, Handler};
use core::time::Duration;
use irys_actors::peer_list_service::{PeerListFacade, ScoreDecreaseReason, ScoreIncreaseReason};
use irys_api_client::ApiClient;
use irys_types::{Address, GossipData, GossipRequest, PeerListItem, RethPeerInfo};
use reqwest::Response;
use serde::Serialize;

#[derive(Debug, Clone)]
pub struct GossipClient {
    pub mining_address: Address,
    client: reqwest::Client,
    timeout: Duration,
}

impl GossipClient {
    #[must_use]
    pub fn new(timeout: Duration, mining_address: Address) -> Self {
        Self {
            mining_address,
            client: reqwest::Client::new(),
            timeout,
        }
    }

    /// Send data to a peer
    ///
    /// # Errors
    ///
    /// If the peer is offline or the request fails, an error is returned.
    pub async fn send_data(&self, peer: &PeerListItem, data: &GossipData) -> GossipResult<()> {
        tracing::trace!("CHECKING IF PEER ONLINE: {:?}", peer);
        Self::check_if_peer_online(peer)?;
        match data {
            GossipData::Chunk(unpacked_chunk) => {
                tracing::trace!(
                    "GOSSIP POSTING to {:?} DATA to: {:?}",
                    format!("http://{}/gossip/chunk", peer.address.gossip),
                    unpacked_chunk
                );
                self.send_data_internal(
                    format!("http://{}/gossip/chunk", peer.address.gossip),
                    unpacked_chunk,
                )
                .await?;
            }
            GossipData::Transaction(irys_transaction_header) => {
                tracing::trace!(
                    "GOSSIP POSTING to {:?} DATA to: {:?}",
                    format!("http://{}/gossip/transaction", peer.address.gossip),
                    irys_transaction_header
                );
                self.send_data_internal(
                    format!("http://{}/gossip/transaction", peer.address.gossip),
                    irys_transaction_header,
                )
                .await?;
            }
            GossipData::Block(irys_block_header) => {
                tracing::trace!(
                    "GOSSIP POSTING to {:?} DATA to: {:?}",
                    format!("http://{}/gossip/block", peer.address.gossip),
                    irys_block_header
                );
                self.send_data_internal(
                    format!("http://{}/gossip/block", peer.address.gossip),
                    &irys_block_header,
                )
                .await?;
            }
        };

        Ok(())
    }

    fn check_if_peer_online(peer: &PeerListItem) -> GossipResult<()> {
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
            .timeout(self.timeout)
            .json(&req)
            .send()
            .await
            .map_err(|error| GossipError::Network(error.to_string()))
    }

    /// Send data to a peer and update their score based on the result
    ///
    /// # Errors
    ///
    /// If the peer is offline or the request fails, an error is returned.
    pub async fn send_data_and_update_score<R, A>(
        &self,
        peer: (&Address, &PeerListItem),
        data: &GossipData,
        peer_list_service: &PeerListFacade<A, R>,
    ) -> GossipResult<()>
    where
        R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
        A: ApiClient + Clone + 'static + Unpin + Default,
    {
        let peer_miner_address = peer.0;
        let peer = peer.1;

        let res = self.send_data(peer, data).await;
        match res {
            Ok(()) => {
                // Successful send, increase score
                if let Err(e) = peer_list_service
                    .increase_peer_score(peer_miner_address, ScoreIncreaseReason::Online)
                    .await
                {
                    tracing::error!("Failed to increase peer score: {}", e);
                }
                Ok(())
            }
            Err(error) => {
                // Failed to send, decrease score
                if let Err(e) = peer_list_service
                    .decrease_peer_score(peer_miner_address, ScoreDecreaseReason::Offline)
                    .await
                {
                    tracing::error!("Failed to decrease peer score: {}", e);
                };
                Err(error)
            }
        }
    }

    fn create_request<T>(&self, data: T) -> GossipRequest<T> {
        GossipRequest {
            miner_address: self.mining_address,
            data,
        }
    }

    /// Request a specific data to be gossiped. Returns true if the peer has the data,
    /// and false if it doesn't.
    pub async fn get_data_request(
        &self,
        peer: &PeerListItem,
        requested_data: RequestedData,
    ) -> GossipResult<bool> {
        let url = format!("http://{}/gossip/get_data", peer.address.gossip);
        let get_data_request = self.create_request(requested_data);

        self.client
            .post(&url)
            .timeout(self.timeout)
            .json(&get_data_request)
            .send()
            .await
            .map_err(|error| GossipError::Network(error.to_string()))?
            .json()
            .await
            .map_err(|error| GossipError::Network(error.to_string()))
    }
}
