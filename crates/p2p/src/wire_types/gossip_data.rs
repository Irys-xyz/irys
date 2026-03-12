use irys_types::{BlockHash, ChunkPathHash, IrysAddress, IrysPeerId, H256};
use reth::revm::primitives::B256;
use reth_ethereum_primitives::Block as RethBlock;
use serde::{Deserialize, Serialize};

use super::{
    BlockBody, CommitmentTransaction, DataTransactionHeader, IngressProof, IrysBlockHeader,
    UnpackedChunk,
};

/// Adding a variant? Update the `impl_mirror_enum_from!` below AND add a
/// fixture entry in `gossip_fixture_tests.rs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GossipDataV1 {
    Chunk(UnpackedChunk),
    Transaction(DataTransactionHeader),
    CommitmentTransaction(CommitmentTransaction),
    Block(IrysBlockHeader),
    ExecutionPayload(RethBlock),
    IngressProof(IngressProof),
}

/// Adding a variant? Update the `impl_mirror_enum_from!` below AND add a
/// fixture entry in `gossip_fixture_tests.rs`.
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

super::impl_mirror_enum_from!(
    irys_types::gossip::v1::GossipDataV1, GossipDataV1 mixed {
        identity: ExecutionPayload;
        convert: Chunk, Transaction, CommitmentTransaction, IngressProof;
        arc_wrap: Block;
    }
);

super::impl_mirror_enum_from!(
    irys_types::gossip::v2::GossipDataV2, GossipDataV2 mixed {
        identity: ExecutionPayload;
        convert: Transaction, CommitmentTransaction, IngressProof;
        arc_wrap: Chunk, BlockHeader, BlockBody;
    }
);

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
