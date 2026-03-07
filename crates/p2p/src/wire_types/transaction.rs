use irys_types::{BoundedFee, IrysAddress, IrysSignature, H256};
use serde::{Deserialize, Serialize};

use super::impl_json_version_tagged_serde;

/// Sovereign wire type for the inner DataTransactionHeaderV1 fields.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DataTransactionHeaderV1Inner {
    pub id: H256,
    pub anchor: H256,
    pub signer: IrysAddress,
    pub data_root: H256,
    #[serde(with = "irys_types::string_u64")]
    pub data_size: u64,
    #[serde(with = "irys_types::string_u64")]
    pub header_size: u64,
    pub term_fee: BoundedFee,
    pub ledger_id: u32,
    #[serde(with = "irys_types::string_u64")]
    pub chain_id: u64,
    pub signature: IrysSignature,
    #[serde(default, with = "irys_types::optional_string_u64")]
    pub bundle_format: Option<u64>,
    #[serde(default)]
    pub perm_fee: Option<BoundedFee>,
}

/// Sovereign wire type for DataTransactionHeader (versioned, IntegerTagged-compatible).
#[derive(Debug, Clone, PartialEq)]
pub enum DataTransactionHeader {
    V1(DataTransactionHeaderV1Inner),
}

impl_json_version_tagged_serde!(DataTransactionHeader { 1 => V1(DataTransactionHeaderV1Inner) });

// -- Conversions --

impl From<&irys_types::DataTransactionHeader> for DataTransactionHeader {
    fn from(h: &irys_types::DataTransactionHeader) -> Self {
        match h {
            irys_types::DataTransactionHeader::V1(wm) => Self::V1(DataTransactionHeaderV1Inner {
                id: wm.tx.id,
                anchor: wm.tx.anchor,
                signer: wm.tx.signer,
                data_root: wm.tx.data_root,
                data_size: wm.tx.data_size,
                header_size: wm.tx.header_size,
                term_fee: wm.tx.term_fee,
                ledger_id: wm.tx.ledger_id,
                chain_id: wm.tx.chain_id,
                signature: wm.tx.signature,
                bundle_format: wm.tx.bundle_format,
                perm_fee: wm.tx.perm_fee,
            }),
        }
    }
}

impl From<DataTransactionHeader> for irys_types::DataTransactionHeader {
    fn from(h: DataTransactionHeader) -> Self {
        match h {
            DataTransactionHeader::V1(inner) => {
                Self::V1(irys_types::DataTransactionHeaderV1WithMetadata {
                    tx: irys_types::DataTransactionHeaderV1 {
                        id: inner.id,
                        anchor: inner.anchor,
                        signer: inner.signer,
                        data_root: inner.data_root,
                        data_size: inner.data_size,
                        header_size: inner.header_size,
                        term_fee: inner.term_fee,
                        ledger_id: inner.ledger_id,
                        chain_id: inner.chain_id,
                        signature: inner.signature,
                        bundle_format: inner.bundle_format,
                        perm_fee: inner.perm_fee,
                    },
                    // Metadata is not transmitted over the wire; initialize to default on deserialization.
                    metadata: Default::default(),
                })
            }
        }
    }
}
