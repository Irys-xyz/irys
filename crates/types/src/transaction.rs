use crate::{address_base58_stringify, Address, Arbitrary, Compact, Node, Proof, Signature};
use alloy_primitives::{keccak256, FixedBytes};
use alloy_rlp::{Encodable, RlpDecodable, RlpEncodable};
use reth_primitives::recover_signer_unchecked;
use serde::{Deserialize, Serialize};

use crate::{Base64, IrysSignature, H256};

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
/// We include the Irys prefix to differentiate from EVM transactions.
pub struct IrysTransactionHeader {
    /// A SHA-256 hash of the transaction signature.
    pub id: H256,

    /// block_hash of a recent (last 50) blocks or the a recent transaction id
    /// from the signer. Multiple transactions can share the same anchor.
    pub anchor: H256,

    /// The ecdsa/secp256k1 public key of the transaction signer
    #[serde(default, with = "address_base58_stringify")]
    pub signer: Address,

    /// The merkle root of the transactions data chunks
    pub data_root: H256,

    /// Size of the transaction data in bytes
    pub data_size: u64,

    /// Funds the storage of the transaction data during the storage term
    pub term_fee: u64,

    /// Bundles are critical for how data items are indexed and settled, different
    /// bundle formats enable different levels of indexing and verification.
    pub bundle_format: u64,

    /// Indicating the type of transaction, pledge, data, schema, etc.
    pub tx_type: u64,

    /// Transaction signature bytes
    pub signature: IrysSignature,

    /// Funds the storage of the transaction for the next 200+ years
    pub perm_fee: Option<u64>,

    /// Destination ledger for the transaction, default is 0 - Permanent Ledger
    pub ledger_num: Option<u64>,
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
        out.put_u8(self.tx_type as u8);
        self.anchor.encode(out);
        self.signer.encode(out);
        self.data_root.encode(out);
        self.data_size.encode(out);
        self.term_fee.encode(out);
        self.bundle_format.encode(out);
        self.tx_type.encode(out);

        // Encode the optional fields if they are provided
        if let Some(perm_fee) = self.perm_fee {
            perm_fee.encode(out);
        }

        if let Some(ledger_num) = self.ledger_num {
            ledger_num.encode(out);
        }
    }

    pub fn signature_hash(&self) -> FixedBytes<32> {
        let mut bytes = Vec::new();
        self.encode_for_signing(&mut bytes);
        
        keccak256(&bytes)
    }

    /// Validate the transaction signature, comparing it to the signer and
    /// recovers the address from the signature.
    pub fn is_signature_valid(&self) -> bool {
        let prehash = self.signature_hash();
        let sig = self.signature.as_bytes();

        // We don't need to compare the recovered signer with the singer in the
        // tx because it is included in the signature_hash
        recover_signer_unchecked(&sig, &prehash).is_ok()
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
            ledger_num: None,
            bundle_format: 0,
            tx_type: 0,
            signature: IrysSignature {
                reth_signature: Signature::test_signature(),
            },
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
    use super::*;
    use serde_json;

    #[test]
    fn test_irys_transaction_header_serde() {
        // Create a sample IrysTransactionHeader
        let original_header = IrysTransactionHeader {
            id: H256::from([0u8; 32]),
            anchor: H256::from([1u8; 32]),
            signer: Address::default(),
            data_root: H256::from([3u8; 32]),
            data_size: 1024,
            term_fee: 100,
            perm_fee: Some(200),
            ledger_num: Some(1),
            bundle_format: 0,
            tx_type: 1,
            signature: IrysSignature {
                reth_signature: Signature::test_signature(),
            },
        };

        // Serialize the IrysTransactionHeader to JSON
        let serialized = serde_json::to_string(&original_header).expect("Failed to serialize");

        //println!("\n{}\n", serialized);

        // Deserialize the JSON back to IrysTransactionHeader
        let deserialized: IrysTransactionHeader =
            serde_json::from_str(&serialized).expect("Failed to deserialize");

        // println!("\n original_header: {:?}\n", original_header);
        // println!("\n deserialized: {:?}\n", deserialized);

        // Ensure the deserialized struct matches the original
        assert_eq!(original_header, deserialized);
    }
}
