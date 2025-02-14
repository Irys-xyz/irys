//! Contains a common set of types used across all of the `irys-chain` modules.
//!
//! This module implements a single location where these types are managed,
//! making them easy to reference and maintain.
use std::fmt;

use crate::{
    generate_data_root, generate_leaves_from_data_roots, option_u64_stringify,
    partition::PartitionHash, resolve_proofs, string_u128, u64_stringify, Arbitrary, Base64,
    Compact, DataRootLeave, H256List, IngressProofsList, IrysSignature, IrysTransactionHeader,
    Proof, H256, U256,
};
use alloy_primitives::{keccak256, Address, B256};
use irys_storage_pricing::{Amount, IrysPrice, Usd};
use serde::{Deserialize, Serialize};

pub type BlockHash = H256;

/// Stores the `vdf_limiter_info` in the [`IrysBlockHeader`]
#[derive(Clone, Debug, Eq, Default, Serialize, Deserialize, PartialEq, Arbitrary, Compact)]
#[serde(rename_all = "camelCase")]
pub struct VDFLimiterInfo {
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
    /// VDF_CHECKPOINT_COUNT_IN_STEP checkpoints from the most recent step in the nonce limiter process.
    pub last_step_checkpoints: H256List,
    /// A list of the output of each step of the nonce limiting process. Note: each step
    /// has VDF_CHECKPOINT_COUNT_IN_STEP checkpoints, the last of which is that step's output.
    /// This field would be more accurately named "steps" as checkpoints are between steps.
    pub steps: H256List,
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

#[derive(Clone, Debug, Eq, Default, Serialize, Deserialize, PartialEq, Arbitrary, Compact)]
/// Stores deserialized fields from a JSON formatted Irys block header.
#[serde(rename_all = "camelCase")]
#[repr(C)]
pub struct IrysBlockHeader {
    /// The block identifier.
    pub block_hash: BlockHash,

    /// The block signature
    pub signature: IrysSignature,

    /// The block height.
    pub height: u64,

    /// Difficulty threshold used to produce the current block.
    pub diff: U256,

    /// The sum of the average number of hashes computed by the network to
    /// produce the past blocks including this one.
    pub cumulative_diff: U256,

    /// timestamp (in milliseconds) since UNIX_EPOCH of the last difficulty adjustment
    #[serde(with = "string_u128")]
    pub last_diff_timestamp: u128,

    /// The solution hash for the block hash(chunk_bytes + partition_chunk_offset + mining_seed)
    pub solution_hash: H256,

    /// The solution hash of the previous block in the chain.
    pub previous_solution_hash: H256,

    /// The solution hash of the last epoch block
    pub last_epoch_hash: H256,

    /// `SHA-256` hash of the PoA chunk (unencoded) bytes.
    pub chunk_hash: H256,

    // Previous block identifier.
    pub previous_block_hash: H256,

    /// The cumulative difficulty of the previous block
    pub previous_cumulative_diff: U256,

    /// The recall chunk proof
    pub poa: PoaData,

    /// The address that the block reward should be sent to
    pub reward_address: Address,

    /// The address of the block producer - used to validate the block hash/signature & the PoA chunk (as the packing key)
    /// We allow for miners to send rewards to a separate address
    pub miner_address: Address,

    /// timestamp (in milliseconds) since UNIX_EPOCH of when the block was discovered/produced
    #[serde(with = "string_u128")]
    pub timestamp: u128,

    /// A list of transaction ledgers, one for each active data ledger
    /// Maintains the block->tx_root->data_root relationship for each block
    /// and ledger.
    pub ledgers: Vec<TransactionLedger>,

    /// Evm block hash (32 bytes)
    pub evm_block_hash: B256,

    /// Extra block data that cannot be inlined here because of `Compact` derive macro
    pub meta: BlockMeta,
}

/// Uitlity struct that manually derives the `Compact` trait.
/// Compact trait only allows a single foreign type to be present
/// in the struct it derives, and it must be moved at the very bottom of the struct definition.
///
/// If a struct has 2 foreign types that are no regisetred in the macros source code, the `Compact`
/// trait **cannot** be automatically derived.
///
/// Once we improve the communication with reth (which is the reason we need `Arbitrary` and `Compact`),
/// We can also get rid of this util struct.
#[derive(Clone, Debug, Eq, Default, Serialize, Deserialize, PartialEq, Arbitrary)]
pub struct BlockMeta {
    /// $IRYS token price expressed in $USD
    pub irys_price: Amount<(IrysPrice, Usd)>,

    /// Metadata about the verifiable delay function, used for block verification purposes
    pub vdf_limiter_info: VDFLimiterInfo,
}

impl Compact for BlockMeta {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        let w1 = self.irys_price.to_compact(buf);
        let w2 = self.vdf_limiter_info.to_compact(buf);
        w1.saturating_add(w2)
    }

    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        let (irys_price, buf) = Amount::from_compact(buf, 16);
        let (vdf_limiter_info, buf) = VDFLimiterInfo::from_compact(buf, len.saturating_sub(16));
        (
            Self {
                irys_price,
                vdf_limiter_info,
            },
            buf,
        )
    }
}

impl IrysBlockHeader {
    /// Proxy method for `Compact::to_compact`
    ///
    /// packs all the header data into a byte buffer.
    ///
    /// ## Warning
    ///
    /// This approach is unsafe and prone to attacks.
    /// ideally we should use udigest crate - https://docs.rs/udigest/latest/udigest/
    /// for reference on why https://www.dfns.co/article/unambiguous-hashing
    pub fn digest_for_signing<B>(&self, buf: &mut B)
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        self.to_compact(buf);
    }

    /// Create a `keccak256` hash of the [`IrysBlockHeader`]
    pub fn signature_hash(&self) -> [u8; 32] {
        // allocate the buffer, guesstimate the required capacity
        let mut bytes = Vec::with_capacity(size_of::<Self>() * 3);
        self.digest_for_signing(&mut bytes);
        // we trim the `BlockHash` and `Signature` from the signing scheme
        let bytes = &bytes[size_of::<BlockHash>() + size_of::<IrysSignature>()..];
        keccak256(&bytes).0
    }

    /// Validates the block hash signature by:
    /// 1.) generating the prehash
    /// 2.) recovering the sender address, and comparing it to the block header's miner_address (miner_address MUST be part of the prehash)
    pub fn is_signature_valid(&self) -> bool {
        self.signature
            .validate_signature(self.signature_hash(), self.miner_address)
    }
}

#[derive(Default, Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Compact, Arbitrary)]
#[serde(rename_all = "camelCase")]
/// Stores deserialized fields from a `poa` (Proof of Access) JSON
pub struct PoaData {
    pub tx_path: Option<Base64>,
    pub data_path: Option<Base64>,
    pub chunk: Base64,
    pub recall_chunk_index: u32,
    pub ledger_id: Option<u32>,
    pub partition_chunk_offset: u32,
    pub partition_hash: PartitionHash,
}

pub type TxRoot = H256;

#[derive(Default, Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Compact, Arbitrary)]
#[serde(rename_all = "camelCase")]
pub struct TransactionLedger {
    /// Unique identifier for this ledger, maps to discriminant in `Ledger` enum
    pub ledger_id: u32,
    /// Root of the merkle tree built from the ledger transaction data_roots
    pub tx_root: H256,
    /// List of transaction ids included in the block
    pub tx_ids: H256List,
    /// The size of this ledger (in chunks) since genesis
    #[serde(default, with = "u64_stringify")]
    pub max_chunk_offset: u64,
    /// This ledger expires after how many epochs
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires: Option<u64>,
    /// When transactions are promoted they must include their ingress proofs
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proofs: Option<IngressProofsList>,
}

impl TransactionLedger {
    /// Computes the tx_root and tx_paths. The TX Root is composed of taking the data_roots of each of the storage
    /// transactions included, in order, and building a merkle tree out of them. The root of this tree is the tx_root.
    pub fn merklize_tx_root(data_txs: &[IrysTransactionHeader]) -> (H256, Vec<Proof>) {
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
        let data_root_leaves = generate_leaves_from_data_roots(&txs_data_roots).unwrap();
        let root = generate_data_root(data_root_leaves.clone()).unwrap();
        let root_id = root.id;
        let proofs = resolve_proofs(root, None).unwrap();
        (H256(root_id), proofs)
    }
}

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
impl IrysBlockHeader {
    pub fn new_mock_header() -> Self {
        use std::str::FromStr;
        use std::time::{SystemTime, UNIX_EPOCH};

        let tx_ids = H256List::new();
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();

        // Create a sample IrysBlockHeader object with mock data
        IrysBlockHeader {
            diff: U256::from(1000),
            cumulative_diff: U256::from(5000),
            last_diff_timestamp: 1622543200,
            solution_hash: H256::zero(),
            previous_solution_hash: H256::zero(),
            last_epoch_hash: H256::random(),
            chunk_hash: H256::zero(),
            height: 42,
            block_hash: H256::zero(),
            previous_block_hash: H256::zero(),
            previous_cumulative_diff: U256::from(4000),
            poa: PoaData {
                tx_path: None,
                data_path: None,
                chunk: Base64::from_str("").unwrap(),
                partition_hash: PartitionHash::zero(),
                partition_chunk_offset: 0,
                recall_chunk_index: 0,
                ledger_id: None,
            },
            reward_address: Address::ZERO,
            signature: IrysSignature::new(alloy_signer::Signature::test_signature()),
            timestamp: now.as_millis(),
            ledgers: vec![
                // Permanent Publish Ledger
                TransactionLedger {
                    ledger_id: 0, // Publish ledger_id
                    tx_root: H256::zero(),
                    tx_ids,
                    max_chunk_offset: 0,
                    expires: None,
                    proofs: None,
                },
                // Term Submit Ledger
                TransactionLedger {
                    ledger_id: 1, // Submit ledger_id
                    tx_root: H256::zero(),
                    tx_ids: H256List::new(),
                    max_chunk_offset: 0,
                    expires: Some(1622543200),
                    proofs: None,
                },
            ],
            evm_block_hash: B256::ZERO,
            miner_address: Address::ZERO,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{irys::IrysSigner, validate_path, CONFIG, MAX_CHUNK_SIZE};

    use super::*;
    use alloy_primitives::Signature;
    use k256::ecdsa::SigningKey;
    use rand::{rngs::StdRng, Rng, SeedableRng};
    use serde_json;
    use std::str::FromStr;
    use zerocopy::IntoBytes;

    #[test]
    fn test_irys_block_header_serde_round_trip() {
        // setup
        let header = mock_header();

        // action
        let serialized = serde_json::to_string_pretty(&header).unwrap();
        let deserialized: IrysBlockHeader = serde_json::from_str(&serialized).unwrap();

        // Assert
        assert_eq!(header, deserialized);
    }

    #[test]
    fn test_irys_header_compacat_round_trip() {
        // setup
        let header = mock_header();
        let mut buf = vec![];

        // actoin
        header.to_compact(&mut buf);
        assert!(!buf.is_empty(), "expect data to be written into the buffer");
        let (derived_header, rest_of_the_buffer) = IrysBlockHeader::from_compact(&buf, buf.len());
        assert!(
            rest_of_the_buffer.is_empty(),
            "the whole buffer should be read"
        );

        // assert
        assert_eq!(derived_header, header);
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

    #[test]
    fn test_irys_block_header_signing() {
        // setup
        let mut header = mock_header();
        let mut rng = rand::thread_rng();
        let signer = SigningKey::random(&mut rng);
        let signer = IrysSigner {
            signer,
            chain_id: CONFIG.irys_chain_id,
            chunk_size: MAX_CHUNK_SIZE,
        };

        // action
        // sign the block header
        signer.sign_block_header(&mut header).unwrap();
        let block_hash = keccak256(header.signature.as_bytes()).0;

        // assert signature
        assert_eq!(H256::from(block_hash), header.block_hash);
        assert!(header.is_signature_valid());
        let mut rng = StdRng::from_seed([42_u8; 32]);

        // assert that updating values changes the hash
        let fields: &[fn(&mut IrysBlockHeader) -> &mut [u8]] = &[
            |h: &mut IrysBlockHeader| h.height.as_mut_bytes(),
            |h: &mut IrysBlockHeader| h.last_diff_timestamp.as_mut_bytes(),
            |h: &mut IrysBlockHeader| h.solution_hash.as_bytes_mut(),
            |h: &mut IrysBlockHeader| h.previous_solution_hash.as_bytes_mut(),
            |h: &mut IrysBlockHeader| h.last_epoch_hash.as_bytes_mut(),
            |h: &mut IrysBlockHeader| h.chunk_hash.as_bytes_mut(),
            |h: &mut IrysBlockHeader| h.previous_block_hash.as_bytes_mut(),
            |h: &mut IrysBlockHeader| h.reward_address.as_mut_bytes(),
            |h: &mut IrysBlockHeader| h.miner_address.as_mut_bytes(),
            |h: &mut IrysBlockHeader| h.timestamp.as_mut_bytes(),
            |h: &mut IrysBlockHeader| h.ledgers[0].ledger_id.as_mut_bytes(),
            |h: &mut IrysBlockHeader| h.ledgers[0].max_chunk_offset.as_mut_bytes(),
            |h: &mut IrysBlockHeader| h.ledgers[0].expires.as_mut().unwrap().as_mut_bytes(),
            |h: &mut IrysBlockHeader| h.evm_block_hash.as_mut_bytes(),
            |h: &mut IrysBlockHeader| h.meta.vdf_limiter_info.global_step_number.as_mut_bytes(),
        ];
        for get_field in fields {
            let mut header_clone = header.clone();
            let get_field = get_field(&mut header_clone);
            rng.fill(get_field);
            assert!(!header_clone.is_signature_valid());
        }

        // assert that changing the block hash, the signature is still valid (because the validatoin does not validate )
        header.block_hash = H256::random();
        assert!(header.is_signature_valid());
    }

    fn mock_header() -> IrysBlockHeader {
        let tx_ids = H256List::new();
        IrysBlockHeader {
            diff: U256::from(1000),
            cumulative_diff: U256::from(5000),
            last_diff_timestamp: 1622543200,
            solution_hash: H256::zero(),
            previous_solution_hash: H256::zero(),
            last_epoch_hash: H256::random(),
            chunk_hash: H256::zero(),
            height: 42,
            block_hash: H256::zero(),
            previous_block_hash: H256::zero(),
            previous_cumulative_diff: U256::from(4000),
            poa: PoaData {
                tx_path: None,
                data_path: None,
                chunk: Base64::from_str("").unwrap(),
                partition_hash: H256::zero(),
                partition_chunk_offset: 0,
                recall_chunk_index: 0,
                ledger_id: None,
            },
            reward_address: Address::ZERO,
            signature: Signature::test_signature().into(),
            timestamp: 1622543200,
            ledgers: vec![TransactionLedger {
                ledger_id: 0, // Publish ledger_id
                tx_root: H256::zero(),
                tx_ids,
                proofs: None,
                max_chunk_offset: 100,
                expires: Some(1622543200),
            }],
            evm_block_hash: B256::ZERO,
            miner_address: Address::ZERO,
            ..Default::default()
        }
    }
}
