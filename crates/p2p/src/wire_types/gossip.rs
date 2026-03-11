use std::sync::Arc;

use irys_types::{BlockHash, ChunkPathHash, IrysAddress, IrysPeerId, H256};
use reth::revm::primitives::B256;
use reth_ethereum_primitives::Block as RethBlock;
use serde::{Deserialize, Serialize};

use super::{
    BlockBody, CommitmentTransaction, DataTransactionHeader, IngressProof, IrysBlockHeader,
    UnpackedChunk,
};

/// Adding a variant? Update the `From` impls below AND add a fixture entry
/// in `gossip_fixture_tests.rs` (the exhaustive coverage test will fail to
/// compile until you do).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GossipDataV1 {
    Chunk(UnpackedChunk),
    Transaction(DataTransactionHeader),
    CommitmentTransaction(CommitmentTransaction),
    Block(IrysBlockHeader),
    ExecutionPayload(RethBlock),
    IngressProof(IngressProof),
}

/// Adding a variant? Update the `From` impls below AND add a fixture entry
/// in `gossip_fixture_tests.rs` (the exhaustive coverage test will fail to
/// compile until you do).
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

/// Adding a variant? Update the `impl_mirror_enum_from!` below AND add a
/// fixture entry in `gossip_fixture_tests.rs`.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub enum GossipDataRequestV1 {
    ExecutionPayload(B256),
    Block(BlockHash),
    Chunk(ChunkPathHash),
    Transaction(H256),
}

/// Adding a variant? Update the `impl_mirror_enum_from!` below AND add a
/// fixture entry in `gossip_fixture_tests.rs`.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
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

impl From<irys_types::gossip::v1::GossipDataV1> for GossipDataV1 {
    fn from(d: irys_types::gossip::v1::GossipDataV1) -> Self {
        match d {
            irys_types::gossip::v1::GossipDataV1::Chunk(c) => Self::Chunk(c.into()),
            irys_types::gossip::v1::GossipDataV1::Transaction(t) => Self::Transaction(t.into()),
            irys_types::gossip::v1::GossipDataV1::CommitmentTransaction(c) => {
                Self::CommitmentTransaction(c.into())
            }
            irys_types::gossip::v1::GossipDataV1::Block(b) => {
                Self::Block(Arc::unwrap_or_clone(b).into())
            }
            irys_types::gossip::v1::GossipDataV1::ExecutionPayload(p) => Self::ExecutionPayload(p),
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

impl From<irys_types::gossip::v2::GossipDataV2> for GossipDataV2 {
    fn from(d: irys_types::gossip::v2::GossipDataV2) -> Self {
        match d {
            irys_types::gossip::v2::GossipDataV2::Chunk(c) => {
                Self::Chunk(Arc::unwrap_or_clone(c).into())
            }
            irys_types::gossip::v2::GossipDataV2::Transaction(t) => Self::Transaction(t.into()),
            irys_types::gossip::v2::GossipDataV2::CommitmentTransaction(c) => {
                Self::CommitmentTransaction(c.into())
            }
            irys_types::gossip::v2::GossipDataV2::BlockHeader(b) => {
                Self::BlockHeader(Arc::unwrap_or_clone(b).into())
            }
            irys_types::gossip::v2::GossipDataV2::BlockBody(b) => {
                Self::BlockBody(Arc::unwrap_or_clone(b).into())
            }
            irys_types::gossip::v2::GossipDataV2::ExecutionPayload(p) => Self::ExecutionPayload(p),
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

super::impl_mirror_enum_from!(
    irys_types::gossip::v1::GossipDataRequestV1,
    GossipDataRequestV1(ExecutionPayload, Block, Chunk, Transaction,)
);

super::impl_mirror_enum_from!(
    irys_types::gossip::v2::GossipDataRequestV2,
    GossipDataRequestV2(ExecutionPayload, BlockHeader, BlockBody, Chunk, Transaction,)
);

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
