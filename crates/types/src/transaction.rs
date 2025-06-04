use crate::{
    address_base58_stringify, optional_string_u64, string_u64, Address, Arbitrary, Base64, Compact,
    ConsensusConfig, IrysSignature, Node, Proof, Signature, TxIngressProof, H256,
};
use alloy_primitives::keccak256;
use alloy_rlp::{Encodable, RlpDecodable, RlpEncodable};
use irys_primitives::CommitmentType;
use serde::{Deserialize, Serialize};

pub type IrysTransactionId = H256;

#[derive(
    Clone,
    Debug,
    Eq,
    Serialize,
    Default,
    Deserialize,
    PartialEq,
    Arbitrary,
    Compact,
    RlpEncodable,
    RlpDecodable,
)]
#[rlp(trailing)]
/// Stores deserialized fields from a JSON formatted Irys transaction header.
/// will decode from strings or numeric literals for u64 fields, due to JS's max safe int being 2^53-1 instead of 2^64
/// We include the Irys prefix to differentiate from EVM transactions.
#[serde(rename_all = "camelCase", default)]
pub struct IrysTransactionHeader {
    /// A SHA-256 hash of the transaction signature.
    #[rlp(skip)]
    #[rlp(default)]
    // NOTE: both rlp skip AND rlp default must be present in order for field skipping to work
    pub id: H256,

    /// The transaction's version
    pub version: u8,

    /// block_hash of a recent (last 50) blocks or the a recent transaction id
    /// from the signer. Multiple transactions can share the same anchor.
    pub anchor: H256,

    /// The ecdsa/secp256k1 public key of the transaction signer
    #[serde(default, with = "address_base58_stringify")]
    pub signer: Address,

    /// The merkle root of the transactions data chunks
    // #[serde(default, with = "address_base58_stringify")]
    pub data_root: H256,

    /// Size of the transaction data in bytes
    #[serde(with = "string_u64")]
    pub data_size: u64,

    /// Funds the storage of the transaction data during the storage term
    #[serde(with = "string_u64")]
    pub term_fee: u64,

    /// Destination ledger for the transaction, default is 0 - Permanent Ledger
    pub ledger_id: u32,

    /// EVM chain ID - used to prevent cross-chain replays
    #[serde(with = "string_u64")]
    pub chain_id: u64,

    /// Transaction signature bytes
    #[rlp(skip)]
    #[rlp(default)]
    pub signature: IrysSignature,

    #[serde(default, with = "optional_string_u64")]
    pub bundle_format: Option<u64>,

    /// Funds the storage of the transaction for the next 200+ years
    #[serde(default, with = "optional_string_u64")]
    pub perm_fee: Option<u64>,

    /// INTERNAL: Signed ingress proofs used to promote this transaction to the Publish ledger
    /// TODO: put these somewhere else?
    #[rlp(skip)]
    #[rlp(default)]
    pub ingress_proofs: Option<TxIngressProof>,
}

impl IrysTransactionHeader {
    /// RLP Encoding of Transactions for Signing
    ///
    /// When RLP encoding a transaction for signing, an extra byte is included
    /// for the transaction type. This serves to simplify future parsing and
    /// decoding of RLP-encoded headers.
    ///
    /// When signing a transaction, the prehash is formed by RLP encoding the
    /// transaction's header fields. It's important to note that the prehash
    ///
    /// **excludes** certain fields:
    ///
    /// - **Transaction ID**: This is excluded from the prehash.
    /// - **Signature fields**: These are not part of the prehash.
    /// - **Optional fields**: Any optional fields that are `Option::None` are
    ///                        also excluded from the prehash.
    ///
    /// This method ensures that the transaction signature reflects only the
    /// essential data needed for validation and security purposes.
    pub fn encode_for_signing(&self, out: &mut dyn alloy_rlp::BufMut) {
        self.encode(out)
    }

    pub fn signature_hash(&self) -> [u8; 32] {
        let mut bytes = Vec::new();
        self.encode_for_signing(&mut bytes);

        keccak256(&bytes).0
    }

    /// Validates the transaction signature by:
    /// 1.) generating the prehash
    /// 2.) recovering the sender address, and comparing it to the tx's sender (sender MUST be part of the prehash)
    pub fn is_signature_valid(&self) -> bool {
        self.signature
            .validate_signature(self.signature_hash(), self.signer)
    }
}

/// Wrapper for the underlying IrysTransactionHeader fields, this wrapper
/// contains the data/chunk/proof info that is necessary for clients to seed
/// a transactions data to the network.
#[derive(Clone, Default, Debug, Serialize, Deserialize, PartialEq)]
pub struct IrysTransaction {
    pub header: IrysTransactionHeader,
    // TODO: make this compatible with stream/iterator data sources
    pub data: Option<Base64>,
    #[serde(skip)]
    pub chunks: Vec<Node>,
    #[serde(skip)]
    pub proofs: Vec<Proof>,
}

impl IrysTransaction {
    pub fn signature_hash(&self) -> [u8; 32] {
        self.header.signature_hash()
    }
}

impl IrysTransactionHeader {
    pub fn new(config: &ConsensusConfig) -> Self {
        IrysTransactionHeader {
            id: H256::zero(),
            anchor: H256::zero(),
            signer: Address::default(),
            data_root: H256::zero(),
            data_size: 0,
            term_fee: 0,
            perm_fee: None,
            ledger_id: 0,
            bundle_format: None,
            version: 0,
            chain_id: config.chain_id,
            signature: Signature::test_signature().into(),
            ingress_proofs: None,
        }
    }
}

pub type TxPath = Vec<u8>;

/// sha256(tx_path)
pub type TxPathHash = H256;

#[derive(
    Clone,
    Debug,
    Eq,
    Serialize,
    Default,
    Deserialize,
    PartialEq,
    Arbitrary,
    Compact,
    RlpEncodable,
    RlpDecodable,
)]
#[rlp(trailing)]
/// Stores deserialized fields from a JSON formatted commitment transaction.
#[serde(rename_all = "camelCase", default)]
pub struct CommitmentTransaction {
    // NOTE: both rlp skip AND rlp default must be present in order for field skipping to work
    #[rlp(skip)]
    #[rlp(default)]
    /// A SHA-256 hash of the transaction signature.
    pub id: H256,

    /// block_hash of a recent (last 50) blocks or the a recent transaction id
    /// from the signer. Multiple transactions can share the same anchor.
    pub anchor: H256,

    /// The ecdsa/secp256k1 public key of the transaction signer
    #[serde(default, with = "address_base58_stringify")]
    pub signer: Address,

    /// The type of commitment Stake/UnStake Pledge/UnPledge
    pub commitment_type: CommitmentType,

    /// The transaction's version
    pub version: u8,

    /// EVM chain ID - used to prevent cross-chain replays
    #[serde(with = "string_u64")]
    pub chain_id: u64,

    /// Pay the fee required to mitigate tx spam
    #[serde(with = "string_u64")]
    pub fee: u64,

    /// Transaction signature bytes
    #[rlp(skip)]
    #[rlp(default)]
    pub signature: IrysSignature,
}

impl CommitmentTransaction {
    /// Rely on RLP encoding for signing
    pub fn encode_for_signing(&self, out: &mut dyn alloy_rlp::BufMut) {
        self.encode(out)
    }

    pub fn signature_hash(&self) -> [u8; 32] {
        let mut bytes = Vec::new();
        self.encode_for_signing(&mut bytes);

        keccak256(&bytes).0
    }

    /// Validates the transaction signature by:
    /// 1.) generating the prehash (signature_hash)
    /// 2.) recovering the sender address, and comparing it to the tx's sender (sender MUST be part of the prehash)
    pub fn is_signature_valid(&self) -> bool {
        self.signature
            .validate_signature(self.signature_hash(), self.signer)
    }
}

// Trait to abstract common behavior
pub trait IrysTransactionCommon {
    fn is_signature_valid(&self) -> bool;
    fn id(&self) -> IrysTransactionId;
    fn total_fee(&self) -> u64;
    fn signer(&self) -> Address;
}

impl IrysTransactionCommon for IrysTransactionHeader {
    fn is_signature_valid(&self) -> bool {
        self.is_signature_valid()
    }

    fn id(&self) -> IrysTransactionId {
        self.id
    }

    fn total_fee(&self) -> u64 {
        self.perm_fee.unwrap_or(0) + self.term_fee
    }

    fn signer(&self) -> Address {
        self.signer
    }
}

impl IrysTransactionCommon for CommitmentTransaction {
    fn is_signature_valid(&self) -> bool {
        self.is_signature_valid()
    }

    fn id(&self) -> IrysTransactionId {
        self.id
    }

    fn total_fee(&self) -> u64 {
        self.fee
    }
    fn signer(&self) -> Address {
        self.signer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::irys::IrysSigner;

    use alloy_rlp::Decodable;

    use k256::ecdsa::SigningKey;
    use serde_json;

    #[test]
    fn test_irys_transaction_header_rlp_round_trip() {
        // setup
        let config = ConsensusConfig::testnet();
        let mut header = mock_header(&config);

        // action
        let mut buffer = vec![];
        header.encode(&mut buffer);
        let decoded = IrysTransactionHeader::decode(&mut buffer.as_slice()).unwrap();

        // Assert
        // zero out the id and signature, those do not get encoded
        header.id = H256::zero();
        header.signature = IrysSignature::new(Signature::try_from([0_u8; 65].as_slice()).unwrap());
        assert_eq!(header, decoded);
    }

    #[test]
    fn test_commitment_transaction_rlp_round_trip() {
        // setup
        let config = ConsensusConfig::testnet();
        let mut header = mock_commitment_tx(&config);

        // action
        let mut buffer = vec![];
        header.encode(&mut buffer);
        let decoded = CommitmentTransaction::decode(&mut buffer.as_slice()).unwrap();

        // Assert
        // zero out the id and signature, those do not get encoded
        header.id = H256::zero();
        header.signature = IrysSignature::new(Signature::try_from([0_u8; 65].as_slice()).unwrap());
        assert_eq!(header, decoded);
    }

    #[test]
    fn test_irys_transaction_header_serde() {
        // Create a sample IrysTransactionHeader
        let config = ConsensusConfig::testnet();
        let original_header = mock_header(&config);

        // Serialize the IrysTransactionHeader to JSON
        let serialized =
            serde_json::to_string_pretty(&original_header).expect("Failed to serialize");

        println!("{}", &serialized);
        // Deserialize the JSON back to IrysTransactionHeader
        let deserialized: IrysTransactionHeader =
            serde_json::from_str(&serialized).expect("Failed to deserialize");

        // Ensure the deserialized struct matches the original
        assert_eq!(original_header, deserialized);
    }

    #[test]
    fn test_commitment_transaction_serde() {
        // Create a sample commitment tx
        let config = ConsensusConfig::testnet();
        let original_tx = mock_commitment_tx(&config);

        // Serialize the commitment tx to JSON
        let serialized = serde_json::to_string_pretty(&original_tx).expect("Failed to serialize");

        println!("{}", &serialized);
        // Deserialize the JSON back to a commitment tx
        let deserialized: CommitmentTransaction =
            serde_json::from_str(&serialized).expect("Failed to deserialize");

        // Ensure the deserialized tx matches the original
        assert_eq!(original_tx, deserialized);
    }

    #[test]
    fn test_tx_encode_and_signing() {
        // setup
        let config = ConsensusConfig::testnet();
        let original_header = mock_header(&config);
        let mut sig_data = Vec::new();
        original_header.encode(&mut sig_data);
        let dec: IrysTransactionHeader =
            IrysTransactionHeader::decode(&mut sig_data.as_slice()).unwrap();

        // action
        let signer = IrysSigner {
            signer: SigningKey::random(&mut rand::thread_rng()),
            chain_id: config.chain_id,
            chunk_size: config.chunk_size,
        };
        let tx = IrysTransaction {
            header: dec,
            ..Default::default()
        };

        let signed_tx = signer.sign_transaction(tx.clone()).unwrap();

        assert!(signed_tx.header.is_signature_valid());
    }

    #[test]
    fn test_commitment_tx_encode_and_signing() {
        // setup
        let config = ConsensusConfig::testnet();
        let original_tx = mock_commitment_tx(&config);
        let mut sig_data = Vec::new();
        original_tx.encode(&mut sig_data);
        let _dec = CommitmentTransaction::decode(&mut sig_data.as_slice()).unwrap();

        // action
        let signer = IrysSigner {
            signer: SigningKey::random(&mut rand::thread_rng()),
            chain_id: config.chain_id,
            chunk_size: config.chunk_size,
        };

        let signed_tx = signer.sign_commitment(original_tx.clone()).unwrap();

        println!(
            "{}",
            serde_json::to_string_pretty(&signed_tx).expect("Failed to serialize")
        );

        assert!(signed_tx.is_signature_valid());
    }

    fn mock_header(config: &ConsensusConfig) -> IrysTransactionHeader {
        
        IrysTransactionHeader {
            id: H256::from([255u8; 32]),
            anchor: H256::from([1u8; 32]),
            signer: Address::default(),
            data_root: H256::from([3u8; 32]),
            data_size: 1024,
            term_fee: 100,
            perm_fee: Some(200),
            ledger_id: 1,
            bundle_format: None,
            chain_id: config.chain_id,
            version: 0,
            ingress_proofs: None,
            signature: Signature::test_signature().into(),
        }
    }

    fn mock_commitment_tx(config: &ConsensusConfig) -> CommitmentTransaction {
        
        CommitmentTransaction {
            id: H256::from([255u8; 32]),
            anchor: H256::from([1u8; 32]),
            signer: Address::default(),
            commitment_type: CommitmentType::Stake,
            version: 0,
            fee: 1,
            chain_id: config.chain_id,
            signature: Signature::test_signature().into(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum IrysTransactionResponse {
    #[serde(rename = "commitment")]
    Commitment(CommitmentTransaction),

    #[serde(rename = "storage")]
    Storage(IrysTransactionHeader),
}

impl From<CommitmentTransaction> for IrysTransactionResponse {
    fn from(tx: CommitmentTransaction) -> Self {
        IrysTransactionResponse::Commitment(tx)
    }
}

impl From<IrysTransactionHeader> for IrysTransactionResponse {
    fn from(tx: IrysTransactionHeader) -> Self {
        IrysTransactionResponse::Storage(tx)
    }
}
