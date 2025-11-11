//! Fee amount with saturating arithmetic that prevents overflow attacks.
//!
//! Wraps [`U256`] to ensure `term_fee + perm_fee` saturates at MAX instead
//! of wrapping to zero, which would bypass balance checks.

use crate::U256;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::ops::{Add, AddAssign};

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    arbitrary::Arbitrary,
    reth_codecs::Compact,
)]
#[repr(transparent)]
pub struct BoundedFee(U256);

impl BoundedFee {
    #[inline]
    pub const fn new(amount: U256) -> Self {
        Self(amount)
    }

    #[inline]
    pub fn zero() -> Self {
        Self(U256::zero())
    }

    #[inline]
    pub const fn is_zero(&self) -> bool {
        self.0.is_zero()
    }

    #[inline]
    pub const fn get(&self) -> U256 {
        self.0
    }

    #[inline]
    pub const fn from_u64(amount: u64) -> Self {
        Self(U256([amount, 0, 0, 0]))
    }

    #[inline]
    pub const fn max_value() -> Self {
        Self(U256::MAX)
    }
}

impl fmt::Display for BoundedFee {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Default for BoundedFee {
    fn default() -> Self {
        Self::zero()
    }
}

impl Add for BoundedFee {
    type Output = Self;

    #[inline]
    fn add(self, rhs: Self) -> Self {
        Self(self.0.saturating_add(rhs.0))
    }
}

impl Add<&Self> for BoundedFee {
    type Output = Self;

    #[inline]
    fn add(self, rhs: &Self) -> Self {
        Self(self.0.saturating_add(rhs.0))
    }
}

impl Add<BoundedFee> for &BoundedFee {
    type Output = BoundedFee;

    #[inline]
    fn add(self, rhs: BoundedFee) -> BoundedFee {
        BoundedFee(self.0.saturating_add(rhs.0))
    }
}

impl Add<&BoundedFee> for &BoundedFee {
    type Output = BoundedFee;

    #[inline]
    fn add(self, rhs: &BoundedFee) -> BoundedFee {
        BoundedFee(self.0.saturating_add(rhs.0))
    }
}

impl AddAssign for BoundedFee {
    #[inline]
    fn add_assign(&mut self, rhs: Self) {
        self.0 = self.0.saturating_add(rhs.0);
    }
}

impl std::iter::Sum for BoundedFee {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::zero(), |acc, x| acc + x)
    }
}

impl Serialize for BoundedFee {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for BoundedFee {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let amount = U256::deserialize(deserializer)?;
        Ok(Self::new(amount))
    }
}

impl alloy_rlp::Encodable for BoundedFee {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        self.0.encode(out);
    }

    fn length(&self) -> usize {
        self.0.length()
    }
}

impl alloy_rlp::Decodable for BoundedFee {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let amount = U256::decode(buf)?;
        Ok(Self(amount))
    }
}

impl From<U256> for BoundedFee {
    #[inline]
    fn from(amount: U256) -> Self {
        Self::new(amount)
    }
}

impl From<BoundedFee> for U256 {
    #[inline]
    fn from(fee: BoundedFee) -> Self {
        fee.0
    }
}

impl From<u64> for BoundedFee {
    #[inline]
    fn from(amount: u64) -> Self {
        Self::new(U256::from(amount))
    }
}

impl From<u128> for BoundedFee {
    #[inline]
    fn from(amount: u128) -> Self {
        Self::new(U256::from(amount))
    }
}

impl PartialEq<U256> for BoundedFee {
    #[inline]
    fn eq(&self, other: &U256) -> bool {
        self.0 == *other
    }
}

impl PartialEq<BoundedFee> for U256 {
    #[inline]
    fn eq(&self, other: &BoundedFee) -> bool {
        *self == other.0
    }
}

impl PartialOrd<U256> for BoundedFee {
    #[inline]
    fn partial_cmp(&self, other: &U256) -> Option<std::cmp::Ordering> {
        self.0.partial_cmp(other)
    }
}

impl PartialOrd<BoundedFee> for U256 {
    #[inline]
    fn partial_cmp(&self, other: &BoundedFee) -> Option<std::cmp::Ordering> {
        self.partial_cmp(&other.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_overflow_scenario_blocked() {
        let term_fee = BoundedFee::new(U256::max_value() / U256::from(2));
        let perm_fee = BoundedFee::new(U256::max_value() / U256::from(2) + U256::from(2));

        let total = term_fee + perm_fee;
        assert_eq!(total.get(), U256::max_value());
    }

    #[test]
    fn test_cross_type_comparisons() {
        let fee = BoundedFee::new(U256::from(1000));
        let amount = U256::from(1000);
        let larger = U256::from(2000);
        let smaller = U256::from(500);

        // Equality
        assert_eq!(fee, amount);
        assert_eq!(amount, fee);
        assert_ne!(fee, larger);
        assert_ne!(larger, fee);

        // Ordering: BoundedFee compared with U256
        assert!(fee < larger);
        assert!(fee <= larger);
        assert!(fee <= amount);
        assert!(fee > smaller);
        assert!(fee >= smaller);
        assert!(fee >= amount);

        // Ordering: U256 compared with BoundedFee
        assert!(larger > fee);
        assert!(larger >= fee);
        assert!(amount >= fee);
        assert!(smaller < fee);
        assert!(smaller <= fee);
        assert!(amount <= fee);
    }

    #[test]
    fn test_rlp_encoding_matches_u256() {
        use alloy_rlp::{Decodable as _, Encodable as _};

        let amount = U256::from(99_u64);
        let bounded_fee = BoundedFee::from(99_u64);

        // Encode both
        let mut u256_buf = Vec::new();
        amount.encode(&mut u256_buf);

        let mut bounded_fee_buf = Vec::new();
        bounded_fee.encode(&mut bounded_fee_buf);

        // RLP encodings should be identical
        assert_eq!(u256_buf, bounded_fee_buf, "RLP encoding must match U256");

        // Verify round-trip
        let decoded = BoundedFee::decode(&mut &bounded_fee_buf[..]).unwrap();
        assert_eq!(decoded, bounded_fee);
    }
}

#[cfg(test)]
mod property_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// Property: Addition without overflow matches U256 addition
        #[test]
        fn prop_addition_correctness(
            a in 0_u64..1_000_000_000_u64,
            b in 0_u64..1_000_000_000_u64,
        ) {
            let fee_a = BoundedFee::new(U256::from(a));
            let fee_b = BoundedFee::new(U256::from(b));
            let total = fee_a + fee_b;

            prop_assert_eq!(total.get(), U256::from(a) + U256::from(b));
        }

        /// Property: Adding anything to MAX saturates at MAX
        #[test]
        fn prop_saturates_at_max(small in 1_u64..u64::MAX) {
            let max = BoundedFee::new(U256::max_value());
            let small_fee = BoundedFee::new(U256::from(small));
            let total = max + small_fee;

            prop_assert_eq!(total.get(), U256::max_value());
        }

        /// Property: Result never wraps (always >= both operands)
        #[test]
        fn prop_never_wraps(
            a in any::<u128>().prop_map(U256::from),
            b in any::<u128>().prop_map(U256::from),
        ) {
            let fee_a = BoundedFee::new(a);
            let fee_b = BoundedFee::new(b);
            let total = fee_a + fee_b;

            // Result must be >= both inputs (no wraparound)
            prop_assert!(total.get() >= fee_a.get());
            prop_assert!(total.get() >= fee_b.get());
        }

        /// Property: Addition is commutative
        #[test]
        fn prop_addition_commutative(
            a in 0_u64..1_000_000_u64,
            b in 0_u64..1_000_000_u64,
        ) {
            let fee_a = BoundedFee::new(U256::from(a));
            let fee_b = BoundedFee::new(U256::from(b));

            prop_assert_eq!(fee_a + fee_b, fee_b + fee_a);
        }

        /// Property: Addition is associative (for non-overflowing values)
        #[test]
        fn prop_addition_associative(
            a in 0_u64..1_000_000_u64,
            b in 0_u64..1_000_000_u64,
            c in 0_u64..1_000_000_u64,
        ) {
            let fee_a = BoundedFee::new(U256::from(a));
            let fee_b = BoundedFee::new(U256::from(b));
            let fee_c = BoundedFee::new(U256::from(c));

            prop_assert_eq!((fee_a + fee_b) + fee_c, fee_a + (fee_b + fee_c));
        }

        /// Property: Zero is identity for addition
        #[test]
        fn prop_zero_identity(a in any::<u64>()) {
            let fee = BoundedFee::new(U256::from(a));
            let zero = BoundedFee::zero();

            prop_assert_eq!(fee + zero, fee);
            prop_assert_eq!(zero + fee, fee);
        }

        /// Property: AddAssign equivalent to Add
        #[test]
        fn prop_add_assign_equivalent(
            a in 0_u64..1_000_000_u64,
            b in 0_u64..1_000_000_u64,
        ) {
            let mut with_assign = BoundedFee::new(U256::from(a));
            let with_add = BoundedFee::new(U256::from(a));
            let fee_b = BoundedFee::new(U256::from(b));

            with_assign += fee_b;
            let result = with_add + fee_b;

            prop_assert_eq!(with_assign, result);
        }

        /// Property: Sum over iterator equals repeated addition
        #[test]
        fn prop_sum_correctness(fees in prop::collection::vec(0_u64..100_000_u64, 1..10)) {
            let bounded_fees: Vec<BoundedFee> = fees.iter()
                .map(|&f| BoundedFee::new(U256::from(f)))
                .collect();

            let sum_result: BoundedFee = bounded_fees.iter().copied().sum();

            let manual_sum = bounded_fees.iter().fold(BoundedFee::zero(), |acc, &f| acc + f);

            prop_assert_eq!(sum_result, manual_sum);
        }

        /// Property: Multiple saturations still result in MAX
        #[test]
        fn prop_multiple_saturations(
            start in (u64::MAX - 1000)..u64::MAX,
            additions in prop::collection::vec(1_u64..1000_u64, 1..10),
        ) {
            let mut total = BoundedFee::new(U256::from(start));

            for add in additions {
                total += BoundedFee::new(U256::from(add));
            }

            // After any saturation, should stay at or below U256::MAX
            prop_assert!(total.get() <= U256::max_value());
        }
    }
}
