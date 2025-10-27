use irys_types::Address;
use irys_types::PeerListItem;

/// Events related to peer lifecycle and activity.
///
/// Keep this minimal and generic so other crates (p2p, actors) can subscribe
/// without introducing circular dependencies.
#[derive(Debug, Clone)]
pub enum PeerEvent {
    /// Emitted when a peer transitions from inactive to active based on its reputation score.
    BecameActive {
        mining_addr: Address,
        peer: PeerListItem,
    },
    /// Emitted when a peer transitions from active/online to inactive or offline
    BecameInactive {
        mining_addr: Address,
        peer: PeerListItem,
    },
    /// Emitted when a peer's important metadata changes (e.g., address/handshake refresh)
    PeerUpdated {
        mining_addr: Address,
        peer: PeerListItem,
    },
    /// Emitted when a peer (usually unstaked) is removed from all caches
    PeerRemoved {
        mining_addr: Address,
        peer: PeerListItem,
    },
}
