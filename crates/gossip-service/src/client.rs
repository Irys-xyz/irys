use std::time::Duration;

use crate::types::{GossipData, GossipError, GossipResult, PeerInfo};

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
    pub async fn send_data(&self, peer: &PeerInfo, data: &GossipData) -> GossipResult<()> {
        if !peer.is_online {
            return Err(GossipError::InvalidPeer("Peer is offline".into()));
        }

        let url = format!("http://{}:{}/gossip/data", peer.ip, peer.port);

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
    pub async fn check_health(&self, peer: &PeerInfo) -> GossipResult<PeerInfo> {
        let url = format!("http://{}:{}/gossip/health", peer.ip, peer.port);

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