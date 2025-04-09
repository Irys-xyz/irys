use core::net::SocketAddr;
use irys_database::reth_db::Database as _;
use irys_database::tables::PeerListItems;
use irys_database::{insert_peer_list_item, walk_all};
use irys_types::{Address, DatabaseProvider, PeerListItem};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;
use tokio::time::Duration;

pub const FLUSH_INTERVAL: Duration = Duration::from_secs(1);
const UPDATE_CHANNEL_SIZE: usize = 100;

#[derive(Debug, Clone)]
pub struct PeerListProvider {
    db: DatabaseProvider,
    cache: Arc<RwLock<HashMap<Address, PeerListItem>>>,
    update_sender: mpsc::Sender<(Address, PeerListItem)>,
}

impl PeerListProvider {
    #[must_use]
    pub fn new(db: DatabaseProvider) -> (Self, mpsc::Receiver<(Address, PeerListItem)>) {
        let (update_sender, update_receiver) = mpsc::channel(UPDATE_CHANNEL_SIZE);
        let cache = Arc::new(RwLock::new(HashMap::new()));

        // Initialize cache from database
        if let Ok(read_tx) = db.tx() {
            if let Ok(peer_list_items) = walk_all::<PeerListItems, _>(&read_tx) {
                if let Ok(mut cache_write) = cache.write() {
                    for (addr, item) in peer_list_items {
                        cache_write.insert(addr, item.0);
                    }
                }
            }
        }

        (
            Self {
                db,
                cache,
                update_sender,
            },
            update_receiver,
        )
    }

    /// Flushes pending updates to the database
    ///
    /// # Errors
    ///
    /// This function will return an error if the database operation fails
    pub fn flush_updates(
        &self,
        pending_updates: &HashMap<Address, PeerListItem>,
    ) -> eyre::Result<()> {
        if pending_updates.is_empty() {
            return Ok(());
        }

        self.db
            .update(|tx| {
                for (addr, peer) in pending_updates {
                    insert_peer_list_item(tx, addr, peer)?;
                }
                Ok(())
            })
            .map_err(|e| eyre::eyre!("Failed to flush peer updates to database: {}", e))?
    }

    /// Returns a list of all known peers.
    ///
    /// # Errors
    ///
    /// This function will return an error if the cache cannot be accessed.
    pub fn all_known_peers(&self) -> eyre::Result<Vec<PeerListItem>> {
        let cache_read = self
            .cache
            .read()
            .map_err(|e| eyre::eyre!("Failed to read cache: {}", e))?;

        Ok(cache_read.values().cloned().collect())
    }

    /// As of March 2025, this function checks if a peer is allowed using its IP address.
    /// This is a temporary solution until we have a more robust way of identifying peers.
    ///
    /// # Errors
    ///
    /// This function will return an error if the cache cannot be accessed.
    pub fn is_peer_allowed(&self, peer: &SocketAddr) -> eyre::Result<Option<PeerListItem>> {
        let cache_read = self
            .cache
            .read()
            .map_err(|e| eyre::eyre!("Failed to read cache: {}", e))?;

        let peer_ip = peer.ip();
        Ok(cache_read
            .values()
            .find(|peer_list_item| peer_list_item.address.gossip.ip() == peer_ip)
            .cloned())
    }

    /// Updates a peer's information in the cache and queues it for database update
    ///
    /// # Errors
    ///
    /// This function will return an error if the cache cannot be accessed.
    pub fn update_peer(&self, peer: &PeerListItem) -> eyre::Result<()> {
        let cache_read = self
            .cache
            .read()
            .map_err(|e| eyre::eyre!("Failed to read cache: {}", e))?;

        // Find the mining address associated with this peer
        let mut mining_address = None;
        for (addr, item) in cache_read.iter() {
            if item.address == peer.address {
                mining_address = Some(*addr);
                break;
            }
        }

        drop(cache_read); // Release the read lock

        if let Some(addr) = mining_address {
            let mut cache_write = self
                .cache
                .write()
                .map_err(|e| eyre::eyre!("Failed to write to cache: {}", e))?;

            cache_write.insert(addr, peer.clone());

            // Queue update for database
            if let Err(e) = self.update_sender.try_send((addr, peer.clone())) {
                tracing::warn!("Failed to queue peer update: {}", e);
            }

            Ok(())
        } else {
            Err(eyre::eyre!("Peer not found in cache"))
        }
    }

    /// Returns peer info for a given peer address.
    ///
    /// # Errors
    ///
    /// This function will return an error if the cache cannot be accessed.
    pub fn get_peer_info(&self, peer: &SocketAddr) -> eyre::Result<Option<PeerListItem>> {
        let cache_read = self
            .cache
            .read()
            .map_err(|e| eyre::eyre!("Failed to read cache: {}", e))?;

        let peer_ip = peer.ip();
        Ok(cache_read
            .values()
            .find(|peer_list_item| peer_list_item.address.gossip.ip() == peer_ip)
            .cloned())
    }

    /// Adds a peer to the peer list.
    ///
    /// # Errors
    ///
    /// This function will return an error if the cache cannot be accessed.
    pub fn add_peer(&self, mining_address: &Address, peer: &PeerListItem) -> eyre::Result<()> {
        let mut cache_write = self
            .cache
            .write()
            .map_err(|e| eyre::eyre!("Failed to write to cache: {}", e))?;

        cache_write.insert(*mining_address, peer.clone());

        // Queue update for database
        if let Err(e) = self.update_sender.try_send((*mining_address, peer.clone())) {
            tracing::warn!("Failed to queue peer update: {}", e);
        }

        Ok(())
    }
}
