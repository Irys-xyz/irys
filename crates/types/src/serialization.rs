use crate::{Arbitrary, IrysSignature};
use alloy_primitives::{bytes, Address};
use alloy_rlp::{Decodable, Encodable, RlpDecodable, RlpEncodable};
use arbitrary::Unstructured;
use base58::{FromBase58, ToBase58};
use bytes::Buf;
use eyre::{Error, OptionExt};
use openssl::sha;
use rand::RngCore;
use reth_codecs::Compact;
use reth_db::table::{Compress, Decompress};
use reth_db_api::table::{Decode, Encode};
use reth_db_api::DatabaseError;
use reth_primitives::transaction::recover_signer;
use serde::{
    de::{self, Error as _},
    Deserialize, Deserializer, Serialize, Serializer,
};
use std::{ops::Index, slice::SliceIndex, str::FromStr};

use fixed_hash::construct_fixed_hash;
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
            .map_err(|e| serde::de::Error::custom(format!("Failed to parse u64: {}", e)))
    }
}

//==============================================================================
// U256 Type
//------------------------------------------------------------------------------
construct_uint! {
    /// 256-bit unsigned integer.
    pub struct U256(4);
}

// Manually implement Arbitrary for U256
impl<'a> Arbitrary<'a> for U256 {
    fn arbitrary(_u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        let mut rng = rand::thread_rng();
        let mut bytes = [0u8; 32]; // 32 bytes for 256 bits
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

impl Encodable for U256 {
    #[inline]
    fn encode(&self, out: &mut dyn bytes::BufMut) {
        let mut buffer = [0_u8; 32];
        self.to_big_endian(&mut buffer);
        buffer.encode(out);
    }
}

impl Decodable for U256 {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let res = <[u8; 32]>::decode(buf)?;
        Ok(Self::from_big_endian(&res))
    }
}

// Manually implement Compact for U256
impl Compact for U256 {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        // Create a temporary byte array for the big-endian representation of `self`
        let mut bytes = [0u8; 32];
        self.to_big_endian(&mut bytes);

        // Write the bytes to the buffer
        buf.put_slice(&bytes);

        // Return the number of bytes written (32 bytes for a U256)
        bytes.len()
    }

    #[inline]
    fn from_compact(mut buf: &[u8], len: usize) -> (Self, &[u8]) {
        let res = Self::from_big_endian(&buf[..len]);
        buf.advance(len);
        (res, buf)
    }
}

//==============================================================================
// H256 Type
//------------------------------------------------------------------------------
construct_fixed_hash! {
    /// A 256-bit hash type (32 bytes).
    pub struct H256(32);
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
        Ok(Self::from_slice(
             value,
        ))
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

//==============================================================================
// TxIngressProof
//------------------------------------------------------------------------------
#[derive(
    Clone,
    Default,
    Compact,
    Eq,
    PartialEq,
    Debug,
    Arbitrary,
    RlpEncodable,
    RlpDecodable,
    Serialize,
    Deserialize,
)]
pub struct TxIngressProof {
    pub proof: H256,
    pub signature: IrysSignature,
}

impl TxIngressProof {
    pub fn pre_validate(&self, data_root: &H256) -> eyre::Result<Address> {
        let mut hasher = sha::Sha256::new();
        hasher.update(&self.proof.0);
        hasher.update(&data_root.0);
        let prehash = hasher.finish();

        let sig = self.signature.as_bytes();
        let recovered_address = recover_signer(&sig[..].try_into()?, prehash.into())
            .ok_or_eyre("Unable to recover signer")?;

        Ok(recovered_address)
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
)]
pub struct IngressProofsList(pub Vec<TxIngressProof>);

impl From<Vec<TxIngressProof>> for IngressProofsList {
    fn from(proofs: Vec<TxIngressProof>) -> Self {
        Self(proofs)
    }
}

//==============================================================================
// Address Base58
//------------------------------------------------------------------------------
pub mod address_base58_stringify {
    use alloy_primitives::Address;
    use base58::{FromBase58, ToBase58};
    use serde::{self, de, Deserialize, Deserializer, Serializer};

    #[allow(dead_code)]
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
            .map_err(|e| format!("Failed to decode from base58 {:?}", e))
            .expect("base58 should prase");

        // Ensure the byte array is exactly 20 bytes
        if bytes.len() != 20 {
            return Err(de::Error::invalid_length(
                bytes.len(),
                &"expected 65 bytes for signature",
            ));
        }

        Ok(Address::from_slice(&bytes))
    }
}

//==============================================================================
// Option<u64>
//------------------------------------------------------------------------------
/// where u64 is represented as a string in the json
pub mod option_u64_stringify {
    use serde::{self, Deserialize, Deserializer, Serializer};
    use serde_json::Value;

    #[allow(dead_code)]
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
        FromBase58::from_base58(base58_string)
            .map_err(|e| format!("Failed to decode from base58 {:?}", e))
            .map(|bytes| Self::from_slice(bytes.as_slice()))
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

    fn compress_to_buf<B: bytes::BufMut + AsMut<[u8]>>(self, buf: &mut B) {
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

#[derive(Default, Debug, Clone, Eq, PartialEq, Compact, Arbitrary, RlpDecodable, RlpEncodable)]
pub struct Base64(pub Vec<u8>);

impl std::fmt::Display for Base64 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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
#[derive(Debug, Default, Clone, Eq, PartialEq, Compact, Arbitrary, RlpEncodable, RlpDecodable)]
pub struct H256List(pub Vec<H256>);

impl H256List {
    // Constructor for an empty H256List
    pub const fn new() -> Self {
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

/// Module containing serialization/deserialization for Option<u64> to/from a string
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

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;
    use serde_json::json;

    #[test]
    fn test_u256_rlp_round_trip() {
        // setup
        let data = U256::one();

        // action
        let mut buffer = vec![];
        Encodable::encode(&data, &mut buffer);
        let decoded = Decodable::decode(&mut buffer.as_slice()).unwrap();

        // Assert
        assert_eq!(data, decoded);
    }

    #[test]
    fn test_u256_to_compact() {
        // Create a U256 value to test
        let original_value = U256::from(123456789u64);

        // Create a buffer to write the compact representation into
        let mut buf = BytesMut::with_capacity(32);

        // Call the to_compact method
        let bytes_written = original_value.to_compact(&mut buf);

        // Ensure that the number of bytes written is 32 (for U256)
        assert_eq!(bytes_written, 32);

        // Check that the buffer now contains the correct big-endian representation
        let expected_bytes = {
            let mut temp = [0u8; 32];
            original_value.to_big_endian(&mut temp);
            temp
        };
        assert_eq!(&buf[..], &expected_bytes[..]);
    }

    #[test]
    fn test_u256_from_compact() {
        // Create a U256 value and convert it to compact bytes
        let original_value = U256::from(123456789u64);
        let mut buf = BytesMut::with_capacity(32);
        original_value.to_compact(&mut buf);

        // Call from_compact to convert the bytes back to U256
        let (decoded_value, _) = U256::from_compact(&buf[..], buf.len());

        // Check that the decoded value matches the original value
        assert_eq!(decoded_value, original_value);
    }

    #[test]
    fn test_h256_rlp_round_trip() {
        // setup
        let data = H256::random();

        // action
        let mut buffer = vec![];
        Encodable::encode(&data, &mut buffer);
        let decoded = Decodable::decode(&mut buffer.as_slice()).unwrap();

        // Assert
        assert_eq!(data, decoded);
    }

    #[test]
    fn test_h256_compact_round_trip() {
        // Create a H256 value and convert it to compact bytes
        let original_value = H256::random();
        let mut buf = BytesMut::with_capacity(32);
        original_value.to_compact(&mut buf);

        // Call from_compact to convert the bytes back to H256
        let (decoded_value, _) = H256::from_compact(&buf[..], buf.len());

        // Check that the decoded value matches the original value
        assert_eq!(decoded_value, original_value);
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
}
