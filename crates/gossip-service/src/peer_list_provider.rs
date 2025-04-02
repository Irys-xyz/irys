use core::net::SocketAddr;
use irys_database::reth_db::Database as _;
use irys_database::tables::{CompactPeerListItem, PeerListItems};
use irys_database::{insert_peer_list_item, walk_all};
use irys_types::{Address, DatabaseProvider, PeerListItem};

#[derive(Debug, Clone)]
pub struct PeerListProvider {
    db: DatabaseProvider,
}

impl PeerListProvider {
    #[must_use]
    pub const fn new(db: DatabaseProvider) -> Self {
        Self { db }
    }

    /// Returns a list of all known peers.
    ///
    /// # Errors
    ///
    /// This function will return an error if the database operation fails.
    pub fn all_known_peers(&self) -> eyre::Result<Vec<CompactPeerListItem>> {
        // Attempt to create a read transaction
        let read_tx = self
            .db
            .tx()
            .map_err(|error| eyre::eyre!("Database error: {}", error))?;

        // Fetch peer list items
        let peer_list_items = walk_all::<PeerListItems, _>(&read_tx)
            .map_err(|error| eyre::eyre!("Read error: {}", error))?;

        // Extract IP addresses and Port (SocketAddr) into a Vec<String>
        let ips: Vec<CompactPeerListItem> = peer_list_items
            .iter()
            .map(|(_miner_addr, entry)| entry.clone())
            .collect();

        Ok(ips)
    }

    /// As of March 2025, this function checks if a peer is allowed using its IP address.
    /// This is a temporary solution until we have a more robust way of identifying peers.
    ///
    /// # Errors
    ///
    /// This function will return an error if the database operation fails.
    pub fn is_peer_allowed(&self, peer: &SocketAddr) -> eyre::Result<Option<CompactPeerListItem>> {
        let known_peers = self.all_known_peers()?;
        let peer_ip = peer.ip();
        Ok(known_peers
            .iter()
            .find(|peer_list_item| peer_list_item.address.gossip.ip() == peer_ip)
            .cloned()
        )
    }

    /// Returns peer info for a given peer address.
    ///
    /// # Errors
    ///
    /// This function will return an error if the database operation fails.
    pub fn get_peer_info(&self, peer: &SocketAddr) -> eyre::Result<Option<CompactPeerListItem>> {
        let known_peers = self.all_known_peers()?;
        Ok(known_peers
            .iter()
            .find(|peer_list_item| peer_list_item.address.gossip == *peer)
            .cloned())
    }

    /// Adds a peer to the peer list.
    ///
    /// # Errors
    ///
    /// This function will return an error if the database operation fails.
    pub fn add_peer(&self, mining_address: &Address, peer: &PeerListItem) -> eyre::Result<()> {
        self.db
            .update(|tx| insert_peer_list_item(tx, mining_address, peer))?
    }
}
