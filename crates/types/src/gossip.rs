use crate::{
    range_specifier::ChunkRangeSpecifier, BlockHash, ChunkPathHash, CommitmentTransaction,
    DataTransactionHeader, IngressProof, IrysAddress, IrysBlockHeader, IrysPeerId,
    IrysTransactionId, UnpackedChunk, H256,
};
use alloy_primitives::B256;
use reth::core::primitives::SealedBlock;
use reth_ethereum_primitives::Block;
use serde::{Deserialize, Serialize};
use std::fmt::Debug;

/// Message for gossiping unpacked PD (Programmable Data) chunks between VersionPD peers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PdChunkMessage {
    pub chunk: UnpackedChunk,
    pub range_specifier: ChunkRangeSpecifier,
    pub timestamp: u64,
}

pub mod v1 {
    use crate::{
        BlockHash, ChunkPathHash, CommitmentTransaction, DataTransactionHeader, GossipCacheKey,
        IngressProof, IrysBlockHeader, UnpackedChunk, H256,
    };
    use alloy_primitives::B256;
    use reth_ethereum_primitives::Block;
    use reth_primitives_traits::SealedBlock;
    use serde::{Deserialize, Serialize};
    use std::fmt::Debug;
    use std::sync::Arc;

    #[derive(Clone, Debug)]
    pub struct GossipBroadcastMessageV1 {
        pub key: GossipCacheKey,
        pub data: GossipDataV1,
    }

    impl GossipBroadcastMessageV1 {
        pub fn new(key: GossipCacheKey, data: GossipDataV1) -> Self {
            Self { key, data }
        }

        pub fn data_type_and_id(&self) -> String {
            self.data.data_type_and_id()
        }
    }

    impl From<SealedBlock<Block>> for GossipBroadcastMessageV1 {
        fn from(sealed_block: SealedBlock<Block>) -> Self {
            let key = GossipCacheKey::sealed_evm_block(&sealed_block);
            let value = GossipDataV1::from(sealed_block);
            Self::new(key, value)
        }
    }

    impl From<UnpackedChunk> for GossipBroadcastMessageV1 {
        fn from(chunk: UnpackedChunk) -> Self {
            let key = GossipCacheKey::chunk(&chunk);
            let value = GossipDataV1::Chunk(chunk);
            Self::new(key, value)
        }
    }

    impl From<DataTransactionHeader> for GossipBroadcastMessageV1 {
        fn from(transaction: DataTransactionHeader) -> Self {
            let key = GossipCacheKey::transaction(&transaction);
            let value = GossipDataV1::Transaction(transaction);
            Self::new(key, value)
        }
    }

    impl From<CommitmentTransaction> for GossipBroadcastMessageV1 {
        fn from(commitment_tx: CommitmentTransaction) -> Self {
            let key = GossipCacheKey::commitment_transaction(&commitment_tx);
            let value = GossipDataV1::CommitmentTransaction(commitment_tx);
            Self::new(key, value)
        }
    }

    impl From<IngressProof> for GossipBroadcastMessageV1 {
        fn from(ingress_proof: IngressProof) -> Self {
            let key = GossipCacheKey::ingress_proof(&ingress_proof);
            let value = GossipDataV1::IngressProof(ingress_proof);
            Self::new(key, value)
        }
    }

    impl From<Arc<IrysBlockHeader>> for GossipBroadcastMessageV1 {
        fn from(block: Arc<IrysBlockHeader>) -> Self {
            let key = GossipCacheKey::irys_block(&block);
            let value = GossipDataV1::Block(block);
            Self::new(key, value)
        }
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub enum GossipDataV1 {
        Chunk(UnpackedChunk),
        Transaction(DataTransactionHeader),
        CommitmentTransaction(CommitmentTransaction),
        Block(Arc<IrysBlockHeader>),
        ExecutionPayload(Block),
        IngressProof(IngressProof),
    }

    impl From<SealedBlock<Block>> for GossipDataV1 {
        fn from(sealed_block: SealedBlock<Block>) -> Self {
            Self::ExecutionPayload(sealed_block.into_block())
        }
    }

    impl GossipDataV1 {
        pub fn data_type_and_id(&self) -> String {
            match self {
                Self::Chunk(chunk) => {
                    format!("chunk data root {}", chunk.data_root)
                }
                Self::Transaction(tx) => {
                    format!("transaction {}", tx.id)
                }
                Self::CommitmentTransaction(commitment_tx) => {
                    format!("commitment transaction {}", commitment_tx.id())
                }
                Self::Block(block) => {
                    format!("block {} height: {}", block.block_hash, block.height)
                }
                Self::ExecutionPayload(execution_payload_data) => {
                    format!(
                        "execution payload for EVM block number {:?}",
                        execution_payload_data.number
                    )
                }
                Self::IngressProof(ingress_proof) => {
                    format!(
                        "ingress proof for data_root: {:?} from {:?}",
                        ingress_proof.data_root,
                        ingress_proof.recover_signer()
                    )
                }
            }
        }
    }

    #[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
    pub enum GossipDataRequestV1 {
        ExecutionPayload(B256),
        Block(BlockHash),
        Chunk(ChunkPathHash),
        Transaction(H256),
    }

    impl Debug for GossipDataRequestV1 {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Block(hash) => write!(f, "block header {hash:?}"),
                Self::ExecutionPayload(block_hash) => {
                    write!(f, "execution payload for block {block_hash:?}")
                }
                Self::Chunk(chunk_path_hash) => {
                    write!(f, "chunk {chunk_path_hash:?}")
                }
                Self::Transaction(tx_id) => {
                    write!(f, "transaction {tx_id:?}")
                }
            }
        }
    }
}

pub mod v2 {
    use crate::{
        BlockBody, BlockHash, ChunkPathHash, CommitmentTransaction, DataTransactionHeader,
        GossipCacheKey, IngressProof, IrysBlockHeader, UnpackedChunk, H256,
    };
    use alloy_primitives::B256;
    use reth_ethereum_primitives::Block;
    use reth_primitives_traits::SealedBlock;
    use serde::{Deserialize, Serialize};
    use std::fmt::Debug;
    use std::sync::Arc;

    #[derive(Clone, Debug)]
    pub struct GossipBroadcastMessageV2 {
        pub key: GossipCacheKey,
        pub data: GossipDataV2,
    }

    impl GossipBroadcastMessageV2 {
        pub fn new(key: GossipCacheKey, data: GossipDataV2) -> Self {
            Self { key, data }
        }

        pub fn data_type_and_id(&self) -> String {
            self.data.data_type_and_id()
        }
    }

    impl From<SealedBlock<Block>> for GossipBroadcastMessageV2 {
        fn from(sealed_block: SealedBlock<Block>) -> Self {
            let key = GossipCacheKey::sealed_evm_block(&sealed_block);
            let value = GossipDataV2::from(sealed_block);
            Self::new(key, value)
        }
    }

    impl From<UnpackedChunk> for GossipBroadcastMessageV2 {
        fn from(chunk: UnpackedChunk) -> Self {
            Self::from(Arc::new(chunk))
        }
    }

    impl From<Arc<UnpackedChunk>> for GossipBroadcastMessageV2 {
        fn from(chunk: Arc<UnpackedChunk>) -> Self {
            let key = GossipCacheKey::chunk(&chunk);
            let value = GossipDataV2::Chunk(chunk);
            Self::new(key, value)
        }
    }

    impl From<DataTransactionHeader> for GossipBroadcastMessageV2 {
        fn from(transaction: DataTransactionHeader) -> Self {
            let key = GossipCacheKey::transaction(&transaction);
            let value = GossipDataV2::Transaction(transaction);
            Self::new(key, value)
        }
    }

    impl From<CommitmentTransaction> for GossipBroadcastMessageV2 {
        fn from(commitment_tx: CommitmentTransaction) -> Self {
            let key = GossipCacheKey::commitment_transaction(&commitment_tx);
            let value = GossipDataV2::CommitmentTransaction(commitment_tx);
            Self::new(key, value)
        }
    }

    impl From<IngressProof> for GossipBroadcastMessageV2 {
        fn from(ingress_proof: IngressProof) -> Self {
            let key = GossipCacheKey::ingress_proof(&ingress_proof);
            let value = GossipDataV2::IngressProof(ingress_proof);
            Self::new(key, value)
        }
    }

    impl From<Arc<IrysBlockHeader>> for GossipBroadcastMessageV2 {
        fn from(block: Arc<IrysBlockHeader>) -> Self {
            let key = GossipCacheKey::irys_block(&block);
            let value = GossipDataV2::BlockHeader(block);
            Self::new(key, value)
        }
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub enum GossipDataV2 {
        Chunk(Arc<UnpackedChunk>),
        Transaction(DataTransactionHeader),
        CommitmentTransaction(CommitmentTransaction),
        BlockHeader(Arc<IrysBlockHeader>),
        BlockBody(Arc<BlockBody>),
        ExecutionPayload(Block),
        IngressProof(IngressProof),
    }

    impl From<SealedBlock<Block>> for GossipDataV2 {
        fn from(sealed_block: SealedBlock<Block>) -> Self {
            Self::ExecutionPayload(sealed_block.into_block())
        }
    }

    impl From<super::v1::GossipDataV1> for GossipDataV2 {
        fn from(v1: super::v1::GossipDataV1) -> Self {
            match v1 {
                super::v1::GossipDataV1::Chunk(chunk) => Self::Chunk(Arc::new(chunk)),
                super::v1::GossipDataV1::Transaction(tx) => Self::Transaction(tx),
                super::v1::GossipDataV1::CommitmentTransaction(tx) => {
                    Self::CommitmentTransaction(tx)
                }
                super::v1::GossipDataV1::Block(block) => Self::BlockHeader(block),
                super::v1::GossipDataV1::ExecutionPayload(payload) => {
                    Self::ExecutionPayload(payload)
                }
                super::v1::GossipDataV1::IngressProof(proof) => Self::IngressProof(proof),
            }
        }
    }

    impl GossipDataV2 {
        pub fn to_v1(&self) -> Option<super::v1::GossipDataV1> {
            match self {
                Self::Chunk(chunk) => {
                    Some(super::v1::GossipDataV1::Chunk(UnpackedChunk::clone(chunk)))
                }
                Self::Transaction(tx) => Some(super::v1::GossipDataV1::Transaction(tx.clone())),
                Self::CommitmentTransaction(commitment_tx) => Some(
                    super::v1::GossipDataV1::CommitmentTransaction(commitment_tx.clone()),
                ),
                Self::BlockHeader(block) => Some(super::v1::GossipDataV1::Block(block.clone())),
                Self::ExecutionPayload(execution_payload_data) => Some(
                    super::v1::GossipDataV1::ExecutionPayload(execution_payload_data.clone()),
                ),
                Self::IngressProof(ingress_proof) => {
                    Some(super::v1::GossipDataV1::IngressProof(ingress_proof.clone()))
                }
                Self::BlockBody(_) => None, // BlockBody does not exist in v1
            }
        }

        pub fn data_type_and_id(&self) -> String {
            match self {
                Self::Chunk(chunk) => {
                    format!("chunk data root {}", chunk.data_root)
                }
                Self::Transaction(tx) => {
                    format!("transaction {}", tx.id)
                }
                Self::CommitmentTransaction(commitment_tx) => {
                    format!("commitment transaction {}", commitment_tx.id())
                }
                Self::BlockHeader(block) => {
                    format!("block {} height: {}", block.block_hash, block.height)
                }
                Self::BlockBody(block_body) => {
                    format!("block body for block {}", block_body.block_hash)
                }
                Self::ExecutionPayload(execution_payload_data) => {
                    format!(
                        "execution payload for EVM block number {:?}",
                        execution_payload_data.number
                    )
                }
                Self::IngressProof(ingress_proof) => {
                    format!(
                        "ingress proof for data_root: {:?} from {:?}",
                        ingress_proof.data_root,
                        ingress_proof.recover_signer()
                    )
                }
            }
        }
    }

    #[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
    pub enum GossipDataRequestV2 {
        ExecutionPayload(B256),
        BlockHeader(BlockHash),
        BlockBody(BlockHash),
        Chunk(ChunkPathHash),
        Transaction(H256),
    }

    impl GossipDataRequestV2 {
        pub fn to_v1(&self) -> Option<super::v1::GossipDataRequestV1> {
            match self {
                Self::ExecutionPayload(b256) => {
                    Some(super::v1::GossipDataRequestV1::ExecutionPayload(*b256))
                }
                Self::BlockHeader(block_hash) => {
                    Some(super::v1::GossipDataRequestV1::Block(*block_hash))
                }
                Self::Chunk(chunk_path_hash) => {
                    Some(super::v1::GossipDataRequestV1::Chunk(*chunk_path_hash))
                }
                Self::Transaction(tx_id) => {
                    Some(super::v1::GossipDataRequestV1::Transaction(*tx_id))
                }
                Self::BlockBody(_) => None, // BlockBody does not exist in v1
            }
        }
    }

    impl From<super::v1::GossipDataRequestV1> for GossipDataRequestV2 {
        fn from(v1: super::v1::GossipDataRequestV1) -> Self {
            match v1 {
                super::v1::GossipDataRequestV1::ExecutionPayload(b256) => {
                    Self::ExecutionPayload(b256)
                }
                super::v1::GossipDataRequestV1::Block(block_hash) => Self::BlockHeader(block_hash),
                super::v1::GossipDataRequestV1::Chunk(chunk_path_hash) => {
                    Self::Chunk(chunk_path_hash)
                }
                super::v1::GossipDataRequestV1::Transaction(tx_id) => Self::Transaction(tx_id),
            }
        }
    }

    impl Debug for GossipDataRequestV2 {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::BlockHeader(hash) => write!(f, "block header {hash:?}"),
                Self::BlockBody(block_hash) => write!(f, "block body {:?}", block_hash),
                Self::ExecutionPayload(block_hash) => {
                    write!(f, "execution payload for block {block_hash:?}")
                }
                Self::Chunk(chunk_path_hash) => {
                    write!(f, "chunk {chunk_path_hash:?}")
                }
                Self::Transaction(tx_id) => {
                    write!(f, "transaction {tx_id:?}")
                }
            }
        }
    }
}

pub mod version_pd {
    use super::PdChunkMessage;
    use crate::{
        BlockBody, CommitmentTransaction, DataTransactionHeader, GossipCacheKey, IngressProof,
        IrysBlockHeader, UnpackedChunk,
    };
    use reth_ethereum_primitives::Block;
    use reth_primitives_traits::SealedBlock;
    use serde::{Deserialize, Serialize};
    use std::sync::Arc;

    #[derive(Clone, Debug)]
    pub struct GossipBroadcastMessageVersionPD {
        pub key: GossipCacheKey,
        pub data: GossipDataVersionPD,
    }

    impl GossipBroadcastMessageVersionPD {
        pub fn new(key: GossipCacheKey, data: GossipDataVersionPD) -> Self {
            Self { key, data }
        }

        pub fn data_type_and_id(&self) -> String {
            self.data.data_type_and_id()
        }
    }

    impl From<SealedBlock<Block>> for GossipBroadcastMessageVersionPD {
        fn from(sealed_block: SealedBlock<Block>) -> Self {
            let key = GossipCacheKey::sealed_evm_block(&sealed_block);
            let value = GossipDataVersionPD::from(sealed_block);
            Self::new(key, value)
        }
    }

    impl From<UnpackedChunk> for GossipBroadcastMessageVersionPD {
        fn from(chunk: UnpackedChunk) -> Self {
            let key = GossipCacheKey::chunk(&chunk);
            Self::new(key, GossipDataVersionPD::Chunk(chunk))
        }
    }

    impl From<Arc<UnpackedChunk>> for GossipBroadcastMessageVersionPD {
        fn from(chunk: Arc<UnpackedChunk>) -> Self {
            let key = GossipCacheKey::chunk(&chunk);
            let value = GossipDataVersionPD::Chunk(UnpackedChunk::clone(&chunk));
            Self::new(key, value)
        }
    }

    impl From<DataTransactionHeader> for GossipBroadcastMessageVersionPD {
        fn from(transaction: DataTransactionHeader) -> Self {
            let key = GossipCacheKey::transaction(&transaction);
            Self::new(key, GossipDataVersionPD::Transaction(transaction))
        }
    }

    impl From<CommitmentTransaction> for GossipBroadcastMessageVersionPD {
        fn from(commitment_tx: CommitmentTransaction) -> Self {
            let key = GossipCacheKey::commitment_transaction(&commitment_tx);
            Self::new(
                key,
                GossipDataVersionPD::CommitmentTransaction(commitment_tx),
            )
        }
    }

    impl From<IngressProof> for GossipBroadcastMessageVersionPD {
        fn from(ingress_proof: IngressProof) -> Self {
            let key = GossipCacheKey::ingress_proof(&ingress_proof);
            Self::new(key, GossipDataVersionPD::IngressProof(ingress_proof))
        }
    }

    impl From<Arc<IrysBlockHeader>> for GossipBroadcastMessageVersionPD {
        fn from(block: Arc<IrysBlockHeader>) -> Self {
            let key = GossipCacheKey::irys_block(&block);
            Self::new(key, GossipDataVersionPD::BlockHeader(block))
        }
    }

    impl From<PdChunkMessage> for GossipBroadcastMessageVersionPD {
        fn from(msg: PdChunkMessage) -> Self {
            let key = GossipCacheKey::pd_chunk(&msg.chunk, msg.range_specifier);
            Self::new(key, GossipDataVersionPD::PdChunk(msg))
        }
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub enum GossipDataVersionPD {
        Chunk(UnpackedChunk),
        Transaction(DataTransactionHeader),
        CommitmentTransaction(CommitmentTransaction),
        BlockHeader(Arc<IrysBlockHeader>),
        BlockBody(Arc<BlockBody>),
        ExecutionPayload(Block),
        IngressProof(IngressProof),
        PdChunk(PdChunkMessage),
    }

    impl From<SealedBlock<Block>> for GossipDataVersionPD {
        fn from(sealed_block: SealedBlock<Block>) -> Self {
            Self::ExecutionPayload(sealed_block.into_block())
        }
    }

    impl GossipDataVersionPD {
        pub fn to_v2(&self) -> Option<super::v2::GossipDataV2> {
            match self {
                Self::Chunk(chunk) => Some(super::v2::GossipDataV2::Chunk(Arc::new(chunk.clone()))),
                Self::Transaction(tx) => Some(super::v2::GossipDataV2::Transaction(tx.clone())),
                Self::CommitmentTransaction(tx) => {
                    Some(super::v2::GossipDataV2::CommitmentTransaction(tx.clone()))
                }
                Self::BlockHeader(block) => {
                    Some(super::v2::GossipDataV2::BlockHeader(block.clone()))
                }
                Self::BlockBody(body) => Some(super::v2::GossipDataV2::BlockBody(body.clone())),
                Self::ExecutionPayload(payload) => {
                    Some(super::v2::GossipDataV2::ExecutionPayload(payload.clone()))
                }
                Self::IngressProof(proof) => {
                    Some(super::v2::GossipDataV2::IngressProof(proof.clone()))
                }
                Self::PdChunk(_) => None, // PdChunk does not exist in V2
            }
        }

        pub fn to_v1(&self) -> Option<super::v1::GossipDataV1> {
            match self {
                Self::Chunk(chunk) => Some(super::v1::GossipDataV1::Chunk(chunk.clone())),
                Self::Transaction(tx) => Some(super::v1::GossipDataV1::Transaction(tx.clone())),
                Self::CommitmentTransaction(tx) => {
                    Some(super::v1::GossipDataV1::CommitmentTransaction(tx.clone()))
                }
                Self::BlockHeader(block) => Some(super::v1::GossipDataV1::Block(block.clone())),
                Self::ExecutionPayload(payload) => {
                    Some(super::v1::GossipDataV1::ExecutionPayload(payload.clone()))
                }
                Self::IngressProof(proof) => {
                    Some(super::v1::GossipDataV1::IngressProof(proof.clone()))
                }
                Self::BlockBody(_) => None, // BlockBody does not exist in V1
                Self::PdChunk(_) => None,   // PdChunk does not exist in V1
            }
        }

        pub fn data_type_and_id(&self) -> String {
            match self {
                Self::Chunk(chunk) => {
                    format!("chunk data root {}", chunk.data_root)
                }
                Self::Transaction(tx) => {
                    format!("transaction {}", tx.id)
                }
                Self::CommitmentTransaction(commitment_tx) => {
                    format!("commitment transaction {}", commitment_tx.id())
                }
                Self::BlockHeader(block) => {
                    format!("block {} height: {}", block.block_hash, block.height)
                }
                Self::BlockBody(block_body) => {
                    format!("block body for block {}", block_body.block_hash)
                }
                Self::ExecutionPayload(execution_payload_data) => {
                    format!(
                        "execution payload for EVM block number {:?}",
                        execution_payload_data.number
                    )
                }
                Self::IngressProof(ingress_proof) => {
                    format!(
                        "ingress proof for data_root: {:?} from {:?}",
                        ingress_proof.data_root,
                        ingress_proof.recover_signer()
                    )
                }
                Self::PdChunk(pd_chunk) => {
                    format!("pd chunk data root {}", pd_chunk.chunk.data_root)
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum GossipCacheKey {
    Chunk(ChunkPathHash),
    Transaction(IrysTransactionId),
    Block(BlockHash),
    ExecutionPayload(B256),
    IngressProof(H256),
    PdChunk(ChunkPathHash, ChunkRangeSpecifier),
}

impl GossipCacheKey {
    pub fn chunk(chunk: &UnpackedChunk) -> Self {
        Self::Chunk(chunk.chunk_path_hash())
    }

    pub fn transaction(transaction: &DataTransactionHeader) -> Self {
        Self::Transaction(transaction.id)
    }

    pub fn commitment_transaction(commitment_tx: &CommitmentTransaction) -> Self {
        Self::Transaction(commitment_tx.id())
    }

    pub fn irys_block(block: &IrysBlockHeader) -> Self {
        Self::Block(block.block_hash)
    }

    pub fn sealed_evm_block(sealed_block: &SealedBlock<Block>) -> Self {
        Self::ExecutionPayload(sealed_block.hash())
    }

    pub fn ingress_proof(ingress_proof: &IngressProof) -> Self {
        Self::IngressProof(ingress_proof.proof)
    }

    pub fn pd_chunk(chunk: &UnpackedChunk, range_specifier: ChunkRangeSpecifier) -> Self {
        Self::PdChunk(chunk.chunk_path_hash(), range_specifier)
    }
}

/// V1 GossipRequest - uses miner_address for identification (backward compatibility)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GossipRequestV1<T> {
    pub miner_address: IrysAddress,
    pub data: T,
}

/// V2 GossipRequest - uses peer_id for identification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GossipRequestV2<T> {
    pub peer_id: IrysPeerId,
    /// Miner address still included for staking checks and verification
    pub miner_address: IrysAddress,
    pub data: T,
}

impl<T> GossipRequestV1<T> {
    pub fn into_v2(self, peer_id: IrysPeerId) -> GossipRequestV2<T> {
        GossipRequestV2 {
            peer_id,
            miner_address: self.miner_address,
            data: self.data,
        }
    }
}

/// VersionPD GossipRequest
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GossipRequestVersionPD<T> {
    pub peer_id: IrysPeerId,
    pub miner_address: IrysAddress,
    pub data: T,
}

/// Legacy type alias for backward compatibility - maps to V1
pub type GossipRequest<T> = GossipRequestV1<T>;

/// Conversion from V2 to V1 (simple projection)
impl<T> From<GossipRequestV2<T>> for GossipRequestV1<T> {
    fn from(v2: GossipRequestV2<T>) -> Self {
        Self {
            miner_address: v2.miner_address,
            data: v2.data,
        }
    }
}

/// Conversion from VersionPD to V2
impl<T> From<GossipRequestVersionPD<T>> for GossipRequestV2<T> {
    fn from(pd: GossipRequestVersionPD<T>) -> Self {
        Self {
            peer_id: pd.peer_id,
            miner_address: pd.miner_address,
            data: pd.data,
        }
    }
}
