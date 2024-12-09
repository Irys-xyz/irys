//! Contains a common set of types used across all of the `irys-chain` modules.
//!
//! This module implements a single location where these types are managed,
//! making them easy to reference and maintain.
use std::{
    fmt,
    ops::{Index, IndexMut},
    str::FromStr,
};

use crate::{
    generate_data_root, generate_leaves_from_data_roots, option_u64_stringify,
    partition::PartitionHash, resolve_proofs, u64_stringify, validate_path, Arbitrary, Base64,
    Compact, DataRootLeave, H256List, IrysSignature, IrysTransactionHeader, Proof, Signature, H256,
    U256,
};

use alloy_primitives::{Address, B256};
use serde::{Deserialize, Serialize};

pub type BlockHash = H256;

#[derive(Clone, Debug, Eq, Default, Serialize, Deserialize, PartialEq, Arbitrary, Compact)]
/// Stores deserialized fields from a JSON formatted Irys block header.
pub struct IrysBlockHeader {
    /// The block identifier.
    pub block_hash: BlockHash,

    /// The block height.
    pub height: u64,

    /// Difficulty threshold used to produce the current block.
    pub diff: U256,

    /// The sum of the average number of hashes computed by the network to
    /// produce the past blocks including this one.
    pub cumulative_diff: U256,

    /// Unix timestamp of the last difficulty adjustment
    pub last_retarget: u64,

    /// The solution hash for the block
    pub solution_hash: H256,

    /// The solution hash of the previous block in the chain.
    pub previous_solution_hash: H256,

    /// The solution hash of the last epoch block
    pub last_epoch_hash: H256,

    /// `SHA-256` hash of the PoA chunk (unencoded) bytes.
    pub chunk_hash: H256,

    // Previous block identifier.
    pub previous_block_hash: H256,
    pub previous_cumulative_diff: U256,

    /// The recall chunk proof
    pub poa: PoaData,

    /// Address of the miner claiming the block reward, also used in validation
    /// of the poa chunks as the packing key.
    pub reward_address: Address,

    /// {KeyType, PubKey} - the public key the block was signed with. The only
    // supported KeyType is currently {rsa, 65537}.
    pub reward_key: Base64,

    /// The block signature
    pub signature: IrysSignature,

    /// timestamp of when the block was discovered/produced
    pub timestamp: u64,

    /// A list of transaction ledgers, one for each active data ledger
    /// Maintains the block->tx_root->data_root relationship for each block
    /// and ledger.
    pub ledgers: Vec<TransactionLedger>,

    pub evm_block_hash: B256,
}

impl IrysBlockHeader {
    pub fn new() -> Self {
        let txids = H256List::new();

        // Create a sample IrysBlockHeader object with mock data
        IrysBlockHeader {
            diff: U256::from(1000),
            cumulative_diff: U256::from(5000),
            last_retarget: 1622543200,
            solution_hash: H256::zero(),
            previous_solution_hash: H256::zero(),
            last_epoch_hash: H256::random(),
            chunk_hash: H256::zero(),
            height: 42,
            block_hash: H256::zero(),
            previous_block_hash: H256::zero(),
            previous_cumulative_diff: U256::from(4000),
            poa: PoaData {
                tx_path: Base64::from_str("").unwrap(),
                data_path: Base64::from_str("").unwrap(),
                chunk: Base64::from_str("").unwrap(),
                partition_hash: PartitionHash::zero(),
                partition_chunk_offset: 0,
                ledger_num: 0,
            },
            reward_address: Address::ZERO,
            reward_key: Base64::from_str("").unwrap(),
            signature: IrysSignature {
                reth_signature: Signature::test_signature(),
            },
            timestamp: 1622543200,
            ledgers: vec![
                // Permanent Publish Ledger
                TransactionLedger {
                    tx_root: H256::zero(),
                    txids,
                    max_chunk_offset: 0,
                    expires: None,
                },
                // Term Submit Ledger
                TransactionLedger {
                    tx_root: H256::zero(),
                    txids: H256List::new(),
                    max_chunk_offset: 0,
                    expires: Some(1622543200),
                },
            ],
            evm_block_hash: B256::ZERO,
        }
    }
}

#[derive(Default, Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Compact, Arbitrary)]
/// Stores deserialized fields from a `poa` (Proof of Access) JSON
pub struct PoaData {
    pub tx_path: Base64,
    pub data_path: Base64,
    pub chunk: Base64,
    pub ledger_num: u64,
    pub partition_chunk_offset: u64, // TODO: implement Compact for u32 ?
    pub partition_hash: PartitionHash,
}

pub type TxRoot = H256;

#[derive(Default, Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Compact, Arbitrary)]
pub struct TransactionLedger {
    pub tx_root: H256,
    /// List of transaction ids included in the block
    pub txids: H256List,
    #[serde(default, with = "u64_stringify")]
    pub max_chunk_offset: u64,
    pub expires: Option<u64>,
}

impl TransactionLedger {
    /// Computes the tx_root and tx_paths. The TX Root is composed of taking the data_roots of each of the storage transactions included, in order, and building a merkle tree out of them. The root of this tree is the tx_root.
    pub fn merklize_tx_root(data_txs: &Vec<IrysTransactionHeader>) -> (H256, Vec<Proof>) {
        if data_txs.is_empty() {
            return (H256::zero(), vec![]);
        }
        let txs_data_roots = data_txs
            .iter()
            .map(|h| DataRootLeave {
                data_root: h.data_root,
                tx_size: h.data_size as usize, // TODO: check this
            })
            .collect::<Vec<DataRootLeave>>();
        //txs_data_roots.push(&[]); // TODO: check this ? mimics merkle::generate_leaves's push as last chunk has max. capacity 32
        let data_root_leaves = generate_leaves_from_data_roots(&txs_data_roots).unwrap();
        let root = generate_data_root(data_root_leaves.clone()).unwrap();
        let root_id = root.id.clone();
        let proofs = resolve_proofs(root, None).unwrap();
        (H256(root_id), proofs)
    }

    // tx_path = proof ?
    // tx_path/proof verification
}

/// Stores the `nonce_limiter_info` in the [`ArweaveBlockHeader`]
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize, Compact)]
pub struct NonceLimiterInfo {
    /// The output of the latest step - the source of the entropy for the mining nonces.
    pub output: H256,
    /// The global sequence number of the nonce limiter step at which the block was found.
    pub global_step_number: u64,
    /// The hash of the latest block mined below the current reset line.
    pub seed: H256,
    /// The hash of the latest block mined below the future reset line.
    pub next_seed: H256,
    /// The output of the latest step of the previous block
    pub prev_output: H256,
    /// NUM_CHECKPOINTS_IN_VDF_STEP from the most recent step in the nonce limiter process.
    pub last_step_checkpoints: H256List,
    /// A list of the output of each step of the nonce limiting process. Note: each step
    /// has NUM_CHECKPOINTS_IN_VDF_STEP, the last of which is that step's output.
    /// This field would be more accurately named "steps" as checkpoints are between steps.
    pub checkpoints: H256List,
    /// The number of SHA2-256 iterations in a single VDF checkpoint. The protocol aims to keep the
    /// checkpoint calculation time to around 40ms by varying this parameter. Note: there are
    /// 25 checkpoints in a single VDF step - so the protocol aims to keep the step calculation at
    /// 1 second by varying this parameter.
    #[serde(default, with = "option_u64_stringify")]
    pub vdf_difficulty: Option<u64>,
    /// The VDF difficulty scheduled for to be applied after the next VDF reset line.
    #[serde(default, with = "option_u64_stringify")]
    pub next_vdf_difficulty: Option<u64>,
}

// Implement the Display trait
impl fmt::Display for IrysBlockHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Convert the struct to a JSON string using serde_json
        match serde_json::to_string_pretty(self) {
            Ok(json) => write!(f, "{}", json), // Write the JSON string to the formatter
            Err(_) => write!(f, "Failed to serialize IrysBlockHeader"), // Handle serialization errors
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Signature;
    use rand::{rngs::StdRng, Rng, SeedableRng};
    use serde_json;
    use std::str::FromStr;
    use zerocopy::IntoBytes;

    #[test]
    fn test_irys_block_header_serialization() {
        let txids = H256List::new();

        // Create a sample IrysBlockHeader object with mock data
        let mut header = IrysBlockHeader {
            diff: U256::from(1000),
            cumulative_diff: U256::from(5000),
            last_retarget: 1622543200,
            solution_hash: H256::zero(),
            previous_solution_hash: H256::zero(),
            last_epoch_hash: H256::random(),
            chunk_hash: H256::zero(),
            height: 42,
            block_hash: H256::zero(),
            previous_block_hash: H256::zero(),
            previous_cumulative_diff: U256::from(4000),
            poa: PoaData {
                tx_path: Base64::from_str("").unwrap(),
                data_path: Base64::from_str("").unwrap(),
                chunk: Base64::from_str("").unwrap(),
                partition_hash: H256::zero(),
                partition_chunk_offset: 0,
                ledger_num: 0,
            },
            reward_address: Address::ZERO,
            reward_key: Base64::from_str("").unwrap(),
            signature: IrysSignature {
                reth_signature: Signature::test_signature(),
            },
            timestamp: 1622543200,
            ledgers: vec![TransactionLedger {
                tx_root: H256::zero(),
                txids,
                max_chunk_offset: 100,
                expires: Some(1622543200),
            }],
            evm_block_hash: B256::ZERO,
        };

        // Use a specific seed
        let seed: [u8; 32] = [0; 32]; // Example: use an array of zeros as the seed

        // Create a seeded RNG
        let mut rng = StdRng::from_seed(seed);

        // Populate some deterministic random hash data (will not pass consensus checks)
        rng.fill(&mut header.chunk_hash.as_bytes_mut()[..]);
        rng.fill(&mut header.solution_hash.as_bytes_mut()[..]);
        rng.fill(&mut header.previous_solution_hash.as_bytes_mut()[..]);
        rng.fill(&mut header.previous_block_hash.as_bytes_mut()[..]);
        rng.fill(&mut header.block_hash.as_bytes_mut()[..]);
        rng.fill(&mut header.reward_address.as_bytes_mut()[..]);

        // Serialize the header to a JSON string
        let serialized = serde_json::to_string_pretty(&header).unwrap();
        println!("\n{}", serialized);

        // Deserialize back to IrysBlockHeader struct
        let deserialized: IrysBlockHeader = serde_json::from_str(&serialized).unwrap();

        // Assert that the deserialized object is equal to the original
        assert_eq!(header, deserialized);
    }

    #[test]
    fn test_validate_tx_path() {
        let mut txs: Vec<IrysTransactionHeader> = vec![IrysTransactionHeader::default(); 10];
        for tx in txs.iter_mut() {
            tx.data_root = H256::from([3u8; 32]);
            tx.data_size = 64
        }

        let (tx_root, proofs) = TransactionLedger::merklize_tx_root(&txs);

        for proof in proofs {
            let encoded_proof = Base64(proof.proof.to_vec());
            validate_path(tx_root.0, &encoded_proof, proof.offset as u128).unwrap();
        }
    }
}
