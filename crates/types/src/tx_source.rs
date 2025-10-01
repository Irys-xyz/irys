/// Identifies where a transaction originated from when being ingested into the mempool.
/// This affects the validation done by the mempool. Transactions arriving from the Api (posted by
/// users) are validated as much as possible immediately, while transactions arriving from
/// Gossip (other nodes) are only partially validated, and are going to pass through the full
/// validation when being included in a block - i.e. produced locally or during prevalidation of
/// the block produced by someone else.
///
/// - Gossip: received from the p2p network
/// - Api: submitted directly via the node's API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TxSource {
    Gossip,
    Api,
}
