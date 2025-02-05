use std::{
    fmt,
    ops::{Add, AddAssign, Deref, DerefMut, Div, Mul, Rem, Sub},
    path::PathBuf,
};

use crate::CONFIG;
use nodit::{
    interval::{ie, ii},
    DiscreteFinite, InclusiveInterval, Interval, IntervalType,
};
use reth_codecs::Compact;
use reth_db::table::{Decode, Encode};
use serde::{Deserialize, Serialize};

pub const MEGABYTE: usize = 1024 * 1024;
pub const GIGABYTE: usize = MEGABYTE * 1024;
pub const TERABYTE: usize = GIGABYTE * 1024;

/// Partition relative chunk offsets
#[derive(
    Default, Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Compact,
)]
pub struct PartitionChunkOffset(u32);

impl Decode for PartitionChunkOffset {
    fn decode(value: &[u8]) -> Result<Self, reth_db::DatabaseError> {
        if value.len() != 4 {
            return Err(reth_db::DatabaseError::Other(
                "Expected 4 bytes, got a different length".to_string(),
            ));
        }
        // Attempt to convert the byte slice into a u32 (big-endian format)
        let decoded_value = u32::from_be_bytes(
            value
                .try_into()
                .map_err(|_| reth_db::DatabaseError::Decode)?,
        );
        Ok(PartitionChunkOffset(decoded_value))
    }
}
impl Encode for PartitionChunkOffset {
    type Encoded = [u8; std::mem::size_of::<PartitionChunkOffset>()];
    fn encode(self) -> Self::Encoded {
        self.0.to_be_bytes()
    }
}

impl DiscreteFinite for PartitionChunkOffset {
    const MIN: Self = PartitionChunkOffset(0);
    const MAX: Self = PartitionChunkOffset(u32::MAX);
    fn up(self) -> Option<Self> {
        self.0.checked_add(1).map(PartitionChunkOffset)
    }
    fn down(self) -> Option<Self> {
        self.0.checked_sub(1).map(PartitionChunkOffset)
    }
}
impl PartitionChunkOffset {
    pub fn value(&self) -> u32 {
        self.0
    }
    pub fn from_be_bytes(bytes: [u8; 4]) -> Self {
        PartitionChunkOffset(u32::from_be_bytes(bytes))
    }
}
impl From<PartitionChunkOffset> for u64 {
    fn from(value: PartitionChunkOffset) -> Self {
        value.0 as u64
    }
}
impl From<PartitionChunkOffset> for u8 {
    fn from(value: PartitionChunkOffset) -> Self {
        value.0 as u8
    }
}
impl Add<u32> for PartitionChunkOffset {
    type Output = Self;
    fn add(self, rhs: u32) -> Self::Output {
        PartitionChunkOffset(self.0 + rhs)
    }
}
impl Sub<PartitionChunkOffset> for PartitionChunkOffset {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self::Output {
        PartitionChunkOffset(self.0 - rhs.0)
    }
}
impl Sub<u32> for PartitionChunkOffset {
    type Output = Self;
    fn sub(self, rhs: u32) -> Self::Output {
        PartitionChunkOffset(self.0 - rhs)
    }
}
impl Div<PartitionChunkOffset> for PartitionChunkOffset {
    type Output = Self;
    fn div(self, rhs: Self) -> Self::Output {
        PartitionChunkOffset(self.0 / rhs.0)
    }
}
impl Div<u32> for PartitionChunkOffset {
    type Output = Self;
    fn div(self, rhs: u32) -> Self::Output {
        PartitionChunkOffset(self.0 / rhs)
    }
}
impl Rem<u32> for PartitionChunkOffset {
    type Output = Self;
    fn rem(self, rhs: u32) -> Self::Output {
        PartitionChunkOffset(self.0 % rhs)
    }
}

impl From<u8> for PartitionChunkOffset {
    fn from(value: u8) -> Self {
        PartitionChunkOffset(value.into())
    }
}
impl From<u32> for PartitionChunkOffset {
    fn from(value: u32) -> Self {
        PartitionChunkOffset(value)
    }
}
impl From<u64> for PartitionChunkOffset {
    fn from(value: u64) -> Self {
        PartitionChunkOffset(value as u32)
    }
}
impl From<i32> for PartitionChunkOffset {
    fn from(value: i32) -> Self {
        PartitionChunkOffset(value as u32)
    }
}
impl fmt::Display for PartitionChunkOffset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl TryFrom<PartitionChunkOffset> for usize {
    type Error = &'static str;
    fn try_from(value: PartitionChunkOffset) -> Result<Self, Self::Error> {
        let value_u32 = value.0;
        value_u32
            .try_into()
            .map_err(|_| "Conversion to usize failed")
    }
}
/// Partition relative chunk interval/ranges
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct PartitionChunkRange(pub Interval<PartitionChunkOffset>);

impl Deref for PartitionChunkRange {
    type Target = Interval<PartitionChunkOffset>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl From<Interval<PartitionChunkOffset>> for PartitionChunkRange {
    fn from(interval: Interval<PartitionChunkOffset>) -> Self {
        Self(interval)
    }
}
impl InclusiveInterval<PartitionChunkOffset> for PartitionChunkRange {
    fn start(&self) -> PartitionChunkOffset {
        self.0.start()
    }
    fn end(&self) -> PartitionChunkOffset {
        self.0.end()
    }
}
impl InclusiveInterval<u32> for PartitionChunkRange {
    fn start(&self) -> u32 {
        self.0.start().0 // Extracts u32 from PartitionChunkOffset
    }

    fn end(&self) -> u32 {
        self.0.end().0 // Extracts u32 from PartitionChunkOffset
    }
}
impl From<Interval<u32>> for PartitionChunkRange {
    fn from(interval: Interval<u32>) -> Self {
        PartitionChunkRange(ii(
            PartitionChunkOffset::from(interval.start()),
            PartitionChunkOffset::from(interval.end()),
        ))
    }
}

/// Ledger Relative chunk offsets
// pub type LedgerChunkOffset = u64;

#[derive(
    Default, Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Compact,
)]
pub struct LedgerChunkOffset(u64);

impl DiscreteFinite for LedgerChunkOffset {
    const MIN: Self = LedgerChunkOffset(0);
    const MAX: Self = LedgerChunkOffset(u64::MAX);
    fn up(self) -> Option<Self> {
        self.0.checked_add(1).map(LedgerChunkOffset)
    }
    fn down(self) -> Option<Self> {
        self.0.checked_sub(1).map(LedgerChunkOffset)
    }
}
impl LedgerChunkOffset {
    pub fn value(&self) -> u64 {
        self.0
    }
    pub fn from_be_bytes(bytes: [u8; 8]) -> Self {
        LedgerChunkOffset(u64::from_be_bytes(bytes))
    }
}

impl From<LedgerChunkOffset> for u64 {
    fn from(value: LedgerChunkOffset) -> Self {
        value.0 as u64
    }
}
impl From<LedgerChunkOffset> for u8 {
    fn from(value: LedgerChunkOffset) -> Self {
        value.0 as u8
    }
}
impl Add<u64> for LedgerChunkOffset {
    type Output = Self;
    fn add(self, rhs: u64) -> Self::Output {
        LedgerChunkOffset(self.0 + rhs)
    }
}
impl Sub<LedgerChunkOffset> for LedgerChunkOffset {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self::Output {
        LedgerChunkOffset(self.0 - rhs.0)
    }
}
impl Sub<u64> for LedgerChunkOffset {
    type Output = Self;
    fn sub(self, rhs: u64) -> Self::Output {
        LedgerChunkOffset(self.0 - rhs)
    }
}
impl Mul<u64> for LedgerChunkOffset {
    type Output = Self;
    fn mul(self, rhs: u64) -> Self::Output {
        LedgerChunkOffset(self.0 * rhs)
    }
}
impl Div<LedgerChunkOffset> for LedgerChunkOffset {
    type Output = Self;
    fn div(self, rhs: Self) -> Self::Output {
        LedgerChunkOffset(self.0 / rhs.0)
    }
}
impl Div<u64> for LedgerChunkOffset {
    type Output = Self;
    fn div(self, rhs: u64) -> Self::Output {
        LedgerChunkOffset(self.0 / rhs)
    }
}
impl Rem<u64> for LedgerChunkOffset {
    type Output = Self;
    fn rem(self, rhs: u64) -> Self::Output {
        LedgerChunkOffset(self.0 % rhs)
    }
}

impl AddAssign<u64> for LedgerChunkOffset {
    fn add_assign(&mut self, rhs: u64) {
        self.0 = self.value() + rhs;
    }
}

impl From<u64> for LedgerChunkOffset {
    fn from(value: u64) -> Self {
        LedgerChunkOffset(value.into())
    }
}
impl From<u8> for LedgerChunkOffset {
    fn from(value: u8) -> Self {
        LedgerChunkOffset(value.into())
    }
}
impl From<i32> for LedgerChunkOffset {
    fn from(value: i32) -> Self {
        LedgerChunkOffset(value.try_into().expect("Value must be non-negative"))
    }
}

impl fmt::Display for LedgerChunkOffset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl TryFrom<LedgerChunkOffset> for usize {
    type Error = &'static str;
    fn try_from(value: LedgerChunkOffset) -> Result<Self, Self::Error> {
        let value_u32 = value.0;
        value_u32
            .try_into()
            .map_err(|_| "Conversion to usize failed")
    }
}

/// Ledger Relative chunk interval/ranges
#[derive(Debug, Copy, Clone)]
pub struct LedgerChunkRange(pub Interval<LedgerChunkOffset>);

impl Deref for LedgerChunkRange {
    type Target = Interval<LedgerChunkOffset>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<Interval<LedgerChunkOffset>> for LedgerChunkRange {
    fn from(interval: Interval<LedgerChunkOffset>) -> Self {
        Self(interval)
    }
}

/// Add impl Into<Interval<u64>> for owned conversion
impl From<LedgerChunkRange> for Interval<LedgerChunkOffset> {
    fn from(range: LedgerChunkRange) -> Self {
        range.0
    }
}

// Add Deref implementation to convert to Interval<u64>
impl AsRef<Interval<LedgerChunkOffset>> for LedgerChunkRange {
    fn as_ref(&self) -> &Interval<LedgerChunkOffset> {
        &self.0
    }
}

impl InclusiveInterval<LedgerChunkOffset> for LedgerChunkRange {
    fn start(&self) -> LedgerChunkOffset {
        self.0.start()
    }

    fn end(&self) -> LedgerChunkOffset {
        self.0.end()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Storage provider config
pub struct PartitionStorageProviderConfig {
    /// vec of intervals to storage module configurations
    pub sm_paths_offsets: Vec<(Interval<u32>, StorageModuleConfig)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]

pub struct StorageModuleConfig {
    pub directory_path: PathBuf,
    pub size_bytes: u64,
    // pub chunks_per_lock_segment: u32,
}

impl Default for StorageModuleConfig {
    fn default() -> Self {
        Self {
            directory_path: "/tmp".into(),
            size_bytes: 100 * CONFIG.chunk_size,
            // chunks_per_lock_segment: 800, // 200MB
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IntervalState {
    // common fields:
    pub chunk_state: ChunkState,
}

impl IntervalState {
    pub fn new(chunk_state: ChunkState) -> Self {
        Self { chunk_state }
    }

    pub fn packed() -> Self {
        Self {
            chunk_state: ChunkState::Packed,
        }
    }
}

// wrapper struct so we can "contain" the custom Eq impl that works off the state enum
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IntervalStateWrapped {
    pub inner: IntervalState,
}

impl Deref for IntervalStateWrapped {
    type Target = IntervalState;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for IntervalStateWrapped {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl IntervalStateWrapped {
    pub fn new(state: IntervalState) -> Self {
        Self { inner: state }
    }
}

impl PartialEq for IntervalStateWrapped {
    fn eq(&self, other: &Self) -> bool {
        // compare the state enum variant, not inner state
        std::mem::discriminant(&self.inner.chunk_state)
            == std::mem::discriminant(&other.inner.chunk_state)
    }
}

impl Eq for IntervalStateWrapped {}

#[derive(Clone, Debug, Copy, Serialize, Deserialize)]
pub enum ChunkState {
    Unpacked,
    Packed,
    Data,
    Writing,
}

/// Splits an interval into n equal-sized intervals
///
/// # Arguments
/// * `interval` - The interval to split
/// * `step` - Number of elements in each chunk
///
/// # Returns
/// * `Result<Vec<Interval>, IntervalSplitError>` - Vector of splitted chunks
///
/// # Examples
/// ```
/// use nodit::interval::ii;
/// use irys_types::storage::{PartitionChunkRange, split_interval};
/// let interval = PartitionChunkRange(ii(0, 4));
/// let splits = split_interval(&interval, 3).unwrap();
/// assert_eq!(splits.len(), 2);
/// assert_eq!(splits[0], PartitionChunkRange(ii(0, 2)));
/// assert_eq!(splits[1], PartitionChunkRange(ii(3, 4)));
/// ```
pub fn split_interval(
    interval: &PartitionChunkRange,
    step: u32,
) -> eyre::Result<Vec<PartitionChunkRange>> {
    if step == 0 {
        return Err(eyre::eyre!("Invalid zero step for split interval"));
    }

    let start = interval.start();
    let end = interval.end();

    if start > end {
        return Err(eyre::eyre!("Invalid interval bounds: [{}, {}]", start, end));
    } else if start == end {
        return Ok(vec![PartitionChunkRange(ii(start, end))]);
    }

    let n = if (end - start + 1) % step == PartitionChunkOffset(0) {
        ((end - start + 1) / step).try_into().unwrap()
    } else {
        ((end - start + 1) / step + 1).try_into().unwrap()
    };

    let mut intervals = Vec::with_capacity(n);

    for i in 0..n {
        let interval_start = start + i as u32 * step;
        let interval_end = if i == n - 1 {
            end + 1 // exclusive end, last chunk may not be full
        } else {
            start + (i as u32 + 1) * step
        };

        intervals.push(PartitionChunkRange(ie(interval_start, interval_end)));
    }
    Ok(intervals)
}

#[cfg(test)]
mod tests {
    use nodit::interval::ii;

    use super::*;

    #[test]
    fn test_split_interval() {
        // interval with just one element
        let interval = PartitionChunkRange(ii(
            PartitionChunkOffset::from(3),
            PartitionChunkOffset::from(3),
        ));
        let splits = split_interval(&interval, 3).unwrap();
        assert_eq!(splits.len(), 1);
        assert_eq!(
            splits[0],
            PartitionChunkRange(ii(
                PartitionChunkOffset::from(3),
                PartitionChunkOffset::from(3)
            ))
        );

        // even interval
        let interval = PartitionChunkRange(ii(
            PartitionChunkOffset::from(0),
            PartitionChunkOffset::from(3),
        ));
        let splits = split_interval(&interval, 3).unwrap();
        assert_eq!(splits.len(), 2);
        assert_eq!(
            splits[0],
            PartitionChunkRange(ii(
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(2)
            ))
        );
        assert_eq!(
            splits[1],
            PartitionChunkRange(ii(
                PartitionChunkOffset::from(3),
                PartitionChunkOffset::from(3)
            ))
        );

        // odd interval
        let interval = PartitionChunkRange(ii(
            PartitionChunkOffset::from(0),
            PartitionChunkOffset::from(4),
        ));
        let splits = split_interval(&interval, 1).unwrap();
        assert_eq!(splits.len(), 5);
        assert_eq!(
            splits[0],
            PartitionChunkRange(ii(
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(0)
            ))
        );
        assert_eq!(
            splits[1],
            PartitionChunkRange(ii(
                PartitionChunkOffset::from(1),
                PartitionChunkOffset::from(1)
            ))
        );
        assert_eq!(
            splits[2],
            PartitionChunkRange(ii(
                PartitionChunkOffset::from(2),
                PartitionChunkOffset::from(2)
            ))
        );
        assert_eq!(
            splits[3],
            PartitionChunkRange(ii(
                PartitionChunkOffset::from(3),
                PartitionChunkOffset::from(3)
            ))
        );
        assert_eq!(
            splits[4],
            PartitionChunkRange(ii(
                PartitionChunkOffset::from(4),
                PartitionChunkOffset::from(4)
            ))
        );

        // odd interval, with step size bigger than it
        let interval = PartitionChunkRange(ie(
            PartitionChunkOffset::from(0),
            PartitionChunkOffset::from(4),
        ));
        let splits = split_interval(&interval, 8).unwrap();
        assert_eq!(splits.len(), 1);
        assert_eq!(
            splits[0],
            PartitionChunkRange(ie(
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(4)
            ))
        );

        // zero step error
        let interval = PartitionChunkRange(ie(
            PartitionChunkOffset::from(0),
            PartitionChunkOffset::from(4),
        ));
        let splits = split_interval(&interval, 0);
        assert!(splits.is_err());

        // even interval not starting in zero, all complete splits
        let interval = PartitionChunkRange(ii(
            PartitionChunkOffset::from(2),
            PartitionChunkOffset::from(7),
        ));
        let splits = split_interval(&interval, 3).unwrap();
        assert_eq!(splits.len(), 2);
        assert_eq!(
            splits[0],
            PartitionChunkRange(ii(
                PartitionChunkOffset::from(2),
                PartitionChunkOffset::from(4)
            ))
        );
        assert_eq!(
            splits[1],
            PartitionChunkRange(ii(
                PartitionChunkOffset::from(5),
                PartitionChunkOffset::from(7)
            ))
        );

        // odd interval not starting in zero, not all complete splits
        let interval = PartitionChunkRange(ii(
            PartitionChunkOffset::from(3),
            PartitionChunkOffset::from(6),
        ));
        let splits = split_interval(&interval, 1).unwrap();
        assert_eq!(splits.len(), 4);
        assert_eq!(
            splits[0],
            PartitionChunkRange(ii(
                PartitionChunkOffset::from(3),
                PartitionChunkOffset::from(3)
            ))
        );
        assert_eq!(
            splits[1],
            PartitionChunkRange(ii(
                PartitionChunkOffset::from(4),
                PartitionChunkOffset::from(4)
            ))
        );
        assert_eq!(
            splits[2],
            PartitionChunkRange(ii(
                PartitionChunkOffset::from(5),
                PartitionChunkOffset::from(5)
            ))
        );
        assert_eq!(
            splits[3],
            PartitionChunkRange(ii(
                PartitionChunkOffset::from(6),
                PartitionChunkOffset::from(6)
            ))
        );

        // odd interval not starting in zero not all complete
        let interval = PartitionChunkRange(ii(
            PartitionChunkOffset::from(5),
            PartitionChunkOffset::from(8),
        ));
        let splits = split_interval(&interval, 3).unwrap();
        assert_eq!(splits.len(), 2);
        assert_eq!(
            splits[0],
            PartitionChunkRange(ii(
                PartitionChunkOffset::from(5),
                PartitionChunkOffset::from(7)
            ))
        );
        assert_eq!(
            splits[1],
            PartitionChunkRange(ii(
                PartitionChunkOffset::from(8),
                PartitionChunkOffset::from(8)
            ))
        );

        // even interval not starting in zero, odd intervals, not all complete splits
        let interval = PartitionChunkRange(ii(
            PartitionChunkOffset::from(3),
            PartitionChunkOffset::from(7),
        ));
        let splits = split_interval(&interval, 2).unwrap();
        assert_eq!(splits.len(), 3);
        assert_eq!(
            splits[0],
            PartitionChunkRange(ii(
                PartitionChunkOffset::from(3),
                PartitionChunkOffset::from(4)
            ))
        );
        assert_eq!(
            splits[1],
            PartitionChunkRange(ii(
                PartitionChunkOffset::from(5),
                PartitionChunkOffset::from(6)
            ))
        );
        assert_eq!(
            splits[2],
            PartitionChunkRange(ii(
                PartitionChunkOffset::from(7),
                PartitionChunkOffset::from(7)
            ))
        );
    }
}
