use irys_domain::PeerList;
use irys_types::chunk_provider::{PdChunkFetchFailure, PdChunkFetchSuccess, PdChunkFetcher};
use irys_types::PeerAddress;

use crate::gossip_client::GossipClient;

/// Bridges the irys-actors ↔ irys-p2p dependency gap.
/// Wraps GossipClient + PeerList and implements the PdChunkFetcher trait
/// defined in irys-types, so PdService can fetch chunks via HTTP + gossip
/// without directly depending on irys-p2p.
pub struct GossipPdChunkFetcher {
    gossip_client: GossipClient,
    peer_list: PeerList,
}

impl GossipPdChunkFetcher {
    pub fn new(gossip_client: GossipClient, peer_list: PeerList) -> Self {
        Self {
            gossip_client,
            peer_list,
        }
    }
}

#[async_trait::async_trait]
impl PdChunkFetcher for GossipPdChunkFetcher {
    async fn fetch_chunk(
        &self,
        peers: &[PeerAddress],
        ledger: u32,
        offset: u64,
    ) -> Result<PdChunkFetchSuccess, PdChunkFetchFailure> {
        match self
            .gossip_client
            .pull_pd_chunk_from_peers(peers, ledger, offset, &self.peer_list)
            .await
        {
            Ok((chunk, serving_peer)) => Ok(PdChunkFetchSuccess {
                chunk,
                serving_peer,
            }),
            Err(e) => Err(PdChunkFetchFailure {
                message: e.to_string(),
                failed_peers: peers.iter().map(|p| p.api).collect(),
            }),
        }
    }
}
