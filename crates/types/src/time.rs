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
    /// Zero timestamp (Unix epoch: 1970-01-01 00:00:00 UTC)
    pub const ZERO: Self = Self(0);

    /// Maximum representable timestamp
    pub const MAX: Self = Self(u64::MAX);

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

    /// Creates a UnixTimestamp from the current system time
    ///
    /// # Panics
    ///
    /// Panics if the system time is before the Unix epoch.
    #[inline]
    pub fn now_or_panic() -> Self {
        Self::now().expect("System time before Unix epoch")
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

    ///
    /// Returns None if overflow occurs
    #[inline]
    pub const fn checked_add_secs(self, secs: u64) -> Option<Self> {
        match self.0.checked_add(secs) {
            Some(val) => Some(Self(val)),
            None => None,
        }
    }

    /// Saturating addition with duration in seconds
    #[inline]
    pub const fn saturating_add_secs(self, secs: u64) -> Self {
        Self(self.0.saturating_add(secs))
    }

    /// Checked subtraction of duration in seconds
    ///
    /// Returns None if underflow occurs
    #[inline]
    pub const fn checked_sub_secs(self, secs: u64) -> Option<Self> {
        match self.0.checked_sub(secs) {
            Some(val) => Some(Self(val)),
            None => None,
        }
    }

    /// Saturating subtraction of duration in seconds
    #[inline]
    pub const fn saturating_sub_secs(self, secs: u64) -> Self {
        Self(self.0.saturating_sub(secs))
    }

    /// Returns the number of seconds between two timestamps
    ///
    /// Returns None if self < other (negative duration)
    #[inline]
    pub const fn seconds_since(self, other: Self) -> Option<u64> {
        self.0.checked_sub(other.0)
    }

    /// Returns the duration between two timestamps
    ///
    /// Returns None if self < other (negative duration)
    #[inline]
    pub fn duration_since(self, other: Self) -> Option<std::time::Duration> {
        self.seconds_since(other)
            .map(std::time::Duration::from_secs)
    }

    /// Returns the saturated number of seconds between two timestamps
    #[inline]
    pub const fn saturating_seconds_since(self, other: Self) -> u64 {
        self.0.saturating_sub(other.0)
    }

    /// Returns the saturated duration between two timestamps
    #[inline]
    pub fn saturating_duration_since(self, other: Self) -> std::time::Duration {
        std::time::Duration::from_secs(self.saturating_seconds_since(other))
    }

    /// Checks if this timestamp has passed (is before current time)
    ///
    /// Returns `false` if unable to determine current time
    #[inline]
    pub fn has_passed(self) -> bool {
        Self::now().is_ok_and(|now| self < now)
    }

    /// Returns true if this is the zero timestamp
    #[inline]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
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
    fn test_zero_timestamp() {
        assert_eq!(UnixTimestamp::ZERO.as_secs(), 0);
        assert!(UnixTimestamp::ZERO.is_zero());
    }

    #[test]
    fn test_from_secs() {
        let ts = UnixTimestamp::from_secs(1609459200);
        assert_eq!(ts.as_secs(), 1609459200);
    }

    #[test]
    fn test_now() {
        let ts = UnixTimestamp::now().expect("Failed to get current time");
        assert!(ts.as_secs() > 0);
        assert!(!ts.is_zero());
    }

    #[test]
    fn test_now_or_panic() {
        let ts = UnixTimestamp::now_or_panic();
        assert!(ts.as_secs() > 0);
        assert!(!ts.is_zero());
    }

    #[test]
    fn test_arithmetic() {
        let ts = UnixTimestamp::from_secs(100);

        assert_eq!(ts.checked_add_secs(50), Some(UnixTimestamp::from_secs(150)));
        assert_eq!(ts.saturating_add_secs(50), UnixTimestamp::from_secs(150));

        assert_eq!(ts.checked_sub_secs(30), Some(UnixTimestamp::from_secs(70)));
        assert_eq!(ts.saturating_sub_secs(30), UnixTimestamp::from_secs(70));

        assert_eq!(ts.checked_sub_secs(200), None);
        assert_eq!(ts.saturating_sub_secs(200), UnixTimestamp::ZERO);
    }

    #[rstest]
    #[case(UnixTimestamp::MAX, 1, None)]
    #[case(UnixTimestamp::MAX, u64::MAX, None)]
    #[case(UnixTimestamp::from_secs(u64::MAX - 1), 1, Some(UnixTimestamp::MAX))]
    #[case(UnixTimestamp::from_secs(u64::MAX - 1), 2, None)]
    #[case(UnixTimestamp::ZERO, 0, Some(UnixTimestamp::ZERO))]
    #[case(UnixTimestamp::ZERO, 1, Some(UnixTimestamp::from_secs(1)))]
    #[case(UnixTimestamp::from_secs(100), 50, Some(UnixTimestamp::from_secs(150)))]
    fn test_checked_add_edge_cases(
        #[case] base: UnixTimestamp,
        #[case] secs: u64,
        #[case] expected: Option<UnixTimestamp>,
    ) {
        assert_eq!(base.checked_add_secs(secs), expected);
    }

    #[rstest]
    #[case(UnixTimestamp::MAX, 1, UnixTimestamp::MAX)]
    #[case(UnixTimestamp::MAX, u64::MAX, UnixTimestamp::MAX)]
    #[case(UnixTimestamp::from_secs(u64::MAX - 1), 10, UnixTimestamp::MAX)]
    #[case(UnixTimestamp::from_secs(100), 50, UnixTimestamp::from_secs(150))]
    fn test_saturating_add_edge_cases(
        #[case] base: UnixTimestamp,
        #[case] secs: u64,
        #[case] expected: UnixTimestamp,
    ) {
        assert_eq!(base.saturating_add_secs(secs), expected);
    }

    #[rstest]
    #[case(UnixTimestamp::ZERO, 1, None)]
    #[case(UnixTimestamp::ZERO, u64::MAX, None)]
    #[case(UnixTimestamp::from_secs(100), 101, None)]
    #[case(UnixTimestamp::from_secs(100), 100, Some(UnixTimestamp::ZERO))]
    #[case(UnixTimestamp::from_secs(100), 50, Some(UnixTimestamp::from_secs(50)))]
    fn test_checked_sub_edge_cases(
        #[case] base: UnixTimestamp,
        #[case] secs: u64,
        #[case] expected: Option<UnixTimestamp>,
    ) {
        assert_eq!(base.checked_sub_secs(secs), expected);
    }

    #[rstest]
    #[case(UnixTimestamp::ZERO, 1, UnixTimestamp::ZERO)]
    #[case(UnixTimestamp::ZERO, u64::MAX, UnixTimestamp::ZERO)]
    #[case(UnixTimestamp::from_secs(50), 100, UnixTimestamp::ZERO)]
    #[case(UnixTimestamp::from_secs(100), 50, UnixTimestamp::from_secs(50))]
    fn test_saturating_sub_edge_cases(
        #[case] base: UnixTimestamp,
        #[case] secs: u64,
        #[case] expected: UnixTimestamp,
    ) {
        assert_eq!(base.saturating_sub_secs(secs), expected);
    }

    #[test]
    fn test_seconds_since() {
        let ts1 = UnixTimestamp::from_secs(100);
        let ts2 = UnixTimestamp::from_secs(50);

        assert_eq!(ts1.seconds_since(ts2), Some(50));
        assert_eq!(ts2.seconds_since(ts1), None);
        assert_eq!(ts1.saturating_seconds_since(ts2), 50);
        assert_eq!(ts2.saturating_seconds_since(ts1), 0);
    }

    #[test]
    fn test_duration_since() {
        let ts1 = UnixTimestamp::from_secs(100);
        let ts2 = UnixTimestamp::from_secs(50);

        assert_eq!(
            ts1.duration_since(ts2),
            Some(std::time::Duration::from_secs(50))
        );

        assert_eq!(ts2.duration_since(ts1), None);

        assert_eq!(
            ts1.saturating_duration_since(ts2),
            std::time::Duration::from_secs(50)
        );
        assert_eq!(
            ts2.saturating_duration_since(ts1),
            std::time::Duration::from_secs(0)
        );
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
                assert_eq!(result.unwrap(), UnixTimestamp::ZERO);
            } else if input == [0xFF; 8] {
                assert_eq!(result.unwrap(), UnixTimestamp::MAX);
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
        assert_eq!(ts, UnixTimestamp::ZERO);
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

        /// Property: checked_add and checked_sub are inverse operations
        #[test]
        fn add_sub_inverse(secs in 0_u64..u64::MAX/2, duration in 0_u64..u64::MAX/2) {
            let ts = UnixTimestamp::from_secs(secs);

            if let Some(added) = ts.checked_add_secs(duration) {
                let subtracted = added.checked_sub_secs(duration);
                prop_assert_eq!(subtracted, Some(ts));
            }
        }

        /// Property: saturating_add always succeeds and is >= original
        #[test]
        fn saturating_add_monotonic(secs in any::<u64>(), duration in any::<u64>()) {
            let ts = UnixTimestamp::from_secs(secs);
            let result = ts.saturating_add_secs(duration);

            // Result should be >= original (monotonic)
            prop_assert!(result >= ts);

            // If no overflow, should equal checked version
            if let Some(checked) = ts.checked_add_secs(duration) {
                prop_assert_eq!(result, checked);
            } else {
                // On overflow, should saturate to MAX
                prop_assert_eq!(result, UnixTimestamp::MAX);
            }
        }

        /// Property: saturating_sub always succeeds and is <= original
        #[test]
        fn saturating_sub_monotonic(secs in any::<u64>(), duration in any::<u64>()) {
            let ts = UnixTimestamp::from_secs(secs);
            let result = ts.saturating_sub_secs(duration);

            // Result should be <= original (monotonic)
            prop_assert!(result <= ts);

            // If no underflow, should equal checked version
            if let Some(checked) = ts.checked_sub_secs(duration) {
                prop_assert_eq!(result, checked);
            } else {
                // On underflow, should saturate to ZERO
                prop_assert_eq!(result, UnixTimestamp::ZERO);
            }
        }

        /// Property: Ordering is consistent with underlying u64
        #[test]
        fn ordering_consistent_with_u64(a in any::<u64>(), b in any::<u64>()) {
            let ts_a = UnixTimestamp::from_secs(a);
            let ts_b = UnixTimestamp::from_secs(b);

            prop_assert_eq!(ts_a < ts_b, a < b);
            prop_assert_eq!(ts_a == ts_b, a == b);
            prop_assert_eq!(ts_a > ts_b, a > b);
            prop_assert_eq!(ts_a <= ts_b, a <= b);
            prop_assert_eq!(ts_a >= ts_b, a >= b);
        }

        /// Property: seconds_since is consistent with subtraction
        #[test]
        fn seconds_since_consistent(a in any::<u64>(), b in any::<u64>()) {
            let ts_a = UnixTimestamp::from_secs(a);
            let ts_b = UnixTimestamp::from_secs(b);

            prop_assert_eq!(ts_a.seconds_since(ts_b), a.checked_sub(b));
            prop_assert_eq!(ts_a.saturating_seconds_since(ts_b), a.saturating_sub(b));
        }

        /// Property: duration_since is consistent with seconds_since
        #[test]
        fn duration_since_consistent(a in any::<u64>(), b in any::<u64>()) {
            let ts_a = UnixTimestamp::from_secs(a);
            let ts_b = UnixTimestamp::from_secs(b);

            let expected_duration = ts_a.seconds_since(ts_b).map(std::time::Duration::from_secs);
            prop_assert_eq!(ts_a.duration_since(ts_b), expected_duration);

            let expected_saturating = std::time::Duration::from_secs(ts_a.saturating_seconds_since(ts_b));
            prop_assert_eq!(ts_a.saturating_duration_since(ts_b), expected_saturating);
        }
    }
}
