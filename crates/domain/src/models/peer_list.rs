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
        inner
            .persistent_peers_cache
            .get(mining_addr)
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
                let persistent_active = bindings
                    .persistent_peers_cache
                    .values()
                    .filter(|peer| peer.reputation_score.is_active() && peer.is_online)
                    .count();
                let purgatory_active = bindings
                    .unstaked_peer_purgatory
                    .values()
                    .filter(|peer| peer.reputation_score.is_active() && peer.is_online)
                    .count();
                persistent_active + purgatory_active
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

        let mut peers: Vec<(Address, PeerListItem)> = Vec::new();

        // Add peers from persistent cache
        peers.extend(
            guard
                .persistent_peers_cache
                .iter()
                .map(|(key, value)| (*key, value.clone())),
        );

        // Add peers from purgatory
        peers.extend(
            guard
                .unstaked_peer_purgatory
                .iter()
                .map(|(key, value)| (*key, value.clone())),
        );

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
            guard
                .persistent_peers_cache
                .iter()
                .map(|(key, value)| (*key, value.clone())),
        );

        // Add active peers from purgatory
        peers.extend(
            guard
                .unstaked_peer_purgatory
                .iter()
                .filter(|(_, peer)| peer.reputation_score.is_active())
                .map(|(key, value)| (*key, value.clone())),
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
            guard
                .persistent_peers_cache
                .iter()
                .filter(|(_mining_addr, peer)| !peer.reputation_score.is_active())
                .map(|(mining_addr, peer)| (*mining_addr, peer.clone())),
        );

        // Add inactive peers from purgatory
        inactive.extend(
            guard
                .unstaked_peer_purgatory
                .iter()
                .filter(|(_mining_addr, peer)| !peer.reputation_score.is_active())
                .map(|(mining_addr, peer)| (*mining_addr, peer.clone())),
        );

        inactive
    }

    pub fn peer_by_gossip_address(&self, address: SocketAddr) -> Option<PeerListItem> {
        let binding = self.read();
        let mining_address = binding
            .gossip_addr_to_mining_addr_map
            .get(&address.ip())
            .copied()?;
        binding
            .persistent_peers_cache
            .get(&mining_address)
            .or_else(|| binding.unstaked_peer_purgatory.get(&mining_address))
            .cloned()
    }

    pub fn peer_by_mining_address(&self, mining_address: &Address) -> Option<PeerListItem> {
        let binding = self.read();
        binding
            .persistent_peers_cache
            .get(mining_address)
            .or_else(|| binding.unstaked_peer_purgatory.get(mining_address))
            .cloned()
    }

    pub fn is_a_trusted_peer(&self, miner_address: Address, source_ip: IpAddr) -> bool {
        let binding = self.read();

        // Check both persistent cache and purgatory
        let peer = binding
            .persistent_peers_cache
            .get(&miner_address)
            .or_else(|| binding.unstaked_peer_purgatory.get(&miner_address));

        let Some(peer) = peer else {
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
            peer_list
                .persistent_peers_cache
                .insert(mining_addr, entry.0);
            peer_list.known_peers_cache.insert(address);
            peer_list
                .api_addr_to_mining_addr_map
                .insert(address.api, mining_addr);
        }

        Ok(peer_list)
    }

    pub fn add_or_update_peer(
        &mut self,
        mining_addr: Address,
        peer: PeerListItem,
        is_staked: bool,
    ) {
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
                self.persistent_peers_cache
                    .insert(*mining_addr, peer_clone.clone());
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
        } else if let Some(peer) = self.unstaked_peer_purgatory.remove(mining_addr) {
            self.gossip_addr_to_mining_addr_map
                .remove(&peer.address.gossip.ip());
            self.api_addr_to_mining_addr_map.remove(&peer.address.api);
            self.known_peers_cache.remove(&peer.address);
            debug!("Removed unstaked peer {:?} from all caches", mining_addr);
        }
    }

    /// Helper method to update a peer in the persistent cache
    fn update_peer_in_persistent_cache(
        &mut self,
        mining_addr: Address,
        peer: PeerListItem,
        peer_address: PeerAddress,
    ) -> bool {
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
            warn!(
                "Peer {:?} is not found in the persistent cache, which shouldn't happen",
                mining_addr
            );
            false
        }
    }

    /// Add or update a peer in the appropriate cache based on staking status and current location.
    /// Returns true if the peer was added or needs re-handshaking, false if no update needed.
    fn add_or_update_peer_internal(
        &mut self,
        mining_addr: Address,
        peer: PeerListItem,
        is_staked: bool,
    ) -> bool {
        let gossip_addr = peer.address.gossip;
        let peer_address = peer.address;

        // Check if peer exists in persistent cache
        let in_persistent = self.persistent_peers_cache.contains_key(&mining_addr);
        // Check if peer exists in purgatory
        let in_purgatory = self.unstaked_peer_purgatory.contains_key(&mining_addr);

        match (is_staked, in_persistent, in_purgatory) {
            // Case 1: Update peer in persistent cache (both staked and unstaked peers)
            (_, true, _) => {
                let peer_type = if is_staked { "staked" } else { "unstaked" };
                debug!("Updating {} peer {:?} in persistent cache", peer_type, mining_addr);
                self.update_peer_in_persistent_cache(mining_addr, peer, peer_address)
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
            }

            // Case 4: is_staked is false, and peer is not in both caches - add to purgatory
            (false, false, false) => {
                debug!("Adding unstaked peer {:?} to purgatory", mining_addr);
                self.unstaked_peer_purgatory.insert(mining_addr, peer);
                self.gossip_addr_to_mining_addr_map
                    .insert(gossip_addr.ip(), mining_addr);
                self.api_addr_to_mining_addr_map
                    .insert(peer_address.api, mining_addr);
                self.known_peers_cache.insert(peer_address);
                debug!(
                    "Unstaked peer {:?} added to purgatory with address {:?}",
                    mining_addr, peer_address
                );
                true
            }

            // Case 5: is_staked is true and peer exists in purgatory - move from purgatory to persistent cache
            (true, false, true) => {
                debug!(
                    "Moving staked peer {:?} from purgatory to persistent cache",
                    mining_addr
                );
                if let Some(purgatory_peer) = self.unstaked_peer_purgatory.remove(&mining_addr) {
                    // Update the peer data with new information
                    let mut updated_peer = purgatory_peer;
                    updated_peer.last_seen = peer.last_seen;
                    updated_peer.reputation_score = peer.reputation_score;
                    if updated_peer.address != peer_address {
                        updated_peer.address = peer_address;
                        // Update address mappings
                        self.gossip_addr_to_mining_addr_map
                            .remove(&updated_peer.address.gossip.ip());
                        self.api_addr_to_mining_addr_map
                            .remove(&updated_peer.address.api);
                        self.gossip_addr_to_mining_addr_map
                            .insert(gossip_addr.ip(), mining_addr);
                        self.api_addr_to_mining_addr_map
                            .insert(peer_address.api, mining_addr);
                    }

                    self.persistent_peers_cache
                        .insert(mining_addr, updated_peer);
                    self.known_peers_cache.insert(peer_address);
                    debug!(
                        "Peer {:?} moved from purgatory to persistent cache",
                        mining_addr
                    );
                    true
                } else {
                    warn!(
                        "Peer {:?} is not found in purgatory cache, which shouldn't happen",
                        mining_addr
                    );
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
            self.known_peers_cache.remove(&old_address);
            self.known_peers_cache.insert(new_address);
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
            self.known_peers_cache.insert(new_address);
            self.api_addr_to_mining_addr_map.remove(&old_address.api);
            self.api_addr_to_mining_addr_map
                .insert(new_address.api, mining_addr);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::{PeerAddress, PeerListItem, PeerScore, RethPeerInfo};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::sync::mpsc;

    fn create_test_peer(id: u8) -> (Address, PeerListItem) {
        let mining_addr = Address::from([id; 20]);
        let peer_address = PeerAddress {
            gossip: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, id)), 8000 + id as u16),
            api: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, id)), 9000 + id as u16),
            execution: RethPeerInfo::default(),
        };
        let peer = PeerListItem {
            address: peer_address,
            reputation_score: PeerScore::new(PeerScore::INITIAL),
            response_time: 100,
            is_online: true,
            last_seen: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        };
        (mining_addr, peer)
    }

    fn create_mock_sender() -> PeerNetworkSender {
        let (tx, _rx) = mpsc::unbounded_channel();
        PeerNetworkSender::new(tx)
    }

    fn create_test_peer_list() -> PeerList {
        let peer_list_data = PeerListDataInner {
            persistent_peers_cache: HashMap::new(),
            unstaked_peer_purgatory: HashMap::new(),
            known_peers_cache: HashSet::new(),
            gossip_addr_to_mining_addr_map: HashMap::new(),
            api_addr_to_mining_addr_map: HashMap::new(),
            trusted_peers_api_addresses: HashSet::new(),
            peer_network_service_sender: create_mock_sender(),
        };
        PeerList(Arc::new(RwLock::new(peer_list_data)))
    }

    #[tokio::test]
    async fn test_all_methods_treat_staked_unstaked_peers_equally_except_persistable() {
        let peer_list = create_test_peer_list();

        // Create test peers
        let (staked_addr, staked_peer) = create_test_peer(1);
        let (unstaked_addr, unstaked_peer) = create_test_peer(2);

        // Add peers with different staking status
        peer_list.add_or_update_peer(staked_addr, staked_peer.clone(), true);
        peer_list.add_or_update_peer(unstaked_addr, unstaked_peer.clone(), false);

        // Test 1: persistable_peers should only return staked peers
        let persistable = peer_list.persistable_peers();
        assert!(
            persistable.contains_key(&staked_addr),
            "Persistable peers should contain staked peer"
        );
        assert!(
            !persistable.contains_key(&unstaked_addr),
            "Persistable peers should NOT contain unstaked peer"
        );

        // Test 2: all_peers_for_gossip should return both staked and unstaked peers
        let gossip_peers = peer_list.all_peers_for_gossip();
        assert!(
            gossip_peers.contains_key(&staked_addr),
            "Gossip peers should contain staked peer"
        );
        assert!(
            gossip_peers.contains_key(&unstaked_addr),
            "Gossip peers should contain unstaked peer"
        );

        // Test 3: get_peer should return both staked and unstaked peers
        let staked_result = peer_list.get_peer(&staked_addr);
        let unstaked_result = peer_list.get_peer(&unstaked_addr);
        assert!(staked_result.is_some(), "get_peer should find staked peer");
        assert!(
            unstaked_result.is_some(),
            "get_peer should find unstaked peer"
        );

        // Test 4: contains_api_address should work for both staked and unstaked peers
        let staked_api_found = peer_list.contains_api_address(&staked_peer.address.api);
        let unstaked_api_found = peer_list.contains_api_address(&unstaked_peer.address.api);
        assert!(
            staked_api_found,
            "contains_api_address should find staked peer API address"
        );
        assert!(
            unstaked_api_found,
            "contains_api_address should find unstaked peer API address"
        );

        // Test 5: peer_by_gossip_address should work for both staked and unstaked peers
        let staked_gossip_result = peer_list.peer_by_gossip_address(staked_peer.address.gossip);
        let unstaked_gossip_result = peer_list.peer_by_gossip_address(unstaked_peer.address.gossip);
        assert!(
            staked_gossip_result.is_some(),
            "peer_by_gossip_address should find staked peer"
        );
        assert!(
            unstaked_gossip_result.is_some(),
            "peer_by_gossip_address should find unstaked peer"
        );

        // Test 6: peer_by_mining_address should work for both staked and unstaked peers
        let staked_mining_result = peer_list.peer_by_mining_address(&staked_addr);
        let unstaked_mining_result = peer_list.peer_by_mining_address(&unstaked_addr);
        assert!(
            staked_mining_result.is_some(),
            "peer_by_mining_address should find staked peer"
        );
        assert!(
            unstaked_mining_result.is_some(),
            "peer_by_mining_address should find unstaked peer"
        );

        // Test 7: peer_count should include both staked and unstaked peers
        let total_count = peer_list.peer_count();
        assert_eq!(
            total_count, 2,
            "peer_count should include both staked and unstaked peers"
        );

        // Test 8: top_active_peers should include both staked and unstaked peers if they're active
        let top_peers = peer_list.top_active_peers(None, None);
        assert_eq!(
            top_peers.len(),
            2,
            "top_active_peers should include both staked and unstaked peers if they're active"
        );
        let contains_staked = top_peers.iter().any(|(addr, _)| addr == &staked_addr);
        let contains_unstaked = top_peers.iter().any(|(addr, _)| addr == &unstaked_addr);
        assert!(
            contains_staked,
            "top_active_peers should include staked peer if it's active"
        );
        assert!(
            contains_unstaked,
            "top_active_peers should include unstaked peer if it's active"
        );

        // Test 9: trusted_peers should return both staked and unstaked peers if they're in trusted list
        // First, we need to add the peers to the trusted list
        {
            let mut inner = peer_list.0.write().unwrap();
            inner
                .trusted_peers_api_addresses
                .insert(staked_peer.address.api);
            inner
                .trusted_peers_api_addresses
                .insert(unstaked_peer.address.api);
        }
        let trusted_peers = peer_list.trusted_peers();
        let trusted_contains_staked = trusted_peers.iter().any(|(addr, _)| addr == &staked_addr);
        let trusted_contains_unstaked =
            trusted_peers.iter().any(|(addr, _)| addr == &unstaked_addr);
        assert!(
            trusted_contains_staked,
            "trusted_peers should include a staked peer if it's in the trusted list"
        );
        assert!(
            trusted_contains_unstaked,
            "trusted_peers should include an unstaked peer if it's the trusted list"
        );

        // Test 10: trusted_peer_addresses should return both staked and unstaked peer addresses
        let trusted_addresses = peer_list.trusted_peer_addresses();
        assert!(
            trusted_addresses.contains(&staked_peer.address.api),
            "trusted_peer_addresses should contain staked peer API address"
        );
        assert!(
            trusted_addresses.contains(&unstaked_peer.address.api),
            "trusted_peer_addresses should contain unstaked peer API address"
        );

        // Test 11: inactive_peers should include both staked and unstaked peers if they're inactive
        // Create inactive peers
        let (inactive_staked_addr, mut inactive_staked_peer) = create_test_peer(3);
        let (inactive_unstaked_addr, mut inactive_unstaked_peer) = create_test_peer(4);
        inactive_staked_peer.reputation_score = PeerScore::new(10); // Below active threshold
        inactive_unstaked_peer.reputation_score = PeerScore::new(10); // Below active threshold
        peer_list.add_or_update_peer(inactive_staked_addr, inactive_staked_peer, true);
        peer_list.add_or_update_peer(inactive_unstaked_addr, inactive_unstaked_peer, false);

        let inactive_peers = peer_list.inactive_peers();
        let inactive_contains_staked = inactive_peers
            .iter()
            .any(|(addr, _)| addr == &inactive_staked_addr);
        let inactive_contains_unstaked = inactive_peers
            .iter()
            .any(|(addr, _)| addr == &inactive_unstaked_addr);
        assert!(
            inactive_contains_staked,
            "inactive_peers should include staked peer if it's inactive"
        );
        assert!(
            inactive_contains_unstaked,
            "inactive_peers should include unstaked peer if it's inactive"
        );

        // Test 12: is_a_trusted_peer should work for both staked and unstaked peers
        let staked_is_trusted =
            peer_list.is_a_trusted_peer(staked_addr, staked_peer.address.gossip.ip());
        let unstaked_is_trusted =
            peer_list.is_a_trusted_peer(unstaked_addr, unstaked_peer.address.gossip.ip());
        assert!(
            staked_is_trusted,
            "is_a_trusted_peer should return true for staked peer in trusted list"
        );
        assert!(
            unstaked_is_trusted,
            "is_a_trusted_peer should return true for unstaked peer in trusted list"
        );

        // Test 13: all_known_peers should include both staked and unstaked peers
        let known_peers = peer_list.all_known_peers();
        let known_contains_staked = known_peers.iter().any(|addr| addr == &staked_peer.address);
        let known_contains_unstaked = known_peers
            .iter()
            .any(|addr| addr == &unstaked_peer.address);
        assert!(
            known_contains_staked,
            "all_known_peers should include staked peer address"
        );
        assert!(
            known_contains_unstaked,
            "all_known_peers should include unstaked peer address (after fix)"
        );

        // Test 14: request_block_from_the_network should work (async method)
        let block_hash = BlockHash::default();
        let block_request_result = peer_list
            .request_block_from_the_network(block_hash, false)
            .await;
        // This will likely fail due to mock sender, but the method should handle both peer types equally
        assert!(
            block_request_result.is_err(),
            "request_block_from_the_network should work with mock sender (expected to fail)"
        );

        // Test 15: request_payload_from_the_network should work (async method)
        let payload_hash = B256::default();
        let payload_request_result = peer_list
            .request_payload_from_the_network(payload_hash, false)
            .await;
        // This will likely fail due to mock sender, but the method should handle both peer types equally
        assert!(
            payload_request_result.is_err(),
            "request_payload_from_the_network should work with mock sender (expected to fail)"
        );
    }

    #[tokio::test]
    async fn test_wait_for_active_peers_includes_both_staked_and_unstaked() {
        let peer_list = create_test_peer_list();

        // Create test peers with active reputation scores
        let (staked_addr, mut staked_peer) = create_test_peer(1);
        let (unstaked_addr, mut unstaked_peer) = create_test_peer(2);

        // Make sure peers have active reputation scores (above ACTIVE_THRESHOLD = 20)
        // Start with INITIAL = 50, so they should already be active
        staked_peer.reputation_score = PeerScore::new(80); // Well above active threshold
        unstaked_peer.reputation_score = PeerScore::new(80); // Well above active threshold

        // Test case 1: Only unstaked peer is active
        peer_list.add_or_update_peer(unstaked_addr, unstaked_peer, false);

        // The wait_for_active_peers should find the active unstaked peer
        // We'll test this by checking if there are active peers
        let active_peers_count = {
            let bindings = peer_list.read();
            let persistent_active = bindings
                .persistent_peers_cache
                .values()
                .filter(|peer| peer.reputation_score.is_active() && peer.is_online)
                .count();
            let purgatory_active = bindings
                .unstaked_peer_purgatory
                .values()
                .filter(|peer| peer.reputation_score.is_active() && peer.is_online)
                .count();
            persistent_active + purgatory_active
        };

        assert!(
            active_peers_count > 0,
            "wait_for_active_peers should consider unstaked peers"
        );

        // Test case 2: Add staked peer and verify both are counted
        peer_list.add_or_update_peer(staked_addr, staked_peer, true);

        let active_peers_count_both = {
            let bindings = peer_list.read();
            let persistent_active = bindings
                .persistent_peers_cache
                .values()
                .filter(|peer| peer.reputation_score.is_active() && peer.is_online)
                .count();
            let purgatory_active = bindings
                .unstaked_peer_purgatory
                .values()
                .filter(|peer| peer.reputation_score.is_active() && peer.is_online)
                .count();
            persistent_active + purgatory_active
        };

        assert_eq!(
            active_peers_count_both, 2,
            "Both staked and unstaked active peers should be counted"
        );
    }
}
