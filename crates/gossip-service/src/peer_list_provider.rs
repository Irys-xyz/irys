use irys_database::reth_db::Database;
use irys_database::tables::{CompactPeerListItem, PeerListItems};
use irys_database::walk_all;
use irys_types::DatabaseProvider;
use std::net::SocketAddr;

#[derive(Debug, Clone)]
pub struct PeerListProvider {
    db: DatabaseProvider,
}

impl PeerListProvider {
    pub fn new(db: DatabaseProvider) -> Self {
        Self { db }
    }

    pub fn all_known_peers(&self) -> eyre::Result<Vec<CompactPeerListItem>> {
        // Attempt to create a read transaction
        let read_tx = self
            .db
            .tx()
            .map_err(|e| eyre::eyre!("Database error: {}", e))?;

        // Fetch peer list items
        let peer_list_items =
            walk_all::<PeerListItems, _>(&read_tx).map_err(|e| eyre::eyre!("Read error: {}", e))?;

        // Extract IP addresses and Port (SocketAddr) into a Vec<String>
        let ips: Vec<CompactPeerListItem> = peer_list_items
            .iter()
            .map(|(_miner_addr, entry)| entry.clone())
            .collect();

        Ok(ips)
    }

    pub fn is_peer_allowed(&self, peer: &SocketAddr) -> eyre::Result<bool> {
        let known_peers = self.all_known_peers()?;
        Ok(known_peers
            .iter()
            .any(|peer_list_item| peer_list_item.address == *peer))
    }

    pub fn get_peer_info(&self, peer: &SocketAddr) -> eyre::Result<Option<CompactPeerListItem>> {
        let known_peers = self.all_known_peers()?;
        Ok(known_peers
            .iter()
            .find(|peer_list_item| peer_list_item.address == *peer)
            .cloned())
    }
}
