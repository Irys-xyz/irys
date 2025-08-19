use crate::{
    BlockHash, ChunkPathHash, CommitmentTransaction, DataTransactionHeader, IngressProof,
    IrysBlockHeader, IrysTransactionId, UnpackedChunk, H256,
};
use alloy_primitives::{Address, B256};
use base58::ToBase58 as _;
use reth::core::primitives::SealedBlock;
use reth_primitives::Block;
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct GossipBroadcastMessage {
    pub key: GossipCacheKey,
    pub data: GossipData,
}

impl GossipBroadcastMessage {
    pub fn new(key: GossipCacheKey, data: GossipData) -> Self {
        Self { key, data }
    }

    pub fn data_type_and_id(&self) -> String {
        self.data.data_type_and_id()
    }
}

impl From<SealedBlock<Block>> for GossipBroadcastMessage {
    fn from(sealed_block: SealedBlock<Block>) -> Self {
        let key = GossipCacheKey::sealed_evm_block(&sealed_block);
        let value = GossipData::from(sealed_block);
        Self::new(key, value)
    }
}

impl From<UnpackedChunk> for GossipBroadcastMessage {
    fn from(chunk: UnpackedChunk) -> Self {
        let key = GossipCacheKey::chunk(&chunk);
        let value = GossipData::Chunk(chunk);
        Self::new(key, value)
    }
}

impl From<DataTransactionHeader> for GossipBroadcastMessage {
    fn from(transaction: DataTransactionHeader) -> Self {
        let key = GossipCacheKey::transaction(&transaction);
        let value = GossipData::Transaction(transaction);
        Self::new(key, value)
    }
}

impl From<CommitmentTransaction> for GossipBroadcastMessage {
    fn from(commitment_tx: CommitmentTransaction) -> Self {
        let key = GossipCacheKey::commitment_transaction(&commitment_tx);
        let value = GossipData::CommitmentTransaction(commitment_tx);
        Self::new(key, value)
    }
}

impl From<IngressProof> for GossipBroadcastMessage {
    fn from(ingress_proof: IngressProof) -> Self {
        let key = GossipCacheKey::ingress_proof(&ingress_proof);
        let value = GossipData::IngressProof(ingress_proof);
        Self::new(key, value)
    }
}

impl From<Arc<IrysBlockHeader>> for GossipBroadcastMessage {
    fn from(block: Arc<IrysBlockHeader>) -> Self {
        let key = GossipCacheKey::irys_block(&block);
        let value = GossipData::Block(block);
        Self::new(key, value)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum GossipCacheKey {
    Chunk(ChunkPathHash),
    Transaction(IrysTransactionId),
    Block(BlockHash),
    ExecutionPayload(B256),
    IngressProof(H256),
}

impl GossipCacheKey {
    pub fn chunk(chunk: &UnpackedChunk) -> Self {
        Self::Chunk(chunk.chunk_path_hash())
    }

    pub fn transaction(transaction: &DataTransactionHeader) -> Self {
        Self::Transaction(transaction.id)
    }

    pub fn commitment_transaction(commitment_tx: &CommitmentTransaction) -> Self {
        Self::Transaction(commitment_tx.id)
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GossipData {
    Chunk(UnpackedChunk),
    Transaction(DataTransactionHeader),
    CommitmentTransaction(CommitmentTransaction),
    Block(Arc<IrysBlockHeader>),
    ExecutionPayload(Block),
    IngressProof(IngressProof),
}

impl From<SealedBlock<Block>> for GossipData {
    fn from(sealed_block: SealedBlock<Block>) -> Self {
        Self::ExecutionPayload(sealed_block.into_block())
    }
}

impl GossipData {
    pub fn data_type_and_id(&self) -> String {
        match self {
            Self::Chunk(chunk) => {
                format!("chunk data root {}", chunk.data_root)
            }
            Self::Transaction(tx) => {
                format!("transaction {}", tx.id.0.to_base58())
            }
            Self::CommitmentTransaction(commitment_tx) => {
                format!("commitment transaction {}", commitment_tx.id.0.to_base58())
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GossipRequest<T> {
    pub miner_address: Address,
    pub data: T,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub enum GossipDataRequest {
    ExecutionPayload(B256),
    Block(BlockHash),
    Chunk(ChunkPathHash),
}

impl Debug for GossipDataRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Block(hash) => write!(f, "block {:?}", hash),
            Self::ExecutionPayload(block_hash) => {
                write!(f, "execution payload for block {:?}", block_hash)
            }
            Self::Chunk(chunk_path_hash) => {
                write!(f, "chunk {:?}", chunk_path_hash)
            }
        }
    }
}
