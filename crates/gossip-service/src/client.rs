use crate::types::{GossipError, GossipResult};
use core::time::Duration;
use irys_database::tables::CompactPeerListItem;
use irys_types::{GossipData, IrysBlockHeader, IrysTransactionHeader, UnpackedChunk};

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

    pub async fn send_data(
        &self,
        peer: &CompactPeerListItem,
        data: &GossipData,
    ) -> GossipResult<()> {
        match data {
            GossipData::Chunk(unpacked_chunk) => self.send_chunk(peer, unpacked_chunk).await,
            GossipData::Transaction(irys_transaction_header) => {
                self.send_transaction(peer, irys_transaction_header).await
            }
            GossipData::Block(irys_block_header) => self.send_block(peer, irys_block_header).await,
        }
    }

    /// Send gossip data to a peer
    pub async fn send_transaction(
        &self,
        peer: &CompactPeerListItem,
        data: &IrysTransactionHeader,
    ) -> GossipResult<()> {
        if !peer.is_online {
            return Err(GossipError::InvalidPeer("Peer is offline".into()));
        }

        let url = format!("http://{}/gossip/transaction", peer.address);

        self.client
            .post(&url)
            .timeout(self.timeout)
            .json(data)
            .send()
            .await
            .map_err(|e| GossipError::Network(e.to_string()))?;

        Ok(())
    }

    pub async fn send_chunk(
        &self,
        peer: &CompactPeerListItem,
        data: &UnpackedChunk,
    ) -> GossipResult<()> {
        if !peer.is_online {
            return Err(GossipError::InvalidPeer("Peer is offline".into()));
        }

        let url = format!("http://{}/gossip/chunk", peer.address);

        self.client
            .post(&url)
            .timeout(self.timeout)
            .json(data)
            .send()
            .await
            .map_err(|e| GossipError::Network(e.to_string()))?;

        Ok(())
    }

    /// Send a block to a peer
    pub async fn send_block(
        &self,
        peer: &CompactPeerListItem,
        data: &IrysBlockHeader,
    ) -> GossipResult<()> {
        if !peer.is_online {
            return Err(GossipError::InvalidPeer("Peer is offline".into()));
        }

        let url = format!("http://{}/gossip/block", peer.address);

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
