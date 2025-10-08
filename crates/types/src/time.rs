use arbitrary::Arbitrary;
use bytes::Buf as _;
use reth_codecs::Compact;
use reth_db::table::{Decode, Encode};
use reth_db::DatabaseError;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::SystemTime;

/// Zero-cost newtype wrapper for Unix timestamps in seconds
///
/// Represents seconds since the Unix epoch (1970-01-01 00:00:00 UTC).
/// Uses u64 internally for maximum range
///
/// # Examples
///
/// ```
/// use irys_types::UnixTimestamp;
///
/// // Get current timestamp
/// let now = UnixTimestamp::now().unwrap();
///
/// // Create from seconds
/// let ts = UnixTimestamp::from_secs(1609459200); // 2021-01-01 00:00:00 UTC
///
/// // Convert to seconds
/// let secs: u64 = ts.as_secs();
/// ```
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    Serialize,
    Deserialize,
    Arbitrary,
)]
#[repr(transparent)]
pub struct UnixTimestamp(u64);

impl UnixTimestamp {
    /// Creates a UnixTimestamp from the current system time
    ///
    /// # Security Considerations
    ///
    /// System clock can be manipulated by privileged users, NTP, or manual changes.
    ///
    /// **Safe for**:
    /// - Cache eviction
    /// - Logging timestamps
    /// - Non-security-critical time tracking
    ///
    /// For distributed systems, ensure time synchronization (NTP) for consistency.
    ///
    /// # Errors
    ///
    /// Returns an error if the system time is before the Unix epoch.
    #[inline]
    pub fn now() -> Result<Self, std::time::SystemTimeError> {
        Ok(Self(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)?
                .as_secs(),
        ))
    }

    /// Creates a UnixTimestamp from seconds since Unix epoch
    #[inline]
    pub const fn from_secs(secs: u64) -> Self {
        Self(secs)
    }

    /// Returns the number of seconds since Unix epoch
    #[inline]
    pub const fn as_secs(self) -> u64 {
        self.0
    }

    /// Saturating subtraction of duration in seconds
    #[inline]
    pub const fn saturating_sub_secs(self, secs: u64) -> Self {
        Self(self.0.saturating_sub(secs))
    }

    /// Returns the saturated number of seconds between two timestamps
    #[inline]
    pub const fn saturating_seconds_since(self, other: Self) -> u64 {
        self.0.saturating_sub(other.0)
    }
}

// Display as Unix timestamp in seconds
impl fmt::Display for UnixTimestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// Allow conversion from u64
impl From<u64> for UnixTimestamp {
    #[inline]
    fn from(secs: u64) -> Self {
        Self(secs)
    }
}

// Allow conversion to u64
impl From<UnixTimestamp> for u64 {
    #[inline]
    fn from(ts: UnixTimestamp) -> Self {
        ts.0
    }
}

// Database encoding (reth_db compatibility)
// Uses big-endian (network byte order) for cross-platform compatibility
impl Encode for UnixTimestamp {
    type Encoded = [u8; 8];

    /// Encodes as big-endian bytes for database portability
    ///
    #[inline]
    fn encode(self) -> Self::Encoded {
        self.0.to_be_bytes()
    }
}

impl Decode for UnixTimestamp {
    /// Decodes from big-endian bytes
    #[inline]
    fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
        if value.len() != 8 {
            return Err(DatabaseError::Decode);
        }
        let bytes: [u8; 8] = value.try_into().map_err(|_| DatabaseError::Decode)?;
        Ok(Self(u64::from_be_bytes(bytes)))
    }
}

// Compact encoding for database storage (reth_codecs compatibility)
impl Compact for UnixTimestamp {
    /// Compact encoding using big-endian for consistency with `Encode` trait
    ///
    /// Note: `buf.put_u64()` from bytes crate uses big-endian by default,
    /// maintaining compatibility with `Encode::encode()`
    #[inline]
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: bytes::BufMut + AsMut<[u8]>,
    {
        buf.put_u64(self.0);
        8
    }

    #[inline]
    fn from_compact(mut buf: &[u8], _len: usize) -> (Self, &[u8]) {
        let val = buf.get_u64();
        (Self(val), buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[test]
    fn test_from_secs() {
        let ts = UnixTimestamp::from_secs(1609459200);
        assert_eq!(ts.as_secs(), 1609459200);
    }

    #[test]
    fn test_now() {
        let ts = UnixTimestamp::now().expect("Failed to get current time");
        assert!(ts.as_secs() > 0);
    }

    #[test]
    fn test_saturating_sub() {
        let ts = UnixTimestamp::from_secs(100);
        assert_eq!(ts.saturating_sub_secs(30), UnixTimestamp::from_secs(70));
        assert_eq!(ts.saturating_sub_secs(200), UnixTimestamp::from_secs(0));
    }

    #[test]
    fn test_saturating_seconds_since() {
        let ts1 = UnixTimestamp::from_secs(100);
        let ts2 = UnixTimestamp::from_secs(50);

        assert_eq!(ts1.saturating_seconds_since(ts2), 50);
        assert_eq!(ts2.saturating_seconds_since(ts1), 0);
    }

    #[test]
    fn test_comparisons() {
        let ts1 = UnixTimestamp::from_secs(100);
        let ts2 = UnixTimestamp::from_secs(200);

        assert!(ts1 < ts2);
        assert!(ts2 > ts1);
        assert!(ts1 <= ts2);
        assert!(ts2 >= ts1);
    }

    #[test]
    fn test_encode_decode() {
        let ts = UnixTimestamp::from_secs(1609459200);
        let encoded = ts.encode();
        let decoded = UnixTimestamp::decode(&encoded).unwrap();
        assert_eq!(ts, decoded);
    }

    #[test]
    fn test_decode_exact_8_bytes_required() {
        // Ensure exactly 8 bytes are required (documented contract)
        assert!(UnixTimestamp::decode(&[0; 7]).is_err());
        assert!(UnixTimestamp::decode(&[0; 9]).is_err());
        assert!(UnixTimestamp::decode(&[0; 8]).is_ok());
    }

    #[test]
    fn test_big_endian_encoding_format() {
        // Critical: Verify big-endian format (cross-platform compatibility)
        let ts = UnixTimestamp::from_secs(0x0102030405060708);
        let encoded = ts.encode();

        // Big-endian: most significant byte first
        assert_eq!(encoded, [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]);

        // Verify round-trip preserves big-endian
        let decoded = UnixTimestamp::decode(&encoded).unwrap();
        assert_eq!(decoded, ts);
    }

    #[rstest]
    #[case(&[], true)]
    #[case(&[0], true)]
    #[case(&[0, 1, 2, 3, 4, 5, 6], true)]
    #[case(&[0, 1, 2, 3, 4, 5, 6, 7, 8], true)]
    #[case(&[0; 7], true)]
    #[case(&[0; 9], true)]
    #[case(&[0; 8], false)]
    #[case(&[0xFF; 8], false)]
    fn test_decode_error_handling(#[case] input: &[u8], #[case] should_error: bool) {
        let result = UnixTimestamp::decode(input);
        assert_eq!(result.is_err(), should_error);

        if !should_error {
            // Verify the decoded values are correct
            if input == [0; 8] {
                assert_eq!(result.unwrap(), UnixTimestamp::from_secs(0));
            } else if input == [0xFF; 8] {
                assert_eq!(result.unwrap(), UnixTimestamp::from_secs(u64::MAX));
            }
        }
    }

    #[test]
    fn test_compact_roundtrip() {
        use bytes::BytesMut;

        let original = UnixTimestamp::from_secs(1609459200);
        let mut buf = BytesMut::with_capacity(8);
        let len = original.to_compact(&mut buf);
        assert_eq!(len, 8);

        let (decoded, remaining) = UnixTimestamp::from_compact(&buf[..], len);
        assert_eq!(decoded, original);
        assert_eq!(remaining.len(), 0);
    }

    #[test]
    fn test_conversions() {
        let ts = UnixTimestamp::from_secs(12345);

        let from_u64: UnixTimestamp = 12345_u64.into();
        assert_eq!(ts, from_u64);

        let to_u64: u64 = ts.into();
        assert_eq!(to_u64, 12345);
    }

    #[test]
    fn test_display() {
        let ts = UnixTimestamp::from_secs(1609459200);
        assert_eq!(format!("{}", ts), "1609459200");
    }

    #[test]
    fn test_ordering() {
        let ts1 = UnixTimestamp::from_secs(100);
        let ts2 = UnixTimestamp::from_secs(200);
        let ts3 = UnixTimestamp::from_secs(200);

        assert!(ts1 < ts2);
        assert!(ts2 > ts1);
        assert_eq!(ts2, ts3);
    }

    #[test]
    fn test_default() {
        let ts: UnixTimestamp = Default::default();
        assert_eq!(ts, UnixTimestamp::from_secs(0));
    }
}

#[cfg(test)]
mod property_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// Property: Database encoding roundtrip preserves all values
        ///
        /// Note: This is the actual storage format, must never fail.
        #[test]
        fn encode_decode_roundtrip(secs in any::<u64>()) {
            let original = UnixTimestamp::from_secs(secs);
            let encoded = original.encode();
            let decoded = UnixTimestamp::decode(&encoded).unwrap();
            prop_assert_eq!(original, decoded);
        }

        /// Property: Compact encoding roundtrip preserves all values
        ///
        /// Tests the reth_codecs Compact trait implementation.
        #[test]
        fn compact_roundtrip(secs in any::<u64>()) {
            use bytes::BytesMut;

            let original = UnixTimestamp::from_secs(secs);
            let mut buf = BytesMut::with_capacity(8);
            let len = original.to_compact(&mut buf);

            let (decoded, remaining) = UnixTimestamp::from_compact(&buf[..], len);
            prop_assert_eq!(original, decoded);
            prop_assert_eq!(remaining.len(), 0);
            prop_assert_eq!(len, 8);
        }

        /// Property: Serde serialization roundtrip preserves all values
        ///
        /// Tests that any UnixTimestamp can be serialized to JSON
        /// and deserialized back to the exact same value.
        #[test]
        fn serde_json_roundtrip(secs in any::<u64>()) {
            let original = UnixTimestamp::from_secs(secs);

            // JSON roundtrip
            let json = serde_json::to_string(&original).unwrap();
            let from_json: UnixTimestamp = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(original, from_json);
        }

        /// Property: saturating_sub always succeeds and is <= original
        #[test]
        fn saturating_sub_monotonic(secs in any::<u64>(), duration in any::<u64>()) {
            let ts = UnixTimestamp::from_secs(secs);
            let result = ts.saturating_sub_secs(duration);

            // Result should be <= original (monotonic)
            prop_assert!(result <= ts);
        }

        /// Property: saturating_seconds_since is consistent with subtraction
        #[test]
        fn saturating_seconds_since_consistent(a in any::<u64>(), b in any::<u64>()) {
            let ts_a = UnixTimestamp::from_secs(a);
            let ts_b = UnixTimestamp::from_secs(b);

            prop_assert_eq!(ts_a.saturating_seconds_since(ts_b), a.saturating_sub(b));
        }
    }
}
