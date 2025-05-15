pub mod block_pool_service;
pub mod cache;
pub mod client;
pub mod peer_list_service;
pub mod server;
mod server_data_handler;
pub mod service;
pub mod types;

pub use cache::GossipCache;
pub use client::GossipClient;
pub use server::GossipServer;
pub use service::GossipService;
pub use service::ServiceHandleWithShutdownSignal;

pub use types::{GossipError, GossipResult};

#[cfg(test)]
mod tests;
