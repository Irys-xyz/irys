use crate::models::PeerEvent;
use alloy_core::primitives::B256;
use irys_database::reth_db::Database as _;
use irys_database::tables::PeerListItems;
use irys_database::walk_all;
use irys_types::{
    Config, DatabaseProvider, PeerAddress, PeerFilterMode, PeerListItem, PeerNetworkError,
    PeerNetworkSender,
};
use irys_types::{IrysAddress, IrysPeerId, ProtocolVersion};
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

#[derive(Clone, Debug)]
pub enum ScoreDecreaseReason {
    BogusData(String),
    Offline(String),
    NetworkError(String),
    SlowResponse,
}

#[derive(Clone, Debug, Copy)]
pub enum ScoreIncreaseReason {
    Online,
    DataRequest,
    TimelyResponse,
}

#[derive(Debug, Clone)]
pub struct PeerListDataInner {
    /// Primary index by peer_id
    persistent_peers_cache: HashMap<IrysPeerId, PeerListItem>,
    unstaked_peer_purgatory: LruCache<IrysPeerId, PeerListItem>,

    /// Mapping to find peer_id from miner_address (for v1 compatibility)
    miner_addr_to_peer_id_map: HashMap<IrysAddress, IrysPeerId>,
    /// Reverse mapping to find miner_address from peer_id (for database storage and events)
    peer_id_to_miner_addr_map: HashMap<IrysPeerId, IrysAddress>,

    /// IP-based lookups for PeerId
    gossip_addr_to_peer_id_map: HashMap<IpAddr, IrysPeerId>,
    api_addr_to_peer_id_map: HashMap<SocketAddr, IrysPeerId>,

    known_peers_cache: HashSet<PeerAddress>,
    trusted_peers_api_to_gossip_addresses: HashMap<SocketAddr, SocketAddr>,
    /// Whitelist of allowed peer API addresses based on peer filter mode
    peer_whitelist: HashSet<SocketAddr>,
    peer_network_service_sender: PeerNetworkSender,
    /// Broadcast channel for peer lifecycle/activity events
    peer_events: broadcast::Sender<PeerEvent>,
    config: Config,
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
        std::collections::hash_map::Iter<'a, IrysPeerId, PeerListItem>,
        lru::Iter<'a, IrysPeerId, PeerListItem>,
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

        let peers: Vec<PeerListItem> = compact_peers
            .into_iter()
            .map(|(peer_id, compact_item)| {
                // Convert from PeerListItemInner (database format) to PeerListItem (application format)
                let inner: irys_types::PeerListItemInner = compact_item.into();
                PeerListItem::from_inner(inner, peer_id)
            })
            .collect();
        let inner = PeerListDataInner::new(peers, peer_service_sender, config, peer_events)?;
        Ok(Self(Arc::new(RwLock::new(inner))))
    }

    pub fn test_mock() -> Result<Self, PeerNetworkError> {
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let inner = PeerListDataInner::new(
            vec![],
            PeerNetworkSender::new(sender),
            &Config::new_with_random_peer_id(irys_types::NodeConfig::testing()),
            broadcast::channel::<PeerEvent>(100).0,
        )?;
        Ok(Self(Arc::new(RwLock::new(inner))))
    }

    pub fn from_peers(
        peers: Vec<PeerListItem>,
        peer_network: PeerNetworkSender,
        config: &Config,
        peer_events: broadcast::Sender<PeerEvent>,
    ) -> Result<Self, PeerNetworkError> {
        let inner = PeerListDataInner::new(peers, peer_network, config, peer_events)?;
        Ok(Self(Arc::new(RwLock::new(inner))))
    }

    pub fn add_or_update_peer(&self, peer: PeerListItem, is_staked: bool) {
        let mut inner = self.0.write().expect("PeerListDataInner lock poisoned");
        inner.add_or_update_peer(peer, is_staked);
    }

    /// Get a peer by their peer_id (V2)
    pub fn peer_by_id(&self, peer_id: &IrysPeerId) -> Option<PeerListItem> {
        let inner = self.read();
        inner
            .persistent_peers_cache
            .get(peer_id)
            .or_else(|| inner.unstaked_peer_purgatory.peek(peer_id))
            .cloned()
    }

    /// Increase peer score by mining address (looks up peer_id via mapping)
    pub fn increase_peer_score(&self, mining_addr: &IrysAddress, reason: ScoreIncreaseReason) {
        let mut inner = self.0.write().expect("PeerListDataInner lock poisoned");
        if let Some(&peer_id) = inner.miner_addr_to_peer_id_map.get(mining_addr) {
            inner.increase_score(&peer_id, reason);
        }
    }

    /// Increase peer score by peer_id directly (for iteration-based callers)
    pub fn increase_peer_score_by_peer_id(
        &self,
        peer_id: &IrysPeerId,
        reason: ScoreIncreaseReason,
    ) {
        let mut inner = self.0.write().expect("PeerListDataInner lock poisoned");
        inner.increase_score(peer_id, reason);
    }

    /// Decrease peer score by mining address (looks up peer_id via mapping)
    pub fn decrease_peer_score(&self, mining_addr: &IrysAddress, reason: ScoreDecreaseReason) {
        let mut inner = self.0.write().expect("PeerListDataInner lock poisoned");
        if let Some(&peer_id) = inner.miner_addr_to_peer_id_map.get(mining_addr) {
            inner.decrease_peer_score(&peer_id, reason);
        }
    }

    /// Decrease peer score by peer_id directly (for iteration-based callers)
    pub fn decrease_peer_score_by_peer_id(
        &self,
        peer_id: &IrysPeerId,
        reason: ScoreDecreaseReason,
    ) {
        let mut inner = self.0.write().expect("PeerListDataInner lock poisoned");
        inner.decrease_peer_score(peer_id, reason);
    }

    pub fn set_is_online(&self, mining_addr: &IrysAddress, is_online: bool) {
        let mut inner = self.0.write().expect("PeerListDataInner lock poisoned");

        // Look up peer_id from mining address
        let peer_id = match inner.miner_addr_to_peer_id_map.get(mining_addr) {
            Some(id) => *id,
            None => return, // Peer not found
        };

        let mut became_active: Option<irys_types::PeerListItem> = None;
        let mut became_inactive: Option<irys_types::PeerListItem> = None;
        if let Some(peer) = inner.persistent_peers_cache.get_mut(&peer_id) {
            let was_active = peer.reputation_score.is_active() && peer.is_online;
            peer.is_online = is_online;
            let now_active = peer.reputation_score.is_active() && peer.is_online;
            if !was_active && now_active {
                became_active = Some(peer.clone());
            }
            if was_active && !now_active {
                became_inactive = Some(peer.clone());
            }
        } else if let Some(peer) = inner.unstaked_peer_purgatory.get_mut(&peer_id) {
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

    /// Set peer's online status by peer_id directly (for iteration-based callers)
    pub fn set_is_online_by_peer_id(&self, peer_id: &IrysPeerId, is_online: bool) {
        let mut inner = self.0.write().expect("PeerListDataInner lock poisoned");

        let mut became_active: Option<irys_types::PeerListItem> = None;
        let mut became_inactive: Option<irys_types::PeerListItem> = None;
        if let Some(peer) = inner.persistent_peers_cache.get_mut(peer_id) {
            let was_active = peer.reputation_score.is_active() && peer.is_online;
            peer.is_online = is_online;
            let now_active = peer.reputation_score.is_active() && peer.is_online;
            if !was_active && now_active {
                became_active = Some(peer.clone());
            }
            if was_active && !now_active {
                became_inactive = Some(peer.clone());
            }
        } else if let Some(peer) = inner.unstaked_peer_purgatory.get_mut(peer_id) {
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
            let mining_addr = peer.mining_address;
            inner.emit_peer_event(PeerEvent::BecameActive { mining_addr, peer });
        }
        if let Some(peer) = became_inactive {
            let mining_addr = peer.mining_address;
            inner.emit_peer_event(PeerEvent::BecameInactive { mining_addr, peer });
        }
    }

    /// Get a peer from any cache (persistent or purgatory) by peer_id
    pub fn get_peer(&self, peer_id: &IrysPeerId) -> Option<PeerListItem> {
        let inner = self.read();
        inner
            .persistent_peers_cache
            .get(peer_id)
            .or_else(|| inner.unstaked_peer_purgatory.peek(peer_id))
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
    pub fn persistable_peers(&self) -> HashMap<IrysPeerId, PeerListItem> {
        let guard = self.read();
        guard.persistent_peers_cache.clone()
    }

    /// Get persistable peers with their mining addresses (for database storage)
    /// Returns tuples of (IrysPeerId, PeerListItem) from the persistent_peers_cache.
    pub fn persistable_peers_with_mining_addr(&self) -> Vec<(IrysPeerId, PeerListItem)> {
        let guard = self.read();
        guard
            .persistent_peers_cache
            .iter()
            .map(|(peer_id, peer)| (*peer_id, peer.clone()))
            .collect()
    }

    pub fn temporary_peers(&self) -> LruCache<IrysPeerId, PeerListItem> {
        self.read().unstaked_peer_purgatory.clone()
    }

    /// Subscribe to peer lifecycle/activity events.
    pub fn subscribe_to_peer_events(&self) -> broadcast::Receiver<PeerEvent> {
        let guard = self.read();
        guard.peer_events.subscribe()
    }

    pub fn contains_api_address(&self, api_address: &SocketAddr) -> bool {
        self.read()
            .api_addr_to_peer_id_map
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

    pub fn all_trusted_peers(&self) -> Vec<(IrysPeerId, PeerListItem)> {
        let guard = self.read();

        let mut peers: Vec<(IrysPeerId, PeerListItem)> = Vec::new();

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

        peers.retain(|(_peer_id, peer)| {
            guard
                .trusted_peers_api_to_gossip_addresses
                .contains_key(&peer.address.api)
        });

        peers.sort_by_key(|(_address, peer)| peer.reputation_score.get());
        peers.reverse();

        peers
    }

    pub fn online_trusted_peers(&self) -> Vec<(IrysPeerId, PeerListItem)> {
        let mut trusted_peers = self.all_trusted_peers();
        trusted_peers.retain(|(_peer_id, peer)| peer.is_online);
        trusted_peers
    }

    pub fn trusted_peer_api_to_gossip_addresses(&self) -> HashMap<SocketAddr, SocketAddr> {
        self.read().trusted_peers_api_to_gossip_addresses.clone()
    }

    pub fn top_active_peers(
        &self,
        limit: Option<usize>,
        exclude_peers: Option<HashSet<IrysPeerId>>,
    ) -> Vec<(IrysPeerId, PeerListItem)> {
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
        let filtered_peers = persistent_peers
            .chain(purgatory_peers)
            .filter(|(peer_id, peer)| {
                let exclude = exclude_peers
                    .as_ref()
                    .is_some_and(|excluded| excluded.contains(peer_id));
                !exclude && peer.reputation_score.is_active() && peer.is_online
            });

        let mut peers: Vec<(IrysPeerId, PeerListItem)> = filtered_peers.collect();

        peers.sort_by_key(|(_address, peer)| peer.reputation_score.get());
        peers.reverse();

        if let Some(truncate) = limit {
            peers.truncate(truncate);
        }

        peers
    }

    pub fn all_peers_sorted_by_score(&self) -> Vec<(IrysPeerId, PeerListItem)> {
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
        let mut peers: Vec<(IrysPeerId, PeerListItem)> = all_peers.collect();

        peers.sort_by_key(|(_address, peer)| peer.reputation_score.get());
        peers.reverse();

        peers
    }

    pub fn inactive_peers(&self) -> Vec<(IrysPeerId, PeerListItem)> {
        let guard = self.read();
        let mut inactive = Vec::new();

        // Add inactive peers from main cache
        inactive.extend(
            guard
                .persistent_peers_cache
                .iter()
                .filter(|(_peer_id, peer)| !peer.reputation_score.is_active())
                .map(|(peer_id, peer)| (*peer_id, peer.clone())),
        );

        // Add inactive peers from purgatory
        inactive.extend(
            guard
                .unstaked_peer_purgatory
                .iter()
                .filter(|(_peer_id, peer)| !peer.reputation_score.is_active())
                .map(|(peer_id, peer)| (*peer_id, peer.clone())),
        );

        inactive
    }

    pub fn peer_by_gossip_address(&self, address: SocketAddr) -> Option<PeerListItem> {
        let binding = self.read();
        let peer_id = binding
            .gossip_addr_to_peer_id_map
            .get(&address.ip())
            .copied()?;
        binding
            .persistent_peers_cache
            .get(&peer_id)
            .or_else(|| binding.unstaked_peer_purgatory.peek(&peer_id))
            .cloned()
    }

    pub fn peer_by_mining_address(&self, mining_address: &IrysAddress) -> Option<PeerListItem> {
        let binding = self.read();
        // Use the miner_address -> peer_id mapping
        let peer_id = binding.miner_addr_to_peer_id_map.get(mining_address)?;
        binding
            .persistent_peers_cache
            .get(peer_id)
            .or_else(|| binding.unstaked_peer_purgatory.peek(peer_id))
            .cloned()
    }

    /// Get mining address for a peer by their peer_id (reverse lookup)
    pub fn mining_address_by_peer_id(&self, peer_id: &IrysPeerId) -> Option<IrysAddress> {
        let binding = self.read();
        binding.peer_id_to_miner_addr_map.get(peer_id).copied()
    }

    pub fn peer_by_api_address(&self, address: SocketAddr) -> Option<PeerListItem> {
        let binding = self.read();
        let peer_id = binding.api_addr_to_peer_id_map.get(&address).copied()?;
        binding
            .persistent_peers_cache
            .get(&peer_id)
            .or_else(|| binding.unstaked_peer_purgatory.peek(&peer_id))
            .cloned()
    }

    pub fn get_trusted_peer_gossip_address(&self, api_address: SocketAddr) -> Option<SocketAddr> {
        let binding = self.read();
        binding
            .config
            .node_config
            .trusted_peers
            .iter()
            .find(|p| p.api == api_address)
            .map(|p| p.gossip)
    }

    pub fn is_a_trusted_peer(&self, miner_address: IrysAddress, source_ip: IpAddr) -> bool {
        let binding = self.read();

        // Look up peer_id from miner address, then check caches
        let peer_id = match binding.miner_addr_to_peer_id_map.get(&miner_address) {
            Some(id) => *id,
            None => return false,
        };

        // Check both persistent cache and purgatory
        let peer = binding
            .persistent_peers_cache
            .get(&peer_id)
            .or_else(|| binding.unstaked_peer_purgatory.peek(&peer_id));

        let Some(peer) = peer else {
            return false;
        };
        let peer_api_ip = peer.address.api.ip();
        let peer_gossip_ip = peer.address.gossip.ip();

        let ip_matches_cached_ip = source_ip == peer_gossip_ip;
        let ip_is_in_a_trusted_list = binding
            .trusted_peers_api_to_gossip_addresses
            .iter()
            .any(|(api, _gossip)| api.ip() == peer_api_ip);

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
        guard
            .trusted_peers_api_to_gossip_addresses
            .contains_key(api_address)
    }

    /// Initiate a handshake with a peer by its API address. If force is set to true, the networking
    /// service will attempt to handshake even if the previous handshake was successful.
    pub fn initiate_handshake(
        &self,
        api_address: SocketAddr,
        gossip_address: SocketAddr,
        force: bool,
    ) {
        let guard = self.read();
        guard.initiate_handshake(api_address, gossip_address, force);
    }
}

impl PeerListDataInner {
    pub fn new(
        peers: Vec<PeerListItem>,
        peer_network_sender: PeerNetworkSender,
        config: &Config,
        peer_events: broadcast::Sender<PeerEvent>,
    ) -> Result<Self, PeerNetworkError> {
        let trusted_peers_api_to_gossip_addresses: HashMap<SocketAddr, SocketAddr> = config
            .node_config
            .trusted_peers
            .iter()
            .map(|p| (p.api, p.gossip))
            .collect();

        // Initialize whitelist based on peer filter mode
        let peer_api_ip_whitelist = match config.node_config.peer_filter_mode {
            PeerFilterMode::Unrestricted => HashSet::new(), // No restrictions
            PeerFilterMode::TrustedOnly | PeerFilterMode::TrustedAndHandshake => {
                let mut ip_whitelist: HashSet<SocketAddr> = trusted_peers_api_to_gossip_addresses
                    .keys()
                    .copied()
                    .collect();
                ip_whitelist.extend(config.node_config.initial_whitelist.clone());
                ip_whitelist
            }
        };

        let mut peer_list = Self {
            persistent_peers_cache: HashMap::new(),
            unstaked_peer_purgatory: LruCache::new(
                std::num::NonZeroUsize::new(UNSTAKED_PEER_PURGATORY_CAPACITY)
                    .expect("Expected to be able to create an LRU cache"),
            ),
            miner_addr_to_peer_id_map: HashMap::new(),
            peer_id_to_miner_addr_map: HashMap::new(),
            gossip_addr_to_peer_id_map: HashMap::new(),
            api_addr_to_peer_id_map: HashMap::new(),
            known_peers_cache: HashSet::new(),
            trusted_peers_api_to_gossip_addresses,
            peer_whitelist: peer_api_ip_whitelist,
            peer_network_service_sender: peer_network_sender,
            peer_events,
            config: config.clone(),
        };

        for mut peer_list_item in peers {
            // If scoring is disabled, set all peer scores to max
            if !config.node_config.p2p_gossip.enable_scoring {
                peer_list_item.reputation_score.set_to_max();
            }

            // At this point, peer_list_item should already have peer_id and mining_address set
            // (either from DB via from_inner() or from creation in application code)
            let peer_id = peer_list_item.peer_id;
            let mining_address = peer_list_item.mining_address;
            let address = peer_list_item.address;

            // Build all the index maps
            peer_list
                .gossip_addr_to_peer_id_map
                .insert(peer_list_item.address.gossip.ip(), peer_id);
            peer_list
                .api_addr_to_peer_id_map
                .insert(address.api, peer_id);
            peer_list
                .miner_addr_to_peer_id_map
                .insert(mining_address, peer_id);
            peer_list
                .peer_id_to_miner_addr_map
                .insert(peer_id, mining_address);
            peer_list
                .persistent_peers_cache
                .insert(peer_id, peer_list_item);
            peer_list.known_peers_cache.insert(address);
        }

        Ok(peer_list)
    }

    /// Helper to emit a peer event to the event bus
    fn emit_peer_event(&self, event: PeerEvent) {
        if let Err(e) = self.peer_events.send(event) {
            tracing::debug!(
                custom.error = ?e,
                "Failed to broadcast peer event"
            );
        }
    }

    pub fn add_or_update_peer(&mut self, mut peer: PeerListItem, is_staked: bool) {
        // If scoring is disabled, set all peer scores to max, the same as in the constructor
        if !self.config.node_config.p2p_gossip.enable_scoring {
            peer.reputation_score.set_to_max();
        }

        // At this point, peer should already have peer_id and mining_address set
        let peer_id = peer.peer_id;
        let mining_addr = peer.mining_address;

        // Determine previous active state (if existed)
        let was_active = self
            .persistent_peers_cache
            .get(&peer_id)
            .map(|p| p.reputation_score.is_active() && p.is_online)
            .or_else(|| {
                self.unstaked_peer_purgatory
                    .peek(&peer_id)
                    .map(|p| p.reputation_score.is_active() && p.is_online)
            })
            .unwrap_or(false);

        let is_updated = self.add_or_update_peer_internal(peer.clone(), is_staked);

        // Determine a new active state
        let now_peer = self
            .persistent_peers_cache
            .get(&peer_id)
            .cloned()
            .or_else(|| self.unstaked_peer_purgatory.peek(&peer_id).cloned());

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
                .get(&peer_id)
                .cloned()
                .or_else(|| self.unstaked_peer_purgatory.peek(&peer_id).cloned())
            {
                self.emit_peer_event(PeerEvent::PeerUpdated {
                    mining_addr,
                    peer: updated_peer,
                });
            }
        }
    }

    pub fn initiate_handshake(
        &self,
        api_address: SocketAddr,
        gossip_address: SocketAddr,
        force: bool,
    ) {
        if let Err(send_error) =
            self.peer_network_service_sender
                .initiate_handshake(api_address, gossip_address, force)
        {
            error!("Failed to send a force announce message: {:?}", send_error);
        }
    }

    pub fn increase_score(&mut self, peer_id: &IrysPeerId, reason: ScoreIncreaseReason) {
        if !self.config.node_config.p2p_gossip.enable_scoring {
            return;
        }

        if let Some(peer) = self.persistent_peers_cache.get_mut(peer_id) {
            let was_active = peer.reputation_score.is_active() && peer.is_online;
            match reason {
                ScoreIncreaseReason::Online => {
                    peer.reputation_score.increase_online();
                }
                ScoreIncreaseReason::DataRequest => {
                    peer.reputation_score.increase_online();
                }
                ScoreIncreaseReason::TimelyResponse => {
                    peer.reputation_score.increase_online();
                }
            }
            let now_active = peer.reputation_score.is_active() && peer.is_online;
            let to_send = (!was_active && now_active).then(|| peer.clone());
            let _ = peer;
            if let Some(peer) = to_send {
                let mining_addr = peer.mining_address;
                self.emit_peer_event(PeerEvent::BecameActive { mining_addr, peer });
            }
        } else if let Some(peer) = self.unstaked_peer_purgatory.get_mut(peer_id) {
            // Update score in purgatory
            let was_active = peer.reputation_score.is_active() && peer.is_online;
            match reason {
                ScoreIncreaseReason::Online => {
                    peer.reputation_score.increase_online();
                }
                ScoreIncreaseReason::DataRequest => {
                    peer.reputation_score.increase_online();
                }
                ScoreIncreaseReason::TimelyResponse => {
                    peer.reputation_score.increase_online();
                }
            }

            if peer.reputation_score.is_persistable() {
                debug!(
                    "Unstaked peer {:?} has reached persistence threshold, promoting to persistent cache",
                    peer_id
                );
                // Move from purgatory to persistent cache
                let peer_clone = peer.clone();
                self.unstaked_peer_purgatory.pop(peer_id);
                self.persistent_peers_cache
                    .insert(*peer_id, peer_clone.clone());
                self.known_peers_cache.insert(peer_clone.address);
            }

            // Check post-state (may be in persistent now)
            let now_peer = self
                .persistent_peers_cache
                .get(peer_id)
                .cloned()
                .or_else(|| self.unstaked_peer_purgatory.peek(peer_id).cloned());
            if let Some(now_peer) = now_peer {
                let now_active = now_peer.reputation_score.is_active() && now_peer.is_online;
                if !was_active && now_active {
                    let mining_addr = now_peer.mining_address;
                    self.emit_peer_event(PeerEvent::BecameActive {
                        mining_addr,
                        peer: now_peer,
                    });
                }
            }
        }
    }

    pub fn decrease_peer_score(&mut self, peer_id: &IrysPeerId, reason: ScoreDecreaseReason) {
        if !self.config.node_config.p2p_gossip.enable_scoring {
            warn!(
                "Would've decreased score for peer {:?}, reason: {:?}",
                peer_id, reason
            );
            return;
        }
        warn!(
            "Decreasing score for peer {:?}, reason: {:?}",
            peer_id, reason
        );

        // Check the persistent cache first
        if let Some(peer_item) = self.persistent_peers_cache.get_mut(peer_id) {
            let was_active = peer_item.reputation_score.is_active() && peer_item.is_online;
            match reason {
                ScoreDecreaseReason::BogusData(message) => {
                    peer_item.reputation_score.decrease_bogus_data(&message);
                }
                ScoreDecreaseReason::Offline(message) => {
                    peer_item.reputation_score.decrease_offline(&message);
                }
                ScoreDecreaseReason::SlowResponse => {
                    peer_item.reputation_score.decrease_slow();
                }
                ScoreDecreaseReason::NetworkError(message) => {
                    peer_item.reputation_score.decrease_offline(&message);
                }
            }

            // Don't propagate inactive peers
            if !peer_item.reputation_score.is_active() {
                warn!(
                    "Peer's {:?} score dropped below an active threshold, removing from the persistent cache",
                    peer_id
                );
                self.known_peers_cache.remove(&peer_item.address);
            }
            let now_active = peer_item.reputation_score.is_active() && peer_item.is_online;
            if was_active && !now_active {
                warn!("Peer {:?} became inactive", peer_id);
                let peer_clone = peer_item.clone();
                let mining_addr = peer_item.mining_address;
                self.emit_peer_event(PeerEvent::BecameInactive {
                    mining_addr,
                    peer: peer_clone,
                });
            }
        } else if let Some(peer) = self.unstaked_peer_purgatory.pop(peer_id) {
            // Get mining_addr from the peer
            let mining_addr = peer.mining_address;
            self.gossip_addr_to_peer_id_map
                .remove(&peer.address.gossip.ip());
            self.api_addr_to_peer_id_map.remove(&peer.address.api);
            self.miner_addr_to_peer_id_map.remove(&mining_addr);
            self.peer_id_to_miner_addr_map.remove(peer_id);
            self.known_peers_cache.remove(&peer.address);
            debug!("Removed unstaked peer {:?} from all caches", peer_id);
            self.emit_peer_event(PeerEvent::PeerRemoved { mining_addr, peer });
        }
    }

    /// Helper method to update a peer in any cache (persistent or purgatory)
    fn update_peer_in_cache<F>(
        &mut self,
        mining_addr: IrysAddress,
        peer_id: IrysPeerId,
        peer: PeerListItem,
        peer_address: PeerAddress,
        cache_getter: F,
        address_updater: fn(&mut Self, IrysAddress, IrysPeerId, PeerAddress, ProtocolVersion),
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
            existing_peer.protocol_version = peer.protocol_version;
            if existing_peer.address != peer_address {
                debug!(
                    "Peer address mismatch, updating from {:?} to {:?}",
                    existing_peer.address, peer_address
                );
                address_updater(
                    self,
                    mining_addr,
                    peer_id,
                    peer_address,
                    peer.protocol_version,
                );
                if let Some(updated_peer) = self
                    .persistent_peers_cache
                    .get(&peer_id)
                    .cloned()
                    .or_else(|| self.unstaked_peer_purgatory.peek(&peer_id).cloned())
                {
                    self.emit_peer_event(PeerEvent::PeerUpdated {
                        mining_addr,
                        peer: updated_peer,
                    });
                }
                true
            } else if handshake_cooldown_expired {
                debug!(
                    "Peer address {} is the same, but the handshake cooldown has expired, so we need to re-handshake",
                    peer_address.gossip.ip()
                );
                address_updater(
                    self,
                    mining_addr,
                    peer_id,
                    peer_address,
                    peer.protocol_version,
                );
                if let Some(updated_peer) = self
                    .persistent_peers_cache
                    .get(&peer_id)
                    .cloned()
                    .or_else(|| self.unstaked_peer_purgatory.peek(&peer_id).cloned())
                {
                    self.emit_peer_event(PeerEvent::PeerUpdated {
                        mining_addr,
                        peer: updated_peer,
                    });
                }
                true
            } else {
                debug!(
                    "Peer {:?} ({}) address is the same, no update needed",
                    mining_addr,
                    peer_address.gossip.ip()
                );
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
        mining_addr: IrysAddress,
        peer_id: IrysPeerId,
        peer: PeerListItem,
        peer_address: PeerAddress,
    ) -> bool {
        self.update_peer_in_cache(
            mining_addr,
            peer_id,
            peer,
            peer_address,
            |slf| slf.persistent_peers_cache.get_mut(&peer_id),
            Self::update_peer_address,
            "persistent",
        )
    }

    /// Helper method to update a peer in the purgatory cache
    fn update_peer_in_purgatory_cache(
        &mut self,
        mining_addr: IrysAddress,
        peer_id: IrysPeerId,
        peer: PeerListItem,
        peer_address: PeerAddress,
    ) -> bool {
        self.update_peer_in_cache(
            mining_addr,
            peer_id,
            peer,
            peer_address,
            |slf| slf.unstaked_peer_purgatory.get_mut(&peer_id),
            Self::update_peer_address_purgatory,
            "purgatory",
        )
    }

    /// Helper method to add a peer to a cache with address mappings
    fn add_peer_to_cache(
        &mut self,
        mining_addr: IrysAddress,
        peer_id: IrysPeerId,
        peer: PeerListItem,
        peer_address: PeerAddress,
        gossip_addr: SocketAddr,
        is_persistent: bool,
    ) {
        if is_persistent {
            self.persistent_peers_cache.insert(peer_id, peer);
        } else {
            self.unstaked_peer_purgatory.put(peer_id, peer);
        }

        self.gossip_addr_to_peer_id_map
            .insert(gossip_addr.ip(), peer_id);
        self.api_addr_to_peer_id_map
            .insert(peer_address.api, peer_id);
        self.miner_addr_to_peer_id_map.insert(mining_addr, peer_id);
        self.peer_id_to_miner_addr_map.insert(peer_id, mining_addr);
        self.known_peers_cache.insert(peer_address);
    }

    /// Helper method to update address mappings
    fn update_address_mappings(
        &mut self,
        _mining_addr: IrysAddress,
        peer_id: IrysPeerId,
        old_address: PeerAddress,
        new_address: PeerAddress,
    ) {
        self.gossip_addr_to_peer_id_map
            .remove(&old_address.gossip.ip());
        self.gossip_addr_to_peer_id_map
            .insert(new_address.gossip.ip(), peer_id);
        self.api_addr_to_peer_id_map.remove(&old_address.api);
        self.api_addr_to_peer_id_map
            .insert(new_address.api, peer_id);
        // miner_addr_to_peer_id_map doesn't change (unless miner address changes, which shouldn't happen)
        self.known_peers_cache.remove(&old_address);
        self.known_peers_cache.insert(new_address);
    }

    /// Add or update a peer in the appropriate cache based on staking status and current location.
    /// Returns true if the peer was added or needs re-handshaking, false if no update needed.
    fn add_or_update_peer_internal(&mut self, peer: PeerListItem, is_staked: bool) -> bool {
        let gossip_addr = peer.address.gossip;
        let peer_address = peer.address;

        // At this point, peer should already have peer_id and mining_address set
        let peer_id = peer.peer_id;
        let mining_addr = peer.mining_address;

        // Check if peer exists by peer_id OR by mining_address (for updates where peer_id changed)
        let existing_peer_id = self.miner_addr_to_peer_id_map.get(&mining_addr).copied();
        let lookup_peer_id = existing_peer_id.unwrap_or(peer_id);

        // Check if peer exists in persistent cache
        let in_persistent = self.persistent_peers_cache.contains_key(&lookup_peer_id);
        // Check if peer exists in purgatory
        let in_purgatory = self.unstaked_peer_purgatory.contains(&lookup_peer_id);

        // If we found an existing peer with a different peer_id, we need to update the mapping
        if let Some(old_peer_id) = existing_peer_id
            && old_peer_id != peer_id
        {
            // Peer ID changed - need to migrate the entry
            debug!(
                "Peer {:?} changed peer_id from {:?} to {:?}, migrating entry",
                mining_addr, old_peer_id, peer_id
            );

            // Remove old entry and update with new peer_id
            if let Some(old_peer) = self.persistent_peers_cache.remove(&old_peer_id) {
                self.persistent_peers_cache.insert(peer_id, old_peer);
            } else if let Some(old_peer) = self.unstaked_peer_purgatory.pop(&old_peer_id) {
                self.unstaked_peer_purgatory.put(peer_id, old_peer);
            }

            // Update all mappings to use new peer_id
            self.miner_addr_to_peer_id_map.insert(mining_addr, peer_id);
            self.peer_id_to_miner_addr_map.remove(&old_peer_id);
            self.peer_id_to_miner_addr_map.insert(peer_id, mining_addr);
            if let Some(peer_item) = self
                .persistent_peers_cache
                .get(&peer_id)
                .or_else(|| self.unstaked_peer_purgatory.peek(&peer_id))
            {
                self.gossip_addr_to_peer_id_map
                    .insert(peer_item.address.gossip.ip(), peer_id);
                self.api_addr_to_peer_id_map
                    .insert(peer_item.address.api, peer_id);
            }
        }

        match (is_staked, in_persistent, in_purgatory) {
            // Case 1: Update peer in persistent cache (both staked and unstaked peers)
            (_, true, _) => {
                let peer_type = if is_staked { "staked" } else { "unstaked" };
                debug!(
                    "Updating {} peer {:?} ({}) in persistent cache",
                    peer_type,
                    mining_addr,
                    peer_address.gossip.ip()
                );
                self.update_peer_in_persistent_cache(mining_addr, peer_id, peer, peer_address)
            }

            // Case 2: is_staked is false and peer is in purgatory - update purgatory
            (false, false, true) => {
                debug!(
                    "Updating unstaked peer {:?} ({}) in purgatory",
                    mining_addr,
                    peer_address.gossip.ip()
                );
                self.update_peer_in_purgatory_cache(mining_addr, peer_id, peer, peer_address)
            }

            // Case 3: is_staked is true and peer is not in both caches - add to persistent cache
            (true, false, false) => {
                debug!("Adding staked peer {:?} to persistent cache", mining_addr);
                self.add_peer_to_cache(mining_addr, peer_id, peer, peer_address, gossip_addr, true);
                debug!(
                    "Peer {:?} added to the peer list with address {:?}",
                    mining_addr, peer_address
                );
                true
            }

            // Case 4: is_staked is false, and peer is not in both caches - add to purgatory
            (false, false, false) => {
                debug!("Adding unstaked peer {:?} to purgatory", mining_addr);
                self.add_peer_to_cache(
                    mining_addr,
                    peer_id,
                    peer,
                    peer_address,
                    gossip_addr,
                    false,
                );
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
                if let Some(purgatory_peer) = self.unstaked_peer_purgatory.pop(&peer_id) {
                    // Update the peer data with new information
                    let mut updated_peer = purgatory_peer;
                    let old_address = updated_peer.address;
                    updated_peer.last_seen = peer.last_seen;
                    updated_peer.reputation_score = peer.reputation_score;
                    updated_peer.protocol_version = peer.protocol_version;

                    if old_address != peer_address {
                        updated_peer.address = peer_address;
                        self.update_address_mappings(
                            mining_addr,
                            peer_id,
                            old_address,
                            peer_address,
                        );
                    }

                    self.persistent_peers_cache.insert(peer_id, updated_peer);
                    self.known_peers_cache.insert(peer_address);
                    debug!(
                        "Peer {:?} ({}) moved from purgatory to persistent cache",
                        mining_addr,
                        peer_address.gossip.ip()
                    );
                    true
                } else {
                    warn!(
                        "Peer {:?} ({}) is not found in purgatory cache, which shouldn't happen",
                        mining_addr,
                        peer_address.gossip.ip()
                    );
                    false
                }
            }
        }
    }

    fn update_peer_address_purgatory(
        &mut self,
        mining_addr: IrysAddress,
        peer_id: IrysPeerId,
        new_address: PeerAddress,
        new_protocol_version: ProtocolVersion,
    ) {
        if let Some(peer) = self.unstaked_peer_purgatory.get_mut(&peer_id) {
            let old_address = peer.address;
            peer.address = new_address;
            peer.protocol_version = new_protocol_version;
            self.update_address_mappings(mining_addr, peer_id, old_address, new_address);
        }
    }

    fn update_peer_address(
        &mut self,
        mining_addr: IrysAddress,
        peer_id: IrysPeerId,
        new_address: PeerAddress,
        new_protocol_version: ProtocolVersion,
    ) {
        if let Some(peer) = self.persistent_peers_cache.get_mut(&peer_id) {
            let old_address = peer.address;
            peer.address = new_address;
            peer.protocol_version = new_protocol_version;
            self.update_address_mappings(mining_addr, peer_id, old_address, new_address);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::{NodeConfig, PeerAddress, PeerListItem, PeerScore, RethPeerInfo};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::sync::mpsc;

    /// Creates a test peer with separate mining_addr and peer_id.
    /// Returns (mining_addr, peer_id, peer) tuple.
    fn create_test_peer(id: u8) -> (IrysAddress, IrysPeerId, PeerListItem) {
        let mining_addr = IrysAddress::from([id; 20]);
        // Generate a different peer_id to ensure we don't rely on peer_id == mining_addr
        let peer_id = IrysPeerId::from([id.wrapping_add(100); 20]);
        let peer_address = PeerAddress {
            gossip: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, id)), 8000 + id as u16),
            api: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, id)), 9000 + id as u16),
            execution: RethPeerInfo::default(),
        };
        let peer = PeerListItem {
            peer_id,
            mining_address: mining_addr,
            address: peer_address,
            reputation_score: PeerScore::new(PeerScore::INITIAL),
            response_time: 100,
            is_online: true,
            last_seen: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            protocol_version: irys_types::ProtocolVersion::default(),
        };
        (mining_addr, peer_id, peer)
    }

    fn create_mock_sender() -> PeerNetworkSender {
        let (tx, _rx) = mpsc::unbounded_channel();
        PeerNetworkSender::new(tx)
    }

    fn create_test_peer_list(config: Config) -> PeerList {
        let (peer_events, _rx) = broadcast::channel(100);
        let peer_list_data = PeerListDataInner {
            persistent_peers_cache: HashMap::new(),
            unstaked_peer_purgatory: LruCache::new(
                std::num::NonZeroUsize::new(UNSTAKED_PEER_PURGATORY_CAPACITY).unwrap(),
            ),
            known_peers_cache: HashSet::new(),
            gossip_addr_to_peer_id_map: HashMap::new(),
            api_addr_to_peer_id_map: HashMap::new(),
            miner_addr_to_peer_id_map: HashMap::new(),
            peer_id_to_miner_addr_map: HashMap::new(),
            trusted_peers_api_to_gossip_addresses: HashMap::new(),
            peer_whitelist: HashSet::new(),
            peer_network_service_sender: create_mock_sender(),
            peer_events,
            config,
        };
        PeerList(Arc::new(RwLock::new(peer_list_data)))
    }

    mod peer_list_scoring_tests {
        use super::*;
        use irys_types::NodeConfig;
        use rstest::rstest;

        #[rstest]
        #[case(ScoreDecreaseReason::BogusData(String::from("test")), 45)]
        #[case(ScoreDecreaseReason::Offline(String::from("test")), 47)]
        #[case(ScoreDecreaseReason::SlowResponse, 49)]
        #[case(ScoreDecreaseReason::NetworkError(String::from("test")), 47)]
        fn test_decrease_peer_score_persistent_cache(
            #[case] reason: ScoreDecreaseReason,
            #[case] expected_score: u16,
        ) {
            let peer_list =
                create_test_peer_list(Config::new_with_random_peer_id(NodeConfig::testing()));
            let (_mining_addr, peer_id, peer) = create_test_peer(1);

            peer_list.add_or_update_peer(peer, true);

            peer_list.decrease_peer_score_by_peer_id(&peer_id, reason);
            let updated_peer = peer_list.get_peer(&peer_id).unwrap();
            assert_eq!(updated_peer.reputation_score.get(), expected_score);
        }

        #[test]
        fn test_multiple_decreases_cumulative() {
            let peer_list =
                create_test_peer_list(Config::new_with_random_peer_id(NodeConfig::testing()));
            let (_mining_addr, peer_id, peer) = create_test_peer(1);

            peer_list.add_or_update_peer(peer, true);

            peer_list.decrease_peer_score_by_peer_id(
                &peer_id,
                ScoreDecreaseReason::BogusData("bogus_data".into()),
            );
            assert_eq!(
                peer_list.get_peer(&peer_id).unwrap().reputation_score.get(),
                45
            );

            peer_list.decrease_peer_score_by_peer_id(
                &peer_id,
                ScoreDecreaseReason::Offline("offline".into()),
            );
            assert_eq!(
                peer_list.get_peer(&peer_id).unwrap().reputation_score.get(),
                42
            );

            peer_list.decrease_peer_score_by_peer_id(&peer_id, ScoreDecreaseReason::SlowResponse);
            assert_eq!(
                peer_list.get_peer(&peer_id).unwrap().reputation_score.get(),
                41
            );

            peer_list.decrease_peer_score_by_peer_id(
                &peer_id,
                ScoreDecreaseReason::NetworkError("network_error".into()),
            );
            assert_eq!(
                peer_list.get_peer(&peer_id).unwrap().reputation_score.get(),
                38
            );
        }

        #[test]
        fn test_decrease_score_removes_inactive_from_known_peers() {
            let peer_list =
                create_test_peer_list(Config::new_with_random_peer_id(NodeConfig::testing()));
            let (_mining_addr, peer_id, mut peer) = create_test_peer(1);
            peer.reputation_score = PeerScore::new(25);

            peer_list.add_or_update_peer(peer.clone(), true);
            assert!(peer_list.all_known_peers().contains(&peer.address));

            peer_list.decrease_peer_score_by_peer_id(
                &peer_id,
                ScoreDecreaseReason::BogusData("bogus".into()),
            );
            let updated_peer = peer_list.get_peer(&peer_id);

            if let Some(p) = updated_peer
                && !p.reputation_score.is_active()
            {
                assert!(!peer_list.all_known_peers().contains(&peer.address));
            }
        }

        #[test]
        fn test_decrease_score_unstaked_peer_removal() {
            let peer_list =
                create_test_peer_list(Config::new_with_random_peer_id(NodeConfig::testing()));
            let (_mining_addr, peer_id, peer) = create_test_peer(1);

            peer_list.add_or_update_peer(peer.clone(), false);
            assert!(peer_list.get_peer(&peer_id).is_some());

            peer_list.decrease_peer_score_by_peer_id(
                &peer_id,
                ScoreDecreaseReason::Offline("offline".into()),
            );
            assert!(peer_list.get_peer(&peer_id).is_none());
            assert!(!peer_list.all_known_peers().contains(&peer.address));
        }

        #[rstest]
        #[case(ScoreIncreaseReason::Online, 51)]
        #[case(ScoreIncreaseReason::DataRequest, 51)]
        #[case(ScoreIncreaseReason::TimelyResponse, 51)]
        fn test_increase_peer_score(
            #[case] reason: ScoreIncreaseReason,
            #[case] expected_score: u16,
        ) {
            let peer_list =
                create_test_peer_list(Config::new_with_random_peer_id(NodeConfig::testing()));
            let (_mining_addr, peer_id, peer) = create_test_peer(1);

            peer_list.add_or_update_peer(peer, true);

            peer_list.increase_peer_score_by_peer_id(&peer_id, reason);
            let updated_peer = peer_list.get_peer(&peer_id).unwrap();
            assert_eq!(updated_peer.reputation_score.get(), expected_score);
        }

        #[test]
        fn test_score_transitions_across_thresholds() {
            let peer_list =
                create_test_peer_list(Config::new_with_random_peer_id(NodeConfig::testing()));
            let (_mining_addr, peer_id, mut peer) = create_test_peer(1);

            peer.reputation_score = PeerScore::new(PeerScore::ACTIVE_THRESHOLD + 2);
            peer_list.add_or_update_peer(peer, true);

            peer_list.decrease_peer_score_by_peer_id(
                &peer_id,
                ScoreDecreaseReason::Offline("offline".into()),
            );
            let updated_peer = peer_list.get_peer(&peer_id).unwrap();

            assert_eq!(updated_peer.reputation_score.get(), 19);
            assert!(!updated_peer.reputation_score.is_active());

            peer_list.increase_peer_score_by_peer_id(&peer_id, ScoreIncreaseReason::Online);
            let final_peer = peer_list.get_peer(&peer_id).unwrap();
            assert_eq!(final_peer.reputation_score.get(), 20);
            assert!(final_peer.reputation_score.is_active());
        }

        #[test]
        fn test_unstaked_peer_operations() {
            let peer_list =
                create_test_peer_list(Config::new_with_random_peer_id(NodeConfig::testing()));
            let (_mining_addr, peer_id, mut peer) = create_test_peer(1);

            peer.reputation_score = PeerScore::new(50);
            peer_list.add_or_update_peer(peer, false);

            let initial_score = peer_list.get_peer(&peer_id).unwrap().reputation_score.get();
            assert_eq!(initial_score, 50);

            peer_list.decrease_peer_score_by_peer_id(
                &peer_id,
                ScoreDecreaseReason::BogusData("bogus".into()),
            );

            let final_peer = peer_list.get_peer(&peer_id);
            assert!(
                final_peer.is_none(),
                "Unstaked peer should be removed after any decrease operation"
            );
        }
    }

    #[tokio::test]
    async fn test_all_methods_treat_staked_unstaked_peers_equally_except_persistable() {
        let peer_list =
            create_test_peer_list(Config::new_with_random_peer_id(NodeConfig::testing()));

        // Create test peers
        let (staked_mining_addr, staked_peer_id, staked_peer) = create_test_peer(1);
        let (unstaked_mining_addr, unstaked_peer_id, unstaked_peer) = create_test_peer(2);

        // Add peers with different staking status
        peer_list.add_or_update_peer(staked_peer.clone(), true);
        peer_list.add_or_update_peer(unstaked_peer.clone(), false);

        // Test 1: persistable_peers should only return staked peers
        let persistable = peer_list.persistable_peers();
        assert!(
            persistable.contains_key(&staked_peer_id),
            "Persistable peers should contain staked peer"
        );
        assert!(
            !persistable.contains_key(&unstaked_peer_id),
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
                .any(|(peer_id, _)| peer_id == &staked_peer_id),
            "Gossip peers should contain staked peer"
        );
        assert!(
            gossip_peers_vec
                .iter()
                .any(|(peer_id, _)| peer_id == &unstaked_peer_id),
            "Gossip peers should contain unstaked peer"
        );

        // Test 3: get_peer should return both staked and unstaked peers
        let staked_result = peer_list.get_peer(&staked_peer_id);
        let unstaked_result = peer_list.get_peer(&unstaked_peer_id);
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
        let staked_mining_result = peer_list.peer_by_mining_address(&staked_mining_addr);
        let unstaked_mining_result = peer_list.peer_by_mining_address(&unstaked_mining_addr);
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
        let contains_staked = top_peers
            .iter()
            .any(|(peer_id, _)| peer_id == &staked_peer_id);
        let contains_unstaked = top_peers
            .iter()
            .any(|(peer_id, _)| peer_id == &unstaked_peer_id);
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
                .trusted_peers_api_to_gossip_addresses
                .insert(staked_peer.address.api, staked_peer.address.gossip);
            inner
                .trusted_peers_api_to_gossip_addresses
                .insert(unstaked_peer.address.api, unstaked_peer.address.gossip);
        }
        let trusted_peers = peer_list.all_trusted_peers();
        let trusted_contains_staked = trusted_peers
            .iter()
            .any(|(peer_id, _)| peer_id == &staked_peer_id);
        let trusted_contains_unstaked = trusted_peers
            .iter()
            .any(|(peer_id, _)| peer_id == &unstaked_peer_id);
        assert!(
            trusted_contains_staked,
            "trusted_peers should include a staked peer if it's in the trusted list"
        );
        assert!(
            trusted_contains_unstaked,
            "trusted_peers should include an unstaked peer if it's the trusted list"
        );

        // Test 10: trusted_peer_addresses should return both staked and unstaked peer addresses
        let trusted_addresses = peer_list.trusted_peer_api_to_gossip_addresses();
        assert!(
            trusted_addresses.contains_key(&staked_peer.address.api),
            "trusted_peer_addresses should contain staked peer API address"
        );
        assert!(
            trusted_addresses.contains_key(&unstaked_peer.address.api),
            "trusted_peer_addresses should contain unstaked peer API address"
        );

        // Test 11: inactive_peers should include both staked and unstaked peers if they're inactive
        // Create inactive peers
        let (_inactive_staked_mining_addr, inactive_staked_peer_id, mut inactive_staked_peer) =
            create_test_peer(3);
        let (_inactive_unstaked_mining_addr, inactive_unstaked_peer_id, mut inactive_unstaked_peer) =
            create_test_peer(4);
        inactive_staked_peer.reputation_score = PeerScore::new(10); // Below active threshold
        inactive_unstaked_peer.reputation_score = PeerScore::new(10); // Below active threshold
        peer_list.add_or_update_peer(inactive_staked_peer, true);
        peer_list.add_or_update_peer(inactive_unstaked_peer, false);

        let inactive_peers = peer_list.inactive_peers();
        let inactive_contains_staked = inactive_peers
            .iter()
            .any(|(peer_id, _)| peer_id == &inactive_staked_peer_id);
        let inactive_contains_unstaked = inactive_peers
            .iter()
            .any(|(peer_id, _)| peer_id == &inactive_unstaked_peer_id);
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
            peer_list.is_a_trusted_peer(staked_mining_addr, staked_peer.address.gossip.ip());
        let unstaked_is_trusted =
            peer_list.is_a_trusted_peer(unstaked_mining_addr, unstaked_peer.address.gossip.ip());
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

    #[test]
    fn test_protocol_version_propagation() {
        let peer_list =
            create_test_peer_list(Config::new_with_random_peer_id(NodeConfig::testing()));

        // Create a peer with protocol version V1
        let (mining_addr, peer_id, mut peer) = create_test_peer(1);
        peer.protocol_version = ProtocolVersion::V1;
        assert_eq!(peer.protocol_version, ProtocolVersion::V1);

        // Add peer to persistent cache (staked)
        peer_list.add_or_update_peer(peer.clone(), true);

        // Verify initial protocol version
        let retrieved = peer_list.peer_by_mining_address(&mining_addr).unwrap();
        assert_eq!(retrieved.protocol_version, ProtocolVersion::V1);

        // Update peer with new protocol version (V2)
        peer.protocol_version = ProtocolVersion::V2;
        peer.last_seen += HANDSHAKE_COOLDOWN + 1; // Ensure cooldown expired
        peer_list.add_or_update_peer(peer.clone(), true);

        // Verify protocol_version is updated via peer_by_mining_address
        let updated = peer_list.peer_by_mining_address(&mining_addr).unwrap();
        assert_eq!(
            updated.protocol_version,
            ProtocolVersion::V2,
            "protocol_version should be updated in persistent cache"
        );

        // Verify protocol_version is visible via peer_by_gossip_address
        let by_gossip = peer_list
            .peer_by_gossip_address(peer.address.gossip)
            .unwrap();
        assert_eq!(
            by_gossip.protocol_version,
            ProtocolVersion::V2,
            "protocol_version should be visible via peer_by_gossip_address"
        );

        // Verify protocol_version is visible via peer_by_api_address
        let by_api = peer_list.peer_by_api_address(peer.address.api).unwrap();
        assert_eq!(
            by_api.protocol_version,
            ProtocolVersion::V2,
            "protocol_version should be visible via peer_by_api_address"
        );

        // Verify protocol_version is visible in all_peers_sorted_by_score
        let all_peers = peer_list.all_peers_sorted_by_score();
        let found_peer = all_peers.iter().find(|(pid, _)| pid == &peer_id).unwrap();
        assert_eq!(
            found_peer.1.protocol_version,
            ProtocolVersion::V2,
            "protocol_version should be visible in all_peers_sorted_by_score"
        );

        // Test protocol_version propagation in purgatory (unstaked)
        let (unstaked_mining_addr, _unstaked_peer_id, mut unstaked_peer) = create_test_peer(2);
        unstaked_peer.protocol_version = ProtocolVersion::V1;
        assert_eq!(unstaked_peer.protocol_version, ProtocolVersion::V1);

        // Add to purgatory
        peer_list.add_or_update_peer(unstaked_peer.clone(), false);

        // Update protocol version for unstaked peer to V2
        unstaked_peer.protocol_version = ProtocolVersion::V2;
        unstaked_peer.last_seen += HANDSHAKE_COOLDOWN + 1;
        peer_list.add_or_update_peer(unstaked_peer, false);

        // Verify protocol_version updated in purgatory
        let unstaked_updated = peer_list
            .peer_by_mining_address(&unstaked_mining_addr)
            .unwrap();
        assert_eq!(
            unstaked_updated.protocol_version,
            ProtocolVersion::V2,
            "protocol_version should be updated in purgatory"
        );

        // Test promotion flow: move unstaked peer to persistent cache with protocol_version preserved
        let (promo_mining_addr, promo_peer_id, mut promo_peer) = create_test_peer(3);
        promo_peer.protocol_version = ProtocolVersion::V2;

        // Add to purgatory first
        peer_list.add_or_update_peer(promo_peer.clone(), false);
        let in_purgatory = peer_list
            .peer_by_mining_address(&promo_mining_addr)
            .unwrap();
        assert_eq!(in_purgatory.protocol_version, ProtocolVersion::V2);

        // Promote to persistent (staked)
        promo_peer.last_seen += HANDSHAKE_COOLDOWN + 1;
        peer_list.add_or_update_peer(promo_peer, true);

        // Verify protocol_version preserved after promotion
        let promoted = peer_list
            .peer_by_mining_address(&promo_mining_addr)
            .unwrap();
        assert_eq!(
            promoted.protocol_version,
            ProtocolVersion::V2,
            "protocol_version should be preserved during promotion from purgatory to persistent"
        );

        // Verify promoted peer is in persistent cache, not purgatory
        let persistable = peer_list.persistable_peers();
        assert!(
            persistable.contains_key(&promo_peer_id),
            "Promoted peer should be in persistent cache"
        );
    }

    #[tokio::test]
    async fn test_wait_for_active_peers_includes_both_staked_and_unstaked() {
        let peer_list =
            create_test_peer_list(Config::new_with_random_peer_id(NodeConfig::testing()));

        // Create test peers with active reputation scores
        let (_staked_mining_addr, _staked_peer_id, mut staked_peer) = create_test_peer(1);
        let (_unstaked_mining_addr, _unstaked_peer_id, mut unstaked_peer) = create_test_peer(2);

        // Make sure peers have active reputation scores (above ACTIVE_THRESHOLD = 20)
        // Start with INITIAL = 50, so they should already be active
        staked_peer.reputation_score = PeerScore::new(80); // Well above active threshold
        unstaked_peer.reputation_score = PeerScore::new(80); // Well above active threshold

        // Test case 1: Only unstaked peer is active
        peer_list.add_or_update_peer(unstaked_peer, false);

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
        peer_list.add_or_update_peer(staked_peer, true);

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
