use irys_types::{BoundedFee, IrysAddress, IrysSignature, H256};
use serde::{Deserialize, Serialize};

use super::{impl_json_version_tagged_serde, impl_versioned_tx_from};

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

impl_versioned_tx_from!(
    irys_types::DataTransactionHeader => DataTransactionHeader {
        V1 {
            gossip: DataTransactionHeaderV1Inner,
            meta: irys_types::DataTransactionHeaderV1WithMetadata,
            tx: irys_types::DataTransactionHeaderV1,
            fields {
                id, anchor, signer, data_root, data_size, header_size,
                term_fee, ledger_id, chain_id, signature, bundle_format, perm_fee,
            }
        },
    }
);

/// Wire type for [`irys_types::IrysTransactionResponse`].
///
/// Mirrors the `#[serde(tag = "type", rename_all = "camelCase")]` representation
/// so that upstream changes to the tagging or variant names are detected.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum IrysTransactionResponse {
    #[serde(rename = "commitment")]
    Commitment(super::CommitmentTransaction),

    #[serde(rename = "storage")]
    Storage(DataTransactionHeader),
}

impl From<&irys_types::IrysTransactionResponse> for IrysTransactionResponse {
    fn from(r: &irys_types::IrysTransactionResponse) -> Self {
        match r {
            irys_types::IrysTransactionResponse::Commitment(c) => Self::Commitment(c.into()),
            irys_types::IrysTransactionResponse::Storage(s) => Self::Storage(s.into()),
        }
    }
}

impl From<IrysTransactionResponse> for irys_types::IrysTransactionResponse {
    fn from(r: IrysTransactionResponse) -> Self {
        match r {
            IrysTransactionResponse::Commitment(c) => Self::Commitment(c.into()),
            IrysTransactionResponse::Storage(s) => Self::Storage(s.into()),
        }
    }
}
