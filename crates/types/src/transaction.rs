use crate::{
    address_base58_stringify, optional_string_u64, string_u64, Address, Arbitrary, Base64, Compact,
    IrysSignature, Node, Proof, Signature, H256, IRYS_CHAIN_ID,
};
use alloy_primitives::{keccak256, FixedBytes};
use alloy_rlp::{Encodable, RlpDecodable, RlpEncodable};
use serde::{Deserialize, Serialize};

pub type IrysTransactionId = H256;

#[derive(
    Clone,
    Debug,
    Eq,
    Serialize,
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
    #[serde(with = "string_u64")]
    pub ledger_num: u64,

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

    pub fn signature_hash(&self) -> FixedBytes<32> {
        let mut bytes = Vec::new();
        self.encode_for_signing(&mut bytes);

        keccak256(&bytes)
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
    pub data: Base64,
    #[serde(skip)]
    pub chunks: Vec<Node>,
    #[serde(skip)]
    pub proofs: Vec<Proof>,
}

impl IrysTransaction {
    pub fn signature_hash(&self) -> [u8; 32] {
        self.header.signature_hash().0
    }
}

impl Default for IrysTransactionHeader {
    fn default() -> Self {
        IrysTransactionHeader {
            id: H256::zero(),
            anchor: H256::zero(),
            signer: Address::default(),
            data_root: H256::zero(),
            data_size: 0,
            term_fee: 0,
            perm_fee: None,
            ledger_num: 0,
            bundle_format: None,
            version: 0,
            chain_id: IRYS_CHAIN_ID,
            signature: Signature::test_signature().into(),
        }
    }
}

pub type TxPath = Vec<u8>;

/// sha256(tx_path)
pub type TxPathHash = H256;

//==============================================================================
// Tests
//------------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use crate::{irys::IrysSigner, IRYS_CHAIN_ID, MAX_CHUNK_SIZE};

    use super::*;
    use alloy_core::hex::{self};
    use alloy_rlp::Decodable;

    use k256::ecdsa::SigningKey;
    use serde_json;

    #[test]
    fn test_irys_transaction_header_serde() {
        // Create a sample IrysTransactionHeader
        let original_header = IrysTransactionHeader {
            id: H256::from([255u8; 32]),
            anchor: H256::from([1u8; 32]),
            signer: Address::default(),
            data_root: H256::from([3u8; 32]),
            data_size: 1024,
            term_fee: 100,
            perm_fee: Some(200),
            ledger_num: 1,
            bundle_format: None,
            chain_id: IRYS_CHAIN_ID,
            version: 0,
            signature: Signature::test_signature().into(),
        };

        // Serialize the IrysTransactionHeader to JSON
        let serialized = serde_json::to_string(&original_header).expect("Failed to serialize");

        println!("{}", &serialized);
        // Deserialize the JSON back to IrysTransactionHeader
        let deserialized: IrysTransactionHeader =
            serde_json::from_str(&serialized).expect("Failed to deserialize");

        // Ensure the deserialized struct matches the original
        assert_eq!(original_header, deserialized);
    }

    const DEV_PRIVATE_KEY: &str =
        "db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0";
    const DEV_ADDRESS: &str = "64f1a2829e0e698c18e7792d6e74f67d89aa0a32";

    #[test]
    fn test_tx_encode_and_signing() {
        // Create a sample IrysTransactionHeader
        // commented out fields are defaulted by the RLP decoder
        let original_header = IrysTransactionHeader {
            // id: H256::from([255u8; 32]),
            id: Default::default(),
            anchor: H256::from([1u8; 32]),
            signer: Address::ZERO,
            data_root: H256::from([3u8; 32]),
            data_size: 1024,
            term_fee: 100,
            // perm_fee: Some(200),
            perm_fee: None,
            ledger_num: 0,
            chain_id: IRYS_CHAIN_ID,
            bundle_format: None,
            version: 0,
            signature: Default::default(),
        };

        let mut sig_data = Vec::new();

        original_header.encode(&mut sig_data);

        let dec: IrysTransactionHeader =
            IrysTransactionHeader::decode(&mut sig_data.as_slice()).unwrap();
        assert_eq!(&dec, &original_header);

        let signer = IrysSigner {
            signer: SigningKey::from_slice(hex::decode(DEV_PRIVATE_KEY).unwrap().as_slice())
                .unwrap(),
            chain_id: IRYS_CHAIN_ID,
            chunk_size: MAX_CHUNK_SIZE,
        };

        let tx = IrysTransaction {
            header: dec,
            ..Default::default()
        };

        let signed_tx = signer.sign_transaction(tx.clone()).unwrap();

        assert!(signed_tx.header.is_signature_valid());
    }
}
