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
    /// Main peer list cache - contains both staked and persistable unstaked peers
    persistent_peers_cache: HashMap<Address, PeerListItem>,
    /// Purgatory for unstaked peers that haven't proven themselves yet
    /// These peers are kept in memory only until they reach the persistence threshold
    unstaked_peer_purgatory: HashMap<Address, PeerListItem>,
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

    pub fn add_or_update_peer(&self, mining_addr: Address, peer: PeerListItem, is_staked: bool) {
        let mut inner = self.0.write().expect("PeerListDataInner lock poisoned");
        inner.add_or_update_peer(mining_addr, peer, is_staked);
    }

    pub fn increase_peer_score(&self, mining_addr: &Address, reason: ScoreIncreaseReason) {
        let mut inner = self.0.write().expect("PeerListDataInner lock poisoned");
        inner.increase_score(mining_addr, reason);
    }

    pub fn decrease_peer_score(&self, mining_addr: &Address, reason: ScoreDecreaseReason) {
        let mut inner = self.0.write().expect("PeerListDataInner lock poisoned");
        inner.decrease_peer_score(mining_addr, reason);
    }

    /// Get a peer from any cache (persistent or purgatory)
    pub fn get_peer(&self, mining_addr: &Address) -> Option<PeerListItem> {
        let inner = self.read();
        inner.persistent_peers_cache.get(mining_addr)
            .or_else(|| inner.unstaked_peer_purgatory.get(mining_addr))
            .cloned()
    }

    pub fn all_known_peers(&self) -> Vec<PeerAddress> {
        self.read().known_peers_cache.iter().copied().collect()
    }

    /// Get all peers (persistent + purgatory) for gossip purposes
    pub fn all_peers_for_gossip(&self) -> HashMap<Address, PeerListItem> {
        let guard = self.read();
        let mut all_peers = guard.persistent_peers_cache.clone();
        // Add active peers from purgatory for gossip
        for (addr, peer) in &guard.unstaked_peer_purgatory {
            all_peers.insert(*addr, peer.clone());
        }
        all_peers
    }

    /// Get only persistable peers (for database storage)
    pub fn persistable_peers(&self) -> HashMap<Address, PeerListItem> {
        let guard = self.read();
        guard.persistent_peers_cache.clone()
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
                    .persistent_peers_cache
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
            .persistent_peers_cache
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

        let mut peers: Vec<(Address, PeerListItem)> = Vec::new();
        
        // Add peers from main cache
        peers.extend(
            guard.persistent_peers_cache
                .iter()
                .map(|(key, value)| (*key, value.clone()))
        );
        
        // Add active peers from purgatory
        peers.extend(
            guard.unstaked_peer_purgatory
                .iter()
                .filter(|(_, peer)| peer.reputation_score.is_active())
                .map(|(key, value)| (*key, value.clone()))
        );

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
        let guard = self.read();
        let mut inactive = Vec::new();
        
        // Add inactive peers from main cache
        inactive.extend(
            guard.persistent_peers_cache
                .iter()
                .filter(|(_mining_addr, peer)| !peer.reputation_score.is_active())
                .map(|(mining_addr, peer)| (*mining_addr, peer.clone()))
        );
        
        // Add inactive peers from purgatory
        inactive.extend(
            guard.unstaked_peer_purgatory
                .iter()
                .filter(|(_mining_addr, peer)| !peer.reputation_score.is_active())
                .map(|(mining_addr, peer)| (*mining_addr, peer.clone()))
        );
        
        inactive
    }

    pub fn peer_by_gossip_address(&self, address: SocketAddr) -> Option<PeerListItem> {
        let binding = self.read();
        let mining_address = binding
            .gossip_addr_to_mining_addr_map
            .get(&address.ip())
            .copied()?;
        binding.persistent_peers_cache.get(&mining_address)
            .or_else(|| binding.unstaked_peer_purgatory.get(&mining_address))
            .cloned()
    }

    pub fn peer_by_mining_address(&self, mining_address: &Address) -> Option<PeerListItem> {
        let binding = self.read();
        binding.persistent_peers_cache.get(mining_address)
            .or_else(|| binding.unstaked_peer_purgatory.get(mining_address))
            .cloned()
    }

    pub fn is_a_trusted_peer(&self, miner_address: Address, source_ip: IpAddr) -> bool {
        let binding = self.read();

        let Some(peer) = binding.persistent_peers_cache.get(&miner_address) else {
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
        let guard = self.read();
        guard.persistent_peers_cache.len() + guard.unstaked_peer_purgatory.len()
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
            persistent_peers_cache: HashMap::new(),
            unstaked_peer_purgatory: HashMap::new(),
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
            peer_list.persistent_peers_cache.insert(mining_addr, entry.0);
            peer_list.known_peers_cache.insert(address);
            peer_list
                .api_addr_to_mining_addr_map
                .insert(address.api, mining_addr);
        }

        Ok(peer_list)
    }

    pub fn add_or_update_peer(&mut self, mining_addr: Address, peer: PeerListItem, is_staked: bool) {
        let is_updated = self.add_or_update_peer_internal(mining_addr, peer.clone(), is_staked);

        if is_updated {
            debug!(
                "Sending PeerUpdated message to the service for persistent peer {:?}",
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
        if let Some(peer) = self.persistent_peers_cache.get_mut(mining_addr) {
            match reason {
                ScoreIncreaseReason::Online => {
                    peer.reputation_score.increase();
                }
                ScoreIncreaseReason::ValidData => {
                    peer.reputation_score.increase();
                }
            }
        } else if let Some(peer) = self.unstaked_peer_purgatory.get_mut(mining_addr) {
            // Update score in purgatory
            match reason {
                ScoreIncreaseReason::Online => {
                    peer.reputation_score.increase();
                }
                ScoreIncreaseReason::ValidData => {
                    peer.reputation_score.increase();
                }
            }
            
            if peer.reputation_score.is_persistable() {
                debug!(
                    "Unstaked peer {:?} has reached persistence threshold, promoting to persistent cache",
                    mining_addr
                );
                // Move from purgatory to persistent cache
                let peer_clone = peer.clone();
                self.unstaked_peer_purgatory.remove(mining_addr);
                self.persistent_peers_cache.insert(*mining_addr, peer_clone.clone());
                self.known_peers_cache.insert(peer_clone.address);
            }
        }
    }

    pub fn decrease_peer_score(&mut self, mining_addr: &Address, reason: ScoreDecreaseReason) {
        // Check persistent cache first
        if let Some(peer_item) = self.persistent_peers_cache.get_mut(mining_addr) {
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
        } else {
            if let Some(peer) = self.unstaked_peer_purgatory.remove(mining_addr) {
                self.gossip_addr_to_mining_addr_map.remove(&peer.address.gossip.ip());
                self.api_addr_to_mining_addr_map.remove(&peer.address.api);
                debug!("Removed unstaked peer {:?} from all caches", mining_addr);
            }
        }
    }

    /// Add or update a peer in the appropriate cache based on staking status and current location.
    /// Returns true if the peer was added or needs re-handshaking, false if no update needed.
    fn add_or_update_peer_internal(&mut self, mining_addr: Address, peer: PeerListItem, is_staked: bool) -> bool {
        let gossip_addr = peer.address.gossip;
        let peer_address = peer.address;

        // Check if peer exists in persistent cache
        let in_persistent = self.persistent_peers_cache.contains_key(&mining_addr);
        // Check if peer exists in purgatory
        let in_purgatory = self.unstaked_peer_purgatory.contains_key(&mining_addr);

        match (is_staked, in_persistent, in_purgatory) {
            // Case 1: is_staked is false and peer is in persistent cache - update persistent cache
            (false, true, _) => {
                debug!("Updating unstaked peer {:?} in persistent cache", mining_addr);
                if let Some(existing_peer) = self.persistent_peers_cache.get_mut(&mining_addr) {
                    let handshake_cooldown_expired =
                        existing_peer.last_seen + HANDSHAKE_COOLDOWN < peer.last_seen;
                    existing_peer.last_seen = peer.last_seen;
                    existing_peer.reputation_score = peer.reputation_score;
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
                    warn!("Peer {:?} is not found in the persistent cache, which shouldn't happen", mining_addr);
                    false
                }
            }
            
            // Case 2: is_staked is false and peer is in purgatory - update purgatory
            (false, false, true) => {
                debug!("Updating unstaked peer {:?} in purgatory", mining_addr);
                if let Some(existing_peer) = self.unstaked_peer_purgatory.get_mut(&mining_addr) {
                    let handshake_cooldown_expired =
                        existing_peer.last_seen + HANDSHAKE_COOLDOWN < peer.last_seen;
                    existing_peer.last_seen = peer.last_seen;
                    existing_peer.reputation_score = peer.reputation_score;
                    if existing_peer.address != peer_address {
                        debug!("Unstaked peer address mismatch, updating to new address");
                        self.update_peer_address_purgatory(mining_addr, peer_address);
                        true
                    } else if handshake_cooldown_expired {
                        debug!("Unstaked peer address is the same, but the handshake cooldown has expired, so we need to re-handshake");
                        true
                    } else {
                        debug!("Unstaked peer address is the same, no update needed");
                        false
                    }
                } else {
                    warn!("Unstaked peer {:?} is not found in the purgatory cache, which shouldn't happen", mining_addr);
                    false
                }
            }
            
            // Case 3: is_staked is true and peer is not in both caches - add to persistent cache
            (true, false, false) => {
                debug!("Adding staked peer {:?} to persistent cache", mining_addr);
                self.persistent_peers_cache.insert(mining_addr, peer);
                self.gossip_addr_to_mining_addr_map.insert(gossip_addr.ip(), mining_addr);
                self.api_addr_to_mining_addr_map.insert(peer_address.api, mining_addr);
                self.known_peers_cache.insert(peer_address);
                debug!("Peer {:?} added to the peer list with address {:?}", mining_addr, peer_address);
                true
            }
            
            // Case 4: is_staked is false and peer is not in both caches - add to purgatory
            (false, false, false) => {
                debug!("Adding unstaked peer {:?} to purgatory", mining_addr);
                self.unstaked_peer_purgatory.insert(mining_addr, peer);
                self.gossip_addr_to_mining_addr_map.insert(gossip_addr.ip(), mining_addr);
                self.api_addr_to_mining_addr_map.insert(peer_address.api, mining_addr);
                // Don't add to known_peers_cache for purgatory peers - they need to prove themselves first
                debug!("Unstaked peer {:?} added to purgatory with address {:?}", mining_addr, peer_address);
                true
            }
            
            // Case 5: is_staked is true and peer exists in persistent cache - update persistent cache
            (true, true, _) => {
                debug!("Updating staked peer {:?} in persistent cache", mining_addr);
                if let Some(existing_peer) = self.persistent_peers_cache.get_mut(&mining_addr) {
                    let handshake_cooldown_expired =
                        existing_peer.last_seen + HANDSHAKE_COOLDOWN < peer.last_seen;
                    existing_peer.last_seen = peer.last_seen;
                    existing_peer.reputation_score = peer.reputation_score;
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
                    warn!("Peer {:?} is not found in the persistent cache, which shouldn't happen", mining_addr);
                    false
                }
            }
            
            // Case 6: is_staked is true and peer exists in purgatory - move from purgatory to persistent cache
            (true, false, true) => {
                debug!("Moving staked peer {:?} from purgatory to persistent cache", mining_addr);
                if let Some(purgatory_peer) = self.unstaked_peer_purgatory.remove(&mining_addr) {
                    // Update the peer data with new information
                    let mut updated_peer = purgatory_peer;
                    updated_peer.last_seen = peer.last_seen;
                    updated_peer.reputation_score = peer.reputation_score;
                    if updated_peer.address != peer_address {
                        updated_peer.address = peer_address;
                        // Update address mappings
                        self.gossip_addr_to_mining_addr_map.remove(&updated_peer.address.gossip.ip());
                        self.api_addr_to_mining_addr_map.remove(&updated_peer.address.api);
                        self.gossip_addr_to_mining_addr_map.insert(gossip_addr.ip(), mining_addr);
                        self.api_addr_to_mining_addr_map.insert(peer_address.api, mining_addr);
                    }
                    
                    self.persistent_peers_cache.insert(mining_addr, updated_peer);
                    self.known_peers_cache.insert(peer_address);
                    debug!("Peer {:?} moved from purgatory to persistent cache", mining_addr);
                    true
                } else {
                    warn!("Peer {:?} is not found in purgatory cache, which shouldn't happen", mining_addr);
                    false
                }
            }
        }
    }

    fn update_peer_address_purgatory(&mut self, mining_addr: Address, new_address: PeerAddress) {
        if let Some(peer) = self.unstaked_peer_purgatory.get_mut(&mining_addr) {
            let old_address = peer.address;
            peer.address = new_address;
            self.gossip_addr_to_mining_addr_map
                .remove(&old_address.gossip.ip());
            self.gossip_addr_to_mining_addr_map
                .insert(new_address.gossip.ip(), mining_addr);
            self.api_addr_to_mining_addr_map.remove(&old_address.api);
            self.api_addr_to_mining_addr_map
                .insert(new_address.api, mining_addr);
        }
    }

    fn update_peer_address(&mut self, mining_addr: Address, new_address: PeerAddress) {
        if let Some(peer) = self.persistent_peers_cache.get_mut(&mining_addr) {
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
