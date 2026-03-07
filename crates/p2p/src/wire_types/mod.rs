//! Sovereign wire types for the gossip protocol.
//!
//! These types decouple the gossip protocol's JSON serialization format from the
//! canonical `irys_types` structs. By owning their own `#[serde]` attributes
//! ("sovereign" — they control their serialization independently), wire types
//! prevent accidental wire-format breakage when internal types are refactored.
//!
//! **Serialization flow:** canonical type → wire type → JSON (outbound).
//! **Deserialization flow:** JSON → wire type → canonical type (inbound).
//!
//! **Parity invariant:** Wire types MUST produce JSON output identical to their
//! canonical `irys_types` counterparts. The server serializes using wire types,
//! but some client deserialization paths still use canonical types directly. If
//! parity drifts, deserialization will fail at runtime. The parity tests in
//! `tests.rs` and fixture tests in `gossip_fixture_tests.rs` enforce this.
//!
//! **Adding a new wire type:**
//! 1. Create a struct with matching serde attributes in the appropriate file.
//! 2. Add `From` impls in both directions (canonical ↔ wire).
//! 3. Add a parity test in `tests.rs`.
//! 4. Add a fixture test in `gossip_fixture_tests.rs`.

mod block;
mod chunk;
mod commitment;
mod gossip;
mod handshake;
mod ingress;
mod transaction;

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
pub(crate) use transaction::*;

/// Implements JSON-specific `Serialize` and `Deserialize` for a versioned enum
/// that flattens as `{"version": N, ...inner_fields}` (IntegerTagged-compatible).
///
/// **JSON-only**: The Serialize impl uses `serde_json::to_value` internally.
/// These types must only be used with JSON serialization — other formats
/// (bincode, msgpack, etc.) will produce incorrect output.
macro_rules! impl_json_version_tagged_serde {
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

pub(crate) use impl_json_version_tagged_serde;

/// Generates bidirectional `From` impls between a canonical type and a mirror
/// wire type that has the same field names.
///
/// - `From<&$canonical> for $wire` — clones each field (works for both Copy and Clone types)
/// - `From<$wire> for $canonical` — moves each field
macro_rules! impl_mirror_from {
    ($canonical:ty => $wire:ty { $($field:ident),* $(,)? }) => {
        impl From<&$canonical> for $wire {
            fn from(src: &$canonical) -> Self {
                Self { $( $field: src.$field.clone(), )* }
            }
        }
        impl From<$wire> for $canonical {
            fn from(src: $wire) -> Self {
                Self { $( $field: src.$field, )* }
            }
        }
    };
}

pub(crate) use impl_mirror_from;
