use irys_types::{BoundedFee, IrysAddress, IrysSignature, H256};
use serde::{Deserialize, Serialize};

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

impl Serialize for DataTransactionHeader {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap as _;
        let (version, inner_value) = match self {
            Self::V1(inner) => (
                1_u8,
                serde_json::to_value(inner).map_err(serde::ser::Error::custom)?,
            ),
        };
        if let serde_json::Value::Object(inner_map) = inner_value {
            let mut map = serializer.serialize_map(Some(inner_map.len() + 1))?;
            map.serialize_entry("version", &version)?;
            for (key, value) in inner_map {
                map.serialize_entry(&key, &value)?;
            }
            map.end()
        } else {
            Err(serde::ser::Error::custom("inner value must be a struct"))
        }
    }
}

impl<'de> Deserialize<'de> for DataTransactionHeader {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        if let serde_json::Value::Object(obj) = value {
            let mut version: Option<u8> = None;
            let mut fields = serde_json::Map::new();
            for (key, val) in obj {
                if key == "version" {
                    version = Some(serde_json::from_value(val).map_err(serde::de::Error::custom)?);
                } else {
                    fields.insert(key, val);
                }
            }
            match version {
                Some(1) => {
                    let inner: DataTransactionHeaderV1Inner =
                        serde_json::from_value(serde_json::Value::Object(fields))
                            .map_err(serde::de::Error::custom)?;
                    Ok(Self::V1(inner))
                }
                Some(v) => Err(serde::de::Error::custom(format!("unknown version: {}", v))),
                None => Err(serde::de::Error::missing_field("version")),
            }
        } else {
            Err(serde::de::Error::custom("expected object"))
        }
    }
}

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

impl TryFrom<DataTransactionHeader> for irys_types::DataTransactionHeader {
    type Error = eyre::Report;
    fn try_from(h: DataTransactionHeader) -> eyre::Result<Self> {
        match h {
            DataTransactionHeader::V1(inner) => Ok(Self::V1(
                irys_types::DataTransactionHeaderV1WithMetadata {
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
                    metadata: Default::default(),
                },
            )),
        }
    }
}
