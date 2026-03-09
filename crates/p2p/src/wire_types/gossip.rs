use std::sync::Arc;

use irys_types::{BlockHash, ChunkPathHash, IrysAddress, IrysPeerId, H256};
use reth::revm::primitives::B256;
use reth_ethereum_primitives::Block as RethBlock;
use serde::{Deserialize, Serialize};

use super::{
    BlockBody, CommitmentTransaction, DataTransactionHeader, IngressProof, IrysBlockHeader,
    UnpackedChunk,
};

/// V1 gossip data envelope. Each variant wraps a wire type for serialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GossipDataV1 {
    Chunk(UnpackedChunk),
    Transaction(DataTransactionHeader),
    CommitmentTransaction(CommitmentTransaction),
    Block(IrysBlockHeader),
    ExecutionPayload(RethBlock),
    IngressProof(IngressProof),
}

/// V2 gossip data envelope. Splits Block into separate BlockHeader and BlockBody variants.
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

/// V1 data request — identifies data to fetch by hash.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub enum GossipDataRequestV1 {
    ExecutionPayload(B256),
    Block(BlockHash),
    Chunk(ChunkPathHash),
    Transaction(H256),
}

/// V2 data request — adds separate BlockHeader/BlockBody request variants.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub enum GossipDataRequestV2 {
    ExecutionPayload(B256),
    BlockHeader(BlockHash),
    BlockBody(BlockHash),
    Chunk(ChunkPathHash),
    Transaction(H256),
}

/// V1 request wrapper — carries miner_address + payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GossipRequestV1<T> {
    pub miner_address: IrysAddress,
    pub data: T,
}

/// V2 request wrapper — adds peer_id to the V1 wrapper.
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

impl From<GossipDataV1> for irys_types::gossip::v1::GossipDataV1 {
    fn from(d: GossipDataV1) -> Self {
        match d {
            GossipDataV1::Chunk(c) => Self::Chunk(c.into()),
            GossipDataV1::Transaction(t) => Self::Transaction(t.into()),
            GossipDataV1::CommitmentTransaction(c) => Self::CommitmentTransaction(c.into()),
            GossipDataV1::Block(b) => {
                let canonical: irys_types::IrysBlockHeader = b.into();
                Self::Block(Arc::new(canonical))
            }
            GossipDataV1::ExecutionPayload(p) => Self::ExecutionPayload(p),
            GossipDataV1::IngressProof(p) => Self::IngressProof(p.into()),
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
            irys_types::gossip::v2::GossipDataV2::IngressProof(p) => Self::IngressProof(p.into()),
        }
    }
}

impl From<GossipDataV2> for irys_types::gossip::v2::GossipDataV2 {
    fn from(d: GossipDataV2) -> Self {
        match d {
            GossipDataV2::Chunk(c) => {
                let canonical: irys_types::UnpackedChunk = c.into();
                Self::Chunk(Arc::new(canonical))
            }
            GossipDataV2::Transaction(t) => Self::Transaction(t.into()),
            GossipDataV2::CommitmentTransaction(c) => Self::CommitmentTransaction(c.into()),
            GossipDataV2::BlockHeader(b) => {
                let canonical: irys_types::IrysBlockHeader = b.into();
                Self::BlockHeader(Arc::new(canonical))
            }
            GossipDataV2::BlockBody(b) => {
                let canonical: irys_types::BlockBody = b.into();
                Self::BlockBody(Arc::new(canonical))
            }
            GossipDataV2::ExecutionPayload(p) => Self::ExecutionPayload(p),
            GossipDataV2::IngressProof(p) => Self::IngressProof(p.into()),
        }
    }
}

// -- Conversions for GossipDataRequestV1/V2 --

super::impl_mirror_enum_from!(
    irys_types::gossip::v1::GossipDataRequestV1,
    GossipDataRequestV1(ExecutionPayload, Block, Chunk, Transaction,)
);

super::impl_mirror_enum_from!(
    irys_types::gossip::v2::GossipDataRequestV2,
    GossipDataRequestV2(ExecutionPayload, BlockHeader, BlockBody, Chunk, Transaction,)
);

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
