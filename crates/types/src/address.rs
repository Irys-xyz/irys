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

use std::{
    borrow::{Borrow, BorrowMut},
    cmp::Ordering,
};

use alloy_primitives::Address as AlloyAddress;

impl From<AlloyAddress> for Address {
    fn from(value: AlloyAddress) -> Self {
        Self(value.0)
    }
}

impl Into<AlloyAddress> for Address {
    fn into(self) -> AlloyAddress {
        AlloyAddress(self.0)
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
    derive_more::FromStr,
    derive_more::Index,
    derive_more::IndexMut,
    derive_more::Into,
    derive_more::IntoIterator,
    derive_more::LowerHex,
    derive_more::UpperHex,
)]
#[repr(transparent)]
pub struct Address(#[into_iterator(owned, ref, ref_mut)] pub alloy_primitives::FixedBytes<20>);

impl From<[u8; 20]> for Address {
    #[inline]
    fn from(value: [u8; 20]) -> Self {
        Self(alloy_primitives::FixedBytes(value))
    }
}
impl From<Address> for [u8; 20] {
    #[inline]
    fn from(value: Address) -> Self {
        value.0 .0
    }
}
impl<'a> From<&'a [u8; 20]> for Address {
    #[inline]
    fn from(value: &'a [u8; 20]) -> Self {
        Self(alloy_primitives::FixedBytes(*value))
    }
}
impl<'a> From<&'a mut [u8; 20]> for Address {
    #[inline]
    fn from(value: &'a mut [u8; 20]) -> Self {
        Self(alloy_primitives::FixedBytes(*value))
    }
}
impl TryFrom<&[u8]> for Address {
    type Error = core::array::TryFromSliceError;
    #[inline]
    fn try_from(slice: &[u8]) -> Result<Self, Self::Error> {
        <&Self as TryFrom<&[u8]>>::try_from(slice).copied()
    }
}
impl TryFrom<&mut [u8]> for Address {
    type Error = core::array::TryFromSliceError;
    #[inline]
    fn try_from(slice: &mut [u8]) -> Result<Self, Self::Error> {
        <Self as TryFrom<&[u8]>>::try_from(&*slice)
    }
}
impl<'a> TryFrom<&'a [u8]> for &'a Address {
    type Error = core::array::TryFromSliceError;
    #[inline]
    #[allow(unsafe_code)]
    fn try_from(slice: &'a [u8]) -> Result<&'a Address, Self::Error> {
        <&[u8; 20] as TryFrom<&[u8]>>::try_from(slice)
            .map(|array_ref| unsafe { core::mem::transmute(array_ref) })
    }
}
impl<'a> TryFrom<&'a mut [u8]> for &'a mut Address {
    type Error = core::array::TryFromSliceError;
    #[inline]
    #[allow(unsafe_code)]
    fn try_from(slice: &'a mut [u8]) -> Result<&'a mut Address, Self::Error> {
        <&mut [u8; 20] as TryFrom<&mut [u8]>>::try_from(slice)
            .map(|array_ref| unsafe { core::mem::transmute(array_ref) })
    }
}
impl AsRef<[u8; 20]> for Address {
    #[inline]
    fn as_ref(&self) -> &[u8; 20] {
        &self.0 .0
    }
}
impl AsMut<[u8; 20]> for Address {
    #[inline]
    fn as_mut(&mut self) -> &mut [u8; 20] {
        &mut self.0 .0
    }
}
impl AsRef<[u8]> for Address {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        &self.0 .0
    }
}
impl AsMut<[u8]> for Address {
    #[inline]
    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.0 .0
    }
}
impl core::fmt::Debug for Address {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Debug::fmt(&self.0, f)
    }
}
impl core::ops::BitAnd<&Self> for Address {
    type Output = Self;
    #[inline]
    fn bitand(self, rhs: &Self) -> Self {
        Self(self.0.bitand(&rhs.0))
    }
}
impl core::ops::BitAndAssign<&Self> for Address {
    #[inline]
    fn bitand_assign(&mut self, rhs: &Self) {
        self.0.bitand_assign(&rhs.0)
    }
}
impl core::ops::BitOr<&Self> for Address {
    type Output = Self;
    #[inline]
    fn bitor(self, rhs: &Self) -> Self {
        Self(self.0.bitor(&rhs.0))
    }
}
impl core::ops::BitOrAssign<&Self> for Address {
    #[inline]
    fn bitor_assign(&mut self, rhs: &Self) {
        self.0.bitor_assign(&rhs.0)
    }
}
impl core::ops::BitXor<&Self> for Address {
    type Output = Self;
    #[inline]
    fn bitxor(self, rhs: &Self) -> Self {
        Self(self.0.bitxor(&rhs.0))
    }
}
impl core::ops::BitXorAssign<&Self> for Address {
    #[inline]
    fn bitxor_assign(&mut self, rhs: &Self) {
        self.0.bitxor_assign(&rhs.0)
    }
}
impl Borrow<[u8]> for Address {
    #[inline]
    fn borrow(&self) -> &[u8] {
        Borrow::borrow(&self.0)
    }
}
impl Borrow<[u8]> for &Address {
    #[inline]
    fn borrow(&self) -> &[u8] {
        Borrow::borrow(&self.0)
    }
}
impl Borrow<[u8]> for &mut Address {
    #[inline]
    fn borrow(&self) -> &[u8] {
        Borrow::borrow(&self.0)
    }
}
impl Borrow<[u8; 20]> for Address {
    #[inline]
    fn borrow(&self) -> &[u8; 20] {
        Borrow::borrow(&self.0)
    }
}
impl Borrow<[u8; 20]> for &Address {
    #[inline]
    fn borrow(&self) -> &[u8; 20] {
        Borrow::borrow(&self.0)
    }
}
impl Borrow<[u8; 20]> for &mut Address {
    #[inline]
    fn borrow(&self) -> &[u8; 20] {
        Borrow::borrow(&self.0)
    }
}
impl BorrowMut<[u8]> for Address {
    #[inline]
    fn borrow_mut(&mut self) -> &mut [u8] {
        BorrowMut::borrow_mut(&mut self.0)
    }
}
impl BorrowMut<[u8]> for &mut Address {
    #[inline]
    fn borrow_mut(&mut self) -> &mut [u8] {
        BorrowMut::borrow_mut(&mut self.0)
    }
}
impl BorrowMut<[u8; 20]> for Address {
    #[inline]
    fn borrow_mut(&mut self) -> &mut [u8; 20] {
        BorrowMut::borrow_mut(&mut self.0)
    }
}
impl BorrowMut<[u8; 20]> for &mut Address {
    #[inline]
    fn borrow_mut(&mut self) -> &mut [u8; 20] {
        BorrowMut::borrow_mut(&mut self.0)
    }
}
impl<'a> From<&'a [u8; 20]> for &'a Address {
    #[inline]
    #[allow(unsafe_code)]
    fn from(value: &'a [u8; 20]) -> &'a Address {
        unsafe { core::mem::transmute::<&'a [u8; 20], &'a Address>(value) }
    }
}
impl<'a> From<&'a mut [u8; 20]> for &'a Address {
    #[inline]
    #[allow(unsafe_code)]
    fn from(value: &'a mut [u8; 20]) -> &'a Address {
        unsafe { core::mem::transmute::<&'a mut [u8; 20], &'a Address>(value) }
    }
}
impl<'a> From<&'a mut [u8; 20]> for &'a mut Address {
    #[inline]
    #[allow(unsafe_code)]
    fn from(value: &'a mut [u8; 20]) -> &'a mut Address {
        unsafe { core::mem::transmute::<&'a mut [u8; 20], &'a mut Address>(value) }
    }
}
impl<'a> From<&'a Address> for &'a [u8; 20] {
    #[inline]
    #[allow(unsafe_code)]
    fn from(value: &'a Address) -> &'a [u8; 20] {
        unsafe { core::mem::transmute::<&'a Address, &'a [u8; 20]>(value) }
    }
}
impl<'a> From<&'a mut Address> for &'a [u8; 20] {
    #[inline]
    #[allow(unsafe_code)]
    fn from(value: &'a mut Address) -> &'a [u8; 20] {
        unsafe { core::mem::transmute::<&'a mut Address, &'a [u8; 20]>(value) }
    }
}
impl<'a> From<&'a mut Address> for &'a mut [u8; 20] {
    #[inline]
    #[allow(unsafe_code)]
    fn from(value: &'a mut Address) -> &'a mut [u8; 20] {
        unsafe { core::mem::transmute::<&'a mut Address, &'a mut [u8; 20]>(value) }
    }
}
impl PartialEq<[u8]> for Address {
    #[inline]
    fn eq(&self, other: &[u8]) -> bool {
        PartialEq::eq(&self.0, other)
    }
}
impl PartialEq<Address> for [u8] {
    #[inline]
    fn eq(&self, other: &Address) -> bool {
        PartialEq::eq(self, &other.0)
    }
}
impl PartialEq<&[u8]> for Address {
    #[inline]
    fn eq(&self, other: &&[u8]) -> bool {
        PartialEq::eq(&self.0, *other)
    }
}
impl PartialEq<Address> for &[u8] {
    #[inline]
    fn eq(&self, other: &Address) -> bool {
        PartialEq::eq(*self, &other.0)
    }
}
impl PartialEq<[u8]> for &Address {
    #[inline]
    fn eq(&self, other: &[u8]) -> bool {
        PartialEq::eq(&self.0, other)
    }
}
impl PartialEq<&Address> for [u8] {
    #[inline]
    fn eq(&self, other: &&Address) -> bool {
        PartialEq::eq(self, &other.0)
    }
}
impl PartialEq<[u8; 20]> for Address {
    #[inline]
    fn eq(&self, other: &[u8; 20]) -> bool {
        PartialEq::eq(&self.0, other)
    }
}
impl PartialEq<Address> for [u8; 20] {
    #[inline]
    fn eq(&self, other: &Address) -> bool {
        PartialEq::eq(self, &other.0)
    }
}
impl PartialEq<&[u8; 20]> for Address {
    #[inline]
    fn eq(&self, other: &&[u8; 20]) -> bool {
        PartialEq::eq(&self.0, *other)
    }
}
impl PartialEq<Address> for &[u8; 20] {
    #[inline]
    fn eq(&self, other: &Address) -> bool {
        PartialEq::eq(*self, &other.0)
    }
}
impl PartialEq<[u8; 20]> for &Address {
    #[inline]
    fn eq(&self, other: &[u8; 20]) -> bool {
        PartialEq::eq(&self.0, other)
    }
}
impl PartialEq<&Address> for [u8; 20] {
    #[inline]
    fn eq(&self, other: &&Address) -> bool {
        PartialEq::eq(self, &other.0)
    }
}
impl PartialOrd<[u8]> for Address {
    #[inline]
    fn partial_cmp(&self, other: &[u8]) -> Option<Ordering> {
        PartialOrd::partial_cmp(&self.0[(..)], other)
    }
}
impl PartialOrd<Address> for [u8] {
    #[inline]
    fn partial_cmp(&self, other: &Address) -> Option<Ordering> {
        PartialOrd::partial_cmp(self, &other.0[(..)])
    }
}
impl PartialOrd<&[u8]> for Address {
    #[inline]
    fn partial_cmp(&self, other: &&[u8]) -> Option<Ordering> {
        PartialOrd::partial_cmp(&self.0[(..)], *other)
    }
}
impl PartialOrd<Address> for &[u8] {
    #[inline]
    fn partial_cmp(&self, other: &Address) -> Option<Ordering> {
        PartialOrd::partial_cmp(*self, &other.0[(..)])
    }
}
impl PartialOrd<[u8]> for &Address {
    #[inline]
    fn partial_cmp(&self, other: &[u8]) -> Option<Ordering> {
        PartialOrd::partial_cmp(&self.0[(..)], other)
    }
}
impl PartialOrd<&Address> for [u8] {
    #[inline]
    fn partial_cmp(&self, other: &&Address) -> Option<Ordering> {
        PartialOrd::partial_cmp(self, &other.0[(..)])
    }
}
impl alloy_primitives::hex::FromHex for Address {
    type Error = alloy_primitives::hex::FromHexError;
    #[inline]
    fn from_hex<T: AsRef<[u8]>>(hex: T) -> Result<Self, Self::Error> {
        alloy_primitives::hex::decode_to_array(hex).map(Self::new)
    }
}
#[cfg_attr(docsrs, doc(cfg(feature = "rlp")))]
impl alloy_rlp::Decodable for Address {
    #[inline]
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        alloy_rlp::Decodable::decode(buf).map(Self)
    }
}
#[cfg_attr(docsrs, doc(cfg(feature = "rlp")))]
impl alloy_rlp::Encodable for Address {
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
    > for Address
{
}

unsafe impl alloy_rlp::MaxEncodedLenAssoc for Address {
    const LEN: usize = { 20 + alloy_rlp::length_of_length(20) };
}
#[cfg_attr(docsrs, doc(cfg(feature = "serde")))]
impl serde::Serialize for Address {
    #[inline]
    // MODIFIED
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // serde::Serialize::serialize(&self.0, serializer)
        if serializer.is_human_readable() {
            serializer.serialize_str(base58::ToBase58::to_base58(&self.0[..]).as_ref())
        } else {
            serializer.serialize_bytes(self.as_slice())
        }
    }
}
#[cfg_attr(docsrs, doc(cfg(feature = "serde")))]
impl<'de> serde::Deserialize<'de> for Address {
    #[inline]
    // MODIFIED
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if !deserializer.is_human_readable() {
            return serde::Deserialize::deserialize(deserializer).map(Self);
        }

        // serde::Deserialize::deserialize(deserializer).map(Self)
        let s: String = serde::Deserialize::deserialize(deserializer)?;

        // Decode the base58 string into bytes
        let bytes = base58::FromBase58::from_base58(s.as_str())
            .map_err(|e| serde::de::Error::custom(format!("Failed to decode from base58 {e:?}")))?;

        // Ensure the byte array is exactly 20 bytes
        if bytes.len() != 20 {
            return Err(serde::de::Error::invalid_length(
                bytes.len(),
                &"expected 20 bytes for address",
            ));
        }

        Ok(Address::from_slice(&bytes))
    }
}

#[cfg_attr(docsrs, doc(cfg(feature = "arbitrary")))]
impl<'a> arbitrary::Arbitrary<'a> for Address {
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
impl proptest::arbitrary::Arbitrary for Address {
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
impl Address {
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
