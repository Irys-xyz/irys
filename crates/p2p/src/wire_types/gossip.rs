use std::sync::Arc;

use irys_types::{BlockHash, ChunkPathHash, IrysAddress, IrysPeerId, H256};
use reth::revm::primitives::B256;
use reth_ethereum_primitives::Block as RethBlock;
use serde::{Deserialize, Serialize};

use super::{
    BlockBody, CommitmentTransaction, DataTransactionHeader, IngressProof, IrysBlockHeader,
    UnpackedChunk,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GossipDataV1 {
    Chunk(UnpackedChunk),
    Transaction(DataTransactionHeader),
    CommitmentTransaction(CommitmentTransaction),
    Block(IrysBlockHeader),
    ExecutionPayload(RethBlock),
    IngressProof(IngressProof),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GossipDataV2 {
    Chunk(UnpackedChunk),
    Transaction(DataTransactionHeader),
    CommitmentTransaction(CommitmentTransaction),
    BlockHeader(IrysBlockHeader),
    BlockBody(BlockBody),
    ExecutionPayload(RethBlock),
    IngressProof(IngressProof),
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub enum GossipDataRequestV1 {
    ExecutionPayload(B256),
    Block(BlockHash),
    Chunk(ChunkPathHash),
    Transaction(H256),
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub enum GossipDataRequestV2 {
    ExecutionPayload(B256),
    BlockHeader(BlockHash),
    BlockBody(BlockHash),
    Chunk(ChunkPathHash),
    Transaction(H256),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GossipRequestV1<T> {
    pub miner_address: IrysAddress,
    pub data: T,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GossipRequestV2<T> {
    pub peer_id: IrysPeerId,
    pub miner_address: IrysAddress,
    pub data: T,
}

// -- Conversions for GossipDataV1 --

impl From<&irys_types::gossip::v1::GossipDataV1> for GossipDataV1 {
    fn from(d: &irys_types::gossip::v1::GossipDataV1) -> Self {
        match d {
            irys_types::gossip::v1::GossipDataV1::Chunk(c) => Self::Chunk(c.into()),
            irys_types::gossip::v1::GossipDataV1::Transaction(t) => Self::Transaction(t.into()),
            irys_types::gossip::v1::GossipDataV1::CommitmentTransaction(c) => {
                Self::CommitmentTransaction(c.into())
            }
            irys_types::gossip::v1::GossipDataV1::Block(b) => Self::Block(b.as_ref().into()),
            irys_types::gossip::v1::GossipDataV1::ExecutionPayload(p) => {
                Self::ExecutionPayload(p.clone())
            }
            irys_types::gossip::v1::GossipDataV1::IngressProof(p) => Self::IngressProof(p.into()),
        }
    }
}

impl TryFrom<GossipDataV1> for irys_types::gossip::v1::GossipDataV1 {
    type Error = eyre::Report;
    fn try_from(d: GossipDataV1) -> eyre::Result<Self> {
        match d {
            GossipDataV1::Chunk(c) => Ok(Self::Chunk(c.into())),
            GossipDataV1::Transaction(t) => Ok(Self::Transaction(t.try_into()?)),
            GossipDataV1::CommitmentTransaction(c) => {
                Ok(Self::CommitmentTransaction(c.try_into()?))
            }
            GossipDataV1::Block(b) => {
                let canonical: irys_types::IrysBlockHeader = b.try_into()?;
                Ok(Self::Block(Arc::new(canonical)))
            }
            GossipDataV1::ExecutionPayload(p) => Ok(Self::ExecutionPayload(p)),
            GossipDataV1::IngressProof(p) => Ok(Self::IngressProof(p.try_into()?)),
        }
    }
}

// -- Conversions for GossipDataV2 --

impl From<&irys_types::gossip::v2::GossipDataV2> for GossipDataV2 {
    fn from(d: &irys_types::gossip::v2::GossipDataV2) -> Self {
        match d {
            irys_types::gossip::v2::GossipDataV2::Chunk(c) => Self::Chunk(c.as_ref().into()),
            irys_types::gossip::v2::GossipDataV2::Transaction(t) => Self::Transaction(t.into()),
            irys_types::gossip::v2::GossipDataV2::CommitmentTransaction(c) => {
                Self::CommitmentTransaction(c.into())
            }
            irys_types::gossip::v2::GossipDataV2::BlockHeader(b) => {
                Self::BlockHeader(b.as_ref().into())
            }
            irys_types::gossip::v2::GossipDataV2::BlockBody(b) => {
                Self::BlockBody(b.as_ref().into())
            }
            irys_types::gossip::v2::GossipDataV2::ExecutionPayload(p) => {
                Self::ExecutionPayload(p.clone())
            }
            irys_types::gossip::v2::GossipDataV2::IngressProof(p) => {
                Self::IngressProof(p.into())
            }
        }
    }
}

impl TryFrom<GossipDataV2> for irys_types::gossip::v2::GossipDataV2 {
    type Error = eyre::Report;
    fn try_from(d: GossipDataV2) -> eyre::Result<Self> {
        match d {
            GossipDataV2::Chunk(c) => {
                let canonical: irys_types::UnpackedChunk = c.into();
                Ok(Self::Chunk(Arc::new(canonical)))
            }
            GossipDataV2::Transaction(t) => Ok(Self::Transaction(t.try_into()?)),
            GossipDataV2::CommitmentTransaction(c) => {
                Ok(Self::CommitmentTransaction(c.try_into()?))
            }
            GossipDataV2::BlockHeader(b) => {
                let canonical: irys_types::IrysBlockHeader = b.try_into()?;
                Ok(Self::BlockHeader(Arc::new(canonical)))
            }
            GossipDataV2::BlockBody(b) => {
                let canonical: irys_types::BlockBody = b.try_into()?;
                Ok(Self::BlockBody(Arc::new(canonical)))
            }
            GossipDataV2::ExecutionPayload(p) => Ok(Self::ExecutionPayload(p)),
            GossipDataV2::IngressProof(p) => Ok(Self::IngressProof(p.try_into()?)),
        }
    }
}

// -- Conversions for GossipDataRequestV1 --

impl From<&irys_types::gossip::v1::GossipDataRequestV1> for GossipDataRequestV1 {
    fn from(r: &irys_types::gossip::v1::GossipDataRequestV1) -> Self {
        match r {
            irys_types::gossip::v1::GossipDataRequestV1::ExecutionPayload(h) => {
                Self::ExecutionPayload(*h)
            }
            irys_types::gossip::v1::GossipDataRequestV1::Block(h) => Self::Block(*h),
            irys_types::gossip::v1::GossipDataRequestV1::Chunk(h) => Self::Chunk(*h),
            irys_types::gossip::v1::GossipDataRequestV1::Transaction(h) => Self::Transaction(*h),
        }
    }
}

impl From<GossipDataRequestV1> for irys_types::gossip::v1::GossipDataRequestV1 {
    fn from(r: GossipDataRequestV1) -> Self {
        match r {
            GossipDataRequestV1::ExecutionPayload(h) => Self::ExecutionPayload(h),
            GossipDataRequestV1::Block(h) => Self::Block(h),
            GossipDataRequestV1::Chunk(h) => Self::Chunk(h),
            GossipDataRequestV1::Transaction(h) => Self::Transaction(h),
        }
    }
}

// -- Conversions for GossipDataRequestV2 --

impl From<&irys_types::gossip::v2::GossipDataRequestV2> for GossipDataRequestV2 {
    fn from(r: &irys_types::gossip::v2::GossipDataRequestV2) -> Self {
        match r {
            irys_types::gossip::v2::GossipDataRequestV2::ExecutionPayload(h) => {
                Self::ExecutionPayload(*h)
            }
            irys_types::gossip::v2::GossipDataRequestV2::BlockHeader(h) => Self::BlockHeader(*h),
            irys_types::gossip::v2::GossipDataRequestV2::BlockBody(h) => Self::BlockBody(*h),
            irys_types::gossip::v2::GossipDataRequestV2::Chunk(h) => Self::Chunk(*h),
            irys_types::gossip::v2::GossipDataRequestV2::Transaction(h) => Self::Transaction(*h),
        }
    }
}

impl From<GossipDataRequestV2> for irys_types::gossip::v2::GossipDataRequestV2 {
    fn from(r: GossipDataRequestV2) -> Self {
        match r {
            GossipDataRequestV2::ExecutionPayload(h) => Self::ExecutionPayload(h),
            GossipDataRequestV2::BlockHeader(h) => Self::BlockHeader(h),
            GossipDataRequestV2::BlockBody(h) => Self::BlockBody(h),
            GossipDataRequestV2::Chunk(h) => Self::Chunk(h),
            GossipDataRequestV2::Transaction(h) => Self::Transaction(h),
        }
    }
}

// -- Conversions for GossipRequestV1/V2 --

impl<T, U: From<T>> From<irys_types::GossipRequestV1<T>> for GossipRequestV1<U> {
    fn from(r: irys_types::GossipRequestV1<T>) -> Self {
        Self {
            miner_address: r.miner_address,
            data: r.data.into(),
        }
    }
}

impl<T, U: From<T>> From<GossipRequestV1<T>> for irys_types::GossipRequestV1<U> {
    fn from(r: GossipRequestV1<T>) -> Self {
        Self {
            miner_address: r.miner_address,
            data: r.data.into(),
        }
    }
}

impl<T, U: From<T>> From<irys_types::GossipRequestV2<T>> for GossipRequestV2<U> {
    fn from(r: irys_types::GossipRequestV2<T>) -> Self {
        Self {
            peer_id: r.peer_id,
            miner_address: r.miner_address,
            data: r.data.into(),
        }
    }
}

impl<T, U: From<T>> From<GossipRequestV2<T>> for irys_types::GossipRequestV2<U> {
    fn from(r: GossipRequestV2<T>) -> Self {
        Self {
            peer_id: r.peer_id,
            miner_address: r.miner_address,
            data: r.data.into(),
        }
    }
}
