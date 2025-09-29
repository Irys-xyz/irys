use irys_primitives::Address;
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
}
