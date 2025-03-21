use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use thiserror::Error;
use irys_types::{IrysTransactionHeader, UnpackedChunk};
use irys_api_server::CombinedBlockHeader;

#[derive(Debug, Error)]
pub enum GossipError {
    #[error("Network error: {0}")]
    Network(String),
    #[error("Invalid peer: {0}")]
    InvalidPeer(String),
    #[error("Cache error: {0}")]
    Cache(String),
    #[error("Internal error: {0}")]
    Internal(String),
}

pub type GossipResult<T> = Result<T, GossipError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub ip: IpAddr,
    pub port: u16,
    pub private_api_port: u16,
    pub score: PeerScore,
    pub last_seen: i64,
    pub is_online: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PeerScore(u8);

impl PeerScore {
    pub const MIN: u8 = 0;
    pub const MAX: u8 = 100;
    pub const INITIAL: u8 = 50;
    pub const ACTIVE_THRESHOLD: u8 = 20;

    pub fn new(score: u8) -> Self {
        Self(score.clamp(Self::MIN, Self::MAX))
    }

    pub fn increase(&mut self) {
        self.0 = (self.0 + 1).min(Self::MAX);
    }

    pub fn decrease_offline(&mut self) {
        self.0 = self.0.saturating_sub(3);
    }

    pub fn decrease_bogus_data(&mut self) {
        self.0 = self.0.saturating_sub(5);
    }

    pub fn is_active(&self) -> bool {
        self.0 >= Self::ACTIVE_THRESHOLD
    }

    pub fn get(&self) -> u8 {
        self.0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GossipData {
    Chunk(UnpackedChunk),
    Transaction(IrysTransactionHeader),
    Block(CombinedBlockHeader),
} 