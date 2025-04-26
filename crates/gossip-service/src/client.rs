#![allow(
    clippy::module_name_repetitions,
    reason = "I have no idea how to name this module to satisfy this lint"
)]
use crate::types::{GossipError, GossipResult};
use actix::Addr;
use core::time::Duration;
use irys_actors::peer_list_service::{
    DecreasePeerScore, IncreasePeerScore, PeerListService, ScoreDecreaseReason, ScoreIncreaseReason,
};
use irys_types::{GossipData, PeerListItem};
use reqwest::Response;
use serde::Serialize;

#[derive(Debug)]
pub struct GossipClient {
    client: reqwest::Client,
    timeout: Duration,
}

impl GossipClient {
    #[must_use]
    pub fn new(timeout: Duration) -> Self {
        Self {
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
        Self::check_if_peer_online(peer)?;
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
            GossipData::Block(irys_block_header) => {
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
        self.client
            .post(&url)
            .timeout(self.timeout)
            .json(data)
            .send()
            .await
            .map_err(|error| GossipError::Network(error.to_string()))
    }

    /// Send data to a peer and update their score based on the result
    ///
    /// # Errors
    ///
    /// If the peer is offline or the request fails, an error is returned.
    pub async fn send_data_and_update_score(
        &self,
        peer: &PeerListItem,
        data: &GossipData,
        peer_list_service: &Addr<PeerListService>,
    ) -> GossipResult<()> {
        let res = self.send_data(peer, data).await;
        match res {
            Ok(()) => {
                // Successful send, increase score
                if let Err(e) = peer_list_service
                    .send(IncreasePeerScore {
                        peer: peer.address.gossip,
                        reason: ScoreIncreaseReason::Online,
                    })
                    .await
                {
                    tracing::error!("Failed to increase peer score: {}", e);
                }
                Ok(())
            }
            Err(error) => {
                // Failed to send, decrease score
                if let Err(e) = peer_list_service
                    .send(DecreasePeerScore {
                        peer: peer.address.gossip,
                        reason: ScoreDecreaseReason::Offline,
                    })
                    .await
                {
                    tracing::error!("Failed to decrease peer score: {}", e);
                };
                Err(error)
            }
        }
    }
}
