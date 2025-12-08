// disable lints - this is a macro expansion that I want to keep as close to the original as possible
// and for some reason, clippy::all... doesn't actually cover all lints??
#![allow(
    clippy::all,
    clippy::pedantic,
    clippy::nursery,
    clippy::unused_trait_names,
    clippy::needless_raw_strings,
    clippy::unseparated_literal_suffix,
    clippy::allow_attributes
)]

use core::fmt;
use std::{
    borrow::{Borrow, BorrowMut},
    cmp::Ordering,
    str::FromStr,
    u8,
};

use alloy_core::hex::FromHex;
use alloy_primitives::{keccak256, Address as AlloyAddress};
use base58::{FromBase58, ToBase58};
use reth_codecs::Compact;
use reth_db::{
    table::{Decode, Encode},
    DatabaseError,
};

// TODO: we can probably just do an std::mem::transmute as the underlying memory layout & contents is identical
impl From<AlloyAddress> for IrysAddress {
    #[inline]
    fn from(value: AlloyAddress) -> Self {
        Self(value.0)
    }
}

impl Into<AlloyAddress> for IrysAddress {
    #[inline]
    fn into(self) -> AlloyAddress {
        AlloyAddress(self.0)
    }
}

impl Into<AlloyAddress> for &IrysAddress {
    #[inline]
    fn into(self) -> AlloyAddress {
        AlloyAddress(self.0)
    }
}

impl Compact for IrysAddress {
    #[inline]
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        self.0.to_compact(buf)
    }
    #[inline]
    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        let (v, buf) = <[u8; core::mem::size_of::<IrysAddress>()]>::from_compact(buf, len);
        (Self::from(v), buf)
    }
}

impl Encode for IrysAddress {
    type Encoded = [u8; 20];

    fn encode(self) -> Self::Encoded {
        self.0 .0
    }
}

impl Decode for IrysAddress {
    fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
        Ok(Self::from_slice(value))
    }
}

impl FromStr for IrysAddress {
    type Err = String;

    fn from_str(str: &str) -> Result<Self, Self::Err> {
        Self::from_hex_or_base58(str)
    }
}

/// As base58 is rather expensive, we want to make sure that input is well formed as soon as possible
/// this constant is computed using the below `compute_max_str_len` function (to_base58() is not a const fn)
const MAX_BS58_ADDRESS_STRING_LENGTH: usize = 28;

// #[test]
// fn compute_max_str_len() {
//     dbg!([u8::MAX; 20].to_base58().len());
// }

const MAX_HEX_ADDRESS_STRING_LENGTH: usize = 2 /* 0x */ + 20 * 2 /* 20 bytes, hex encoding */;

// sanity check for the above
// #[test]
// fn compute_max_str_len() {
//     dbg!(alloy_primitives::hex::encode_prefixed(&[u8::MAX; 20]).len());
// }

impl IrysAddress {
    // TODO: better error types
    pub fn from_base58(str: &str) -> Result<Self, String> {
        // reject strings if they are larger than `MAX_BS58_ADDRESS_STRING_LENGTH`
        if str.len() > MAX_BS58_ADDRESS_STRING_LENGTH {
            return Err("Input above max allowed base58 length".to_owned());
        }
        let decoded = str.from_base58().map_err(|e| format!("{:?}", &e))?;

        let arr = TryInto::<[u8; 20]>::try_into(decoded.as_slice()).map_err(|e| e.to_string())?;

        Ok(IrysAddress::from(&arr))
    }

    pub fn from_hex_or_base58(str: &str) -> Result<Self, String> {
        // TODO: maybe figure out which is more efficient: try hex first or try base58 first
        if let Ok(decoded) = Self::from_base58(str) {
            return Ok(decoded);
        }
        // max allowable length here is `MAX_HEX_ADDRESS_STRING_LENGTH`
        if str.len() > MAX_HEX_ADDRESS_STRING_LENGTH {
            return Err("Input above max allowed hex length".to_owned());
        }
        IrysAddress::from_hex(str).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("0x0000000000000000000000000000000000000001", true)]
    #[case("0x742d35cc6634c0532925a3b844bc9e7595f0beb0", true)]
    #[case("not_an_address", false)]
    #[case("0x", false)]
    #[case("0xinvalid", false)]
    // TODO: a test for `MAX_BS58_ADDRESS_STRING_LENGTH`
    fn test_parse_address(#[case] input: &str, #[case] should_succeed: bool) {
        let result = IrysAddress::from_str(input);
        assert_eq!(result.is_ok(), should_succeed);
    }
}

// Recursive expansion of wrap_fixed_bytes! macro
// contains some modifications, like changing import scoping from `crate::private`, disabling of feature related code we don't need, and a modified serialize/deserialize impl (to base58 instead of hex)
// the custom deserialize in particular is important!
// ===============================================

#[doc = r" An Ethereum address, 20 bytes in length."]
#[doc = r""]
#[doc = r" This type is separate from [`B160`](crate::B160) / [`FixedBytes<20>`]"]
#[doc = r" and is declared with the [`wrap_fixed_bytes!`] macro. This allows us"]
#[doc = r" to implement address-specific functionality."]
#[doc = r""]
#[doc = r" The main difference with the generic [`FixedBytes`] implementation is that"]
#[doc = r" [`Display`] formats the address using its [EIP-55] checksum"]
#[doc = r" ([`to_checksum`])."]
#[doc = r" Use [`Debug`] to display the raw bytes without the checksum."]
#[doc = r""]
#[doc = r" [EIP-55]: https://eips.ethereum.org/EIPS/eip-55"]
#[doc = r" [`Debug`]: fmt::Debug"]
#[doc = r" [`Display`]: fmt::Display"]
#[doc = r" [`to_checksum`]: Address::to_checksum"]
#[doc = r""]
#[doc = r" # Examples"]
#[doc = r""]
#[doc = r" Parsing and formatting:"]
#[doc = r""]
#[doc = r" ```"]
#[doc = r" use alloy_primitives::{address, Address};"]
#[doc = r""]
#[doc = r#" let checksummed = "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045";"#]
#[doc = r#" let expected = address!("0xd8da6bf26964af9d7eed9e03e53415d37aa96045");"#]
#[doc = r#" let address = Address::parse_checksummed(checksummed, None).expect("valid checksum");"#]
#[doc = r" assert_eq!(address, expected);"]
#[doc = r""]
#[doc = r" // Format the address with the checksum"]
#[doc = r" assert_eq!(address.to_string(), checksummed);"]
#[doc = r" assert_eq!(address.to_checksum(None), checksummed);"]
#[doc = r""]
#[doc = r" // Format the compressed checksummed address"]
#[doc = r#" assert_eq!(format!("{address:#}"), "0xd8dA…6045");"#]
#[doc = r""]
#[doc = r" // Format the address without the checksum"]
#[doc = r#" assert_eq!(format!("{address:?}"), "0xd8da6bf26964af9d7eed9e03e53415d37aa96045");"#]
#[doc = r" ```"]
#[derive(
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    derive_more::AsMut,
    derive_more::AsRef,
    derive_more::BitAnd,
    derive_more::BitAndAssign,
    derive_more::BitOr,
    derive_more::BitOrAssign,
    derive_more::BitXor,
    derive_more::BitXorAssign,
    derive_more::Not,
    derive_more::Deref,
    derive_more::DerefMut,
    derive_more::From,
    // derive_more::FromStr,
    derive_more::Index,
    derive_more::IndexMut,
    derive_more::Into,
    derive_more::IntoIterator,
    derive_more::LowerHex,
    derive_more::UpperHex,
)]
#[repr(transparent)]
pub struct IrysAddress(#[into_iterator(owned, ref, ref_mut)] pub alloy_primitives::FixedBytes<20>);

impl From<[u8; 20]> for IrysAddress {
    #[inline]
    fn from(value: [u8; 20]) -> Self {
        Self(alloy_primitives::FixedBytes(value))
    }
}
impl From<IrysAddress> for [u8; 20] {
    #[inline]
    fn from(value: IrysAddress) -> Self {
        value.0 .0
    }
}
impl<'a> From<&'a [u8; 20]> for IrysAddress {
    #[inline]
    fn from(value: &'a [u8; 20]) -> Self {
        Self(alloy_primitives::FixedBytes(*value))
    }
}
impl<'a> From<&'a mut [u8; 20]> for IrysAddress {
    #[inline]
    fn from(value: &'a mut [u8; 20]) -> Self {
        Self(alloy_primitives::FixedBytes(*value))
    }
}
impl TryFrom<&[u8]> for IrysAddress {
    type Error = core::array::TryFromSliceError;
    #[inline]
    fn try_from(slice: &[u8]) -> Result<Self, Self::Error> {
        <&Self as TryFrom<&[u8]>>::try_from(slice).copied()
    }
}
impl TryFrom<&mut [u8]> for IrysAddress {
    type Error = core::array::TryFromSliceError;
    #[inline]
    fn try_from(slice: &mut [u8]) -> Result<Self, Self::Error> {
        <Self as TryFrom<&[u8]>>::try_from(&*slice)
    }
}
impl<'a> TryFrom<&'a [u8]> for &'a IrysAddress {
    type Error = core::array::TryFromSliceError;
    #[inline]
    #[allow(unsafe_code)]
    fn try_from(slice: &'a [u8]) -> Result<&'a IrysAddress, Self::Error> {
        <&[u8; 20] as TryFrom<&[u8]>>::try_from(slice)
            .map(|array_ref| unsafe { core::mem::transmute(array_ref) })
    }
}
impl<'a> TryFrom<&'a mut [u8]> for &'a mut IrysAddress {
    type Error = core::array::TryFromSliceError;
    #[inline]
    #[allow(unsafe_code)]
    fn try_from(slice: &'a mut [u8]) -> Result<&'a mut IrysAddress, Self::Error> {
        <&mut [u8; 20] as TryFrom<&mut [u8]>>::try_from(slice)
            .map(|array_ref| unsafe { core::mem::transmute(array_ref) })
    }
}
impl AsRef<[u8; 20]> for IrysAddress {
    #[inline]
    fn as_ref(&self) -> &[u8; 20] {
        &self.0 .0
    }
}
impl AsMut<[u8; 20]> for IrysAddress {
    #[inline]
    fn as_mut(&mut self) -> &mut [u8; 20] {
        &mut self.0 .0
    }
}
impl AsRef<[u8]> for IrysAddress {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        &self.0 .0
    }
}
impl AsMut<[u8]> for IrysAddress {
    #[inline]
    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.0 .0
    }
}

impl core::fmt::Debug for IrysAddress {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Debug::fmt(&self.0.to_base58(), f)
    }
}

impl core::ops::BitAnd<&Self> for IrysAddress {
    type Output = Self;
    #[inline]
    fn bitand(self, rhs: &Self) -> Self {
        Self(self.0.bitand(&rhs.0))
    }
}
impl core::ops::BitAndAssign<&Self> for IrysAddress {
    #[inline]
    fn bitand_assign(&mut self, rhs: &Self) {
        self.0.bitand_assign(&rhs.0)
    }
}
impl core::ops::BitOr<&Self> for IrysAddress {
    type Output = Self;
    #[inline]
    fn bitor(self, rhs: &Self) -> Self {
        Self(self.0.bitor(&rhs.0))
    }
}
impl core::ops::BitOrAssign<&Self> for IrysAddress {
    #[inline]
    fn bitor_assign(&mut self, rhs: &Self) {
        self.0.bitor_assign(&rhs.0)
    }
}
impl core::ops::BitXor<&Self> for IrysAddress {
    type Output = Self;
    #[inline]
    fn bitxor(self, rhs: &Self) -> Self {
        Self(self.0.bitxor(&rhs.0))
    }
}
impl core::ops::BitXorAssign<&Self> for IrysAddress {
    #[inline]
    fn bitxor_assign(&mut self, rhs: &Self) {
        self.0.bitxor_assign(&rhs.0)
    }
}
impl Borrow<[u8]> for IrysAddress {
    #[inline]
    fn borrow(&self) -> &[u8] {
        Borrow::borrow(&self.0)
    }
}
impl Borrow<[u8]> for &IrysAddress {
    #[inline]
    fn borrow(&self) -> &[u8] {
        Borrow::borrow(&self.0)
    }
}
impl Borrow<[u8]> for &mut IrysAddress {
    #[inline]
    fn borrow(&self) -> &[u8] {
        Borrow::borrow(&self.0)
    }
}
impl Borrow<[u8; 20]> for IrysAddress {
    #[inline]
    fn borrow(&self) -> &[u8; 20] {
        Borrow::borrow(&self.0)
    }
}
impl Borrow<[u8; 20]> for &IrysAddress {
    #[inline]
    fn borrow(&self) -> &[u8; 20] {
        Borrow::borrow(&self.0)
    }
}
impl Borrow<[u8; 20]> for &mut IrysAddress {
    #[inline]
    fn borrow(&self) -> &[u8; 20] {
        Borrow::borrow(&self.0)
    }
}
impl BorrowMut<[u8]> for IrysAddress {
    #[inline]
    fn borrow_mut(&mut self) -> &mut [u8] {
        BorrowMut::borrow_mut(&mut self.0)
    }
}
impl BorrowMut<[u8]> for &mut IrysAddress {
    #[inline]
    fn borrow_mut(&mut self) -> &mut [u8] {
        BorrowMut::borrow_mut(&mut self.0)
    }
}
impl BorrowMut<[u8; 20]> for IrysAddress {
    #[inline]
    fn borrow_mut(&mut self) -> &mut [u8; 20] {
        BorrowMut::borrow_mut(&mut self.0)
    }
}
impl BorrowMut<[u8; 20]> for &mut IrysAddress {
    #[inline]
    fn borrow_mut(&mut self) -> &mut [u8; 20] {
        BorrowMut::borrow_mut(&mut self.0)
    }
}
impl<'a> From<&'a [u8; 20]> for &'a IrysAddress {
    #[inline]
    #[allow(unsafe_code)]
    fn from(value: &'a [u8; 20]) -> &'a IrysAddress {
        unsafe { core::mem::transmute::<&'a [u8; 20], &'a IrysAddress>(value) }
    }
}
impl<'a> From<&'a mut [u8; 20]> for &'a IrysAddress {
    #[inline]
    #[allow(unsafe_code)]
    fn from(value: &'a mut [u8; 20]) -> &'a IrysAddress {
        unsafe { core::mem::transmute::<&'a mut [u8; 20], &'a IrysAddress>(value) }
    }
}
impl<'a> From<&'a mut [u8; 20]> for &'a mut IrysAddress {
    #[inline]
    #[allow(unsafe_code)]
    fn from(value: &'a mut [u8; 20]) -> &'a mut IrysAddress {
        unsafe { core::mem::transmute::<&'a mut [u8; 20], &'a mut IrysAddress>(value) }
    }
}
impl<'a> From<&'a IrysAddress> for &'a [u8; 20] {
    #[inline]
    #[allow(unsafe_code)]
    fn from(value: &'a IrysAddress) -> &'a [u8; 20] {
        unsafe { core::mem::transmute::<&'a IrysAddress, &'a [u8; 20]>(value) }
    }
}
impl<'a> From<&'a mut IrysAddress> for &'a [u8; 20] {
    #[inline]
    #[allow(unsafe_code)]
    fn from(value: &'a mut IrysAddress) -> &'a [u8; 20] {
        unsafe { core::mem::transmute::<&'a mut IrysAddress, &'a [u8; 20]>(value) }
    }
}
impl<'a> From<&'a mut IrysAddress> for &'a mut [u8; 20] {
    #[inline]
    #[allow(unsafe_code)]
    fn from(value: &'a mut IrysAddress) -> &'a mut [u8; 20] {
        unsafe { core::mem::transmute::<&'a mut IrysAddress, &'a mut [u8; 20]>(value) }
    }
}
impl PartialEq<[u8]> for IrysAddress {
    #[inline]
    fn eq(&self, other: &[u8]) -> bool {
        PartialEq::eq(&self.0, other)
    }
}
impl PartialEq<IrysAddress> for [u8] {
    #[inline]
    fn eq(&self, other: &IrysAddress) -> bool {
        PartialEq::eq(self, &other.0)
    }
}
impl PartialEq<&[u8]> for IrysAddress {
    #[inline]
    fn eq(&self, other: &&[u8]) -> bool {
        PartialEq::eq(&self.0, *other)
    }
}
impl PartialEq<IrysAddress> for &[u8] {
    #[inline]
    fn eq(&self, other: &IrysAddress) -> bool {
        PartialEq::eq(*self, &other.0)
    }
}
impl PartialEq<[u8]> for &IrysAddress {
    #[inline]
    fn eq(&self, other: &[u8]) -> bool {
        PartialEq::eq(&self.0, other)
    }
}
impl PartialEq<&IrysAddress> for [u8] {
    #[inline]
    fn eq(&self, other: &&IrysAddress) -> bool {
        PartialEq::eq(self, &other.0)
    }
}
impl PartialEq<[u8; 20]> for IrysAddress {
    #[inline]
    fn eq(&self, other: &[u8; 20]) -> bool {
        PartialEq::eq(&self.0, other)
    }
}
impl PartialEq<IrysAddress> for [u8; 20] {
    #[inline]
    fn eq(&self, other: &IrysAddress) -> bool {
        PartialEq::eq(self, &other.0)
    }
}
impl PartialEq<&[u8; 20]> for IrysAddress {
    #[inline]
    fn eq(&self, other: &&[u8; 20]) -> bool {
        PartialEq::eq(&self.0, *other)
    }
}
impl PartialEq<IrysAddress> for &[u8; 20] {
    #[inline]
    fn eq(&self, other: &IrysAddress) -> bool {
        PartialEq::eq(*self, &other.0)
    }
}
impl PartialEq<[u8; 20]> for &IrysAddress {
    #[inline]
    fn eq(&self, other: &[u8; 20]) -> bool {
        PartialEq::eq(&self.0, other)
    }
}
impl PartialEq<&IrysAddress> for [u8; 20] {
    #[inline]
    fn eq(&self, other: &&IrysAddress) -> bool {
        PartialEq::eq(self, &other.0)
    }
}
impl PartialOrd<[u8]> for IrysAddress {
    #[inline]
    fn partial_cmp(&self, other: &[u8]) -> Option<Ordering> {
        PartialOrd::partial_cmp(&self.0[..], other)
    }
}
impl PartialOrd<IrysAddress> for [u8] {
    #[inline]
    fn partial_cmp(&self, other: &IrysAddress) -> Option<Ordering> {
        PartialOrd::partial_cmp(self, &other.0[..])
    }
}
impl PartialOrd<&[u8]> for IrysAddress {
    #[inline]
    fn partial_cmp(&self, other: &&[u8]) -> Option<Ordering> {
        PartialOrd::partial_cmp(&self.0[..], *other)
    }
}
impl PartialOrd<IrysAddress> for &[u8] {
    #[inline]
    fn partial_cmp(&self, other: &IrysAddress) -> Option<Ordering> {
        PartialOrd::partial_cmp(*self, &other.0[..])
    }
}
impl PartialOrd<[u8]> for &IrysAddress {
    #[inline]
    fn partial_cmp(&self, other: &[u8]) -> Option<Ordering> {
        PartialOrd::partial_cmp(&self.0[..], other)
    }
}
impl PartialOrd<&IrysAddress> for [u8] {
    #[inline]
    fn partial_cmp(&self, other: &&IrysAddress) -> Option<Ordering> {
        PartialOrd::partial_cmp(self, &other.0[..])
    }
}
impl alloy_primitives::hex::FromHex for IrysAddress {
    type Error = alloy_primitives::hex::FromHexError;
    #[inline]
    fn from_hex<T: AsRef<[u8]>>(hex: T) -> Result<Self, Self::Error> {
        alloy_primitives::hex::decode_to_array(hex).map(Self::new)
    }
}
#[cfg_attr(docsrs, doc(cfg(feature = "rlp")))]
impl alloy_rlp::Decodable for IrysAddress {
    #[inline]
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        alloy_rlp::Decodable::decode(buf).map(Self)
    }
}
#[cfg_attr(docsrs, doc(cfg(feature = "rlp")))]
impl alloy_rlp::Encodable for IrysAddress {
    #[inline]
    fn length(&self) -> usize {
        alloy_rlp::Encodable::length(&self.0)
    }
    #[inline]
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        alloy_rlp::Encodable::encode(&self.0, out)
    }
}
unsafe impl
    alloy_rlp::MaxEncodedLen<
        {
            {
                20 + alloy_rlp::length_of_length(20)
            }
        },
    > for IrysAddress
{
}

unsafe impl alloy_rlp::MaxEncodedLenAssoc for IrysAddress {
    const LEN: usize = { 20 + alloy_rlp::length_of_length(20) };
}
#[cfg_attr(docsrs, doc(cfg(feature = "serde")))]
impl serde::Serialize for IrysAddress {
    #[inline]
    // MODIFIED
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            // always serialize as a base58 string
            serializer.serialize_str(&self.to_base58())
        } else {
            serializer.serialize_bytes(self.as_slice())
        }
    }
}
#[cfg_attr(docsrs, doc(cfg(feature = "serde")))]
impl<'de> serde::Deserialize<'de> for IrysAddress {
    #[inline]
    // MODIFIED
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if !deserializer.is_human_readable() {
            // defer to inner FixedBytes impl
            return serde::Deserialize::deserialize(deserializer).map(Self);
        }

        // serde::Deserialize::deserialize(deserializer).map(Self)
        let s: String = serde::Deserialize::deserialize(deserializer)?;

        // Decode the base58 string into bytes
        // let bytes = base58::FromBase58::from_base58(s.as_str())
        //     .map_err(|e| serde::de::Error::custom(format!("Failed to decode from base58 {e:?}")))?;

        // // Ensure the byte array is exactly 20 bytes
        // if bytes.len() != 20 {
        //     return Err(serde::de::Error::invalid_length(
        //         bytes.len(),
        //         &"expected 20 bytes for address",
        //     ));
        // }

        // Ok(IrysAddress::from_slice(&bytes))

        Self::from_hex_or_base58(s.as_str()).map_err(|e| serde::de::Error::custom(e))
    }
}

#[cfg_attr(docsrs, doc(cfg(feature = "arbitrary")))]
impl<'a> arbitrary::Arbitrary<'a> for IrysAddress {
    #[inline]
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        <alloy_primitives::FixedBytes<20> as arbitrary::Arbitrary>::arbitrary(u).map(Self)
    }
    #[inline]
    fn arbitrary_take_rest(u: arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        <alloy_primitives::FixedBytes<20> as arbitrary::Arbitrary>::arbitrary_take_rest(u).map(Self)
    }
    #[inline]
    fn size_hint(depth: usize) -> (usize, Option<usize>) {
        <alloy_primitives::FixedBytes<20> as arbitrary::Arbitrary>::size_hint(depth)
    }
}
#[cfg_attr(docsrs, doc(cfg(feature = "arbitrary")))]
impl proptest::arbitrary::Arbitrary for IrysAddress {
    type Parameters =
        <alloy_primitives::FixedBytes<20> as proptest::arbitrary::Arbitrary>::Parameters;
    type Strategy = proptest::strategy::Map<
        <alloy_primitives::FixedBytes<20> as proptest::arbitrary::Arbitrary>::Strategy,
        fn(alloy_primitives::FixedBytes<20>) -> Self,
    >;
    #[inline]
    fn arbitrary() -> Self::Strategy {
        use proptest::strategy::Strategy;
        <alloy_primitives::FixedBytes<20> as proptest::arbitrary::Arbitrary>::arbitrary()
            .prop_map(Self)
    }
    #[inline]
    fn arbitrary_with(args: Self::Parameters) -> Self::Strategy {
        use proptest::strategy::Strategy;
        <alloy_primitives::FixedBytes<20> as proptest::arbitrary::Arbitrary>::arbitrary_with(args)
            .prop_map(Self)
    }
}
// #[cfg_attr(docsrs, doc(cfg(feature = "rand")))]
// impl rand::distr::Distribution<Address> for rand::distr::StandardUniform {
//     #[inline]
//     fn sample<R: rand::Rng + ?Sized>(&self, rng: &mut R) -> Address {
//         <Address>::random_with(rng)
//     }
// }
impl IrysAddress {
    #[doc = r" Array of Zero bytes."]
    pub const ZERO: Self = Self(alloy_primitives::FixedBytes::ZERO);
    #[doc = r" Wraps the given byte array in this type."]
    #[inline]
    pub const fn new(bytes: [u8; 20]) -> Self {
        Self(alloy_primitives::FixedBytes(bytes))
    }
    #[doc = r" Creates a new byte array with the last byte set to `x`."]
    #[inline]
    pub const fn with_last_byte(x: u8) -> Self {
        Self(alloy_primitives::FixedBytes::with_last_byte(x))
    }
    #[doc = r" Creates a new byte array where all bytes are set to `byte`."]
    #[inline]
    pub const fn repeat_byte(byte: u8) -> Self {
        Self(alloy_primitives::FixedBytes::repeat_byte(byte))
    }
    #[doc = r" Returns the size of this array in bytes."]
    #[inline]
    pub const fn len_bytes() -> usize {
        20
    }
    #[doc = r" Creates a new fixed byte array with the default cryptographic random number"]
    #[doc = r" generator."]
    #[doc = r""]
    #[doc = r#" This is `rand::thread_rng` if the "rand" and "std" features are enabled, otherwise"#]
    #[doc = r" it uses `getrandom::getrandom`. Both are cryptographically secure."]
    #[inline]
    #[track_caller]
    #[cfg_attr(docsrs, doc(cfg(feature = "getrandom")))]
    pub fn random() -> Self {
        Self(alloy_primitives::FixedBytes::random())
    }

    pub fn to_alloy_address(self) -> AlloyAddress {
        self.into()
    }
    // #[doc = r" Tries to create a new fixed byte array with the default cryptographic random number"]
    // #[doc = r" generator."]
    // #[doc = r""]
    // #[doc = r" See [`random`](Self::random) for more details."]
    // #[inline]
    // #[cfg_attr(docsrs, doc(cfg(feature = "getrandom")))]
    // pub fn try_random() -> Result<Self, alloy_primitives::getrandom::Error> {
    //     alloy_primitives::FixedBytes::try_random().map(Self)
    // }
    // #[doc = r" Fills this fixed byte array with the default cryptographic random number generator."]
    // #[doc = r""]
    // #[doc = r" See [`random`](Self::random) for more details."]
    // #[inline]
    // #[track_caller]
    // #[cfg_attr(docsrs, doc(cfg(feature = "getrandom")))]
    // pub fn randomize(&mut self) {
    //     self.0.randomize();
    // }
    // #[doc = r" Tries to fill this fixed byte array with the default cryptographic random number"]
    // #[doc = r" generator."]
    // #[doc = r""]
    // #[doc = r" See [`random`](Self::random) for more details."]
    // #[inline]
    // #[cfg_attr(docsrs, doc(cfg(feature = "getrandom")))]
    // pub fn try_randomize(&mut self) -> Result<(), getrandom::Error> {
    //     self.0.try_randomize()
    // }
    // #[doc = r" Creates a new fixed byte array with the given random number generator."]
    // #[inline]
    // #[doc(alias = "random_using")]
    // #[cfg_attr(docsrs, doc(cfg(feature = "rand")))]
    // pub fn random_with<R: rand::RngCore + ?Sized>(rng: &mut R) -> Self {
    //     Self(alloy_primitives::FixedBytes::random_with(rng))
    // }
    // #[doc = r" Tries to create a new fixed byte array with the given random number generator."]
    // #[inline]
    // #[cfg_attr(docsrs, doc(cfg(feature = "rand")))]
    // pub fn try_random_with<R: rand::TryRngCore + ?Sized>(rng: &mut R) -> Result<Self, R::Error> {
    //     alloy_primitives::FixedBytes::try_random_with(rng).map(Self)
    // }
    // #[doc = r" Fills this fixed byte array with the given random number generator."]
    // #[inline]
    // #[doc(alias = "randomize_using")]
    // #[cfg_attr(docsrs, doc(cfg(feature = "rand")))]
    // pub fn randomize_with<R: rand::RngCore + ?Sized>(&mut self, rng: &mut R) {
    //     self.0.randomize_with(rng);
    // }
    // #[doc = r" Tries to fill this fixed byte array with the given random number generator."]
    // #[inline]
    // #[cfg_attr(docsrs, doc(cfg(feature = "rand")))]
    // pub fn try_randomize_with<R: rand::TryRngCore + ?Sized>(
    //     &mut self,
    //     rng: &mut R,
    // ) -> Result<(), R::Error> {
    //     self.0.try_randomize_with(rng)
    // }
    #[doc = r" Create a new byte array from the given slice `src`."]
    #[doc = r""]
    #[doc = r" For a fallible version, use the `TryFrom<&[u8]>` implementation."]
    #[doc = r""]
    #[doc = r" # Note"]
    #[doc = r""]
    #[doc = r" The given bytes are interpreted in big endian order."]
    #[doc = r""]
    #[doc = r" # Panics"]
    #[doc = r""]
    #[doc = r" If the length of `src` and the number of bytes in `Self` do not match."]
    #[inline]
    #[track_caller]
    pub fn from_slice(src: &[u8]) -> Self {
        match Self::try_from(src) {
            Ok(x) => x,
            Err(_) => {
                panic!(
                    "cannot convert a slice of length {} to {}",
                    src.len(),
                    stringify!(Address)
                );
            }
        }
    }
    #[doc = r" Create a new byte array from the given slice `src`, left-padding it"]
    #[doc = r" with zeroes if necessary."]
    #[doc = r""]
    #[doc = r" # Note"]
    #[doc = r""]
    #[doc = r" The given bytes are interpreted in big endian order."]
    #[doc = r""]
    #[doc = r" # Panics"]
    #[doc = r""]
    #[doc = r" Panics if `src.len() > N`."]
    #[inline]
    #[track_caller]
    pub fn left_padding_from(value: &[u8]) -> Self {
        Self(alloy_primitives::FixedBytes::left_padding_from(value))
    }
    #[doc = r" Create a new byte array from the given slice `src`, right-padding it"]
    #[doc = r" with zeroes if necessary."]
    #[doc = r""]
    #[doc = r" # Note"]
    #[doc = r""]
    #[doc = r" The given bytes are interpreted in big endian order."]
    #[doc = r""]
    #[doc = r" # Panics"]
    #[doc = r""]
    #[doc = r" Panics if `src.len() > N`."]
    #[inline]
    #[track_caller]
    pub fn right_padding_from(value: &[u8]) -> Self {
        Self(alloy_primitives::FixedBytes::right_padding_from(value))
    }
    #[doc = r" Returns the inner bytes array."]
    #[inline]
    pub const fn into_array(self) -> [u8; 20] {
        self.0 .0
    }
    #[doc = r" Returns `true` if all bits set in `b` are also set in `self`."]
    #[inline]
    pub fn covers(&self, b: &Self) -> bool {
        &(*b & *self) == b
    }
    #[doc = r" Compile-time equality. NOT constant-time equality."]
    pub const fn const_eq(&self, other: &Self) -> bool {
        self.0.const_eq(&other.0)
    }
    #[doc = r" Computes the bitwise AND of two `FixedBytes`."]
    pub const fn bit_and(self, rhs: Self) -> Self {
        Self(self.0.bit_and(rhs.0))
    }
    #[doc = r" Computes the bitwise OR of two `FixedBytes`."]
    pub const fn bit_or(self, rhs: Self) -> Self {
        Self(self.0.bit_or(rhs.0))
    }
    #[doc = r" Computes the bitwise XOR of two `FixedBytes`."]
    pub const fn bit_xor(self, rhs: Self) -> Self {
        Self(self.0.bit_xor(rhs.0))
    }
}

// impl From<U160> for IrysAddress {
//     #[inline]
//     fn from(value: U160) -> Self {
//         Self(FixedBytes(value.to_be_bytes()))
//     }
// }

// impl From<IrysAddress> for U160 {
//     #[inline]
//     fn from(value: IrysAddress) -> Self {
//         Self::from_be_bytes(value.0 .0)
//     }
// }

impl fmt::Display for IrysAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // let checksum = self.to_checksum_buffer(None);
        // let checksum = checksum.as_str();
        // if f.alternate() {
        //     // If the alternate flag is set, use middle-out compression
        //     // "0x" + first 4 bytes + "…" + last 4 bytes
        //     f.write_str(&checksum[..6])?;
        //     f.write_str("…")?;
        //     f.write_str(&checksum[38..])
        // } else {
        //     f.write_str(checksum)
        // }
        f.write_str(&self.to_base58())
    }
}

impl IrysAddress {
    /// NOTE: This block of dead code is here so if we ever need to update the impl/implement one of these methods for parity, we can do so easily

    // /// Creates an Ethereum address from an EVM word's upper 20 bytes
    // /// (`word[12..]`).
    // ///
    // /// # Examples
    // ///
    // /// ```
    // /// # use alloy_primitives::{address, b256, Address};
    // /// let word = b256!("0x000000000000000000000000d8da6bf26964af9d7eed9e03e53415d37aa96045");
    // /// assert_eq!(Address::from_word(word), address!("0xd8da6bf26964af9d7eed9e03e53415d37aa96045"));
    // /// ```
    // #[inline]
    // #[must_use]
    // pub fn from_word(word: FixedBytes<32>) -> Self {
    //     Self(FixedBytes(word[12..].try_into().unwrap()))
    // }

    // /// Left-pads the address to 32 bytes (EVM word size).
    // ///
    // /// # Examples
    // ///
    // /// ```
    // /// # use alloy_primitives::{address, b256, Address};
    // /// assert_eq!(
    // ///     address!("0xd8da6bf26964af9d7eed9e03e53415d37aa96045").into_word(),
    // ///     b256!("0x000000000000000000000000d8da6bf26964af9d7eed9e03e53415d37aa96045"),
    // /// );
    // /// ```
    // #[inline]
    // #[must_use]
    // pub fn into_word(&self) -> FixedBytes<32> {
    //     let mut word = [0; 32];
    //     word[12..].copy_from_slice(self.as_slice());
    //     FixedBytes(word)
    // }

    // /// Parse an Ethereum address, verifying its [EIP-55] checksum.
    // ///
    // /// You can optionally specify an [EIP-155 chain ID] to check the address
    // /// using [EIP-1191].
    // ///
    // /// [EIP-55]: https://eips.ethereum.org/EIPS/eip-55
    // /// [EIP-155 chain ID]: https://eips.ethereum.org/EIPS/eip-155
    // /// [EIP-1191]: https://eips.ethereum.org/EIPS/eip-1191
    // ///
    // /// # Errors
    // ///
    // /// This method returns an error if the provided string does not match the
    // /// expected checksum.
    // ///
    // /// # Examples
    // ///
    // /// ```
    // /// # use alloy_primitives::{address, Address};
    // /// let checksummed = "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045";
    // /// let address = Address::parse_checksummed(checksummed, None).unwrap();
    // /// let expected = address!("0xd8da6bf26964af9d7eed9e03e53415d37aa96045");
    // /// assert_eq!(address, expected);
    // /// ```
    // pub fn parse_checksummed<S: AsRef<str>>(
    //     s: S,
    //     chain_id: Option<u64>,
    // ) -> Result<Self, AddressError> {
    //     fn parse_checksummed(s: &str, chain_id: Option<u64>) -> Result<Address, AddressError> {
    //         // checksummed addresses always start with the "0x" prefix
    //         if !s.starts_with("0x") {
    //             return Err(AddressError::Hex(hex::FromHexError::InvalidStringLength));
    //         }

    //         let address: Address = s.parse()?;
    //         if s == address.to_checksum_buffer(chain_id).as_str() {
    //             Ok(address)
    //         } else {
    //             Err(AddressError::InvalidChecksum)
    //         }
    //     }

    //     parse_checksummed(s.as_ref(), chain_id)
    // }

    // /// Encodes an Ethereum address to its [EIP-55] checksum into a heap-allocated string.
    // ///
    // /// You can optionally specify an [EIP-155 chain ID] to encode the address
    // /// using [EIP-1191].
    // ///
    // /// [EIP-55]: https://eips.ethereum.org/EIPS/eip-55
    // /// [EIP-155 chain ID]: https://eips.ethereum.org/EIPS/eip-155
    // /// [EIP-1191]: https://eips.ethereum.org/EIPS/eip-1191
    // ///
    // /// # Examples
    // ///
    // /// ```
    // /// # use alloy_primitives::{address, Address};
    // /// let address = address!("0xd8da6bf26964af9d7eed9e03e53415d37aa96045");
    // ///
    // /// let checksummed: String = address.to_checksum(None);
    // /// assert_eq!(checksummed, "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045");
    // ///
    // /// let checksummed: String = address.to_checksum(Some(1));
    // /// assert_eq!(checksummed, "0xD8Da6bf26964Af9d7EEd9e03e53415d37AA96045");
    // /// ```
    // #[inline]
    // #[must_use]
    // pub fn to_checksum(&self, chain_id: Option<u64>) -> String {
    //     self.to_checksum_buffer(chain_id).as_str().into()
    // }

    // /// Encodes an Ethereum address to its [EIP-55] checksum into the given buffer.
    // ///
    // /// For convenience, the buffer is returned as a `&mut str`, as the bytes
    // /// are guaranteed to be valid UTF-8.
    // ///
    // /// You can optionally specify an [EIP-155 chain ID] to encode the address
    // /// using [EIP-1191].
    // ///
    // /// [EIP-55]: https://eips.ethereum.org/EIPS/eip-55
    // /// [EIP-155 chain ID]: https://eips.ethereum.org/EIPS/eip-155
    // /// [EIP-1191]: https://eips.ethereum.org/EIPS/eip-1191
    // ///
    // /// # Panics
    // ///
    // /// Panics if `buf` is not exactly 42 bytes long.
    // ///
    // /// # Examples
    // ///
    // /// ```
    // /// # use alloy_primitives::{address, Address};
    // /// let address = address!("0xd8da6bf26964af9d7eed9e03e53415d37aa96045");
    // /// let mut buf = [0; 42];
    // ///
    // /// let checksummed: &mut str = address.to_checksum_raw(&mut buf, None);
    // /// assert_eq!(checksummed, "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045");
    // ///
    // /// let checksummed: &mut str = address.to_checksum_raw(&mut buf, Some(1));
    // /// assert_eq!(checksummed, "0xD8Da6bf26964Af9d7EEd9e03e53415d37AA96045");
    // /// ```
    // #[inline]
    // #[must_use]
    // pub fn to_checksum_raw<'a>(&self, buf: &'a mut [u8], chain_id: Option<u64>) -> &'a mut str {
    //     let buf: &mut [u8; 42] = buf.try_into().expect("buffer must be exactly 42 bytes long");
    //     self.to_checksum_inner(buf, chain_id);
    //     // SAFETY: All bytes in the buffer are valid UTF-8.
    //     unsafe { str::from_utf8_unchecked_mut(buf) }
    // }

    // /// Encodes an Ethereum address to its [EIP-55] checksum into a stack-allocated buffer.
    // ///
    // /// You can optionally specify an [EIP-155 chain ID] to encode the address
    // /// using [EIP-1191].
    // ///
    // /// [EIP-55]: https://eips.ethereum.org/EIPS/eip-55
    // /// [EIP-155 chain ID]: https://eips.ethereum.org/EIPS/eip-155
    // /// [EIP-1191]: https://eips.ethereum.org/EIPS/eip-1191
    // ///
    // /// # Examples
    // ///
    // /// ```
    // /// # use alloy_primitives::{address, Address, AddressChecksumBuffer};
    // /// let address = address!("0xd8da6bf26964af9d7eed9e03e53415d37aa96045");
    // ///
    // /// let mut buffer: AddressChecksumBuffer = address.to_checksum_buffer(None);
    // /// assert_eq!(buffer.as_str(), "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045");
    // ///
    // /// let checksummed: &str = buffer.format(&address, Some(1));
    // /// assert_eq!(checksummed, "0xD8Da6bf26964Af9d7EEd9e03e53415d37AA96045");
    // /// ```
    // #[inline]
    // pub fn to_checksum_buffer(&self, chain_id: Option<u64>) -> AddressChecksumBuffer {
    //     // SAFETY: The buffer is initialized by `format`.
    //     let mut buf = unsafe { AddressChecksumBuffer::new() };
    //     buf.format(self, chain_id);
    //     buf
    // }

    // // https://eips.ethereum.org/EIPS/eip-55
    // // > In English, convert the address to hex, but if the `i`th digit is a letter (ie. it’s one of
    // // > `abcdef`) print it in uppercase if the `4*i`th bit of the hash of the lowercase hexadecimal
    // // > address is 1 otherwise print it in lowercase.
    // //
    // // https://eips.ethereum.org/EIPS/eip-1191
    // // > [...] If the chain id passed to the function belongs to a network that opted for using this
    // // > checksum variant, prefix the address with the chain id and the `0x` separator before
    // // > calculating the hash. [...]
    // #[allow(clippy::wrong_self_convention)]
    // fn to_checksum_inner(&self, buf: &mut [u8; 42], chain_id: Option<u64>) {
    //     buf[0] = b'0';
    //     buf[1] = b'x';
    //     hex::encode_to_slice(self, &mut buf[2..]).unwrap();

    //     let mut hasher = crate::Keccak256::new();
    //     match chain_id {
    //         Some(chain_id) => {
    //             hasher.update(itoa::Buffer::new().format(chain_id).as_bytes());
    //             // Clippy suggests an unnecessary copy.
    //             #[allow(clippy::needless_borrows_for_generic_args)]
    //             hasher.update(&*buf);
    //         }
    //         None => hasher.update(&buf[2..]),
    //     }
    //     let hash = hasher.finalize();

    //     for (i, out) in buf[2..].iter_mut().enumerate() {
    //         // This is made branchless for easier vectorization.
    //         // Get the i-th nibble of the hash.
    //         let hash_nibble = (hash[i / 2] >> (4 * (1 - i % 2))) & 0xf;
    //         // Make the character ASCII uppercase if it's a hex letter and the hash nibble is >= 8.
    //         // We can use a simpler comparison for checking if the character is a hex letter because
    //         // we know `out` is a hex-encoded character (`b'0'..=b'9' | b'a'..=b'f'`).
    //         *out ^= 0b0010_0000 * ((*out >= b'a') & (hash_nibble >= 8)) as u8;
    //     }
    // }

    // /// Computes the `create` address for this address and nonce:
    // ///
    // /// `keccak256(rlp([sender, nonce]))[12:]`
    // ///
    // /// # Examples
    // ///
    // /// ```
    // /// # use alloy_primitives::{address, Address};
    // /// let sender = address!("0xb20a608c624Ca5003905aA834De7156C68b2E1d0");
    // ///
    // /// let expected = address!("0x00000000219ab540356cBB839Cbe05303d7705Fa");
    // /// assert_eq!(sender.create(0), expected);
    // ///
    // /// let expected = address!("0xe33c6e89e69d085897f98e92b06ebd541d1daa99");
    // /// assert_eq!(sender.create(1), expected);
    // /// ```
    // #[cfg(feature = "rlp")]
    // #[inline]
    // #[must_use]
    // pub fn create(&self, nonce: u64) -> Self {
    //     use alloy_rlp::{EMPTY_LIST_CODE, EMPTY_STRING_CODE, Encodable};

    //     // max u64 encoded length is `1 + u64::BYTES`
    //     const MAX_LEN: usize = 1 + (1 + 20) + 9;

    //     let len = 22 + nonce.length();
    //     debug_assert!(len <= MAX_LEN);

    //     let mut out = [0u8; MAX_LEN];

    //     // list header
    //     // minus 1 to account for the list header itself
    //     out[0] = EMPTY_LIST_CODE + len as u8 - 1;

    //     // address header + address
    //     out[1] = EMPTY_STRING_CODE + 20;
    //     out[2..22].copy_from_slice(self.as_slice());

    //     // nonce
    //     nonce.encode(&mut &mut out[22..]);

    //     let hash = keccak256(&out[..len]);
    //     Self::from_word(hash)
    // }

    // /// Computes the `CREATE2` address of a smart contract as specified in
    // /// [EIP-1014]:
    // ///
    // /// `keccak256(0xff ++ address ++ salt ++ keccak256(init_code))[12:]`
    // ///
    // /// The `init_code` is the code that, when executed, produces the runtime
    // /// bytecode that will be placed into the state, and which typically is used
    // /// by high level languages to implement a ‘constructor’.
    // ///
    // /// [EIP-1014]: https://eips.ethereum.org/EIPS/eip-1014
    // ///
    // /// # Examples
    // ///
    // /// ```
    // /// # use alloy_primitives::{address, b256, bytes, Address};
    // /// let address = address!("0x8ba1f109551bD432803012645Ac136ddd64DBA72");
    // /// let salt = b256!("0x7c5ea36004851c764c44143b1dcb59679b11c9a68e5f41497f6cf3d480715331");
    // /// let init_code = bytes!("6394198df16000526103ff60206004601c335afa6040516060f3");
    // /// let expected = address!("0x533ae9d683B10C02EbDb05471642F85230071FC3");
    // /// assert_eq!(address.create2_from_code(salt, init_code), expected);
    // /// ```
    // #[must_use]
    // pub fn create2_from_code<S, C>(&self, salt: S, init_code: C) -> Self
    // where
    //     // not `AsRef` because `[u8; N]` does not implement `AsRef<[u8; N]>`
    //     S: Borrow<[u8; 32]>,
    //     C: AsRef<[u8]>,
    // {
    //     self._create2(salt.borrow(), &keccak256(init_code.as_ref()).0)
    // }

    // /// Computes the `CREATE2` address of a smart contract as specified in
    // /// [EIP-1014], taking the pre-computed hash of the init code as input:
    // ///
    // /// `keccak256(0xff ++ address ++ salt ++ init_code_hash)[12:]`
    // ///
    // /// The `init_code` is the code that, when executed, produces the runtime
    // /// bytecode that will be placed into the state, and which typically is used
    // /// by high level languages to implement a ‘constructor’.
    // ///
    // /// [EIP-1014]: https://eips.ethereum.org/EIPS/eip-1014
    // ///
    // /// # Examples
    // ///
    // /// ```
    // /// # use alloy_primitives::{address, b256, Address};
    // /// let address = address!("0x5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f");
    // /// let salt = b256!("0x2b2f5776e38002e0c013d0d89828fdb06fee595ea2d5ed4b194e3883e823e350");
    // /// let init_code_hash =
    // ///     b256!("0x96e8ac4277198ff8b6f785478aa9a39f403cb768dd02cbee326c3e7da348845f");
    // /// let expected = address!("0x0d4a11d5EEaaC28EC3F61d100daF4d40471f1852");
    // /// assert_eq!(address.create2(salt, init_code_hash), expected);
    // /// ```
    // #[must_use]
    // pub fn create2<S, H>(&self, salt: S, init_code_hash: H) -> Self
    // where
    //     // not `AsRef` because `[u8; N]` does not implement `AsRef<[u8; N]>`
    //     S: Borrow<[u8; 32]>,
    //     H: Borrow<[u8; 32]>,
    // {
    //     self._create2(salt.borrow(), init_code_hash.borrow())
    // }

    // // non-generic inner function
    // fn _create2(&self, salt: &[u8; 32], init_code_hash: &[u8; 32]) -> Self {
    //     // note: creating a temporary buffer and copying everything over performs
    //     // much better than calling `Keccak::update` multiple times
    //     let mut bytes = [0; 85];
    //     bytes[0] = 0xff;
    //     bytes[1..21].copy_from_slice(self.as_slice());
    //     bytes[21..53].copy_from_slice(salt);
    //     bytes[53..85].copy_from_slice(init_code_hash);
    //     let hash = keccak256(bytes);
    //     Self::from_word(hash)
    // }

    // /// Computes the address created by the `EOFCREATE` opcode, where `self` is the sender.
    // ///
    // /// The address is calculated as `keccak256(0xff || sender32 || salt)[12:]`, where sender32 is
    // /// the sender address left-padded to 32 bytes with zeros.
    // ///
    // /// See [EIP-7620](https://eips.ethereum.org/EIPS/eip-7620) for more details.
    // ///
    // /// <div class="warning">
    // /// This function's stability is not guaranteed. It may change in the future as the EIP is
    // /// not yet accepted.
    // /// </div>
    // ///
    // /// # Examples
    // ///
    // /// ```
    // /// # use alloy_primitives::{address, b256, Address};
    // /// let address = address!("0xb20a608c624Ca5003905aA834De7156C68b2E1d0");
    // /// let salt = b256!("0x7c5ea36004851c764c44143b1dcb59679b11c9a68e5f41497f6cf3d480715331");
    // /// // Create an address using CREATE_EOF
    // /// let eof_address = address.create_eof(salt);
    // /// ```
    // #[must_use]
    // #[doc(alias = "eof_create")]
    // pub fn create_eof<S>(&self, salt: S) -> Self
    // where
    //     // not `AsRef` because `[u8; N]` does not implement `AsRef<[u8; N]>`
    //     S: Borrow<[u8; 32]>,
    // {
    //     self._create_eof(salt.borrow())
    // }

    // // non-generic inner function
    // fn _create_eof(&self, salt: &[u8; 32]) -> Self {
    //     let mut buffer = [0; 65];
    //     buffer[0] = 0xff;
    //     // 1..13 is zero pad (already initialized to 0)
    //     buffer[13..33].copy_from_slice(self.as_slice());
    //     buffer[33..].copy_from_slice(salt);
    //     Self::from_word(keccak256(buffer))
    // }

    /// Instantiate by hashing public key bytes.
    ///
    /// # Panics
    ///
    /// If the input is not exactly 64 bytes
    pub fn from_raw_public_key(pubkey: &[u8]) -> Self {
        assert_eq!(pubkey.len(), 64, "raw public key must be 64 bytes");
        let digest = keccak256(pubkey);
        Self::from_slice(&digest[12..])
    }

    /// Converts an ECDSA verifying key to its corresponding Ethereum address.
    #[inline]

    pub fn from_public_key(pubkey: &k256::ecdsa::VerifyingKey) -> Self {
        use k256::elliptic_curve::sec1::ToEncodedPoint;
        let affine: &k256::AffinePoint = pubkey.as_ref();
        let encoded = affine.to_encoded_point(false);
        Self::from_raw_public_key(&encoded.as_bytes()[1..])
    }

    /// Converts an ECDSA signing key to its corresponding Ethereum address.
    #[inline]

    pub fn from_private_key(private_key: &k256::ecdsa::SigningKey) -> Self {
        Self::from_public_key(private_key.verifying_key())
    }
}
