// Recursive expansion of fixed_hash::construct_fixed_hash! macro
// with some tweaks to de-expand assert_eq! and override Debug & Display
// ===================================================
#![allow(clippy::all)] // disable lints - this is a macro expansion that I want to keep as close to the original as possible

use base58::ToBase58 as _;

#[repr(C)]
#[doc = r" A 256-bit hash type (32 bytes)."]
pub struct H256(pub [u8; 32]);

impl From<[u8; 32]> for H256 {
    #[doc = r" Constructs a hash type from the given bytes array of fixed length."]
    #[doc = r""]
    #[doc = r" # Note"]
    #[doc = r""]
    #[doc = r" The given bytes are interpreted in big endian order."]
    #[inline]
    fn from(bytes: [u8; 32]) -> Self {
        H256(bytes)
    }
}
impl<'a> From<&'a [u8; 32]> for H256 {
    #[doc = r" Constructs a hash type from the given reference"]
    #[doc = r" to the bytes array of fixed length."]
    #[doc = r""]
    #[doc = r" # Note"]
    #[doc = r""]
    #[doc = r" The given bytes are interpreted in big endian order."]
    #[inline]
    fn from(bytes: &'a [u8; 32]) -> Self {
        H256(*bytes)
    }
}
impl<'a> From<&'a mut [u8; 32]> for H256 {
    #[doc = r" Constructs a hash type from the given reference"]
    #[doc = r" to the mutable bytes array of fixed length."]
    #[doc = r""]
    #[doc = r" # Note"]
    #[doc = r""]
    #[doc = r" The given bytes are interpreted in big endian order."]
    #[inline]
    fn from(bytes: &'a mut [u8; 32]) -> Self {
        H256(*bytes)
    }
}
impl From<H256> for [u8; 32] {
    #[inline]
    fn from(s: H256) -> Self {
        s.0
    }
}
impl AsRef<[u8]> for H256 {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}
impl AsMut<[u8]> for H256 {
    #[inline]
    fn as_mut(&mut self) -> &mut [u8] {
        self.as_bytes_mut()
    }
}
impl H256 {
    #[doc = r" Returns a new fixed hash where all bits are set to the given byte."]
    #[inline]
    pub const fn repeat_byte(byte: u8) -> H256 {
        H256([byte; 32])
    }
    #[doc = r" Returns a new zero-initialized fixed hash."]
    #[inline]
    pub const fn zero() -> H256 {
        H256::repeat_byte(0u8)
    }
    #[doc = r" Returns the size of this hash in bytes."]
    #[inline]
    pub const fn len_bytes() -> usize {
        32
    }
    #[doc = r" Extracts a byte slice containing the entire fixed hash."]
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
    #[doc = r" Extracts a mutable byte slice containing the entire fixed hash."]
    #[inline]
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        &mut self.0
    }
    #[doc = r" Extracts a reference to the byte array containing the entire fixed hash."]
    #[inline]
    pub const fn as_fixed_bytes(&self) -> &[u8; 32] {
        &self.0
    }
    #[doc = r" Extracts a reference to the byte array containing the entire fixed hash."]
    #[inline]
    pub fn as_fixed_bytes_mut(&mut self) -> &mut [u8; 32] {
        &mut self.0
    }
    #[doc = r" Returns the inner bytes array."]
    #[inline]
    pub const fn to_fixed_bytes(self) -> [u8; 32] {
        self.0
    }
    #[doc = r" Returns a constant raw pointer to the value."]
    #[inline]
    pub fn as_ptr(&self) -> *const u8 {
        self.as_bytes().as_ptr()
    }
    #[doc = r" Returns a mutable raw pointer to the value."]
    #[inline]
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.as_bytes_mut().as_mut_ptr()
    }
    #[doc = r" Assign the bytes from the byte slice `src` to `self`."]
    #[doc = r""]
    #[doc = r" # Note"]
    #[doc = r""]
    #[doc = r" The given bytes are interpreted in big endian order."]
    #[doc = r""]
    #[doc = r" # Panics"]
    #[doc = r""]
    #[doc = r" If the length of `src` and the number of bytes in `self` do not match."]
    pub fn assign_from_slice(&mut self, src: &[u8]) {
        assert_eq!(src.len(), 32);
        self.as_bytes_mut().copy_from_slice(src);
    }
    #[doc = r" Create a new fixed-hash from the given slice `src`."]
    #[doc = r""]
    #[doc = r" # Note"]
    #[doc = r""]
    #[doc = r" The given bytes are interpreted in big endian order."]
    #[doc = r""]
    #[doc = r" # Panics"]
    #[doc = r""]
    #[doc = r" If the length of `src` and the number of bytes in `Self` do not match."]
    pub fn from_slice(src: &[u8]) -> Self {
        assert_eq!(src.len(), 32);
        let mut ret = Self::zero();
        ret.assign_from_slice(src);
        ret
    }
    #[doc = r" Returns `true` if all bits set in `b` are also set in `self`."]
    #[inline]
    pub fn covers(&self, b: &Self) -> bool {
        &(b & self) == b
    }
    #[doc = r" Returns `true` if no bits are set."]
    #[inline]
    pub fn is_zero(&self) -> bool {
        self.as_bytes().iter().all(|&byte| byte == 0u8)
    }
}
impl fixed_hash::core_::fmt::Debug for H256 {
    fn fmt(&self, f: &mut fixed_hash::core_::fmt::Formatter) -> fixed_hash::core_::fmt::Result {
        // f.write_fmt(core::format_args!("{:#x}", self))
        write!(f, "{}", &&self.0.to_base58())
    }
}
impl fixed_hash::core_::fmt::Display for H256 {
    fn fmt(&self, f: &mut fixed_hash::core_::fmt::Formatter) -> fixed_hash::core_::fmt::Result {
        write!(f, "{}", &self.0[0..4].to_base58())?;
        f.write_fmt(core::format_args!("…"))?;
        write!(f, "{}", &self.0[32 - 4..32].to_base58())?;
        Ok(())
    }
}
impl fixed_hash::core_::fmt::LowerHex for H256 {
    fn fmt(&self, f: &mut fixed_hash::core_::fmt::Formatter) -> fixed_hash::core_::fmt::Result {
        if f.alternate() {
            f.write_fmt(core::format_args!("0x"))?;
        }
        for i in &self.0[..] {
            f.write_fmt(core::format_args!("{:02x}", i))?;
        }
        Ok(())
    }
}
impl fixed_hash::core_::fmt::UpperHex for H256 {
    fn fmt(&self, f: &mut fixed_hash::core_::fmt::Formatter) -> fixed_hash::core_::fmt::Result {
        if f.alternate() {
            f.write_fmt(core::format_args!("0X"))?;
        }
        for i in &self.0[..] {
            f.write_fmt(core::format_args!("{:02X}", i))?;
        }
        Ok(())
    }
}
impl fixed_hash::core_::marker::Copy for H256 {}

impl fixed_hash::core_::clone::Clone for H256 {
    fn clone(&self) -> H256 {
        let mut ret = H256::zero();
        ret.0.copy_from_slice(&self.0);
        ret
    }
}
impl fixed_hash::core_::cmp::Eq for H256 {}

impl fixed_hash::core_::cmp::PartialOrd for H256 {
    fn partial_cmp(&self, other: &Self) -> Option<fixed_hash::core_::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl fixed_hash::core_::hash::Hash for H256 {
    fn hash<H>(&self, state: &mut H)
    where
        H: fixed_hash::core_::hash::Hasher,
    {
        state.write(&self.0);
    }
}
impl<I> fixed_hash::core_::ops::Index<I> for H256
where
    I: fixed_hash::core_::slice::SliceIndex<[u8]>,
{
    type Output = I::Output;
    #[inline]
    fn index(&self, index: I) -> &I::Output {
        &self.as_bytes()[index]
    }
}
impl<I> fixed_hash::core_::ops::IndexMut<I> for H256
where
    I: fixed_hash::core_::slice::SliceIndex<[u8], Output = [u8]>,
{
    #[inline]
    fn index_mut(&mut self, index: I) -> &mut I::Output {
        &mut self.as_bytes_mut()[index]
    }
}
impl fixed_hash::core_::default::Default for H256 {
    #[inline]
    fn default() -> Self {
        Self::zero()
    }
}
impl<'r> fixed_hash::core_::ops::BitOrAssign<&'r H256> for H256 {
    fn bitor_assign(&mut self, rhs: &'r H256) {
        for (lhs, rhs) in self.as_bytes_mut().iter_mut().zip(rhs.as_bytes()) {
            *lhs |= rhs;
        }
    }
}
impl fixed_hash::core_::ops::BitOrAssign<H256> for H256 {
    #[inline]
    fn bitor_assign(&mut self, rhs: H256) {
        *self |= &rhs;
    }
}
impl<'l, 'r> fixed_hash::core_::ops::BitOr<&'r H256> for &'l H256 {
    type Output = H256;
    fn bitor(self, rhs: &'r H256) -> Self::Output {
        let mut ret = self.clone();
        ret |= rhs;
        ret
    }
}
impl fixed_hash::core_::ops::BitOr<H256> for H256 {
    type Output = H256;
    #[inline]
    fn bitor(self, rhs: Self) -> Self::Output {
        &self | &rhs
    }
}
impl<'r> fixed_hash::core_::ops::BitAndAssign<&'r H256> for H256 {
    fn bitand_assign(&mut self, rhs: &'r H256) {
        for (lhs, rhs) in self.as_bytes_mut().iter_mut().zip(rhs.as_bytes()) {
            *lhs &= rhs;
        }
    }
}
impl fixed_hash::core_::ops::BitAndAssign<H256> for H256 {
    #[inline]
    fn bitand_assign(&mut self, rhs: H256) {
        *self &= &rhs;
    }
}
impl<'l, 'r> fixed_hash::core_::ops::BitAnd<&'r H256> for &'l H256 {
    type Output = H256;
    fn bitand(self, rhs: &'r H256) -> Self::Output {
        let mut ret = self.clone();
        ret &= rhs;
        ret
    }
}
impl fixed_hash::core_::ops::BitAnd<H256> for H256 {
    type Output = H256;
    #[inline]
    fn bitand(self, rhs: Self) -> Self::Output {
        &self & &rhs
    }
}
impl<'r> fixed_hash::core_::ops::BitXorAssign<&'r H256> for H256 {
    fn bitxor_assign(&mut self, rhs: &'r H256) {
        for (lhs, rhs) in self.as_bytes_mut().iter_mut().zip(rhs.as_bytes()) {
            *lhs ^= rhs;
        }
    }
}
impl fixed_hash::core_::ops::BitXorAssign<H256> for H256 {
    #[inline]
    fn bitxor_assign(&mut self, rhs: H256) {
        *self ^= &rhs;
    }
}
impl<'l, 'r> fixed_hash::core_::ops::BitXor<&'r H256> for &'l H256 {
    type Output = H256;
    fn bitxor(self, rhs: &'r H256) -> Self::Output {
        let mut ret = self.clone();
        ret ^= rhs;
        ret
    }
}
impl fixed_hash::core_::ops::BitXor<H256> for H256 {
    type Output = H256;
    #[inline]
    fn bitxor(self, rhs: Self) -> Self::Output {
        &self ^ &rhs
    }
}
#[doc = r" Utilities using the `byteorder` crate."]
impl H256 {
    #[doc = r" Returns the least significant `n` bytes as slice."]
    #[doc = r""]
    #[doc = r" # Panics"]
    #[doc = r""]
    #[doc = r" If `n` is greater than the number of bytes in `self`."]
    #[inline]
    fn least_significant_bytes(&self, n: usize) -> &[u8] {
        assert_eq!(true, n <= Self::len_bytes());
        &self[(Self::len_bytes() - n)..]
    }
    fn to_low_u64_with_byteorder<B>(&self) -> u64
    where
        B: fixed_hash::byteorder::ByteOrder,
    {
        let mut buf = [0x0; 8];
        let capped = fixed_hash::core_::cmp::min(Self::len_bytes(), 8);
        buf[(8 - capped)..].copy_from_slice(self.least_significant_bytes(capped));
        B::read_u64(&buf)
    }
    #[doc = r" Returns the lowest 8 bytes interpreted as big-endian."]
    #[doc = r""]
    #[doc = r" # Note"]
    #[doc = r""]
    #[doc = r" For hash type with less than 8 bytes the missing bytes"]
    #[doc = r" are interpreted as being zero."]
    #[inline]
    pub fn to_low_u64_be(&self) -> u64 {
        self.to_low_u64_with_byteorder::<fixed_hash::byteorder::BigEndian>()
    }
    #[doc = r" Returns the lowest 8 bytes interpreted as little-endian."]
    #[doc = r""]
    #[doc = r" # Note"]
    #[doc = r""]
    #[doc = r" For hash type with less than 8 bytes the missing bytes"]
    #[doc = r" are interpreted as being zero."]
    #[inline]
    pub fn to_low_u64_le(&self) -> u64 {
        self.to_low_u64_with_byteorder::<fixed_hash::byteorder::LittleEndian>()
    }
    #[doc = r" Returns the lowest 8 bytes interpreted as native-endian."]
    #[doc = r""]
    #[doc = r" # Note"]
    #[doc = r""]
    #[doc = r" For hash type with less than 8 bytes the missing bytes"]
    #[doc = r" are interpreted as being zero."]
    #[inline]
    pub fn to_low_u64_ne(&self) -> u64 {
        self.to_low_u64_with_byteorder::<fixed_hash::byteorder::NativeEndian>()
    }
    fn from_low_u64_with_byteorder<B>(val: u64) -> Self
    where
        B: fixed_hash::byteorder::ByteOrder,
    {
        let mut buf = [0x0; 8];
        B::write_u64(&mut buf, val);
        let capped = fixed_hash::core_::cmp::min(Self::len_bytes(), 8);
        let mut bytes = [0x0; fixed_hash::core_::mem::size_of::<Self>()];
        bytes[(Self::len_bytes() - capped)..].copy_from_slice(&buf[..capped]);
        Self::from_slice(&bytes)
    }
    #[doc = r" Creates a new hash type from the given `u64` value."]
    #[doc = r""]
    #[doc = r" # Note"]
    #[doc = r""]
    #[doc = r" - The given `u64` value is interpreted as big endian."]
    #[doc = r" - Ignores the most significant bits of the given value"]
    #[doc = r"   if the hash type has less than 8 bytes."]
    #[inline]
    pub fn from_low_u64_be(val: u64) -> Self {
        Self::from_low_u64_with_byteorder::<fixed_hash::byteorder::BigEndian>(val)
    }
    #[doc = r" Creates a new hash type from the given `u64` value."]
    #[doc = r""]
    #[doc = r" # Note"]
    #[doc = r""]
    #[doc = r" - The given `u64` value is interpreted as little endian."]
    #[doc = r" - Ignores the most significant bits of the given value"]
    #[doc = r"   if the hash type has less than 8 bytes."]
    #[inline]
    pub fn from_low_u64_le(val: u64) -> Self {
        Self::from_low_u64_with_byteorder::<fixed_hash::byteorder::LittleEndian>(val)
    }
    #[doc = r" Creates a new hash type from the given `u64` value."]
    #[doc = r""]
    #[doc = r" # Note"]
    #[doc = r""]
    #[doc = r" - The given `u64` value is interpreted as native endian."]
    #[doc = r" - Ignores the most significant bits of the given value"]
    #[doc = r"   if the hash type has less than 8 bytes."]
    #[inline]
    pub fn from_low_u64_ne(val: u64) -> Self {
        Self::from_low_u64_with_byteorder::<fixed_hash::byteorder::NativeEndian>(val)
    }
}
impl fixed_hash::rand::distributions::Distribution<H256>
    for fixed_hash::rand::distributions::Standard
{
    fn sample<R: fixed_hash::rand::Rng + ?Sized>(&self, rng: &mut R) -> H256 {
        let mut ret = H256::zero();
        for byte in ret.as_bytes_mut().iter_mut() {
            *byte = rng.gen();
        }
        ret
    }
}
#[doc = r" Utilities using the `rand` crate."]
impl H256 {
    #[doc = r" Assign `self` to a cryptographically random value using the"]
    #[doc = r" given random number generator."]
    pub fn randomize_using<R>(&mut self, rng: &mut R)
    where
        R: fixed_hash::rand::Rng + ?Sized,
    {
        use fixed_hash::rand::distributions::Distribution;
        *self = fixed_hash::rand::distributions::Standard.sample(rng);
    }
    #[doc = r" Assign `self` to a cryptographically random value."]
    pub fn randomize(&mut self) {
        let mut rng = fixed_hash::rand::rngs::OsRng;
        self.randomize_using(&mut rng);
    }
    #[doc = r" Create a new hash with cryptographically random content using the"]
    #[doc = r" given random number generator."]
    pub fn random_using<R>(rng: &mut R) -> Self
    where
        R: fixed_hash::rand::Rng + ?Sized,
    {
        let mut ret = Self::zero();
        ret.randomize_using(rng);
        ret
    }
    #[doc = r" Create a new hash with cryptographically random content."]
    pub fn random() -> Self {
        let mut hash = Self::zero();
        hash.randomize();
        hash
    }
}
impl fixed_hash::core_::cmp::PartialEq for H256 {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}
impl fixed_hash::core_::cmp::Ord for H256 {
    #[inline]
    fn cmp(&self, other: &Self) -> fixed_hash::core_::cmp::Ordering {
        self.as_bytes().cmp(other.as_bytes())
    }
}
impl fixed_hash::core_::str::FromStr for H256 {
    type Err = fixed_hash::rustc_hex::FromHexError;
    #[doc = r" Creates a hash type instance from the given string."]
    #[doc = r""]
    #[doc = r" # Note"]
    #[doc = r""]
    #[doc = r" The given input string is interpreted in big endian."]
    #[doc = r""]
    #[doc = r" # Errors"]
    #[doc = r""]
    #[doc = r" - When encountering invalid non hex-digits"]
    #[doc = r" - Upon empty string input or invalid input length in general"]
    fn from_str(
        input: &str,
    ) -> fixed_hash::core_::result::Result<H256, fixed_hash::rustc_hex::FromHexError> {
        let input = input.strip_prefix("0x").unwrap_or(input);
        let mut iter = fixed_hash::rustc_hex::FromHexIter::new(input);
        let mut result = Self::zero();
        for byte in result.as_mut() {
            *byte = iter.next().ok_or(Self::Err::InvalidHexLength)??;
        }
        if iter.next().is_some() {
            return Err(Self::Err::InvalidHexLength);
        }
        Ok(result)
    }
}
