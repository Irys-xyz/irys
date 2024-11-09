//! Contains a common set of types used across all of the `irys-chain` modules.
//!
//! This module implements a single location where these types are managed,
//! making them easy to reference and maintain.

use crate::U256;
use std::{fmt, str::FromStr};

use crate::{
    option_u64_stringify, Arbitrary, Base64, Compact, H256List, IrysSignature, Signature, H256,
};
use alloy_primitives::{Address, B256};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, Default, Serialize, Deserialize, PartialEq, Arbitrary, Compact)]
/// Stores deserialized fields from a JSON formatted Irys block header.
pub struct IrysBlockHeader {
    /// The block identifier.
    pub block_hash: H256,

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

    /// The block height.
    pub height: u64,

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
                    ledger_size: U256::from(0),
                    expires: None,
                },
                // Term Submit Ledger
                TransactionLedger {
                    tx_root: H256::zero(),
                    txids: H256List::new(),
                    ledger_size: U256::from(0),
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
}

#[derive(Default, Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Compact, Arbitrary)]
pub struct TransactionLedger {
    pub tx_root: H256,
    /// List of transaction ids included in the block
    pub txids: H256List,
    pub ledger_size: U256,
    pub expires: Option<u64>,
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
                ledger_size: U256::from(100),
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
        let serialized = serde_json::to_string(&header).unwrap();
        // println!("\n{}", serialized);

        // Deserialize back to IrysBlockHeader struct
        let deserialized: IrysBlockHeader = serde_json::from_str(&serialized).unwrap();

        // Assert that the deserialized object is equal to the original
        assert_eq!(header, deserialized);
    }
}
