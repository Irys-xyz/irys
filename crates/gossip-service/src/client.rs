#![allow(
    clippy::module_name_repetitions,
    reason = "I have no idea how to name this module to satisfy this lint"
)]
use crate::types::{GossipError, GossipResult};
use core::time::Duration;
use irys_database::tables::CompactPeerListItem;
use irys_types::GossipData;
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
    pub async fn send_data(
        &self,
        peer: &CompactPeerListItem,
        data: &GossipData,
    ) -> GossipResult<()> {
        Self::check_if_peer_online(peer)?;
        match data {
            GossipData::Chunk(unpacked_chunk) => {
                self.send_data_internal(
                    format!("http://{}/gossip/chunk", peer.address),
                    unpacked_chunk,
                )
                .await?;
            }
            GossipData::Transaction(irys_transaction_header) => {
                self.send_data_internal(
                    format!("http://{}/gossip/transaction", peer.address),
                    irys_transaction_header,
                )
                .await?;
            }
            GossipData::Block(irys_block_header) => {
                self.send_data_internal(
                    format!("http://{}/gossip/block", peer.address),
                    &irys_block_header,
                )
                .await?;
            }
        };

        Ok(())
    }

    fn check_if_peer_online(peer: &CompactPeerListItem) -> GossipResult<()> {
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

    /// Check the health of a peer
    ///
    /// # Errors
    ///
    /// If the health check fails or the response is not valid JSON, an error is returned.
    pub async fn check_health(
        &self,
        peer: &CompactPeerListItem,
    ) -> GossipResult<CompactPeerListItem> {
        let url = format!("http://{}/gossip/health", peer.address);

        let response = self
            .client
            .get(&url)
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|error| GossipError::Network(error.to_string()))?;

        if !response.status().is_success() {
            return Err(GossipError::Network(format!(
                "Health check failed with status: {}",
                response.status()
            )));
        }

        response
            .json()
            .await
            .map_err(|error| GossipError::Network(error.to_string()))
    }
}
