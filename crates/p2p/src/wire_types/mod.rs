//! Sovereign wire types for the gossip protocol.
//!
//! These types decouple the gossip protocol's JSON serialization format from the
//! canonical `irys_types` structs. By owning their own `#[serde]` attributes, wire types
//! prevent accidental wire-format breakage when internal types are refactored.
//!
//! **Serialization flow:** canonical type → wire type → JSON (outbound).
//! **Deserialization flow:** JSON → wire type → canonical type (inbound).
//!
//! **Adding a new wire type:**
//! 1. Create a struct with matching serde attributes in the appropriate file.
//! 2. Add `From` impls in both directions (canonical ↔ wire).
//! 3. Add a roundtrip test in `tests.rs`.

mod block;
mod block_index;
mod chunk;
mod commitment;
mod gossip;
mod handshake;
mod ingress;
mod node_info;
mod transaction;

#[cfg(test)]
pub(crate) mod test_helpers;
#[cfg(test)]
mod tests;

pub(crate) use block::*;
pub(crate) use block_index::*;
pub(crate) use chunk::*;
pub(crate) use commitment::*;
pub(crate) use gossip::*;
pub(crate) use handshake::*;
pub(crate) use ingress::*;
pub(crate) use node_info::*;
pub(crate) use transaction::*;

// These envelope/response types live in `crate::types` but ARE wire types —
// they're serialized directly into gossip HTTP responses. Re-exported here so
// the wire protocol surface is defined in one place and fixture-tested alongside
// the rest of the wire types.
pub(crate) use crate::types::{GossipResponse, HandshakeRequirementReason, RejectionReason};

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
/// - `From<$canonical> for $wire` — moves each field
/// - `From<$wire> for $canonical` — moves each field
///
/// Both conversions take ownership. If you only have a reference and need to
/// convert, clone explicitly at the call site.
macro_rules! impl_mirror_from {
    // Basic form: all fields are moved verbatim.
    ($canonical:ty => $wire:ty { $($field:ident),* $(,)? }) => {
        impl From<$canonical> for $wire {
            fn from(src: $canonical) -> Self {
                Self { $( $field: src.$field, )* }
            }
        }
        impl From<$wire> for $canonical {
            fn from(src: $wire) -> Self {
                Self { $( $field: src.$field, )* }
            }
        }
    };

    // Extended form: `move` fields are moved verbatim, `convert` fields call
    // `.into()`, and `convert_iter` fields call `.into_iter().map(Into::into).collect()`.
    ($canonical:ty => $wire:ty {
        $($field:ident),* $(,)?
    }
    $(convert { $($conv:ident),* $(,)? })?
    $(convert_iter { $($conv_iter:ident),* $(,)? })?
    ) => {
        impl From<$canonical> for $wire {
            fn from(src: $canonical) -> Self {
                Self {
                    $( $field: src.$field, )*
                    $($( $conv: src.$conv.into(), )*)?
                    $($( $conv_iter: src.$conv_iter.into_iter().map(Into::into).collect(), )*)?
                }
            }
        }
        impl From<$wire> for $canonical {
            fn from(src: $wire) -> Self {
                Self {
                    $( $field: src.$field, )*
                    $($( $conv: src.$conv.into(), )*)?
                    $($( $conv_iter: src.$conv_iter.into_iter().map(Into::into).collect(), )*)?
                }
            }
        }
    };
}

pub(crate) use impl_mirror_from;

/// Generates bidirectional `From` impls between a canonical enum and a mirror
/// wire enum that has the same variant names and field names.
///
/// - `From<$canonical> for $wire` — moves each field/value
/// - `From<$wire> for $canonical` — moves each field/value
///
/// Both conversions take ownership. If you only have a reference and need to
/// convert, clone explicitly at the call site.
///
/// Two forms:
/// - **Struct/unit variants** (curly braces): `Type, Wire { Variant { field }, Unit, ... }`
/// - **Tuple variants** (parens): `Type, Wire ( Variant1, Variant2, ... )`
///
/// Uses a type alias in an anonymous const to work around the macro_rules
/// limitation that prevents mixing path repetitions with variant repetitions.
macro_rules! impl_mirror_enum_from {
    // Struct & unit variant form
    ($canonical:path, $wire:ident {
        $( $variant:ident $({ $($field:ident),* $(,)? })? ),* $(,)?
    }) => {
        const _: () = {
            type _Canonical = $canonical;
            impl From<_Canonical> for $wire {
                fn from(src: _Canonical) -> Self {
                    match src {
                        $( _Canonical::$variant $({ $($field),* })? =>
                            Self::$variant $({ $($field),* })?, )*
                    }
                }
            }
            impl From<$wire> for _Canonical {
                fn from(src: $wire) -> Self {
                    match src {
                        $( $wire::$variant $({ $($field),* })? =>
                            Self::$variant $({ $($field),* })?, )*
                    }
                }
            }
        };
    };

    // Tuple variant form — each variant has a single unnamed field of the same type
    ($canonical:path, $wire:ident ( $( $variant:ident ),* $(,)? )) => {
        const _: () = {
            type _Canonical = $canonical;
            impl From<_Canonical> for $wire {
                fn from(src: _Canonical) -> Self {
                    match src {
                        $( _Canonical::$variant(v) => Self::$variant(v), )*
                    }
                }
            }
            impl From<$wire> for _Canonical {
                fn from(src: $wire) -> Self {
                    match src {
                        $( $wire::$variant(v) => Self::$variant(v), )*
                    }
                }
            }
        };
    };
}

pub(crate) use impl_mirror_enum_from;

/// Generates bidirectional `From` impls for versioned transaction enums that
/// use the `*WithMetadata { tx, metadata }` wrapper pattern.
///
/// - `From<$canonical_enum> for $wire_enum` — moves fields, calls `.into()` on `convert` fields
/// - `From<$wire_enum> for $canonical_enum` — moves fields, calls `.into()` on `convert` fields,
///   sets `metadata: Default::default()`
///
/// Field lists are specified per-variant so they participate in the same
/// repetition level (a Rust macro_rules limitation).
macro_rules! impl_versioned_tx_from {
    (
        $canonical_enum:path => $wire_enum:ident {
            $( $variant:ident {
                gossip: $gossip_inner:ident,
                meta: $meta_type:path,
                tx: $tx_type:path,
                fields { $($field:ident),* $(,)? }
                $(convert { $($conv_field:ident),* $(,)? })?
            } ),+ $(,)?
        }
    ) => {
        const _: () = {
            type _Canonical = $canonical_enum;
            impl From<_Canonical> for $wire_enum {
                fn from(ct: _Canonical) -> Self {
                    match ct {
                        $(
                            _Canonical::$variant(wm) => Self::$variant($gossip_inner {
                                $( $field: wm.tx.$field, )*
                                $($( $conv_field: wm.tx.$conv_field.into(), )*)?
                            }),
                        )+
                    }
                }
            }
            impl From<$wire_enum> for _Canonical {
                fn from(ct: $wire_enum) -> Self {
                    match ct {
                        $(
                            $wire_enum::$variant(inner) => {
                                type _Meta = $meta_type;
                                type _Tx = $tx_type;
                                Self::$variant(_Meta {
                                    tx: _Tx {
                                        $( $field: inner.$field, )*
                                        $($( $conv_field: inner.$conv_field.into(), )*)?
                                    },
                                    metadata: Default::default(),
                                })
                            },
                        )+
                    }
                }
            }
        };
    };
}

pub(crate) use impl_versioned_tx_from;
