use alloy_core::primitives::B256;
use irys_database::reth_db::Database as _;
use irys_database::tables::PeerListItems;
use irys_database::walk_all;
use irys_primitives::Address;
use irys_types::{
    BlockHash, Config, DatabaseProvider, PeerAddress, PeerListItem, PeerNetworkError,
    PeerNetworkSender,
};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tracing::{debug, error, warn};

pub(crate) const MILLISECONDS_IN_SECOND: u64 = 1000;
pub(crate) const HANDSHAKE_COOLDOWN: u64 = MILLISECONDS_IN_SECOND * 5;

#[derive(Clone, Debug, Copy)]
pub enum ScoreDecreaseReason {
    BogusData,
    Offline,
}

#[derive(Clone, Debug, Copy)]
pub enum ScoreIncreaseReason {
    Online,
    ValidData,
}

#[derive(Debug, Clone)]
pub struct PeerListDataInner {
    gossip_addr_to_mining_addr_map: HashMap<IpAddr, Address>,
    api_addr_to_mining_addr_map: HashMap<SocketAddr, Address>,
    peer_list_cache: HashMap<Address, PeerListItem>,
    known_peers_cache: HashSet<PeerAddress>,
    trusted_peers_api_addresses: HashSet<SocketAddr>,
    peer_network_service_sender: PeerNetworkSender,
}

#[derive(Clone, Debug)]
pub struct PeerList(Arc<RwLock<PeerListDataInner>>);

impl PeerList {
    pub fn new(
        config: &Config,
        db: &DatabaseProvider,
        peer_service_sender: PeerNetworkSender,
    ) -> Result<Self, PeerNetworkError> {
        let inner = PeerListDataInner::new(config, db, peer_service_sender)?;
        Ok(Self(Arc::new(RwLock::new(inner))))
    }

    pub fn add_or_update_peer(&self, mining_addr: Address, peer: PeerListItem) {
        let mut inner = self.0.write().expect("PeerListDataInner lock poisoned");
        inner.add_or_update_peer(mining_addr, peer);
    }

    pub fn increase_peer_score(&self, mining_addr: &Address, reason: ScoreIncreaseReason) {
        let mut inner = self.0.write().expect("PeerListDataInner lock poisoned");
        inner.increase_score(mining_addr, reason);
    }

    pub fn decrease_peer_score(&self, mining_addr: &Address, reason: ScoreDecreaseReason) {
        let mut inner = self.0.write().expect("PeerListDataInner lock poisoned");
        inner.decrease_peer_score(mining_addr, reason);
    }

    pub fn all_known_peers(&self) -> Vec<PeerAddress> {
        self.read().known_peers_cache.iter().copied().collect()
    }

    pub fn all_know_peers_with_mining_address(&self) -> HashMap<Address, PeerListItem> {
        let guard = self.read();
        guard.peer_list_cache.clone()
    }

    pub fn contains_api_address(&self, api_address: &SocketAddr) -> bool {
        self.read()
            .api_addr_to_mining_addr_map
            .contains_key(api_address)
    }

    pub async fn wait_for_active_peers(&self) {
        loop {
            let active_peers_count = {
                let bindings = self.read();
                bindings
                    .peer_list_cache
                    .values()
                    .filter(|peer| peer.reputation_score.is_active() && peer.is_online)
                    .count()
            };

            if active_peers_count > 0 {
                return;
            }

            // Check for active peers every second
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    pub fn trusted_peers(&self) -> Vec<(Address, PeerListItem)> {
        let guard = self.read();

        let mut peers: Vec<(Address, PeerListItem)> = guard
            .peer_list_cache
            .iter()
            .map(|(key, value)| (*key, value.clone()))
            .collect();

        peers.retain(|(_miner_address, peer)| {
            guard
                .trusted_peers_api_addresses
                .contains(&peer.address.api)
        });

        peers.sort_by_key(|(_address, peer)| peer.reputation_score.get());
        peers.reverse();

        peers
    }

    pub fn trusted_peer_addresses(&self) -> HashSet<SocketAddr> {
        self.read().trusted_peers_api_addresses.clone()
    }

    pub fn top_active_peers(
        &self,
        limit: Option<usize>,
        exclude_peers: Option<HashSet<Address>>,
    ) -> Vec<(Address, PeerListItem)> {
        let guard = self.read();

        let mut peers: Vec<(Address, PeerListItem)> = guard
            .peer_list_cache
            .iter()
            .map(|(key, value)| (*key, value.clone()))
            .collect();

        peers.retain(|(miner_address, peer)| {
            let exclude = if let Some(exclude_peers) = &exclude_peers {
                exclude_peers.contains(miner_address)
            } else {
                false
            };
            !exclude && peer.reputation_score.is_active() && peer.is_online
        });

        peers.sort_by_key(|(_address, peer)| peer.reputation_score.get());
        peers.reverse();

        if let Some(truncate) = limit {
            peers.truncate(truncate);
        }

        peers
    }

    pub fn inactive_peers(&self) -> Vec<(Address, PeerListItem)> {
        self.read()
            .peer_list_cache
            .iter()
            .filter(|(_mining_addr, peer)| !peer.reputation_score.is_active())
            .map(|(mining_addr, peer)| {
                // Clone or copy the fields we need for the async operation
                let peer_item = peer.clone();
                let mining_addr = *mining_addr;
                (mining_addr, peer_item)
            })
            .collect()
    }

    pub fn peer_by_gossip_address(&self, address: SocketAddr) -> Option<PeerListItem> {
        let binding = self.read();
        let mining_address = binding
            .gossip_addr_to_mining_addr_map
            .get(&address.ip())
            .copied()?;
        binding.peer_list_cache.get(&mining_address).cloned()
    }

    pub fn peer_by_mining_address(&self, mining_address: &Address) -> Option<PeerListItem> {
        let binding = self.read();
        binding.peer_list_cache.get(mining_address).cloned()
    }

    pub fn is_a_trusted_peer(&self, miner_address: Address, source_ip: IpAddr) -> bool {
        let binding = self.read();

        let Some(peer) = binding.peer_list_cache.get(&miner_address) else {
            return false;
        };
        let peer_api_ip = peer.address.api.ip();
        let peer_gossip_ip = peer.address.gossip.ip();

        let ip_matches_cached_ip = source_ip == peer_gossip_ip;
        let ip_is_in_a_trusted_list = binding
            .trusted_peers_api_addresses
            .iter()
            .any(|socket_addr| socket_addr.ip() == peer_api_ip);

        ip_matches_cached_ip && ip_is_in_a_trusted_list
    }

    pub async fn request_block_from_the_network(
        &self,
        block_hash: BlockHash,
        use_trusted_peers_only: bool,
    ) -> Result<(), PeerNetworkError> {
        let sender = self
            .0
            .read()
            .expect("PeerListDataInner lock poisoned")
            .peer_network_service_sender
            .clone();
        sender
            .request_block_from_network(block_hash, use_trusted_peers_only)
            .await
    }

    pub async fn request_payload_from_the_network(
        &self,
        evm_payload_hash: B256,
        use_trusted_peers_only: bool,
    ) -> Result<(), PeerNetworkError> {
        let sender = self
            .0
            .read()
            .expect("PeerListDataInner lock poisoned")
            .peer_network_service_sender
            .clone();
        sender
            .request_payload_from_network(evm_payload_hash, use_trusted_peers_only)
            .await
    }

    pub fn read(&self) -> std::sync::RwLockReadGuard<'_, PeerListDataInner> {
        self.0.read().expect("PeerListDataInner lock poisoned")
    }

    pub fn peer_count(&self) -> usize {
        self.read().peer_list_cache.len()
    }
}

impl PeerListDataInner {
    pub fn new(
        config: &Config,
        db: &DatabaseProvider,
        peer_service_sender: PeerNetworkSender,
    ) -> Result<Self, PeerNetworkError> {
        let mut peer_list = Self {
            gossip_addr_to_mining_addr_map: HashMap::new(),
            api_addr_to_mining_addr_map: HashMap::new(),
            peer_list_cache: HashMap::new(),
            known_peers_cache: HashSet::new(),
            trusted_peers_api_addresses: config
                .node_config
                .trusted_peers
                .iter()
                .map(|p| p.api)
                .collect(),
            peer_network_service_sender: peer_service_sender,
        };

        let read_tx = db.tx().map_err(PeerNetworkError::from)?;

        let peer_list_items =
            walk_all::<PeerListItems, _>(&read_tx).map_err(PeerNetworkError::from)?;

        for (mining_addr, entry) in peer_list_items {
            let address = entry.address;
            peer_list
                .gossip_addr_to_mining_addr_map
                .insert(entry.address.gossip.ip(), mining_addr);
            peer_list.peer_list_cache.insert(mining_addr, entry.0);
            peer_list.known_peers_cache.insert(address);
            peer_list
                .api_addr_to_mining_addr_map
                .insert(address.api, mining_addr);
        }

        Ok(peer_list)
    }

    pub fn add_or_update_peer(&mut self, mining_addr: Address, peer: PeerListItem) {
        let is_updated = self.add_or_update_peer_internal(mining_addr, peer.clone());

        if is_updated {
            debug!(
                "Sending PeerUpdated message to the service for peer {:?}",
                mining_addr
            );
            // Notify the peer list service that a peer was updated
            if let Err(e) = self
                .peer_network_service_sender
                .announce_yourself_to_peer(peer)
            {
                error!("Failed to send peer updated message: {:?}", e);
            }
        }
    }

    pub fn increase_score(&mut self, mining_addr: &Address, reason: ScoreIncreaseReason) {
        if let Some(peer) = self.peer_list_cache.get_mut(mining_addr) {
            match reason {
                ScoreIncreaseReason::Online => {
                    peer.reputation_score.increase();
                }
                ScoreIncreaseReason::ValidData => {
                    peer.reputation_score.increase();
                }
            }
        }
    }

    pub fn decrease_peer_score(&mut self, mining_addr: &Address, reason: ScoreDecreaseReason) {
        if let Some(peer_item) = self.peer_list_cache.get_mut(mining_addr) {
            match reason {
                ScoreDecreaseReason::BogusData => {
                    peer_item.reputation_score.decrease_bogus_data();
                }
                ScoreDecreaseReason::Offline => {
                    peer_item.reputation_score.decrease_offline();
                }
            }

            // Don't propagate inactive peers
            if !peer_item.reputation_score.is_active() {
                self.known_peers_cache.remove(&peer_item.address);
            }
        }
    }

    /// Add a peer to the peer list. Returns true if the peer was added, false if it already exists.
    fn add_or_update_peer_internal(&mut self, mining_addr: Address, peer: PeerListItem) -> bool {
        let gossip_addr = peer.address.gossip;
        let peer_address = peer.address;

        if let std::collections::hash_map::Entry::Vacant(peer_cache) =
            self.peer_list_cache.entry(mining_addr)
        {
            debug!("Adding peer {:?} to the peer list", mining_addr);
            peer_cache.insert(peer);
            self.gossip_addr_to_mining_addr_map
                .insert(gossip_addr.ip(), mining_addr);
            self.api_addr_to_mining_addr_map
                .insert(peer_address.api, mining_addr);
            self.known_peers_cache.insert(peer_address);
            debug!(
                "Peer {:?} added to the peer list with address {:?}",
                mining_addr, peer_address
            );
            true
        } else {
            debug!(
                "Peer {:?} already exists in the peer list, checking if the address needs updating",
                mining_addr
            );
            if let Some(existing_peer) = self.peer_list_cache.get_mut(&mining_addr) {
                let handshake_cooldown_expired =
                    existing_peer.last_seen + HANDSHAKE_COOLDOWN < peer.last_seen;
                existing_peer.last_seen = peer.last_seen;
                if existing_peer.address != peer_address {
                    debug!("Peer address mismatch, updating to new address");
                    self.update_peer_address(mining_addr, peer_address);
                    true
                } else if handshake_cooldown_expired {
                    debug!("Peer address is the same, but the handshake cooldown has expired, so we need to re-handshake");
                    true
                } else {
                    debug!("Peer address is the same, no update needed");
                    false
                }
            } else {
                warn!(
                    "Peer {:?} is not found in the peer list cache, which shouldn't happen",
                    mining_addr
                );
                false
            }
        }
    }

    fn update_peer_address(&mut self, mining_addr: Address, new_address: PeerAddress) {
        if let Some(peer) = self.peer_list_cache.get_mut(&mining_addr) {
            let old_address = peer.address;
            peer.address = new_address;
            self.gossip_addr_to_mining_addr_map
                .remove(&old_address.gossip.ip());
            self.gossip_addr_to_mining_addr_map
                .insert(new_address.gossip.ip(), mining_addr);
            self.known_peers_cache.remove(&old_address);
            self.known_peers_cache.insert(old_address);
            self.api_addr_to_mining_addr_map.remove(&old_address.api);
            self.api_addr_to_mining_addr_map
                .insert(new_address.api, mining_addr);
        }
    }
}
