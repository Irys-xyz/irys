use irys_types::{IrysPeerId, PeerAddress};
use rand::seq::SliceRandom as _;
use std::collections::HashSet;

/// Select k push targets: ⌈k/2⌉ from top-scored, remainder random from the rest.
///
/// Candidates must already be filtered — excluded peers (e.g. the chunk
/// origin or peers that already have the chunk) should be removed before
/// calling this function.
///
/// When fewer candidates than k are available the entire candidate list is
/// returned. Returned order is unspecified.
#[must_use]
pub(crate) fn select_push_targets(
    k: u32,
    candidates: &[(IrysPeerId, PeerAddress, f64)],
) -> Vec<(IrysPeerId, PeerAddress)> {
    if candidates.is_empty() || k == 0 {
        return vec![];
    }

    let k = k as usize;
    let half = k.div_ceil(2);
    let other_half = k - half;

    // Sort by score descending for top-half selection.
    let mut sorted: Vec<_> = candidates.to_vec();
    sorted.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

    let mut selected: HashSet<IrysPeerId> = HashSet::new();
    let mut result: Vec<(IrysPeerId, PeerAddress)> = Vec::with_capacity(k);

    // Take the top-scored half.
    for (id, addr, _) in sorted.iter().take(half) {
        if selected.insert(*id) {
            result.push((*id, *addr));
        }
    }

    // Randomly sample the remaining half from what is left.
    let mut remaining: Vec<_> = sorted
        .into_iter()
        .filter(|(id, _, _)| !selected.contains(id))
        .collect();

    let mut rng = rand::thread_rng();
    remaining.shuffle(&mut rng);

    for (id, addr, _) in remaining.into_iter().take(other_half) {
        if selected.insert(id) {
            result.push((id, addr));
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn make_candidate(byte: u8, score: f64) -> (IrysPeerId, PeerAddress, f64) {
        let addr: SocketAddr = format!("127.0.0.1:{}", 3000 + byte as u16)
            .parse()
            .expect("valid SocketAddr");
        (
            IrysPeerId::from([byte; 20]),
            PeerAddress {
                api: addr,
                gossip: addr,
                execution: Default::default(),
            },
            score,
        )
    }

    #[test]
    fn test_empty_candidates_returns_empty() {
        let result = select_push_targets(4, &[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_zero_k_returns_empty() {
        let candidates = vec![make_candidate(1, 10.0)];
        let result = select_push_targets(0, &candidates);
        assert!(result.is_empty());
    }

    #[test]
    fn test_fewer_candidates_than_k() {
        let candidates = vec![make_candidate(1, 10.0), make_candidate(2, 5.0)];
        let result = select_push_targets(4, &candidates);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_returns_exactly_k_when_enough_candidates() {
        let candidates: Vec<_> = (1..=10_u8)
            .map(|i| make_candidate(i, 10.0 - i as f64))
            .collect();
        let result = select_push_targets(4, &candidates);
        assert_eq!(result.len(), 4);
    }

    #[test]
    fn test_no_duplicates() {
        let candidates: Vec<_> = (1..=10_u8)
            .map(|i| make_candidate(i, 10.0 - i as f64))
            .collect();
        let result = select_push_targets(4, &candidates);
        let ids: HashSet<_> = result.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids.len(), result.len());
    }

    #[test]
    fn test_top_scored_included() {
        // k=4 → top 2 (k/2) must always be the two highest-scored peers.
        let candidates = vec![
            make_candidate(1, 100.0),
            make_candidate(2, 50.0),
            make_candidate(3, 1.0),
            make_candidate(4, 1.0),
            make_candidate(5, 1.0),
            make_candidate(6, 1.0),
        ];
        let result = select_push_targets(4, &candidates);
        let ids: HashSet<_> = result.iter().map(|(id, _)| *id).collect();
        assert!(
            ids.contains(&IrysPeerId::from([1; 20])),
            "highest-scored peer must be selected"
        );
        assert!(
            ids.contains(&IrysPeerId::from([2; 20])),
            "second-highest peer must be selected"
        );
        assert_eq!(result.len(), 4);
    }

    #[test]
    fn test_k1_returns_one() {
        let candidates: Vec<_> = (1..=5_u8).map(|i| make_candidate(i, i as f64)).collect();
        // k=1 → half=1, other_half=0 → the single top-scored peer.
        let result = select_push_targets(1, &candidates);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_k1_top_scored_bias() {
        // With k=1 and half=1 (ceiling division), the single selected peer
        // is always the top-scored one.
        let candidates: Vec<_> = (1..=5_u8).map(|i| make_candidate(i, i as f64)).collect();
        let result = select_push_targets(1, &candidates);
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].0,
            IrysPeerId::from([5; 20]),
            "k=1 must select the highest-scored peer"
        );
    }
}
