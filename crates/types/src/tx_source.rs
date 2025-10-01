/// Identifies where a transaction originated from when being ingested into the mempool.
///
/// - Gossip: received from the p2p network
/// - Api: submitted directly via the node's API
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TxSource {
    Gossip,
    Api,
}
