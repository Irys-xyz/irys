use crate::types::{GossipError, GossipResult};
use irys_database::tables::CompactPeerListItem;
use irys_types::GossipData;
use std::time::Duration;

#[derive(Debug)]
pub struct GossipClient {
    client: reqwest::Client,
    timeout: Duration,
}

impl GossipClient {
    pub fn new(timeout: Duration) -> Self {
        Self {
            client: reqwest::Client::new(),
            timeout,
        }
    }

    /// Send gossip data to a peer
    pub async fn send_data(
        &self,
        peer: &CompactPeerListItem,
        data: &GossipData,
    ) -> GossipResult<()> {
        if !peer.is_online {
            return Err(GossipError::InvalidPeer("Peer is offline".into()));
        }

        let url = format!("http://{}/gossip/data", peer.address);

        self.client
            .post(&url)
            .timeout(self.timeout)
            .json(data)
            .send()
            .await
            .map_err(|e| GossipError::Network(e.to_string()))?;

        Ok(())
    }

    /// Check the health of a peer
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
            .map_err(|e| GossipError::Network(e.to_string()))?;

        if !response.status().is_success() {
            return Err(GossipError::Network(format!(
                "Health check failed with status: {}",
                response.status()
            )));
        }

        response
            .json()
            .await
            .map_err(|e| GossipError::Network(e.to_string()))
    }
}
