#![allow(clippy::manual_div_ceil, clippy::assign_op_pattern)]

use crate::Arbitrary;
use crate::ingress::IngressProof;
use alloy_primitives::{FixedBytes, bytes};
use alloy_rlp::{Decodable, Encodable, RlpDecodable, RlpEncodable};
use arbitrary::Unstructured;
use base58::{FromBase58, ToBase58 as _};
use bytes::Buf as _;
use derive_more::Deref;
use eyre::Error;
use eyre::eyre;
use rand::RngCore as _;
use reth_codecs::Compact;
use reth_db::table::{Compress, Decompress};
use reth_db_api::DatabaseError;
use reth_db_api::table::{Decode, Encode};
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, Error as _},
};
use std::fmt;
use std::{ops::Index, slice::SliceIndex, str::FromStr};
use uint::construct_uint;

//==============================================================================
// u64 Type
//------------------------------------------------------------------------------
pub mod u64_stringify {
    use serde::{self, Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &u64, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Convert u64 to string
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<u64, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s: String = Deserialize::deserialize(deserializer)?;

        // Parse string back to u128
        s.parse::<u64>()
            .map_err(|e| serde::de::Error::custom(format!("Failed to parse u64: {e}")))
    }
}

//==============================================================================
// U256 Type
//------------------------------------------------------------------------------
construct_uint! {
    /// 256-bit unsigned integer built from four little-endian `u64` limbs.
    pub struct U256(4);
}

impl U256 {
    /// Convert to 32 little-endian bytes, independent of host endianness.
    #[inline]
    pub fn to_le_bytes(&self) -> [u8; 32] {
        let mut out = [0_u8; 32];
        for (i, limb) in self.0.iter().enumerate() {
            out[i * 8..(i + 1) * 8].copy_from_slice(&limb.to_le_bytes());
        }
        out
    }

    /// Convert to 32 big-endian bytes, independent of host endianness.
    #[inline]
    pub fn to_be_bytes(&self) -> [u8; 32] {
        let mut out = [0_u8; 32];
        for (i, limb) in self.0.iter().rev().enumerate() {
            out[i * 8..(i + 1) * 8].copy_from_slice(&limb.to_be_bytes());
        }
        out
    }

    /// Recreate `U256` from 32 little-endian bytes.
    #[inline]
    pub fn from_le_bytes(bytes: [u8; 32]) -> Self {
        let mut limbs = [0_u64; 4];
        for i in 0..4 {
            let chunk: [u8; 8] = bytes[i * 8..(i + 1) * 8].try_into().unwrap();
            limbs[i] = u64::from_le_bytes(chunk);
        }
        Self(limbs)
    }

    /// Recreate `U256` from 32 big-endian bytes.
    #[inline]
    pub fn from_be_bytes(bytes: [u8; 32]) -> Self {
        let mut limbs = [0_u64; 4];
        for i in 0..4 {
            let chunk: [u8; 8] = bytes[i * 8..(i + 1) * 8].try_into().unwrap();
            limbs[3 - i] = u64::from_be_bytes(chunk);
        }
        Self(limbs)
    }
}

impl From<alloy_primitives::U256> for U256 {
    #[inline]
    fn from(value: alloy_primitives::U256) -> Self {
        Self::from_le_bytes(value.to_le_bytes())
    }
}

impl From<U256> for alloy_primitives::U256 {
    #[inline]
    fn from(value: U256) -> Self {
        Self::from_le_bytes(value.to_le_bytes())
    }
}

#[cfg(test)]
mod u256_le_be_to_from_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn le_roundtrip(raw in prop::array::uniform32(any::<u8>())) {
            let x = U256::from_le_bytes(raw);
            prop_assert_eq!(U256::from_le_bytes(x.to_le_bytes()), x);
        }

        #[test]
        fn be_roundtrip(raw in prop::array::uniform32(any::<u8>())) {
            let x = U256::from_le_bytes(raw);
            prop_assert_eq!(U256::from_be_bytes(x.to_be_bytes()), x);
        }

        #[test]
        fn alloy_conversion_roundtrip(raw in prop::array::uniform32(any::<u8>())) {
            let custom_original = U256::from_le_bytes(raw);

            let alloy_val: alloy_primitives::U256 = custom_original.into();
            prop_assert_eq!(alloy_val.to_le_bytes(), raw);

            let custom_roundtrip: U256 = alloy_val.into();
            prop_assert_eq!(custom_original, custom_roundtrip);
        }
    }
}

// Manually implement Arbitrary for U256
impl<'a> Arbitrary<'a> for U256 {
    fn arbitrary(_u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        let mut rng = rand::thread_rng();
        let mut bytes = [0_u8; 32]; // 32 bytes for 256 bits
        rng.fill_bytes(&mut bytes);

        Ok(Self::from_big_endian(&bytes))
    }
}

impl Encode for U256 {
    type Encoded = [u8; 32];

    fn encode(self) -> Self::Encoded {
        bytemuck::cast(self.0)
    }
}

impl Decode for U256 {
    fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
        let res = bytemuck::try_from_bytes::<[u64; 4]>(value).map_err(|_| DatabaseError::Decode)?;
        let res = Self(*res);
        Ok(res)
    }
}

// NOTE: RLP has specific standards for encoding numbers
// so we defer to the correct impl used by alloy's U256
impl Encodable for U256 {
    #[inline]
    fn encode(&self, out: &mut dyn bytes::BufMut) {
        let int: alloy_primitives::U256 = (*self).into();
        int.encode(out);
    }
}

impl Decodable for U256 {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        alloy_primitives::U256::decode(buf).map(Into::into)
    }
}

// Manually implement Compact for U256
impl Compact for U256 {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        // Create a temporary byte array for the big-endian representation of `self`
        let mut bytes = [0_u8; 32];
        self.to_big_endian(&mut bytes);
        bytes.to_compact(buf)
    }

    #[inline]
    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        // Disambiguate and call the correct H256::from method
        let (v, remaining_buf) = <[u8; 32]>::from_compact(buf, len);
        // Fully qualify this call to avoid calling DecodeHash::from
        (<Self as From<[u8; 32]>>::from(v), remaining_buf)
    }
}

//==============================================================================
// H256 Type
//------------------------------------------------------------------------------

pub use crate::h256::H256;

impl H256 {
    /// Decodes a H256 from a string. This will panic if the input is malformed!
    pub fn from_base58(string: &str) -> Self {
        Self::from_base58_result(string).expect("to parse base58 string into H256")
    }

    pub fn from_base58_result(string: &str) -> eyre::Result<Self> {
        let decoded = string
            .from_base58()
            .map_err(|e| eyre!("Invalid base58 string: {:?}", &e))?;
        let array: [u8; 32] = decoded
            .as_slice()
            .try_into()
            .map_err(|_| eyre!("Decoded base58 has {} bytes, expected 32", decoded.len()))?;
        Ok(Self(array))
    }
}

// Manually implement Arbitrary for H256
impl<'a> Arbitrary<'a> for H256 {
    fn arbitrary(_u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        Ok(Self::random())
    }
}

impl Encode for H256 {
    type Encoded = [u8; 32];

    fn encode(self) -> Self::Encoded {
        self.0
    }
}

impl Decode for H256 {
    fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
        let arr: [u8; 32] = value.try_into().map_err(|_| DatabaseError::Decode)?;
        Ok(Self(arr))
    }
}

impl From<H256> for FixedBytes<32> {
    fn from(val: H256) -> Self {
        Self(val.0)
    }
}

impl Encodable for H256 {
    #[inline]
    fn length(&self) -> usize {
        self.0.length()
    }

    #[inline]
    fn encode(&self, out: &mut dyn bytes::BufMut) {
        self.0.encode(out);
    }
}

impl Decodable for H256 {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        Decodable::decode(buf).map(Self)
    }
}

#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Default,
    Compact,
    Serialize,
    Deserialize,
    Arbitrary,
    RlpDecodable,
    RlpEncodable,
    Deref,
)]
pub struct IngressProofsList(pub Vec<IngressProof>);

impl From<Vec<IngressProof>> for IngressProofsList {
    fn from(proofs: Vec<IngressProof>) -> Self {
        Self(proofs)
    }
}

//==============================================================================
// Address Base58
//------------------------------------------------------------------------------
pub mod address_base58_stringify {
    use alloy_primitives::Address;
    use base58::{FromBase58, ToBase58 as _};
    use serde::{self, Deserialize, Deserializer, Serializer, de};

    pub fn serialize<S>(value: &Address, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(value.0.to_base58().as_ref())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Address, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s: String = Deserialize::deserialize(deserializer)?;

        // Decode the base58 string into bytes
        let bytes = FromBase58::from_base58(s.as_str())
            .map_err(|e| de::Error::custom(format!("Failed to decode from base58 {e:?}")))?;

        // Ensure the byte array is exactly 20 bytes
        if bytes.len() != 20 {
            return Err(de::Error::invalid_length(
                bytes.len(),
                &"expected 20 bytes for address",
            ));
        }

        Ok(Address::from_slice(&bytes))
    }
}
pub mod option_address_base58_stringify {
    use super::address_base58_stringify;
    use alloy_primitives::Address;
    use serde::{Deserialize as _, Deserializer, Serializer};

    pub fn serialize<S>(value: &Option<Address>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(address) => address_base58_stringify::serialize(address, serializer),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Address>, D::Error>
    where
        D: Deserializer<'de>,
    {
        match Option::deserialize(deserializer)? {
            None => Ok(None),
            Some(s) => {
                // re-deserialize the string to an Address
                let s_deserializer =
                    serde::de::value::StringDeserializer::<serde::de::value::Error>::new(s);
                match address_base58_stringify::deserialize(s_deserializer) {
                    Ok(address) => Ok(Some(address)),
                    Err(e) => Err(serde::de::Error::custom(format!(
                        "Failed to deserialize address: {}",
                        e
                    ))),
                }
            }
        }
    }
}

//==============================================================================
// Option<u64>
//------------------------------------------------------------------------------
/// where u64 is represented as a string in the json
pub mod option_u64_stringify {
    use serde::{self, Deserialize as _, Deserializer, Serializer};
    use serde_json::Value;

    pub fn serialize<S>(value: &Option<u64>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(number) => serializer.serialize_str(&number.to_string()),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let opt_val: Option<Value> = Option::deserialize(deserializer)?;

        match opt_val {
            Some(Value::String(s)) => s.parse::<u64>().map(Some).map_err(serde::de::Error::custom),
            Some(_) => Err(serde::de::Error::custom("Invalid type")),
            None => Ok(None),
        }
    }
}

//==============================================================================
// U256
//------------------------------------------------------------------------------

/// Implement Serialize for U256
impl Serialize for U256 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.to_string().as_str())
    }
}

/// Implement Deserialize for U256
impl<'de> Deserialize<'de> for U256 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s: String = Deserialize::deserialize(deserializer)?;
        Self::from_dec_str(&s).map_err(serde::de::Error::custom)
    }
}

//==============================================================================
// H256
//------------------------------------------------------------------------------
impl H256 {
    pub fn to_vec(self) -> Vec<u8> {
        self.0.to_vec()
    }

    /// Gets u32 from first 4 bytes
    pub fn to_u32(&self) -> u32 {
        let bytes = self.as_bytes();
        u32::from_be_bytes(bytes[0..4].try_into().unwrap())
    }
}

// Implement Serialize for H256
impl Serialize for H256 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_bytes().to_base58().as_ref())
    }
}

// Implement Deserialize for H256
impl<'de> Deserialize<'de> for H256 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s: String = Deserialize::deserialize(deserializer)?;
        DecodeHash::from(&s).map_err(D::Error::custom)
    }
}

/// Decode from encoded base58 string into H256 bytes.
pub trait DecodeHash: Sized {
    fn from(base58_string: &str) -> Result<Self, String>;
    fn empty() -> Self;
}

impl DecodeHash for H256 {
    fn from(base58_string: &str) -> Result<Self, String> {
        let bytes = FromBase58::from_base58(base58_string)
            .map_err(|e| format!("Failed to decode from base58 {e:?}"))?;
        let arr: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
            format!(
                "Invalid H256 length: expected 32 bytes, got {}",
                bytes.len()
            )
        })?;
        Ok(Self(arr))
    }

    fn empty() -> Self {
        Self::zero()
    }
}

impl Compact for H256 {
    #[inline]
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        self.0.to_compact(buf)
    }

    #[inline]
    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        // Disambiguate and call the correct H256::from method
        let (v, remaining_buf) = <[u8; 32]>::from_compact(buf, len);
        // Fully qualify this call to avoid calling DecodeHash::from
        (<Self as From<[u8; 32]>>::from(v), remaining_buf)
    }
}

impl Compress for H256 {
    type Compressed = Vec<u8>;

    fn compress_to_buf<B: bytes::BufMut + AsMut<[u8]>>(&self, buf: &mut B) {
        let _ = Compact::to_compact(&self, buf);
    }
}

impl Decompress for H256 {
    fn decompress(value: &[u8]) -> Result<Self, DatabaseError> {
        let (obj, _) = Compact::from_compact(value, value.len());
        Ok(obj)
    }
}

//==============================================================================
// Base64 Type
//------------------------------------------------------------------------------
/// A struct of [`Vec<u8>`] used for all `base64_url` encoded fields. This is
/// used for large fields like proof chunk data.

#[derive(Default, Debug, Clone, Eq, PartialEq, Arbitrary, RlpDecodable, RlpEncodable)]
pub struct Base64(pub Vec<u8>);

impl std::fmt::Display for Base64 {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        // format larger (>8 bytes) Base64 strings as <abcd>...<wxyz>
        let trunc_len = 4;
        if self.0.len() <= 2 * trunc_len {
            write!(f, "{}", base64_url::encode(&self.0))
        } else {
            write!(
                f,
                "{}...{}",
                base64_url::encode(&self.0[..trunc_len]),
                base64_url::encode(&self.0[self.0.len() - trunc_len..])
            )
        }
    }
}

impl Compact for Base64 {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        let len = self.0.len();
        buf.put_slice(&self.0);
        len
    }

    fn from_compact(mut buf: &[u8], len: usize) -> (Self, &[u8]) {
        let vec = Vec::from(&buf[..len]);
        buf.advance(len);
        (Self(vec), buf)
    }
}

/// Converts a base64url encoded string to a Base64 struct.
impl FromStr for Base64 {
    type Err = base64_url::base64::DecodeError;
    fn from_str(str: &str) -> Result<Self, base64_url::base64::DecodeError> {
        let result = base64_url::decode(str)?;
        Ok(Self(result))
    }
}

impl From<Vec<u8>> for Base64 {
    fn from(value: Vec<u8>) -> Self {
        Self(value)
    }
}

impl From<Base64> for Vec<u8> {
    fn from(value: Base64) -> Self {
        value.0
    }
}

impl AsRef<[u8]> for Base64 {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl Base64 {
    pub fn from_utf8_str(str: &str) -> Result<Self, Error> {
        Ok(Self(str.as_bytes().to_vec()))
    }
    pub fn to_utf8_string(&self) -> Result<String, Error> {
        Ok(String::from_utf8(self.0.clone())?)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn as_slice(&self) -> &[u8] {
        self.0.as_slice()
    }

    pub fn split_at(&self, mid: usize) -> (&[u8], &[u8]) {
        self.0.split_at(mid)
    }

    pub fn to_vec(self) -> Vec<u8> {
        self.0
    }
}

impl Serialize for Base64 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&base64_url::encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for Base64 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Vis;
        impl serde::de::Visitor<'_> for Vis {
            type Value = Base64;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a base64 string")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                base64_url::decode(v)
                    .map(Base64)
                    .map_err(|_| de::Error::custom("failed to decode base64 string"))
            }
        }
        deserializer.deserialize_str(Vis)
    }
}

//==============================================================================
// H256List Type
//------------------------------------------------------------------------------
/// A struct of [`Vec<H256>`] used for lists of [`Base64`] encoded hashes
#[derive(Default, Clone, Eq, PartialEq, Compact, Arbitrary, RlpEncodable, RlpDecodable, Deref)]
pub struct H256List(pub Vec<H256>);

impl H256List {
    // Constructor for an empty H256List
    pub fn new() -> Self {
        Self(Vec::new())
    }

    // Constructor for an initialized H256List
    pub fn with_capacity(capacity: usize) -> Self {
        Self(Vec::with_capacity(capacity))
    }

    pub fn push(&mut self, value: H256) {
        self.0.push(value)
    }

    pub fn reverse(&mut self) {
        self.0.reverse()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, H256> {
        self.0.iter()
    }

    pub fn get(&self, index: usize) -> Option<&<usize as SliceIndex<[H256]>>::Output> {
        self.0.get(index)
    }
}

impl fmt::Debug for H256List {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("H256List(")?;
        f.write_str("[\n\t")?;

        let mut first = true;
        for item in &self.0 {
            if !first {
                f.write_str(",\n\t")?;
            }
            first = false;

            // Write the base58-encoded hash
            write!(f, "{}", item)?;
        }

        f.write_str("\n])")?;
        Ok(())
    }
}

impl Index<usize> for H256List {
    type Output = H256;

    fn index(&self, index: usize) -> &Self::Output {
        &self.0[index]
    }
}

impl PartialEq<Vec<H256>> for H256List {
    fn eq(&self, other: &Vec<H256>) -> bool {
        &self.0 == other
    }
}

impl PartialEq<H256List> for Vec<H256> {
    fn eq(&self, other: &H256List) -> bool {
        self == &other.0
    }
}

// Implement Serialize for H256 base64url encoded Array
impl Serialize for H256List {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Serialize self.0 (Vec<Base64>) directly
        self.0.serialize(serializer)
    }
}

// Implement Deserialize for H256 base64url encoded Array
impl<'de> Deserialize<'de> for H256List {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Deserialize a Vec<Base64> and then wrap it in Base64Array
        Vec::<H256>::deserialize(deserializer).map(H256List)
    }
}

//==============================================================================
// Uint <-> string HTTP/JSON serialization/deserialization
//------------------------------------------------------------------------------

/// Module containing serialization/deserialization for usize to/from a string
pub mod string_usize {
    use super::*;

    pub fn deserialize<'de, D>(deserializer: D) -> Result<usize, D::Error>
    where
        D: Deserializer<'de>,
    {
        string_or_number_to_int(deserializer)
    }

    pub fn serialize<S>(value: &usize, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&value.to_string())
    }
}

/// Module containing serialization/deserialization for u64 to/from a string
pub mod string_u64 {
    use super::*;

    pub fn deserialize<'de, D>(deserializer: D) -> Result<u64, D::Error>
    where
        D: Deserializer<'de>,
    {
        string_or_number_to_int(deserializer)
    }

    pub fn serialize<S>(value: &u64, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&value.to_string())
    }
}

/// Module containing serialization/deserialization for `Option<u64>` to/from a string
pub mod optional_string_u64 {
    use super::*;

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
    where
        D: Deserializer<'de>,
    {
        string_or_number_to_optional_int(deserializer)
    }

    pub fn serialize<S>(value: &Option<u64>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(v) => serializer.serialize_str(&v.to_string()),
            None => serializer.serialize_none(),
        }
    }
}

pub mod string_u128 {
    use super::*;

    pub fn deserialize<'de, D>(deserializer: D) -> Result<u128, D::Error>
    where
        D: Deserializer<'de>,
    {
        string_or_number_to_int(deserializer)
    }

    pub fn serialize<S>(value: &u128, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&value.to_string())
    }
}

// note: U256 doesn't have a string_ serialisation mod, as it doesn't need one

fn string_or_number_to_int<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr + serde::Deserialize<'de>,
    T::Err: std::fmt::Display,
{
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum StringOrNumber<T> {
        String(String),
        Number(T),
    }

    match StringOrNumber::deserialize(deserializer)? {
        StringOrNumber::String(s) => T::from_str(&s).map_err(serde::de::Error::custom),
        StringOrNumber::Number(n) => Ok(n),
    }
}

fn string_or_number_to_optional_int<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr + serde::Deserialize<'de>,
    T::Err: std::fmt::Display,
{
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum StringOrNumber<T> {
        String(String),
        Number(T),
        Null,
    }

    match StringOrNumber::deserialize(deserializer)? {
        StringOrNumber::String(s) => {
            if s.is_empty() {
                Ok(None)
            } else {
                T::from_str(&s).map(Some).map_err(serde::de::Error::custom)
            }
        }
        StringOrNumber::Number(n) => Ok(Some(n)),
        StringOrNumber::Null => Ok(None),
    }
}

pub mod unix_timestamp_string_serde {
    use chrono::{DateTime, Utc};
    use serde::{self, Deserialize as _, Deserializer, Serializer};

    use crate::UnixTimestamp;

    pub fn deserialize<'de, D>(deserializer: D) -> Result<UnixTimestamp, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;

        Ok(UnixTimestamp::from_secs(
            DateTime::parse_from_rfc3339(&s)
                .map_err(|e| serde::de::Error::custom(format!("invalid timestamp: {}", &e)))?
                .timestamp() as u64,
        ))
    }

    pub fn serialize<S>(timestamp: &UnixTimestamp, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let dt = DateTime::<Utc>::from_timestamp(timestamp.as_secs() as i64, 0)
            .ok_or_else(|| serde::ser::Error::custom("invalid timestamp"))?;

        serializer.serialize_str(&dt.to_rfc3339())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;
    use proptest::prelude::*;
    use serde_json::json;

    fn arb_u256() -> impl Strategy<Value = U256> {
        prop::array::uniform32(any::<u8>()).prop_map(U256::from_le_bytes)
    }

    proptest! {
        #[test]
        fn u256_rlp_roundtrip(val in arb_u256()) {
            let mut buffer = vec![];
            Encodable::encode(&val, &mut buffer);
            let decoded: U256 = Decodable::decode(&mut buffer.as_slice()).unwrap();
            prop_assert_eq!(val, decoded);
        }

        #[test]
        fn u256_compact_roundtrip(val in arb_u256()) {
            let mut buf = BytesMut::with_capacity(32);
            let bytes_written = val.to_compact(&mut buf);
            prop_assert_eq!(bytes_written, 32);

            let expected_bytes = {
                let mut temp = [0_u8; 32];
                val.to_big_endian(&mut temp);
                temp
            };
            prop_assert_eq!(&buf[..], &expected_bytes[..]);

            let (decoded, remaining) = U256::from_compact(&buf[..], buf.len());
            prop_assert!(remaining.is_empty(), "U256 from_compact left trailing bytes");
            prop_assert_eq!(val, decoded);
        }

        #[test]
        fn h256_rlp_roundtrip(raw in prop::array::uniform32(any::<u8>())) {
            let data = H256(raw);
            let mut buffer = vec![];
            Encodable::encode(&data, &mut buffer);
            let decoded: H256 = Decodable::decode(&mut buffer.as_slice()).unwrap();
            prop_assert_eq!(data, decoded);
        }

        #[test]
        fn h256_compact_roundtrip(raw in prop::array::uniform32(any::<u8>())) {
            let original = H256(raw);
            let mut buf = BytesMut::with_capacity(32);
            original.to_compact(&mut buf);
            let (decoded, remaining) = H256::from_compact(&buf[..], buf.len());
            prop_assert_eq!(original, decoded);
            prop_assert!(remaining.is_empty(), "H256 from_compact left trailing bytes");
        }

        #[test]
        fn base64_compact_roundtrip(data in prop::collection::vec(any::<u8>(), 0..256)) {
            let original = Base64::from(data);
            let mut buf = BytesMut::with_capacity(original.0.len());
            original.to_compact(&mut buf);
            let (decoded, remaining) = Base64::from_compact(&buf[..], buf.len());
            prop_assert_eq!(original, decoded);
            prop_assert!(remaining.is_empty(), "Base64 from_compact left trailing bytes");
        }
    }

    #[test]
    fn test_option_u256_rlp() {
        #[derive(Debug, Eq, PartialEq, RlpEncodable, RlpDecodable)]
        #[rlp(trailing)]
        struct Test {
            a: U256,
            b: Option<U256>,
        }

        let data1 = Test {
            a: U256::from(42_u64),
            b: Some(U256::zero()),
        };

        let data2 = Test {
            a: U256::from(42_u64),
            b: None,
        };

        let mut buffer1 = vec![];
        data1.encode(&mut buffer1);
        let decoded = Test::decode(&mut &buffer1[..]).unwrap();
        // Some(0) is decoded as None -- RLP encodes zero as empty, indistinguishable from None
        assert_ne!(data1, decoded);

        let mut buffer2 = vec![];
        data2.encode(&mut buffer2);
        // Unequal encodings: Some value gets an additional trailing byte
        assert_ne!(buffer1, buffer2);
        let decoded2 = Test::decode(&mut &buffer2[..]).unwrap();
        assert_eq!(decoded2, data2);
    }

    #[test]
    fn test_string_or_number_to_u64() {
        let json_number: serde_json::Value = json!(42);
        let json_string: serde_json::Value = json!("42");

        let number: Result<Result<u64, _>, _> = serde_json::from_value(json_number)
            .map(|v: serde_json::Value| string_or_number_to_int(v));
        let string: Result<Result<u64, _>, _> = serde_json::from_value(json_string)
            .map(|v: serde_json::Value| string_or_number_to_int(v));

        assert_eq!(number.unwrap().unwrap(), 42);
        assert_eq!(string.unwrap().unwrap(), 42);
    }

    #[test]
    fn test_string_or_number_to_optional_u64() {
        let json_number: serde_json::Value = json!(42);
        let json_string: serde_json::Value = json!("42");
        let json_null: serde_json::Value = json!(null);
        let json_empty: serde_json::Value = json!("");

        let number: Result<Result<Option<u64>, _>, _> = serde_json::from_value(json_number)
            .map(|v: serde_json::Value| string_or_number_to_optional_int(v));
        let string: Result<Result<Option<u64>, _>, _> = serde_json::from_value(json_string)
            .map(|v: serde_json::Value| string_or_number_to_optional_int(v));
        let null: Result<Result<Option<u64>, _>, _> = serde_json::from_value(json_null)
            .map(|v: serde_json::Value| string_or_number_to_optional_int(v));
        let empty: Result<Result<Option<u64>, _>, _> = serde_json::from_value(json_empty)
            .map(|v: serde_json::Value| string_or_number_to_optional_int(v));

        assert_eq!(number.unwrap().unwrap(), Some(42));
        assert_eq!(string.unwrap().unwrap(), Some(42));
        assert_eq!(null.unwrap().unwrap(), None);
        assert_eq!(empty.unwrap().unwrap(), None);
    }

    #[test]
    fn test_u256_serde_json_string_serialization() {
        #[derive(Serialize, Deserialize, Debug, PartialEq)]
        struct TestStruct {
            value: U256,
        }

        let one_e18 = U256::from(1_000_000_000_000_000_000_u64);
        let test_struct = TestStruct { value: one_e18 };

        let serialized = serde_json::to_string(&test_struct).unwrap();
        assert_eq!(serialized, r#"{"value":"1000000000000000000"}"#);

        let deserialized: TestStruct = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, test_struct);
    }

    mod unix_timestamp_string_serde_tests {
        use super::super::unix_timestamp_string_serde;
        use crate::UnixTimestamp;

        #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
        struct TestStruct {
            #[serde(with = "unix_timestamp_string_serde")]
            timestamp: UnixTimestamp,
        }

        #[test]
        fn test_round_trip_serialize_deserialize() {
            let original_ts = UnixTimestamp::from_secs(1609459200);
            let test_struct = TestStruct {
                timestamp: original_ts,
            };

            let serialized = serde_json::to_string(&test_struct).unwrap();

            assert!(serialized.contains("2021-01-01T00:00:00+00:00"));

            let deserialized: TestStruct = serde_json::from_str(&serialized).unwrap();

            assert_eq!(deserialized.timestamp, original_ts);
        }

        #[test]
        fn test_deserialize_known_rfc3339_string() {
            let json = r#"{"timestamp":"2026-01-15T11:30:00+00:00"}"#;

            let deserialized: TestStruct = serde_json::from_str(json).unwrap();

            assert_eq!(deserialized.timestamp.as_secs(), 1768476600);
        }

        #[test]
        fn test_deserialize_malformed_string_returns_error() {
            let json = r#"{"timestamp":"not-a-timestamp"}"#;

            let result: Result<TestStruct, _> = serde_json::from_str(json);

            assert!(result.is_err());
        }
    }

    #[test]
    #[should_panic(expected = "to parse base58 string")]
    fn h256_from_base58_panics_on_invalid_input() {
        let _ = H256::from_base58("!!!invalid-base58!!!");
    }

    #[test]
    fn h256_from_base58_result_returns_err_on_invalid() {
        assert!(H256::from_base58_result("!!!invalid!!!").is_err());
    }

    #[test]
    fn h256_from_base58_result_returns_err_on_wrong_length() {
        use base58::ToBase58 as _;

        let encoded = [0_u8; 31].to_base58();
        let err = H256::from_base58_result(&encoded).unwrap_err();
        assert!(
            err.to_string().contains("31 bytes, expected 32"),
            "expected length error, got: {err}"
        );
    }
}
