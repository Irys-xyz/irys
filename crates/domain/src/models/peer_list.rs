use crate::models::PeerEvent;
use alloy_core::primitives::B256;
use irys_database::reth_db::Database as _;
use irys_database::tables::PeerListItems;
use irys_database::walk_all;
use irys_primitives::Address;
use irys_types::{
    Config, DatabaseProvider, PeerAddress, PeerFilterMode, PeerListItem, PeerNetworkError,
    PeerNetworkSender,
};
use lru::LruCache;
use std::collections::{HashMap, HashSet};
use std::iter::Chain;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, RwLock, RwLockReadGuard};
use std::time::Duration;
use tokio::sync::broadcast;
use tracing::{debug, error, warn};

const UNSTAKED_PEER_PURGATORY_CAPACITY: usize = 500;

pub(crate) const MILLISECONDS_IN_SECOND: u64 = 1000;
pub(crate) const HANDSHAKE_COOLDOWN: u64 = MILLISECONDS_IN_SECOND * 5;

#[derive(Clone, Debug, Copy)]
pub enum ScoreDecreaseReason {
    BogusData,
    Offline,
    SlowResponse,
    NoResponse,
}

#[derive(Clone, Debug, Copy)]
pub enum ScoreIncreaseReason {
    Online,
    ValidData,
    DataRequest,
    TimelyResponse,
}

#[derive(Debug, Clone)]
pub struct PeerListDataInner {
    gossip_addr_to_mining_addr_map: HashMap<IpAddr, Address>,
    api_addr_to_mining_addr_map: HashMap<SocketAddr, Address>,
    /// Main peer list cache - contains both staked and persistable unstaked peers
    persistent_peers_cache: HashMap<Address, PeerListItem>,
    /// Purgatory for unstaked peers that haven't proven themselves yet
    /// These peers are kept in memory only until they reach the persistence threshold
    unstaked_peer_purgatory: LruCache<Address, PeerListItem>,
    known_peers_cache: HashSet<PeerAddress>,
    trusted_peers_api_addresses: HashSet<SocketAddr>,
    /// Whitelist of allowed peer API addresses based on peer filter mode
    peer_whitelist: HashSet<SocketAddr>,
    peer_network_service_sender: PeerNetworkSender,
    /// Broadcast channel for peer lifecycle/activity events
    peer_events: broadcast::Sender<PeerEvent>,
}

/// Iterator for all peers (persistent + purgatory) for gossip purposes
pub struct AllPeersReadGuard<'a> {
    guard: RwLockReadGuard<'a, PeerListDataInner>,
}

impl<'a> AllPeersReadGuard<'a> {
    fn new(guard: RwLockReadGuard<'a, PeerListDataInner>) -> Self {
        Self { guard }
    }

    pub fn iter(
        &'a self,
    ) -> Chain<
        std::collections::hash_map::Iter<'a, Address, PeerListItem>,
        lru::Iter<'a, Address, PeerListItem>,
    > {
        self.guard
            .persistent_peers_cache
            .iter()
            .chain(self.guard.unstaked_peer_purgatory.iter())
    }
}

#[derive(Clone, Debug)]
pub struct PeerList(Arc<RwLock<PeerListDataInner>>);

impl PeerList {
    pub fn new(
        config: &Config,
        db: &DatabaseProvider,
        peer_service_sender: PeerNetworkSender,
        peer_events: broadcast::Sender<PeerEvent>,
    ) -> Result<Self, PeerNetworkError> {
        let read_tx = db.tx().map_err(PeerNetworkError::from)?;
        let compact_peers =
            walk_all::<PeerListItems, _>(&read_tx).map_err(PeerNetworkError::from)?;

        let peers = compact_peers
            .into_iter()
            .map(|(address, compact_item)| (address, PeerListItem::from(compact_item)))
            .collect();
        let inner = PeerListDataInner::new(peers, peer_service_sender, config, peer_events)?;
        Ok(Self(Arc::new(RwLock::new(inner))))
    }

    pub fn test_mock() -> Result<Self, PeerNetworkError> {
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let inner = PeerListDataInner::new(
            vec![],
            PeerNetworkSender::new(sender),
            &Config::new(irys_types::NodeConfig::testing()),
            broadcast::channel::<PeerEvent>(100).0,
        )?;
        Ok(Self(Arc::new(RwLock::new(inner))))
    }

    pub fn from_peers(
        peers: Vec<(Address, PeerListItem)>,
        peer_network: PeerNetworkSender,
        config: &Config,
        peer_events: broadcast::Sender<PeerEvent>,
    ) -> Result<Self, PeerNetworkError> {
        let inner = PeerListDataInner::new(peers, peer_network, config, peer_events)?;
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

    pub fn set_is_online(&self, mining_addr: &Address, is_online: bool) {
        let mut inner = self.0.write().expect("PeerListDataInner lock poisoned");
        let mut became_active: Option<irys_types::PeerListItem> = None;
        let mut became_inactive: Option<irys_types::PeerListItem> = None;
        if let Some(peer) = inner.persistent_peers_cache.get_mut(mining_addr) {
            let was_active = peer.reputation_score.is_active() && peer.is_online;
            peer.is_online = is_online;
            let now_active = peer.reputation_score.is_active() && peer.is_online;
            if !was_active && now_active {
                became_active = Some(peer.clone());
            }
            if was_active && !now_active {
                became_inactive = Some(peer.clone());
            }
        } else if let Some(peer) = inner.unstaked_peer_purgatory.get_mut(mining_addr) {
            let was_active = peer.reputation_score.is_active() && peer.is_online;
            peer.is_online = is_online;
            let now_active = peer.reputation_score.is_active() && peer.is_online;
            if !was_active && now_active {
                became_active = Some(peer.clone());
            }
            if was_active && !now_active {
                became_inactive = Some(peer.clone());
            }
        }
        if let Some(peer) = became_active {
            inner.emit_peer_event(PeerEvent::BecameActive {
                mining_addr: *mining_addr,
                peer,
            });
        }
        if let Some(peer) = became_inactive {
            inner.emit_peer_event(PeerEvent::BecameInactive {
                mining_addr: *mining_addr,
                peer,
            });
        }
    }

    /// Get a peer from any cache (persistent or purgatory)
    pub fn get_peer(&self, mining_addr: &Address) -> Option<PeerListItem> {
        let inner = self.read();
        inner
            .persistent_peers_cache
            .get(mining_addr)
            .or_else(|| inner.unstaked_peer_purgatory.peek(mining_addr))
            .cloned()
    }

    pub fn all_known_peers(&self) -> Vec<PeerAddress> {
        self.read().known_peers_cache.iter().copied().collect()
    }

    /// Get all peers (persistent + purgatory) for gossip purposes
    pub fn all_peers(&self) -> AllPeersReadGuard<'_> {
        let guard = self.read();
        AllPeersReadGuard::new(guard)
    }

    /// Get only persistable peers (for database storage)
    pub fn persistable_peers(&self) -> HashMap<Address, PeerListItem> {
        let guard = self.read();
        guard.persistent_peers_cache.clone()
    }

    pub fn temporary_peers(&self) -> LruCache<Address, PeerListItem> {
        self.read().unstaked_peer_purgatory.clone()
    }

    /// Subscribe to peer lifecycle/activity events.
    pub fn subscribe_to_peer_events(&self) -> broadcast::Receiver<PeerEvent> {
        let guard = self.read();
        guard.peer_events.subscribe()
    }

    pub fn contains_api_address(&self, api_address: &SocketAddr) -> bool {
        self.read()
            .api_addr_to_mining_addr_map
            .contains_key(api_address)
    }

    pub async fn wait_for_active_peers(&self) {
        // Fast path: return immediately if any active peers exist
        {
            let bindings = self.read();
            let persistent_active = bindings
                .persistent_peers_cache
                .values()
                .any(|peer| peer.reputation_score.is_active() && peer.is_online);
            let purgatory_active = bindings
                .unstaked_peer_purgatory
                .iter()
                .map(|(_, v)| v)
                .any(|peer| peer.reputation_score.is_active() && peer.is_online);
            if persistent_active || purgatory_active {
                return;
            }
        }

        // Slow path: subscribe and wait for the next BecameActive event
        let mut rx = self.subscribe_to_peer_events();
        loop {
            match rx.recv().await {
                Ok(PeerEvent::BecameActive { .. }) => return,
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    warn!("peer events channel closed while waiting for active peers");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    rx = self.subscribe_to_peer_events();
                }
            }
        }
    }

    pub fn all_trusted_peers(&self) -> Vec<(Address, PeerListItem)> {
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

    pub fn online_trusted_peers(&self) -> Vec<(Address, PeerListItem)> {
        let mut trusted_peers = self.all_trusted_peers();
        trusted_peers.retain(|(_miner_address, peer)| peer.is_online);
        trusted_peers
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

        // Create a chained iterator that combines both peer sources
        let persistent_peers = guard
            .persistent_peers_cache
            .iter()
            .map(|(key, value)| (*key, value.clone()));

        let purgatory_peers = guard
            .unstaked_peer_purgatory
            .iter()
            .filter(|(_, peer)| peer.reputation_score.is_active())
            .map(|(key, value)| (*key, value.clone()));

        // Chain iterators and apply all filters in one pass
        let filtered_peers =
            persistent_peers
                .chain(purgatory_peers)
                .filter(|(miner_address, peer)| {
                    let exclude = exclude_peers
                        .as_ref()
                        .is_some_and(|excluded| excluded.contains(miner_address));
                    !exclude && peer.reputation_score.is_active() && peer.is_online
                });

        let mut peers: Vec<(Address, PeerListItem)> = filtered_peers.collect();

        peers.sort_by_key(|(_address, peer)| peer.reputation_score.get());
        peers.reverse();

        if let Some(truncate) = limit {
            peers.truncate(truncate);
        }

        peers
    }

    pub fn all_peers_sorted_by_score(&self) -> Vec<(Address, PeerListItem)> {
        let guard = self.read();

        // Create a chained iterator that combines both peer sources
        let persistent_peers = guard
            .persistent_peers_cache
            .iter()
            .map(|(key, value)| (*key, value.clone()));

        let purgatory_peers = guard
            .unstaked_peer_purgatory
            .iter()
            .map(|(key, value)| (*key, value.clone()));

        let all_peers = persistent_peers.chain(purgatory_peers);
        let mut peers: Vec<(Address, PeerListItem)> = all_peers.collect();

        peers.sort_by_key(|(_address, peer)| peer.reputation_score.get());
        peers.reverse();

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
            .or_else(|| binding.unstaked_peer_purgatory.peek(&mining_address))
            .cloned()
    }

    pub fn peer_by_mining_address(&self, mining_address: &Address) -> Option<PeerListItem> {
        let binding = self.read();
        binding
            .persistent_peers_cache
            .get(mining_address)
            .or_else(|| binding.unstaked_peer_purgatory.peek(mining_address))
            .cloned()
    }

    pub fn is_a_trusted_peer(&self, miner_address: Address, source_ip: IpAddr) -> bool {
        let binding = self.read();

        // Check both persistent cache and purgatory
        let peer = binding
            .persistent_peers_cache
            .get(&miner_address)
            .or_else(|| binding.unstaked_peer_purgatory.peek(&miner_address));

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

    pub async fn request_payload_from_the_network(
        &self,
        evm_payload_hash: B256,
        use_trusted_peers_only: bool,
    ) -> Result<(), PeerNetworkError> {
        let sender = {
            self.0
                .read()
                .expect("PeerListDataInner lock poisoned")
                .peer_network_service_sender
                .clone()
        };
        sender
            .request_payload_to_be_gossiped_from_network(evm_payload_hash, use_trusted_peers_only)
            .await
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, PeerListDataInner> {
        self.0.read().expect("PeerListDataInner lock poisoned")
    }

    pub fn peer_count(&self) -> usize {
        let guard = self.read();
        guard.persistent_peers_cache.len() + guard.unstaked_peer_purgatory.len()
    }

    /// Check if a peer API address is allowed based on the peer filter mode
    pub fn is_peer_allowed(&self, api_address: &SocketAddr) -> bool {
        let guard = self.read();
        // If whitelist is empty, all peers are allowed (unrestricted mode)
        guard.peer_whitelist.is_empty() || guard.peer_whitelist.contains(api_address)
    }

    /// Add peers to the whitelist (used for TrustedAndHandshake mode)
    pub fn add_peers_to_whitelist(&self, peer_addresses: Vec<SocketAddr>) {
        let mut guard = self.0.write().expect("PeerListDataInner lock poisoned");
        for address in peer_addresses {
            guard.peer_whitelist.insert(address);
        }
    }

    /// Check if an API address is a trusted peer
    pub fn is_trusted_peer(&self, api_address: &SocketAddr) -> bool {
        let guard = self.read();
        guard.trusted_peers_api_addresses.contains(api_address)
    }

    /// Initiate a handshake with a peer by its API address. If force is set to true, the networking
    /// service will attempt to handshake even if the previous handshake was successful.
    pub fn initiate_handshake(&self, api_address: SocketAddr, force: bool) {
        let guard = self.read();
        guard.initiate_handshake(api_address, force);
    }
}

impl PeerListDataInner {
    pub fn new(
        peers: Vec<(Address, PeerListItem)>,
        peer_network_sender: PeerNetworkSender,
        config: &Config,
        peer_events: broadcast::Sender<PeerEvent>,
    ) -> Result<Self, PeerNetworkError> {
        let trusted_peers_api_addresses: HashSet<SocketAddr> = config
            .node_config
            .trusted_peers
            .iter()
            .map(|p| p.api)
            .collect();

        // Initialize whitelist based on peer filter mode
        let peer_api_ip_whitelist = match config.node_config.peer_filter_mode {
            PeerFilterMode::Unrestricted => HashSet::new(), // No restrictions
            PeerFilterMode::TrustedOnly | PeerFilterMode::TrustedAndHandshake => {
                let mut ip_whitelist = trusted_peers_api_addresses.clone();
                ip_whitelist.extend(config.node_config.initial_whitelist.clone());
                ip_whitelist
            }
        };

        let mut peer_list = Self {
            gossip_addr_to_mining_addr_map: HashMap::new(),
            api_addr_to_mining_addr_map: HashMap::new(),
            persistent_peers_cache: HashMap::new(),
            unstaked_peer_purgatory: LruCache::new(
                std::num::NonZeroUsize::new(UNSTAKED_PEER_PURGATORY_CAPACITY)
                    .expect("Expected to be able to create an LRU cache"),
            ),
            known_peers_cache: HashSet::new(),
            trusted_peers_api_addresses,
            peer_whitelist: peer_api_ip_whitelist,
            peer_network_service_sender: peer_network_sender,
            peer_events,
        };

        for (mining_address, peer_list_item) in peers {
            let address = peer_list_item.address;
            peer_list
                .gossip_addr_to_mining_addr_map
                .insert(peer_list_item.address.gossip.ip(), mining_address);
            peer_list
                .persistent_peers_cache
                .insert(mining_address, peer_list_item);
            peer_list.known_peers_cache.insert(address);
            peer_list
                .api_addr_to_mining_addr_map
                .insert(address.api, mining_address);
        }

        Ok(peer_list)
    }

    /// Helper to emit a peer event to the event bus
    fn emit_peer_event(&self, event: PeerEvent) {
        let _ = self.peer_events.send(event);
    }

    pub fn add_or_update_peer(
        &mut self,
        mining_addr: Address,
        peer: PeerListItem,
        is_staked: bool,
    ) {
        // Determine previous active state (if existed)
        let was_active = self
            .persistent_peers_cache
            .get(&mining_addr)
            .map(|p| p.reputation_score.is_active() && p.is_online)
            .or_else(|| {
                self.unstaked_peer_purgatory
                    .peek(&mining_addr)
                    .map(|p| p.reputation_score.is_active() && p.is_online)
            })
            .unwrap_or(false);

        let is_updated = self.add_or_update_peer_internal(mining_addr, peer.clone(), is_staked);

        // Determine new active state
        let now_peer = self
            .persistent_peers_cache
            .get(&mining_addr)
            .cloned()
            .or_else(|| self.unstaked_peer_purgatory.peek(&mining_addr).cloned());

        if let Some(now_peer) = now_peer {
            let now_active = now_peer.reputation_score.is_active() && now_peer.is_online;
            if !was_active && now_active {
                self.emit_peer_event(PeerEvent::BecameActive {
                    mining_addr,
                    peer: now_peer,
                });
            }
        }

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
            // Emit a generic PeerUpdated for other subscribers
            if let Some(updated_peer) = self
                .persistent_peers_cache
                .get(&mining_addr)
                .cloned()
                .or_else(|| self.unstaked_peer_purgatory.peek(&mining_addr).cloned())
            {
                self.emit_peer_event(PeerEvent::PeerUpdated {
                    mining_addr,
                    peer: updated_peer,
                });
            }
        }
    }

    pub fn initiate_handshake(&self, api_address: SocketAddr, force: bool) {
        if let Err(send_error) = self
            .peer_network_service_sender
            .initiate_handshake(api_address, force)
        {
            error!("Failed to send a force announce message: {:?}", send_error);
        }
    }

    pub fn increase_score(&mut self, mining_addr: &Address, reason: ScoreIncreaseReason) {
        if let Some(peer) = self.persistent_peers_cache.get_mut(mining_addr) {
            let was_active = peer.reputation_score.is_active() && peer.is_online;
            match reason {
                ScoreIncreaseReason::Online => {
                    peer.reputation_score.increase_by(1);
                }
                ScoreIncreaseReason::ValidData => {
                    peer.reputation_score.increase_by(3);
                }
                ScoreIncreaseReason::DataRequest => {
                    peer.reputation_score.increase_by(1);
                }
                ScoreIncreaseReason::TimelyResponse => {
                    peer.reputation_score.increase_by(2);
                }
            }
            let now_active = peer.reputation_score.is_active() && peer.is_online;
            let to_send = (!was_active && now_active).then(|| peer.clone());
            let _ = peer;
            if let Some(peer) = to_send {
                self.emit_peer_event(PeerEvent::BecameActive {
                    mining_addr: *mining_addr,
                    peer,
                });
            }
        } else if let Some(peer) = self.unstaked_peer_purgatory.get_mut(mining_addr) {
            // Update score in purgatory
            let was_active = peer.reputation_score.is_active() && peer.is_online;
            match reason {
                ScoreIncreaseReason::Online => {
                    peer.reputation_score.increase_by(1);
                }
                ScoreIncreaseReason::ValidData => {
                    peer.reputation_score.increase_by(3);
                }
                ScoreIncreaseReason::DataRequest => {
                    peer.reputation_score.increase_by(1);
                }
                ScoreIncreaseReason::TimelyResponse => {
                    peer.reputation_score.increase_by(2);
                }
            }

            if peer.reputation_score.is_persistable() {
                debug!(
                    "Unstaked peer {:?} has reached persistence threshold, promoting to persistent cache",
                    mining_addr
                );
                // Move from purgatory to persistent cache
                let peer_clone = peer.clone();
                self.unstaked_peer_purgatory.pop(mining_addr);
                self.persistent_peers_cache
                    .insert(*mining_addr, peer_clone.clone());
                self.known_peers_cache.insert(peer_clone.address);
            }

            // Check post-state (may be in persistent now)
            let now_peer = self
                .persistent_peers_cache
                .get(mining_addr)
                .cloned()
                .or_else(|| self.unstaked_peer_purgatory.peek(mining_addr).cloned());
            if let Some(now_peer) = now_peer {
                let now_active = now_peer.reputation_score.is_active() && now_peer.is_online;
                if !was_active && now_active {
                    self.emit_peer_event(PeerEvent::BecameActive {
                        mining_addr: *mining_addr,
                        peer: now_peer,
                    });
                }
            }
        }
    }

    pub fn decrease_peer_score(&mut self, mining_addr: &Address, reason: ScoreDecreaseReason) {
        // Check persistent cache first
        if let Some(peer_item) = self.persistent_peers_cache.get_mut(mining_addr) {
            let was_active = peer_item.reputation_score.is_active() && peer_item.is_online;
            match reason {
                ScoreDecreaseReason::BogusData => {
                    peer_item.reputation_score.decrease_bogus_data();
                }
                ScoreDecreaseReason::Offline => {
                    peer_item.reputation_score.decrease_offline();
                }
                ScoreDecreaseReason::SlowResponse => {
                    peer_item.reputation_score.decrease();
                }
                ScoreDecreaseReason::NoResponse => {
                    peer_item.reputation_score.decrease_offline();
                }
            }

            // Don't propagate inactive peers
            if !peer_item.reputation_score.is_active() {
                self.known_peers_cache.remove(&peer_item.address);
            }
            let now_active = peer_item.reputation_score.is_active() && peer_item.is_online;
            if was_active && !now_active {
                let peer_clone = peer_item.clone();
                self.emit_peer_event(PeerEvent::BecameInactive {
                    mining_addr: *mining_addr,
                    peer: peer_clone,
                });
            }
        } else if let Some(peer) = self.unstaked_peer_purgatory.pop(mining_addr) {
            self.gossip_addr_to_mining_addr_map
                .remove(&peer.address.gossip.ip());
            self.api_addr_to_mining_addr_map.remove(&peer.address.api);
            self.known_peers_cache.remove(&peer.address);
            debug!("Removed unstaked peer {:?} from all caches", mining_addr);
            self.emit_peer_event(PeerEvent::PeerRemoved {
                mining_addr: *mining_addr,
                peer,
            });
        }
    }

    /// Helper method to update a peer in any cache (persistent or purgatory)
    fn update_peer_in_cache<F>(
        &mut self,
        mining_addr: Address,
        peer: PeerListItem,
        peer_address: PeerAddress,
        cache_getter: F,
        address_updater: fn(&mut Self, Address, PeerAddress),
        cache_name: &str,
    ) -> bool
    where
        F: FnOnce(&mut Self) -> Option<&mut PeerListItem>,
    {
        if let Some(existing_peer) = cache_getter(self) {
            let handshake_cooldown_expired =
                existing_peer.last_seen + HANDSHAKE_COOLDOWN < peer.last_seen;
            existing_peer.last_seen = peer.last_seen;
            existing_peer.reputation_score = peer.reputation_score;
            if existing_peer.address != peer_address {
                debug!("Peer address mismatch, updating to new address");
                address_updater(self, mining_addr, peer_address);
                if let Some(updated_peer) = self
                    .persistent_peers_cache
                    .get(&mining_addr)
                    .cloned()
                    .or_else(|| self.unstaked_peer_purgatory.peek(&mining_addr).cloned())
                {
                    self.emit_peer_event(PeerEvent::PeerUpdated {
                        mining_addr,
                        peer: updated_peer,
                    });
                }
                true
            } else if handshake_cooldown_expired {
                debug!("Peer address is the same, but the handshake cooldown has expired, so we need to re-handshake");
                address_updater(self, mining_addr, peer_address);
                if let Some(updated_peer) = self
                    .persistent_peers_cache
                    .get(&mining_addr)
                    .cloned()
                    .or_else(|| self.unstaked_peer_purgatory.peek(&mining_addr).cloned())
                {
                    self.emit_peer_event(PeerEvent::PeerUpdated {
                        mining_addr,
                        peer: updated_peer,
                    });
                }
                true
            } else {
                debug!("Peer address is the same, no update needed");
                false
            }
        } else {
            warn!(
                "Peer {:?} is not found in the {} cache, which shouldn't happen",
                mining_addr, cache_name
            );
            false
        }
    }

    /// Helper method to update a peer in the persistent cache
    fn update_peer_in_persistent_cache(
        &mut self,
        mining_addr: Address,
        peer: PeerListItem,
        peer_address: PeerAddress,
    ) -> bool {
        self.update_peer_in_cache(
            mining_addr,
            peer,
            peer_address,
            |slf| slf.persistent_peers_cache.get_mut(&mining_addr),
            Self::update_peer_address,
            "persistent",
        )
    }

    /// Helper method to update a peer in the purgatory cache
    fn update_peer_in_purgatory_cache(
        &mut self,
        mining_addr: Address,
        peer: PeerListItem,
        peer_address: PeerAddress,
    ) -> bool {
        self.update_peer_in_cache(
            mining_addr,
            peer,
            peer_address,
            |slf| slf.unstaked_peer_purgatory.get_mut(&mining_addr),
            Self::update_peer_address_purgatory,
            "purgatory",
        )
    }

    /// Helper method to add a peer to a cache with address mappings
    fn add_peer_to_cache(
        &mut self,
        mining_addr: Address,
        peer: PeerListItem,
        peer_address: PeerAddress,
        gossip_addr: SocketAddr,
        is_persistent: bool,
    ) {
        if is_persistent {
            self.persistent_peers_cache.insert(mining_addr, peer);
        } else {
            self.unstaked_peer_purgatory.put(mining_addr, peer);
        }

        self.gossip_addr_to_mining_addr_map
            .insert(gossip_addr.ip(), mining_addr);
        self.api_addr_to_mining_addr_map
            .insert(peer_address.api, mining_addr);
        self.known_peers_cache.insert(peer_address);
    }

    /// Helper method to update address mappings
    fn update_address_mappings(
        &mut self,
        mining_addr: Address,
        old_address: PeerAddress,
        new_address: PeerAddress,
    ) {
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
        let in_purgatory = self.unstaked_peer_purgatory.contains(&mining_addr);

        match (is_staked, in_persistent, in_purgatory) {
            // Case 1: Update peer in persistent cache (both staked and unstaked peers)
            (_, true, _) => {
                let peer_type = if is_staked { "staked" } else { "unstaked" };
                debug!(
                    "Updating {} peer {:?} in persistent cache",
                    peer_type, mining_addr
                );
                self.update_peer_in_persistent_cache(mining_addr, peer, peer_address)
            }

            // Case 2: is_staked is false and peer is in purgatory - update purgatory
            (false, false, true) => {
                debug!("Updating unstaked peer {:?} in purgatory", mining_addr);
                self.update_peer_in_purgatory_cache(mining_addr, peer, peer_address)
            }

            // Case 3: is_staked is true and peer is not in both caches - add to persistent cache
            (true, false, false) => {
                debug!("Adding staked peer {:?} to persistent cache", mining_addr);
                self.add_peer_to_cache(mining_addr, peer, peer_address, gossip_addr, true);
                debug!(
                    "Peer {:?} added to the peer list with address {:?}",
                    mining_addr, peer_address
                );
                true
            }

            // Case 4: is_staked is false, and peer is not in both caches - add to purgatory
            (false, false, false) => {
                debug!("Adding unstaked peer {:?} to purgatory", mining_addr);
                self.add_peer_to_cache(mining_addr, peer, peer_address, gossip_addr, false);
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
                if let Some(purgatory_peer) = self.unstaked_peer_purgatory.pop(&mining_addr) {
                    // Update the peer data with new information
                    let mut updated_peer = purgatory_peer;
                    let old_address = updated_peer.address;
                    updated_peer.last_seen = peer.last_seen;
                    updated_peer.reputation_score = peer.reputation_score;

                    if old_address != peer_address {
                        updated_peer.address = peer_address;
                        self.update_address_mappings(mining_addr, old_address, peer_address);
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
            self.update_address_mappings(mining_addr, old_address, new_address);
        }
    }

    fn update_peer_address(&mut self, mining_addr: Address, new_address: PeerAddress) {
        if let Some(peer) = self.persistent_peers_cache.get_mut(&mining_addr) {
            let old_address = peer.address;
            peer.address = new_address;
            self.update_address_mappings(mining_addr, old_address, new_address);
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
        let (peer_events, _rx) = broadcast::channel(100);
        let peer_list_data = PeerListDataInner {
            persistent_peers_cache: HashMap::new(),
            unstaked_peer_purgatory: LruCache::new(
                std::num::NonZeroUsize::new(UNSTAKED_PEER_PURGATORY_CAPACITY).unwrap(),
            ),
            known_peers_cache: HashSet::new(),
            gossip_addr_to_mining_addr_map: HashMap::new(),
            api_addr_to_mining_addr_map: HashMap::new(),
            trusted_peers_api_addresses: HashSet::new(),
            peer_whitelist: HashSet::new(),
            peer_network_service_sender: create_mock_sender(),
            peer_events,
        };
        PeerList(Arc::new(RwLock::new(peer_list_data)))
    }

    mod peer_list_scoring_tests {
        use super::*;
        use rstest::rstest;

        #[rstest]
        #[case(ScoreDecreaseReason::BogusData, 45)]
        #[case(ScoreDecreaseReason::Offline, 47)]
        #[case(ScoreDecreaseReason::SlowResponse, 49)]
        #[case(ScoreDecreaseReason::NoResponse, 47)]
        fn test_decrease_peer_score_persistent_cache(
            #[case] reason: ScoreDecreaseReason,
            #[case] expected_score: u16,
        ) {
            let peer_list = create_test_peer_list();
            let (addr, peer) = create_test_peer(1);

            peer_list.add_or_update_peer(addr, peer, true);

            peer_list.decrease_peer_score(&addr, reason);
            let updated_peer = peer_list.get_peer(&addr).unwrap();
            assert_eq!(updated_peer.reputation_score.get(), expected_score);
        }

        #[test]
        fn test_multiple_decreases_cumulative() {
            let peer_list = create_test_peer_list();
            let (addr, peer) = create_test_peer(1);

            peer_list.add_or_update_peer(addr, peer, true);

            peer_list.decrease_peer_score(&addr, ScoreDecreaseReason::BogusData);
            assert_eq!(
                peer_list.get_peer(&addr).unwrap().reputation_score.get(),
                45
            );

            peer_list.decrease_peer_score(&addr, ScoreDecreaseReason::Offline);
            assert_eq!(
                peer_list.get_peer(&addr).unwrap().reputation_score.get(),
                42
            );

            peer_list.decrease_peer_score(&addr, ScoreDecreaseReason::SlowResponse);
            assert_eq!(
                peer_list.get_peer(&addr).unwrap().reputation_score.get(),
                41
            );

            peer_list.decrease_peer_score(&addr, ScoreDecreaseReason::NoResponse);
            assert_eq!(
                peer_list.get_peer(&addr).unwrap().reputation_score.get(),
                38
            );
        }

        #[test]
        fn test_decrease_score_removes_inactive_from_known_peers() {
            let peer_list = create_test_peer_list();
            let (addr, mut peer) = create_test_peer(1);
            peer.reputation_score = PeerScore::new(25);

            peer_list.add_or_update_peer(addr, peer.clone(), true);
            assert!(peer_list.all_known_peers().contains(&peer.address));

            peer_list.decrease_peer_score(&addr, ScoreDecreaseReason::BogusData);
            let updated_peer = peer_list.get_peer(&addr);

            if let Some(p) = updated_peer {
                if !p.reputation_score.is_active() {
                    assert!(!peer_list.all_known_peers().contains(&peer.address));
                }
            }
        }

        #[test]
        fn test_decrease_score_unstaked_peer_removal() {
            let peer_list = create_test_peer_list();
            let (addr, peer) = create_test_peer(1);

            peer_list.add_or_update_peer(addr, peer.clone(), false);
            assert!(peer_list.get_peer(&addr).is_some());

            peer_list.decrease_peer_score(&addr, ScoreDecreaseReason::Offline);
            assert!(peer_list.get_peer(&addr).is_none());
            assert!(!peer_list.all_known_peers().contains(&peer.address));
        }

        #[rstest]
        #[case(ScoreIncreaseReason::ValidData, 53)]
        #[case(ScoreIncreaseReason::Online, 51)]
        #[case(ScoreIncreaseReason::DataRequest, 51)]
        #[case(ScoreIncreaseReason::TimelyResponse, 52)]
        fn test_increase_peer_score(
            #[case] reason: ScoreIncreaseReason,
            #[case] expected_score: u16,
        ) {
            let peer_list = create_test_peer_list();
            let (addr, peer) = create_test_peer(1);

            peer_list.add_or_update_peer(addr, peer, true);

            peer_list.increase_peer_score(&addr, reason);
            let updated_peer = peer_list.get_peer(&addr).unwrap();
            assert_eq!(updated_peer.reputation_score.get(), expected_score);
        }

        #[test]
        fn test_score_transitions_across_thresholds() {
            let peer_list = create_test_peer_list();
            let (addr, mut peer) = create_test_peer(1);

            peer.reputation_score = PeerScore::new(PeerScore::ACTIVE_THRESHOLD + 2);
            peer_list.add_or_update_peer(addr, peer, true);

            peer_list.decrease_peer_score(&addr, ScoreDecreaseReason::Offline);
            let updated_peer = peer_list.get_peer(&addr).unwrap();

            assert_eq!(updated_peer.reputation_score.get(), 19);
            assert!(!updated_peer.reputation_score.is_active());

            peer_list.increase_peer_score(&addr, ScoreIncreaseReason::Online);
            let final_peer = peer_list.get_peer(&addr).unwrap();
            assert_eq!(final_peer.reputation_score.get(), 20);
            assert!(final_peer.reputation_score.is_active());
        }

        #[test]
        fn test_unstaked_peer_operations() {
            let peer_list = create_test_peer_list();
            let (addr, mut peer) = create_test_peer(1);

            peer.reputation_score = PeerScore::new(50);
            peer_list.add_or_update_peer(addr, peer, false);

            let initial_score = peer_list.get_peer(&addr).unwrap().reputation_score.get();
            assert_eq!(initial_score, 50);

            peer_list.increase_peer_score(&addr, ScoreIncreaseReason::ValidData);
            let after_increase_score = peer_list.get_peer(&addr).unwrap().reputation_score.get();
            assert_eq!(after_increase_score, 53);

            peer_list.decrease_peer_score(&addr, ScoreDecreaseReason::BogusData);

            let final_peer = peer_list.get_peer(&addr);
            assert!(
                final_peer.is_none(),
                "Unstaked peer should be removed after any decrease operation"
            );
        }
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
        let gossip_peers_vec: Vec<_> = peer_list
            .all_peers()
            .iter()
            .map(|(a, p)| (*a, p.clone()))
            .collect();
        assert!(
            gossip_peers_vec
                .iter()
                .any(|(addr, _)| addr == &staked_addr),
            "Gossip peers should contain staked peer"
        );
        assert!(
            gossip_peers_vec
                .iter()
                .any(|(addr, _)| addr == &unstaked_addr),
            "Gossip peers should contain unstaked peer"
        );

        // Test 3: get_peer should return both staked and unstaked peers
        let staked_result = peer_list.get_peer(&staked_addr);
        let unstaked_result = peer_list.get_peer(&unstaked_addr);
        assert!(staked_result.is_some(), "get_peer should find staked peer");
        assert!(
            unstaked_result.is_some(),
            "get_peer should find an unstaked peer"
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
        let trusted_peers = peer_list.all_trusted_peers();
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

        // // Test 14: request_block_from_the_network should work (async method)
        // let block_hash = BlockHash::default();
        // let block_request_result = peer_list
        //     .request_block_from_the_network(block_hash, false)
        //     .await;
        // // This will likely fail due to mock sender, but the method should handle both peer types equally
        // assert!(
        //     block_request_result.is_err(),
        //     "request_block_from_the_network should work with mock sender (expected to fail)"
        // );
        //
        // // Test 15: request_payload_from_the_network should work (async method)
        // let payload_hash = B256::default();
        // let payload_request_result = peer_list
        //     .request_payload_from_the_network(payload_hash, false)
        //     .await;
        // // This will likely fail due to mock sender, but the method should handle both peer types equally
        // assert!(
        //     payload_request_result.is_err(),
        //     "request_payload_from_the_network should work with mock sender (expected to fail)"
        // );
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
                .iter()
                .map(|(_, v)| v)
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
                .iter()
                .map(|(_, v)| v)
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
