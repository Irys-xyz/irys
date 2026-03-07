pub(crate) mod block;
pub(crate) mod chunk;
pub(crate) mod commitment;
pub(crate) mod gossip;
pub(crate) mod handshake;
pub(crate) mod ingress;
pub(crate) mod response;
pub(crate) mod transaction;

#[cfg(test)]
pub(crate) mod test_helpers;
#[cfg(test)]
mod tests;

pub(crate) use block::*;
pub(crate) use chunk::*;
pub(crate) use commitment::*;
pub(crate) use gossip::*;
pub(crate) use handshake::*;
pub(crate) use ingress::*;
pub(crate) use response::*;
pub(crate) use transaction::*;

/// Implements `Serialize` and `Deserialize` for a versioned enum that flattens
/// as `{"version": N, ...inner_fields}` (IntegerTagged-compatible JSON).
///
/// NOTE: The Serialize impl uses `serde_json::to_value` internally and is
/// designed specifically for JSON wire format. It may not work correctly with
/// other serialization formats.
macro_rules! impl_version_tagged_serde {
    ($enum_name:ident { $($version:literal => $variant:ident($inner:ty)),+ $(,)? }) => {
        impl serde::Serialize for $enum_name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                use serde::ser::SerializeMap as _;
                let (version, inner_value) = match self {
                    $(
                        Self::$variant(inner) => (
                            $version as u8,
                            serde_json::to_value(inner).map_err(serde::ser::Error::custom)?,
                        ),
                    )+
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

        impl<'de> serde::Deserialize<'de> for $enum_name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let value = <serde_json::Value as serde::Deserialize>::deserialize(deserializer)?;
                if let serde_json::Value::Object(obj) = value {
                    let mut version: Option<u8> = None;
                    let mut fields = serde_json::Map::new();
                    for (key, val) in obj {
                        if key == "version" {
                            version = Some(
                                serde_json::from_value(val)
                                    .map_err(serde::de::Error::custom)?,
                            );
                        } else {
                            fields.insert(key, val);
                        }
                    }
                    let inner_value = serde_json::Value::Object(fields);
                    match version {
                        $(
                            Some($version) => {
                                let inner: $inner = serde_json::from_value(inner_value)
                                    .map_err(serde::de::Error::custom)?;
                                Ok(Self::$variant(inner))
                            }
                        )+
                        Some(v) => Err(serde::de::Error::custom(
                            format!("unknown {} version: {}", stringify!($enum_name), v),
                        )),
                        None => Err(serde::de::Error::missing_field("version")),
                    }
                } else {
                    Err(serde::de::Error::custom(
                        concat!("expected object for ", stringify!($enum_name)),
                    ))
                }
            }
        }
    };
}

pub(crate) use impl_version_tagged_serde;
