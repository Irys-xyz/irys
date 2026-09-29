// Deeply-nested `tracing::instrument` async blocks (e.g. the sync task in
// `chain_sync`) exceed the default type-layout query depth of 128.
#![recursion_limit = "256"]
// This crate decides what to do with peer-supplied protocol enums, so a
// wildcard arm here hands every future variant an answer chosen for its
// neighbours — how a peer rejecting us for a chain-id mismatch came to be
// reported as reachable. Name the variants instead; add `#[expect]` at the
// site when the enum is foreign and cannot be matched exhaustively.
// Production paths only: a test that stops covering a new variant fails on its
// own terms, and pinning every fixture match adds noise without protecting the
// protocol.
#![cfg_attr(not(test), deny(clippy::wildcard_enum_match_arm))]

mod block_pool;
mod block_status_provider;
mod cache;
mod chain_sync;
mod gossip_client;
mod gossip_data_handler;
#[cfg(test)]
mod gossip_fixture_tests;
mod gossip_service;
mod metrics;
mod peer_network_service;
mod rate_limiting;
mod server;
#[cfg(test)]
mod tests;
mod types;
pub(crate) mod wire_types;

pub use block_pool::{BlockPool, BlockPoolError};
pub use block_status_provider::{BlockStatus, BlockStatusProvider};
pub use cache::GossipCache;
pub use chain_sync::{
    ChainSyncError, ChainSyncResult, ChainSyncService, ChainSyncServiceInner,
    SyncChainServiceFacade, SyncChainServiceMessage,
};
pub use gossip_client::GossipClient;
pub use gossip_data_handler::GossipDataHandler;
pub use gossip_service::P2PService;
pub use gossip_service::ServiceHandleWithShutdownSignal;
pub use gossip_service::spawn_p2p_server_watcher_task;
pub use peer_network_service::{PeerListServiceError, spawn_peer_network_service};
pub use rate_limiting::DataRequestTracker;
pub use server::GossipServer;
pub use types::{GossipError, GossipResponse, GossipResult, GossipRoutes, RejectionReason};
