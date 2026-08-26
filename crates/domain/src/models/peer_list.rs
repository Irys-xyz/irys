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
const PENDING_HANDSHAKE_CAPACITY: usize = 1024;

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

    /// Old `peer_id` rows left behind by V1→V2 migration or a duplicate load.
    /// `PeerNetworkService::flush` is the sole DB writer and drains this set.
    pending_db_removals: HashSet<IrysPeerId>,
    trusted_peers_api_to_gossip_addresses: HashMap<SocketAddr, SocketAddr>,
    /// Outbound handshake has no mining address, so observations wait here
    /// until `add_or_update_peer` can insert a real `PeerListItem`.
    pending_handshake: LruCache<SocketAddr, (Option<semver::Version>, Option<irys_types::H256>)>,
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
            let was_active = peer.is_active();
            peer.is_online = is_online;
            let now_active = peer.is_active();
            if !was_active && now_active {
                became_active = Some(peer.clone());
            }
            if was_active && !now_active {
                became_inactive = Some(peer.clone());
            }
        } else if let Some(peer) = inner.unstaked_peer_purgatory.get_mut(&peer_id) {
            let was_active = peer.is_active();
            peer.is_online = is_online;
            let now_active = peer.is_active();
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
            let was_active = peer.is_active();
            peer.is_online = is_online;
            let now_active = peer.is_active();
            if !was_active && now_active {
                became_active = Some(peer.clone());
            }
            if was_active && !now_active {
                became_inactive = Some(peer.clone());
            }
        } else if let Some(peer) = inner.unstaked_peer_purgatory.get_mut(peer_id) {
            let was_active = peer.is_active();
            peer.is_online = is_online;
            let now_active = peer.is_active();
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

    /// Addresses we advertise (`GET /v1/peer-list`, handshake `peers`).
    /// One socket triple per `peer_id`; low-score peers are omitted.
    pub fn all_known_peers(&self) -> Vec<PeerAddress> {
        self.read().advertised_addresses()
    }

    /// `peer_id`s that must be deleted from the peer-list table on the next flush.
    pub fn take_pending_db_removals(&self) -> HashSet<IrysPeerId> {
        let mut inner = self.0.write().expect("PeerListDataInner lock poisoned");
        std::mem::take(&mut inner.pending_db_removals)
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

    /// Wait until at least `min_count` peers are active+online, or `timeout` elapses.
    ///
    /// Returns the count of active+online peers when the wait ends. If the count is
    /// `>= min_count` the wait was satisfied; if it is less, the timeout fired and
    /// the caller is expected to proceed best-effort.
    pub async fn wait_for_active_peers(&self, min_count: usize, timeout: Duration) -> usize {
        let count_active = || -> usize {
            let bindings = self.read();
            let persistent = bindings
                .persistent_peers_cache
                .values()
                .filter(|peer| peer.is_active())
                .count();
            let purgatory = bindings
                .unstaked_peer_purgatory
                .iter()
                .map(|(_, v)| v)
                .filter(|peer| peer.is_active())
                .count();
            persistent + purgatory
        };

        // Fast path: already satisfied
        let initial = count_active();
        if initial >= min_count {
            return initial;
        }

        // Slow path: subscribe and recount on every event wakeup. Recounting (rather
        // than tracking deltas) ensures BecameInactive transitions correctly drop
        // the count.
        let mut rx = self.subscribe_to_peer_events();
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            let current = count_active();
            if current >= min_count {
                return current;
            }

            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Ok(_)) => continue,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                    warn!("peer events channel closed while waiting for active peers");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    rx = self.subscribe_to_peer_events();
                }
                // Deadline elapsed: return whatever the current count is.
                Err(_) => return count_active(),
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
            .filter(|(_, peer)| peer.reputation_score.is_reputable())
            .map(|(key, value)| (*key, value.clone()));

        // Chain iterators and apply all filters in one pass
        let filtered_peers = persistent_peers
            .chain(purgatory_peers)
            .filter(|(peer_id, peer)| {
                let exclude = exclude_peers
                    .as_ref()
                    .is_some_and(|excluded| excluded.contains(peer_id));
                !exclude && peer.is_active()
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

    /// Peers worth re-probing. Keyed on `is_active`, not reputation alone:
    /// that misses well-scored peers that are merely offline, and returns
    /// nothing at all when scoring is disabled.
    pub fn inactive_peers(&self) -> Vec<(IrysPeerId, PeerListItem)> {
        let guard = self.read();
        let mut inactive = Vec::new();

        // Add inactive peers from main cache
        inactive.extend(
            guard
                .persistent_peers_cache
                .iter()
                .filter(|(_peer_id, peer)| !peer.is_active())
                .map(|(peer_id, peer)| (*peer_id, peer.clone())),
        );

        // Add inactive peers from purgatory
        inactive.extend(
            guard
                .unstaked_peer_purgatory
                .iter()
                .filter(|(_peer_id, peer)| !peer.is_active())
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

    /// Evict a peer (identified by its API address) from the in-memory cache and
    /// every lookup index, returning the removed peer if it was present. Used
    /// when a handshake is rejected on network-membership grounds (e.g. a
    /// `chain_id` mismatch): the gossip data plane (`check_peer_v*`) trusts cache
    /// membership, so a peer we can no longer peer with must be removed from the
    /// cache, not merely left un-handshaked.
    pub fn remove_peer_by_api_address(&self, api_address: &SocketAddr) -> Option<PeerListItem> {
        let mut guard = self.0.write().expect("PeerListDataInner lock poisoned");
        guard.remove_peer_by_api_address(api_address)
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

    /// Record handshake-observed software version / consensus config hash.
    pub fn observe_handshake(
        &self,
        api_address: SocketAddr,
        software_version: Option<semver::Version>,
        consensus_config_hash: Option<irys_types::H256>,
    ) {
        let mut inner = self.0.write().expect("PeerListDataInner lock poisoned");
        inner.observe_handshake(api_address, software_version, consensus_config_hash);
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
            pending_db_removals: HashSet::new(),
            pending_handshake: LruCache::new(
                std::num::NonZeroUsize::new(PENDING_HANDSHAKE_CAPACITY)
                    .expect("PENDING_HANDSHAKE_CAPACITY > 0"),
            ),
            trusted_peers_api_to_gossip_addresses,
            peer_whitelist: peer_api_ip_whitelist,
            peer_network_service_sender: peer_network_sender,
            peer_events,
            config: config.clone(),
        };

        // One row per process: the generated `peer_id` is the identity, and the
        // gossip listen socket is unique per process (two nodes cannot bind it).
        // A V1 leftover (`peer_id == mining_address`) can sit next to a V2 row
        // for the same process after upgrade; the flush path only inserts, so
        // the leftover key survives restart unless we drop it here and stage it
        // for delete. Do not collapse on mining address — observers never stake
        // and two processes can share a copied mining key.
        let mut by_gossip: HashMap<SocketAddr, PeerListItem> = HashMap::new();
        for mut peer_list_item in peers {
            if !config.node_config.p2p_gossip.enable_scoring {
                peer_list_item.reputation_score.set_to_max();
            }

            let gossip = peer_list_item.address.gossip;
            match by_gossip.remove(&gossip) {
                None => {
                    by_gossip.insert(gossip, peer_list_item);
                }
                Some(existing) => {
                    let (keep, drop) = Self::prefer_canonical_item(existing, peer_list_item);
                    if drop.peer_id != keep.peer_id {
                        peer_list.pending_db_removals.insert(drop.peer_id);
                    }
                    by_gossip.insert(keep.address.gossip, keep);
                }
            }
        }

        for peer_list_item in by_gossip.into_values() {
            let peer_id = peer_list_item.peer_id;
            let mining_address = peer_list_item.mining_address;
            let address = peer_list_item.address;

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
        }

        Ok(peer_list)
    }

    fn observe_handshake(
        &mut self,
        api_address: SocketAddr,
        software_version: Option<semver::Version>,
        consensus_config_hash: Option<irys_types::H256>,
    ) {
        let Some(peer_id) = self.api_addr_to_peer_id_map.get(&api_address).copied() else {
            self.pending_handshake
                .put(api_address, (software_version, consensus_config_hash));
            return;
        };
        let peer = if let Some(peer) = self.persistent_peers_cache.get_mut(&peer_id) {
            Some(peer)
        } else {
            self.unstaked_peer_purgatory.get_mut(&peer_id)
        };
        if let Some(peer) = peer {
            peer.merge_handshake_observed(software_version, consensus_config_hash);
            self.pending_handshake.pop(&api_address);
        }
    }

    fn apply_pending_handshake(&mut self, api_address: SocketAddr) {
        let Some((software_version, consensus_config_hash)) =
            self.pending_handshake.pop(&api_address)
        else {
            return;
        };
        let Some(peer_id) = self.api_addr_to_peer_id_map.get(&api_address).copied() else {
            debug!(
                "Dropped pending handshake observation for {api_address}: peer not in address map"
            );
            return;
        };
        let peer = if let Some(peer) = self.persistent_peers_cache.get_mut(&peer_id) {
            Some(peer)
        } else {
            self.unstaked_peer_purgatory.get_mut(&peer_id)
        };
        if let Some(peer) = peer {
            peer.merge_handshake_observed(software_version, consensus_config_hash);
        } else {
            debug!(
                "Dropped pending handshake observation for {api_address}: peer {peer_id:?} not in cache"
            );
        }
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
            .map(PeerListItem::is_active)
            .or_else(|| {
                self.unstaked_peer_purgatory
                    .peek(&peer_id)
                    .map(PeerListItem::is_active)
            })
            .unwrap_or(false);

        let is_updated = self.add_or_update_peer_internal(peer.clone(), is_staked);
        self.apply_pending_handshake(peer.address.api);

        // Determine a new active state
        let now_peer = self
            .persistent_peers_cache
            .get(&peer_id)
            .cloned()
            .or_else(|| self.unstaked_peer_purgatory.peek(&peer_id).cloned());

        if let Some(now_peer) = now_peer {
            let now_active = now_peer.is_active();
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
            let was_active = peer.is_active();
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
            let now_active = peer.is_active();
            let to_send = (!was_active && now_active).then(|| peer.clone());
            let _ = peer;
            if let Some(peer) = to_send {
                let mining_addr = peer.mining_address;
                self.emit_peer_event(PeerEvent::BecameActive { mining_addr, peer });
            }
        } else if let Some(peer) = self.unstaked_peer_purgatory.get_mut(peer_id) {
            // Update score in purgatory
            let was_active = peer.is_active();
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
                self.persistent_peers_cache.insert(*peer_id, peer_clone);
            }

            // Check post-state (may be in persistent now)
            let now_peer = self
                .persistent_peers_cache
                .get(peer_id)
                .cloned()
                .or_else(|| self.unstaked_peer_purgatory.peek(peer_id).cloned());
            if let Some(now_peer) = now_peer {
                let now_active = now_peer.is_active();
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
            let was_active = peer_item.is_active();
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
                    peer_item.reputation_score.decrease_network_error(&message);
                }
            }

            // Don't propagate inactive peers. `all_known_peers` projects from
            // `is_reputable()`, so dropping below the threshold unlists the
            // address without deleting the cached item.
            if !peer_item.reputation_score.is_reputable() {
                warn!(
                    "Peer's {:?} score dropped below an active threshold, excluding from advertised peer list",
                    peer_id
                );
            }
            let now_active = peer_item.is_active();
            if was_active && !now_active {
                warn!("Peer {:?} became inactive", peer_id);
                let peer_clone = peer_item.clone();
                let mining_addr = peer_item.mining_address;
                self.emit_peer_event(PeerEvent::BecameInactive {
                    mining_addr,
                    peer: peer_clone,
                });
            }
        } else {
            let should_evict =
                if let Some(peer_item) = self.unstaked_peer_purgatory.get_mut(peer_id) {
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
                        // Transient network errors should not drive purgatory
                        // eviction. An unstaked peer reachable now can become
                        // unreachable for seconds at a time during honest
                        // overload. Keep the peer in purgatory and let the
                        // next successful request re-confirm liveness instead
                        // of eroding the reputation toward eviction.
                        ScoreDecreaseReason::NetworkError(message) => {
                            debug!(
                                ?peer_id,
                                ?message,
                                "Network error on unstaked purgatory peer ignored \
                                 (no reputation decrement)"
                            );
                        }
                    }
                    !peer_item.reputation_score.is_reputable()
                } else {
                    false
                };

            if should_evict && let Some(peer) = self.unstaked_peer_purgatory.pop(peer_id) {
                let mining_addr = peer.mining_address;
                self.gossip_addr_to_peer_id_map
                    .remove(&peer.address.gossip.ip());
                self.api_addr_to_peer_id_map.remove(&peer.address.api);
                self.miner_addr_to_peer_id_map.remove(&mining_addr);
                self.peer_id_to_miner_addr_map.remove(peer_id);
                debug!("Removed unstaked peer {:?} from all caches", peer_id);
                self.emit_peer_event(PeerEvent::PeerRemoved { mining_addr, peer });
            }
        }
    }

    /// Remove a peer from the persistent cache (or purgatory) and every lookup
    /// map, mirroring the purgatory-eviction cleanup above. Emits `PeerRemoved`.
    /// Returns the removed peer if it was present.
    fn remove_peer_by_api_address(&mut self, api_address: &SocketAddr) -> Option<PeerListItem> {
        let peer_id = *self.api_addr_to_peer_id_map.get(api_address)?;
        let peer = self
            .persistent_peers_cache
            .remove(&peer_id)
            .or_else(|| self.unstaked_peer_purgatory.pop(&peer_id))?;
        let mining_addr = peer.mining_address;
        self.gossip_addr_to_peer_id_map
            .remove(&peer.address.gossip.ip());
        self.api_addr_to_peer_id_map.remove(&peer.address.api);
        self.miner_addr_to_peer_id_map.remove(&mining_addr);
        self.peer_id_to_miner_addr_map.remove(&peer_id);
        debug!(
            "Evicted peer {:?} (api {:?}) from all caches",
            peer_id, api_address
        );
        self.emit_peer_event(PeerEvent::PeerRemoved {
            mining_addr,
            peer: peer.clone(),
        });
        Some(peer)
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
            existing_peer.merge_handshake_meta(&peer);
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
        } else if let Some((evicted_id, evicted)) = self.unstaked_peer_purgatory.push(peer_id, peer)
            && evicted_id != peer_id
        {
            self.unlink_peer_lookups(evicted_id, &evicted);
        }

        self.gossip_addr_to_peer_id_map
            .insert(gossip_addr.ip(), peer_id);
        self.api_addr_to_peer_id_map
            .insert(peer_address.api, peer_id);
        self.miner_addr_to_peer_id_map.insert(mining_addr, peer_id);
        self.peer_id_to_miner_addr_map.insert(peer_id, mining_addr);
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
        self.pending_handshake.pop(&old_address.api);
        // miner_addr_to_peer_id_map doesn't change (unless miner address changes, which shouldn't happen)
    }

    fn advertised_addresses(&self) -> Vec<PeerAddress> {
        self.persistent_peers_cache
            .values()
            .chain(self.unstaked_peer_purgatory.iter().map(|(_, peer)| peer))
            .filter(|peer| peer.reputation_score.is_reputable())
            .map(|peer| peer.address)
            .collect()
    }

    /// V1 handshakes set `peer_id` to the mining address. A later V2 handshake
    /// from the same process uses the generated gossip id.
    fn is_v1_identity(peer: &PeerListItem) -> bool {
        peer.peer_id == IrysPeerId::from(peer.mining_address)
    }

    fn prefer_canonical_item(a: PeerListItem, b: PeerListItem) -> (PeerListItem, PeerListItem) {
        let a_v1 = Self::is_v1_identity(&a);
        let b_v1 = Self::is_v1_identity(&b);
        match (a_v1, b_v1) {
            (true, false) => (b, a),
            (false, true) => (a, b),
            _ if b.last_seen > a.last_seen => (b, a),
            _ => (a, b),
        }
    }

    fn unlink_peer_lookups(&mut self, peer_id: IrysPeerId, peer: &PeerListItem) {
        if self
            .gossip_addr_to_peer_id_map
            .get(&peer.address.gossip.ip())
            == Some(&peer_id)
        {
            self.gossip_addr_to_peer_id_map
                .remove(&peer.address.gossip.ip());
        }
        if self.api_addr_to_peer_id_map.get(&peer.address.api) == Some(&peer_id) {
            self.api_addr_to_peer_id_map.remove(&peer.address.api);
        }
        if self.miner_addr_to_peer_id_map.get(&peer.mining_address) == Some(&peer_id) {
            self.miner_addr_to_peer_id_map.remove(&peer.mining_address);
        }
        self.peer_id_to_miner_addr_map.remove(&peer_id);
        self.pending_handshake.pop(&peer.address.api);
    }

    fn find_peer_id_sharing_gossip(
        &self,
        gossip: SocketAddr,
        except: IrysPeerId,
    ) -> Option<IrysPeerId> {
        for (id, peer) in &self.persistent_peers_cache {
            if *id != except && peer.address.gossip == gossip {
                return Some(*id);
            }
        }
        for (id, peer) in self.unstaked_peer_purgatory.iter() {
            if *id != except && peer.address.gossip == gossip {
                return Some(*id);
            }
        }
        None
    }

    /// Drop extra cache entries that share this process's gossip socket.
    /// Does not emit `PeerRemoved`: the peer is still present under `keep`.
    fn evict_duplicate_identities(&mut self, gossip: SocketAddr, keep: IrysPeerId) {
        let mut extras = Vec::new();
        for (id, peer) in &self.persistent_peers_cache {
            if peer.address.gossip == gossip && *id != keep {
                extras.push(*id);
            }
        }
        for (id, peer) in self.unstaked_peer_purgatory.iter() {
            if peer.address.gossip == gossip && *id != keep {
                extras.push(*id);
            }
        }

        for extra_id in extras {
            let extra = self
                .persistent_peers_cache
                .remove(&extra_id)
                .or_else(|| self.unstaked_peer_purgatory.pop(&extra_id));
            if let Some(extra) = extra {
                debug!(
                    gossip_addr = %gossip,
                    dropped_peer_id = ?extra_id,
                    kept_peer_id = ?keep,
                    "Dropping duplicate peer identity for the same gossip socket"
                );
                self.unlink_peer_lookups(extra_id, &extra);
                self.pending_db_removals.insert(extra_id);
            }
        }
    }

    /// Add or update a peer in the appropriate cache based on staking status and current location.
    /// Returns true if the peer was added or needs re-handshaking, false if no update needed.
    fn add_or_update_peer_internal(&mut self, peer: PeerListItem, is_staked: bool) -> bool {
        let gossip_addr = peer.address.gossip;
        let peer_address = peer.address;

        // Identity is `peer_id` (generated at first start). Mining address is
        // the chain key and is not unique: observers never stake, and two
        // processes can copy the same mining key. Same gossip listen socket
        // means the same process (V1 leftover or a regenerated `peer_key.bin`).
        let peer_id = peer.peer_id;
        let mining_addr = peer.mining_address;

        if let Some(old_peer_id) = self.find_peer_id_sharing_gossip(gossip_addr, peer_id) {
            debug!(
                "Peer {:?} changed peer_id from {:?} to {:?}, migrating entry",
                mining_addr, old_peer_id, peer_id
            );

            // Remove old entry, sync the inner peer_id field, then re-insert under
            // the new key. Without the field update, server.rs:check_peer_v2 keeps
            // reading the stale field and rejects every V2 gossip request.
            if let Some(mut old_peer) = self.persistent_peers_cache.remove(&old_peer_id) {
                old_peer.peer_id = peer_id;
                self.persistent_peers_cache.insert(peer_id, old_peer);
            } else if let Some(mut old_peer) = self.unstaked_peer_purgatory.pop(&old_peer_id) {
                old_peer.peer_id = peer_id;
                self.unstaked_peer_purgatory.put(peer_id, old_peer);
            }

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
            // Flush only inserts current keys; the old row stays on disk unless
            // we stage it. Drop the new key from the delete set if a previous
            // collapse had marked it.
            self.pending_db_removals.insert(old_peer_id);
            self.pending_db_removals.remove(&peer_id);
        }

        self.evict_duplicate_identities(gossip_addr, peer_id);

        let in_persistent = self.persistent_peers_cache.contains_key(&peer_id);
        let in_purgatory = self.unstaked_peer_purgatory.contains(&peer_id);

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
                    updated_peer.merge_handshake_meta(&peer);

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
    use irys_types::{
        H256, NodeConfig, PeerAddress, PeerListItem, PeerScore, ProtocolVersion, RethPeerInfo,
    };
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
            ..Default::default()
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
            pending_db_removals: HashSet::new(),
            pending_handshake: LruCache::new(
                std::num::NonZeroUsize::new(PENDING_HANDSHAKE_CAPACITY)
                    .expect("PENDING_HANDSHAKE_CAPACITY > 0"),
            ),
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

    /// Reputation alone stranded this peer: excluded from selection for being
    /// offline, and invisible to the check that would find it again.
    #[test]
    fn inactive_peers_includes_a_reputable_peer_that_is_offline() {
        let peer_list =
            create_test_peer_list(Config::new_with_random_peer_id(NodeConfig::testing()));

        let (_, offline_peer_id, mut offline_peer) = create_test_peer(1);
        offline_peer.reputation_score = PeerScore::new(PeerScore::MAX);
        offline_peer.is_online = false;
        peer_list.add_or_update_peer(offline_peer, true);

        // The control: reputable *and* reachable, so it is in use and must not
        // be handed to the health check.
        let (_, healthy_peer_id, healthy_peer) = create_test_peer(2);
        peer_list.add_or_update_peer(healthy_peer, true);

        let inactive: Vec<_> = peer_list
            .inactive_peers()
            .into_iter()
            .map(|(peer_id, _)| peer_id)
            .collect();

        assert!(
            inactive.contains(&offline_peer_id),
            "a reputable but unreachable peer must still be probed"
        );
        assert!(
            !inactive.contains(&healthy_peer_id),
            "a usable peer must not be handed to the health check"
        );
    }

    /// Scoring disabled pins every score to MAX, which retires a
    /// reputation-only health check entirely.
    #[test]
    fn inactive_peers_still_finds_offline_peers_when_scoring_is_disabled() {
        let mut node_config = NodeConfig::testing();
        node_config.p2p_gossip.enable_scoring = false;
        let peer_list = create_test_peer_list(Config::new_with_random_peer_id(node_config));

        let (_, offline_peer_id, mut offline_peer) = create_test_peer(1);
        offline_peer.is_online = false;
        peer_list.add_or_update_peer(offline_peer, true);

        let inactive: Vec<_> = peer_list
            .inactive_peers()
            .into_iter()
            .map(|(peer_id, _)| peer_id)
            .collect();

        assert_eq!(
            inactive,
            vec![offline_peer_id],
            "an offline peer must be probeable with scoring disabled"
        );
    }

    /// Evicting a staked peer by its API address must clear it from every
    /// lookup path so the gossip data plane (`check_peer_v*`) stops trusting it.
    #[test]
    fn remove_peer_by_api_address_clears_all_lookups() {
        let peer_list =
            create_test_peer_list(Config::new_with_random_peer_id(NodeConfig::testing()));
        let (mining_addr, peer_id, peer) = create_test_peer(1);
        let api = peer.address.api;
        peer_list.add_or_update_peer(peer.clone(), true);
        // Subscribe after the add so the only event we observe is the removal.
        let mut events = peer_list.subscribe_to_peer_events();

        // Sanity: present via every lookup before eviction.
        assert!(peer_list.peer_by_api_address(api).is_some());
        assert!(
            peer_list
                .peer_by_gossip_address(peer.address.gossip)
                .is_some()
        );
        assert!(peer_list.get_peer(&peer_id).is_some());
        assert!(peer_list.peer_by_mining_address(&mining_addr).is_some());
        assert_eq!(peer_list.peer_count(), 1);

        let removed = peer_list.remove_peer_by_api_address(&api);

        assert_eq!(
            removed.map(|p| p.peer_id),
            Some(peer_id),
            "eviction should return the removed peer"
        );
        assert!(
            peer_list.peer_by_api_address(api).is_none(),
            "peer must be gone from the api-address index"
        );
        assert!(
            peer_list.get_peer(&peer_id).is_none(),
            "peer must be gone from the persistent cache"
        );
        assert!(
            peer_list.peer_by_mining_address(&mining_addr).is_none(),
            "peer must be gone from the mining-address index"
        );
        assert!(
            !peer_list.all_known_peers().contains(&peer.address),
            "peer must be gone from the known-peers cache"
        );
        assert!(
            peer_list
                .peer_by_gossip_address(peer.address.gossip)
                .is_none(),
            "peer must be gone from the gossip-address index"
        );
        assert_eq!(peer_list.peer_count(), 0);

        match events.try_recv() {
            Ok(PeerEvent::PeerRemoved {
                peer: removed_peer, ..
            }) => assert_eq!(
                removed_peer.peer_id, peer_id,
                "PeerRemoved must carry the evicted peer"
            ),
            other => panic!("expected a PeerRemoved event, got {other:?}"),
        }
    }

    mod peer_list_scoring_tests {
        use super::*;
        use irys_types::NodeConfig;
        use rstest::rstest;

        #[rstest]
        #[case(ScoreDecreaseReason::BogusData(String::from("test")), 45)]
        #[case(ScoreDecreaseReason::Offline(String::from("test")), 47)]
        #[case(ScoreDecreaseReason::SlowResponse, 49)]
        // NetworkError is -1 (distinct from Offline -3) because a network
        // timeout against an overloaded-but-honest peer is not the same
        // signal as a peer being deliberately unreachable.
        #[case(ScoreDecreaseReason::NetworkError(String::from("test")), 49)]
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
            // 41 - 1 = 40 (NetworkError penalty is -1, not -3)
            assert_eq!(
                peer_list.get_peer(&peer_id).unwrap().reputation_score.get(),
                40
            );
        }

        #[test]
        fn test_decrease_score_removes_inactive_from_known_peers() {
            let peer_list =
                create_test_peer_list(Config::new_with_random_peer_id(NodeConfig::testing()));
            let (_mining_addr, peer_id, mut peer) = create_test_peer(1);
            // BogusData penalty is 5, ACTIVE_THRESHOLD is 10: 14 - 5 = 9 < 10 (inactive)
            peer.reputation_score = PeerScore::new(14);

            peer_list.add_or_update_peer(peer.clone(), true);
            assert!(peer_list.all_known_peers().contains(&peer.address));

            peer_list.decrease_peer_score_by_peer_id(
                &peer_id,
                ScoreDecreaseReason::BogusData("bogus".into()),
            );
            let updated_peer = peer_list.get_peer(&peer_id);

            if let Some(p) = updated_peer
                && !p.reputation_score.is_reputable()
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

            // Unstaked peers are evicted from purgatory only after the
            // reputation score crosses the active threshold so honest
            // overload doesn't churn unstaked peers. Offline decrements by 3;
            // INITIAL is 50; ACTIVE_THRESHOLD is 10; so (50 − 10) ÷ 3 = 14
            // decrements take the score to 8 (< 10).
            for _ in 0..14 {
                peer_list.decrease_peer_score_by_peer_id(
                    &peer_id,
                    ScoreDecreaseReason::Offline("offline".into()),
                );
            }
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

            // Start just above the threshold so Offline (-3) pushes below,
            // and Online (+1) pulls back to exactly the threshold.
            peer.reputation_score = PeerScore::new(PeerScore::ACTIVE_THRESHOLD + 2);
            peer_list.add_or_update_peer(peer, true);

            peer_list.decrease_peer_score_by_peer_id(
                &peer_id,
                ScoreDecreaseReason::Offline("offline".into()),
            );
            let updated_peer = peer_list.get_peer(&peer_id).unwrap();

            assert_eq!(
                updated_peer.reputation_score.get(),
                PeerScore::ACTIVE_THRESHOLD - 1
            );
            assert!(!updated_peer.reputation_score.is_reputable());

            peer_list.increase_peer_score_by_peer_id(&peer_id, ScoreIncreaseReason::Online);
            let final_peer = peer_list.get_peer(&peer_id).unwrap();
            assert_eq!(
                final_peer.reputation_score.get(),
                PeerScore::ACTIVE_THRESHOLD
            );
            assert!(final_peer.reputation_score.is_reputable());
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

            // BogusData decrements by 5; ACTIVE_THRESHOLD is 10. After 8
            // decrements the score is 50 − 40 = 10 (still active); 9
            // decrements take it to 5 (< 10), which trips the eviction gate.
            for _ in 0..9 {
                peer_list.decrease_peer_score_by_peer_id(
                    &peer_id,
                    ScoreDecreaseReason::BogusData("bogus".into()),
                );
            }

            let final_peer = peer_list.get_peer(&peer_id);
            assert!(
                final_peer.is_none(),
                "Unstaked peer should be removed once the reputation score \
                 crosses ACTIVE_THRESHOLD"
            );
        }

        /// Honest-overload regression: a transient network error against an
        /// unstaked purgatory peer must not drive eviction. Even a flood of
        /// `NetworkError` signals (the kind the divergence cascade produced)
        /// should leave the peer's score and presence intact.
        #[test]
        fn network_error_on_unstaked_purgatory_peer_does_not_decrement_or_evict() {
            let peer_list =
                create_test_peer_list(Config::new_with_random_peer_id(NodeConfig::testing()));
            let (_mining_addr, peer_id, mut peer) = create_test_peer(1);
            peer.reputation_score = PeerScore::new(50);
            peer_list.add_or_update_peer(peer, false);

            for _ in 0..1_000 {
                peer_list.decrease_peer_score_by_peer_id(
                    &peer_id,
                    ScoreDecreaseReason::NetworkError("transient".into()),
                );
            }

            let final_peer = peer_list
                .get_peer(&peer_id)
                .expect("purgatory peer must survive network errors");
            assert_eq!(
                final_peer.reputation_score.get(),
                50,
                "NetworkError must not decrement an unstaked purgatory peer's reputation"
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
        inactive_staked_peer.reputation_score = PeerScore::new(PeerScore::ACTIVE_THRESHOLD - 1); // Below active threshold
        inactive_unstaked_peer.reputation_score = PeerScore::new(PeerScore::ACTIVE_THRESHOLD - 1); // Below active threshold
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

        // Make sure peers have active reputation scores (above ACTIVE_THRESHOLD = 10)
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
                .filter(|peer| peer.is_active())
                .count();
            let purgatory_active = bindings
                .unstaked_peer_purgatory
                .iter()
                .map(|(_, v)| v)
                .filter(|peer| peer.is_active())
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
                .filter(|peer| peer.is_active())
                .count();
            let purgatory_active = bindings
                .unstaked_peer_purgatory
                .iter()
                .map(|(_, v)| v)
                .filter(|peer| peer.is_active())
                .count();
            persistent_active + purgatory_active
        };

        assert_eq!(
            active_peers_count_both, 2,
            "Both staked and unstaked active peers should be counted"
        );
    }

    #[tokio::test]
    async fn wait_for_n_peers_returns_immediately_when_already_satisfied() {
        let peer_list =
            create_test_peer_list(Config::new_with_random_peer_id(NodeConfig::testing()));

        let (_addr_a, _id_a, peer_a) = create_test_peer(1);
        let (_addr_b, _id_b, peer_b) = create_test_peer(2);
        peer_list.add_or_update_peer(peer_a, true);
        peer_list.add_or_update_peer(peer_b, true);

        // Both peers start with PeerScore::INITIAL = 50 which is >= PeerScore::ACTIVE_THRESHOLD
        // and is_online = true, so both are active+online.

        let start = std::time::Instant::now();
        let count = peer_list
            .wait_for_active_peers(2, Duration::from_secs(5))
            .await;
        let elapsed = start.elapsed();

        assert!(count >= 2, "expected at least 2 active peers, got {count}");
        assert!(
            elapsed < Duration::from_millis(100),
            "fast path should return ~immediately, took {:?}",
            elapsed
        );
    }

    #[tokio::test]
    async fn wait_for_n_peers_satisfied_via_events() {
        let peer_list =
            create_test_peer_list(Config::new_with_random_peer_id(NodeConfig::testing()));
        let waiter = peer_list.clone();

        let handle = tokio::spawn(async move {
            waiter
                .wait_for_active_peers(2, Duration::from_secs(2))
                .await
        });

        // Give the waiter time to enter the slow path
        tokio::time::sleep(Duration::from_millis(50)).await;

        let (_addr_a, _id_a, peer_a) = create_test_peer(1);
        peer_list.add_or_update_peer(peer_a, true);

        tokio::time::sleep(Duration::from_millis(50)).await;

        let (_addr_b, _id_b, peer_b) = create_test_peer(2);
        peer_list.add_or_update_peer(peer_b, true);

        let count = handle.await.expect("waiter task");
        assert!(
            count >= 2,
            "expected at least 2 after second peer added, got {count}"
        );
    }

    #[tokio::test]
    async fn wait_for_n_peers_timeout_partial() {
        let peer_list =
            create_test_peer_list(Config::new_with_random_peer_id(NodeConfig::testing()));

        let (_addr, _id, peer) = create_test_peer(1);
        peer_list.add_or_update_peer(peer, true);

        let start = std::time::Instant::now();
        let count = peer_list
            .wait_for_active_peers(3, Duration::from_millis(200))
            .await;
        let elapsed = start.elapsed();

        assert_eq!(count, 1, "only one peer was added");
        assert!(
            elapsed >= Duration::from_millis(190),
            "should have waited near the full timeout, elapsed={:?}",
            elapsed
        );
    }

    #[tokio::test]
    async fn wait_for_n_peers_timeout_zero_peers() {
        let peer_list =
            create_test_peer_list(Config::new_with_random_peer_id(NodeConfig::testing()));

        let count = peer_list
            .wait_for_active_peers(1, Duration::from_millis(200))
            .await;

        assert_eq!(count, 0, "no peers were added");
    }

    #[tokio::test]
    async fn wait_for_n_peers_recounts_when_peer_goes_offline() {
        let peer_list =
            create_test_peer_list(Config::new_with_random_peer_id(NodeConfig::testing()));

        let (addr_a, _id_a, peer_a) = create_test_peer(1);
        let (_addr_b, _id_b, peer_b) = create_test_peer(2);
        peer_list.add_or_update_peer(peer_a, true);
        peer_list.add_or_update_peer(peer_b, true);

        let waiter = peer_list.clone();
        let handle = tokio::spawn(async move {
            // N=3 with only 2 peers online; we want the slow path so the offline
            // transition is observed via PeerEvent.
            waiter
                .wait_for_active_peers(3, Duration::from_millis(300))
                .await
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        // Flip peer A offline mid-wait. This emits PeerEvent::BecameInactive.
        peer_list.set_is_online(&addr_a, false);

        let count = handle.await.expect("waiter task");
        assert_eq!(
            count, 1,
            "BecameInactive should drop A from the count without satisfying the wait"
        );
    }

    #[test]
    fn peer_id_change_with_same_address_updates_inner_peer_id_field() {
        // Regression: when a peer regenerates its libp2p key (e.g. peer_key.bin
        // wiped by reset.yml), it re-handshakes with the same mining_address and
        // gossip/api address but a fresh peer_id. The cache must re-key the entry
        // AND update the inner PeerListItem.peer_id field — server.rs:check_peer_v2
        // reads that field to authorise V2 gossip and rejects with HandshakeRequired
        // on mismatch, so a stale field traps the cluster in a handshake loop.
        let peer_list =
            create_test_peer_list(Config::new_with_random_peer_id(NodeConfig::testing()));
        let (mining_addr, peer_id_old, peer) = create_test_peer(1);
        let peer_address = peer.address;

        peer_list.add_or_update_peer(peer.clone(), true);

        let peer_id_new = IrysPeerId::from([99_u8; 20]);
        let mut updated = peer;
        updated.peer_id = peer_id_new;
        peer_list.add_or_update_peer(updated, true);

        let stored = peer_list
            .peer_by_id(&peer_id_new)
            .expect("peer reachable by new peer_id");
        assert!(
            peer_list.peer_by_id(&peer_id_old).is_none(),
            "old peer_id must be evicted",
        );
        assert_eq!(
            stored.peer_id, peer_id_new,
            "inner peer_id field must match new peer_id",
        );
        assert_eq!(stored.mining_address, mining_addr);

        let inner = peer_list.read();
        assert_eq!(
            inner.miner_addr_to_peer_id_map.get(&mining_addr).copied(),
            Some(peer_id_new),
        );
        assert!(!inner.peer_id_to_miner_addr_map.contains_key(&peer_id_old));
        assert_eq!(
            inner.peer_id_to_miner_addr_map.get(&peer_id_new).copied(),
            Some(mining_addr),
        );
        assert_eq!(
            inner
                .gossip_addr_to_peer_id_map
                .get(&peer_address.gossip.ip())
                .copied(),
            Some(peer_id_new),
        );
        assert_eq!(
            inner
                .api_addr_to_peer_id_map
                .get(&peer_address.api)
                .copied(),
            Some(peer_id_new),
        );
    }

    #[test]
    fn promoting_unstaked_peer_keeps_handshake_version_and_hash() {
        let peer_list =
            create_test_peer_list(Config::new_with_random_peer_id(NodeConfig::testing()));
        let (_addr, peer_id, mut peer) = create_test_peer(1);
        peer.protocol_version = ProtocolVersion::V1;
        peer.software_version = Some("4.0.5+irys-rs".parse().unwrap());
        peer.consensus_config_hash = None;
        peer_list.add_or_update_peer(peer.clone(), false);

        peer.protocol_version = ProtocolVersion::V2;
        peer.software_version = Some("4.0.6+irys-rs".parse().unwrap());
        peer.consensus_config_hash = Some(H256::from([7; 32]));
        peer_list.add_or_update_peer(peer, true);

        let stored = peer_list.peer_by_id(&peer_id).unwrap();
        assert_eq!(stored.protocol_version, ProtocolVersion::V2);
        assert_eq!(stored.software_version_string(), "4.0.6+irys-rs");
        assert_eq!(stored.consensus_config_hash, Some(H256::from([7; 32])));
    }

    #[test]
    fn outbound_handshake_observation_applies_when_peer_is_inserted() {
        let peer_list =
            create_test_peer_list(Config::new_with_random_peer_id(NodeConfig::testing()));
        let (_addr, peer_id, peer) = create_test_peer(1);

        peer_list.observe_handshake(
            peer.address.api,
            Some("4.0.6+irys-rs.6cbc03b".parse().unwrap()),
            Some(H256::from([9; 32])),
        );
        assert!(
            peer_list.peer_by_id(&peer_id).is_none(),
            "observation must not invent a peer without mining address"
        );

        peer_list.add_or_update_peer(peer, true);
        let stored = peer_list.peer_by_id(&peer_id).unwrap();
        assert_eq!(stored.software_version_string(), "4.0.6+irys-rs.6cbc03b");
        assert_eq!(stored.consensus_config_hash, Some(H256::from([9; 32])));
    }

    fn list_from_loaded_peers(peers: Vec<PeerListItem>) -> PeerList {
        let (tx, _rx) = mpsc::unbounded_channel();
        let (events, _) = broadcast::channel::<PeerEvent>(100);
        PeerList::from_peers(
            peers,
            PeerNetworkSender::new(tx),
            &Config::new_with_random_peer_id(NodeConfig::testing()),
            events,
        )
        .expect("peer list")
    }

    /// Same host as the mainnet duplicate: one gossip socket, two API ports.
    fn mainnet_shaped_peer(
        mining: IrysAddress,
        peer_id: IrysPeerId,
        api_port: u16,
        last_seen: u64,
    ) -> PeerListItem {
        PeerListItem {
            peer_id,
            mining_address: mining,
            address: PeerAddress {
                gossip: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(89, 35, 53, 102)), 9009),
                api: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(89, 35, 53, 102)), api_port),
                execution: RethPeerInfo {
                    peering_tcp_addr: SocketAddr::new(
                        IpAddr::V4(Ipv4Addr::new(89, 35, 53, 102)),
                        30303,
                    ),
                    ..RethPeerInfo::default()
                },
            },
            reputation_score: PeerScore::new(PeerScore::INITIAL),
            response_time: 100,
            is_online: true,
            last_seen,
            protocol_version: ProtocolVersion::V2,
            ..Default::default()
        }
    }

    fn advertised_api_ports(peer_list: &PeerList) -> Vec<u16> {
        let mut ports: Vec<u16> = peer_list
            .all_known_peers()
            .into_iter()
            .map(|addr| addr.api.port())
            .collect();
        ports.sort_unstable();
        ports
    }

    #[test]
    fn db_load_collapses_v1_and_v2_same_miner_to_one_address() {
        let mining = IrysAddress::from([0x4A; 20]);
        let v1_id = IrysPeerId::from(mining);
        let v2_id = IrysPeerId::from([0xB2; 20]);
        let v1 = mainnet_shaped_peer(mining, v1_id, 80, 1_000);
        let v2 = mainnet_shaped_peer(mining, v2_id, 8080, 2_000);

        let peer_list = list_from_loaded_peers(vec![v1, v2]);

        assert_eq!(peer_list.peer_count(), 1, "one cached item per miner");
        assert_eq!(advertised_api_ports(&peer_list), vec![8080]);
        assert!(peer_list.peer_by_id(&v1_id).is_none());
        assert_eq!(
            peer_list.peer_by_id(&v2_id).map(|p| p.address.api.port()),
            Some(8080)
        );
        assert_eq!(
            peer_list.take_pending_db_removals(),
            std::iter::once(v1_id).collect()
        );
    }

    #[test]
    fn handshake_api_port_change_keeps_latest_socket() {
        let peer_list =
            create_test_peer_list(Config::new_with_random_peer_id(NodeConfig::testing()));
        let mining = IrysAddress::from([0x11; 20]);
        let peer_id = IrysPeerId::from([0x22; 20]);
        peer_list.add_or_update_peer(mainnet_shaped_peer(mining, peer_id, 80, 1_000), true);
        peer_list.add_or_update_peer(mainnet_shaped_peer(mining, peer_id, 8080, 2_000), true);

        assert_eq!(peer_list.peer_count(), 1);
        assert_eq!(advertised_api_ports(&peer_list), vec![8080]);
        let stored = peer_list.peer_by_id(&peer_id).unwrap();
        assert_eq!(stored.address.api.port(), 8080);
        assert_eq!(stored.address.gossip.port(), 9009);
    }

    #[test]
    fn v1_then_v2_handshake_migrates_and_drops_old_address() {
        let peer_list =
            create_test_peer_list(Config::new_with_random_peer_id(NodeConfig::testing()));
        let mining = IrysAddress::from([0x33; 20]);
        let v1_id = IrysPeerId::from(mining);
        let v2_id = IrysPeerId::from([0x44; 20]);
        peer_list.add_or_update_peer(mainnet_shaped_peer(mining, v1_id, 80, 1_000), true);
        peer_list.add_or_update_peer(mainnet_shaped_peer(mining, v2_id, 8080, 2_000), true);

        assert_eq!(peer_list.peer_count(), 1);
        assert_eq!(advertised_api_ports(&peer_list), vec![8080]);
        assert!(peer_list.peer_by_id(&v1_id).is_none());
        assert_eq!(
            peer_list.peer_by_mining_address(&mining).unwrap().peer_id,
            v2_id
        );
        assert!(
            peer_list.take_pending_db_removals().contains(&v1_id),
            "old peer_id must be staged so flush deletes the leftover DB row"
        );
    }

    #[test]
    fn two_miners_on_the_same_ip_both_appear() {
        let peer_list =
            create_test_peer_list(Config::new_with_random_peer_id(NodeConfig::testing()));
        let miner_a = IrysAddress::from([0x01; 20]);
        let miner_b = IrysAddress::from([0x02; 20]);
        peer_list.add_or_update_peer(
            mainnet_shaped_peer(miner_a, IrysPeerId::from([0xA1; 20]), 8080, 1_000),
            true,
        );
        let mut other = mainnet_shaped_peer(miner_b, IrysPeerId::from([0xB1; 20]), 8081, 1_000);
        other.address.gossip.set_port(9010);
        peer_list.add_or_update_peer(other, true);

        assert_eq!(peer_list.peer_count(), 2);
        let mut ports = advertised_api_ports(&peer_list);
        ports.sort_unstable();
        assert_eq!(ports, vec![8080, 8081]);
    }

    #[test]
    fn observers_sharing_a_mining_key_stay_two_rows() {
        // Observers never stake; they still have a config key. Two processes
        // that copied the same key must remain two peers — identity is peer_id
        // (and the gossip listen socket), not mining address.
        let peer_list =
            create_test_peer_list(Config::new_with_random_peer_id(NodeConfig::testing()));
        let shared_key = IrysAddress::from([0x0B; 20]);
        let observer_a = IrysPeerId::from([0xA1; 20]);
        let observer_b = IrysPeerId::from([0xB2; 20]);
        peer_list.add_or_update_peer(
            mainnet_shaped_peer(shared_key, observer_a, 8080, 1_000),
            false,
        );
        let mut other = mainnet_shaped_peer(shared_key, observer_b, 8081, 1_000);
        other.address.gossip.set_port(9010);
        peer_list.add_or_update_peer(other, false);

        assert_eq!(peer_list.peer_count(), 2);
        assert!(peer_list.peer_by_id(&observer_a).is_some());
        assert!(peer_list.peer_by_id(&observer_b).is_some());
        assert_eq!(advertised_api_ports(&peer_list), vec![8080, 8081]);
    }

    #[test]
    fn handshake_after_duplicate_load_keeps_one_row() {
        let mining = IrysAddress::from([0x55; 20]);
        let v1_id = IrysPeerId::from(mining);
        let v2_id = IrysPeerId::from([0x66; 20]);
        let peer_list = list_from_loaded_peers(vec![
            mainnet_shaped_peer(mining, v1_id, 80, 1_000),
            mainnet_shaped_peer(mining, v2_id, 8080, 1_500),
        ]);
        peer_list.add_or_update_peer(mainnet_shaped_peer(mining, v2_id, 8080, 3_000), true);

        assert_eq!(peer_list.peer_count(), 1);
        assert_eq!(advertised_api_ports(&peer_list), vec![8080]);
        assert_eq!(peer_list.all_peers().iter().count(), 1);
    }

    #[test]
    fn recovered_score_is_advertised_again() {
        let peer_list =
            create_test_peer_list(Config::new_with_random_peer_id(NodeConfig::testing()));
        let (_mining, peer_id, mut peer) = create_test_peer(1);
        peer.reputation_score = PeerScore::new(14);
        peer_list.add_or_update_peer(peer.clone(), true);
        assert!(peer_list.all_known_peers().contains(&peer.address));

        peer_list.decrease_peer_score_by_peer_id(
            &peer_id,
            ScoreDecreaseReason::BogusData("bogus".into()),
        );
        assert!(!peer_list.all_known_peers().contains(&peer.address));

        peer_list.increase_peer_score_by_peer_id(&peer_id, ScoreIncreaseReason::Online);
        assert!(
            peer_list.all_known_peers().contains(&peer.address),
            "a peer that recovers to the reputable threshold must be advertised again"
        );
    }
}
