use irys_types::IrysPeerId;
use moka::sync::Cache;
use std::collections::HashSet;
use std::sync::{Arc, RwLock};
use std::time::Duration;

const INBOUND_PUSH_TRACKER_TTL: Duration = Duration::from_secs(300);

pub struct InboundPushTracker {
    cache: Cache<(u32, u64), Arc<RwLock<HashSet<IrysPeerId>>>>,
}

impl InboundPushTracker {
    pub fn new() -> Self {
        Self {
            cache: Cache::builder()
                .time_to_live(INBOUND_PUSH_TRACKER_TTL)
                .build(),
        }
    }

    pub fn record_inbound(&self, ledger: u32, offset: u64, peer_id: IrysPeerId) {
        let entry = self
            .cache
            .get_with((ledger, offset), || Arc::new(RwLock::new(HashSet::new())));
        entry.write().unwrap().insert(peer_id);
    }

    pub fn get_known_sources(&self, ledger: u32, offset: u64) -> HashSet<IrysPeerId> {
        self.cache
            .get(&(ledger, offset))
            .map(|entry| entry.read().unwrap().clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_peer_id(byte: u8) -> IrysPeerId {
        IrysPeerId::from([byte; 20])
    }

    #[test]
    fn test_record_and_retrieve() {
        let tracker = InboundPushTracker::new();
        let peer_a = test_peer_id(0xAA);
        let peer_b = test_peer_id(0xBB);

        tracker.record_inbound(0, 100, peer_a);
        tracker.record_inbound(0, 100, peer_b);

        let sources = tracker.get_known_sources(0, 100);
        assert_eq!(sources.len(), 2);
        assert!(sources.contains(&peer_a));
        assert!(sources.contains(&peer_b));
    }

    #[test]
    fn test_unknown_key_returns_empty() {
        let tracker = InboundPushTracker::new();
        let sources = tracker.get_known_sources(0, 999);
        assert!(sources.is_empty());
    }

    #[test]
    fn test_different_offsets_are_independent() {
        let tracker = InboundPushTracker::new();
        let peer_a = test_peer_id(0xAA);

        tracker.record_inbound(0, 100, peer_a);

        assert_eq!(tracker.get_known_sources(0, 100).len(), 1);
        assert_eq!(tracker.get_known_sources(0, 200).len(), 0);
    }
}
