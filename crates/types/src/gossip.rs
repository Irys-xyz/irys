use crate::{CommitmentTransaction, IrysBlockHeader, IrysTransactionHeader, UnpackedChunk};
use alloy_primitives::{Address, B256};
use base58::ToBase58 as _;
use reth::builder::Block as _;
use reth::core::primitives::SealedBlock;
use reth_primitives::Block;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GossipExecutionPayloadData {
    pub evm_block_hash: B256,
    pub evm_block: Block,
}

impl GossipExecutionPayloadData {
    pub fn new(evm_block_hash: B256, evm_block: Block) -> Self {
        Self {
            evm_block_hash,
            evm_block,
        }
    }

    pub fn stored_hash(&self) -> B256 {
        self.evm_block_hash
    }

    pub fn seal_and_verify_hash(self) -> Option<SealedBlock<Block>> {
        let sealed_block = self.evm_block.seal_slow();
        if self.evm_block_hash == sealed_block.hash() {
            Some(sealed_block)
        } else {
            tracing::warn!(
                "Execution payload hash mismatch: expected {}, got {}",
                self.evm_block_hash,
                sealed_block.hash()
            );
            None
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GossipData {
    Chunk(UnpackedChunk),
    Transaction(IrysTransactionHeader),
    CommitmentTransaction(CommitmentTransaction),
    Block(IrysBlockHeader),
    ExecutionPayload(GossipExecutionPayloadData),
}

impl From<SealedBlock<Block>> for GossipData {
    fn from(sealed_block: SealedBlock<Block>) -> Self {
        Self::ExecutionPayload(GossipExecutionPayloadData::new(
            sealed_block.hash(),
            sealed_block.into_block(),
        ))
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
                    "execution payload for EVM block {:?}",
                    execution_payload_data.stored_hash()
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
