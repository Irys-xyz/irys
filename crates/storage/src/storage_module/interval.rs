use std::ops::{Deref, DerefMut};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nodit::interval::{ie, ii};
use nodit::{Interval, NoditMap};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Copy, Serialize, Deserialize)]
pub enum PackingState {
    Unpacked,
    Packed,
    Data,
    WriteLocked,
}

#[derive(Debug, Clone)]
pub struct WriteLock {
    /// unix epoch ms when first locked
    pub locked_at: u64, // TODO @JesseTheRobot use `Duration` or some better type instead?
    /// unix epoch ms when the last refresh occurred
    pub refreshed_at: u64,
}

pub fn get_now() -> u64 {
    get_duration_since_epoch().as_millis() as u64
}
pub fn get_duration_since_epoch() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Time went backwards")
}

impl WriteLock {
    pub fn new(now: Option<u64>) -> Self {
        let now = now.unwrap_or(get_now());
        return Self {
            locked_at: now.clone(),
            refreshed_at: now,
        };
    }

    pub fn refresh(&mut self) {
        self.refreshed_at = get_now();
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IntervalState {
    // common fields:
    pub state: PackingState,
}

// wrapper struct so we can "contain" the custom Eq impl that works off the state enum
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IntervalStateWrapped {
    pub inner: IntervalState,
}

impl Deref for IntervalStateWrapped {
    type Target = IntervalState;

    fn deref(&self) -> &Self::Target {
        return &self.inner;
    }
}

impl DerefMut for IntervalStateWrapped {
    fn deref_mut(&mut self) -> &mut Self::Target {
        return &mut self.inner;
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
        std::mem::discriminant(&self.inner.state) == std::mem::discriminant(&other.inner.state)
    }
}

impl Eq for IntervalStateWrapped {}

#[test]
fn interval_test() -> eyre::Result<()> {
    let mut map = NoditMap::<u32, Interval<u32>, IntervalStateWrapped>::new();
    let psm = IntervalStateWrapped::new(IntervalState {
        state: PackingState::Unpacked,
    });
    map.insert_merge_touching_if_values_equal(ii(0, 1024), psm.clone())
        .unwrap();
    let mut psm2 = psm.clone();
    psm2.state = PackingState::Packed;
    map.insert_merge_touching_if_values_equal(ii(1025, 1032), psm2.clone())
        .unwrap();
    map.insert_merge_touching_if_values_equal(ii(1033, 1042), psm2.clone())
        .unwrap();
    map.insert_merge_touching_if_values_equal(ii(1043, 1050), psm.clone())
        .unwrap();

    let mut iter = map.iter();
    assert_eq!(iter.next(), Some((&ii(0, 1024), &psm)));
    assert_eq!(iter.next(), Some((&ii(1025, 1042), &psm2)));
    assert_eq!(iter.next(), Some((&ii(1043, 1050), &psm)));

    Ok(())
}

#[test]
fn interval_overwrite_test() -> eyre::Result<()> {
    let mut map = NoditMap::<u32, Interval<u32>, IntervalStateWrapped>::new();
    let psm = IntervalStateWrapped::new(IntervalState {
        state: PackingState::Unpacked,
    });
    map.insert_merge_touching_if_values_equal(ii(0, 1024), psm.clone())
        .unwrap();
    let mut psm2 = psm.clone();
    psm2.state = PackingState::Packed;
    map.insert_merge_touching_if_values_equal(ii(1025, 1032), psm2.clone())
        .unwrap();
    map.insert_merge_touching_if_values_equal(ii(1033, 1042), psm2.clone())
        .unwrap();
    map.insert_merge_touching_if_values_equal(ii(1043, 1050), psm.clone())
        .unwrap();
    let psm3 = IntervalStateWrapped::new(IntervalState {
        state: PackingState::Data,
    });
    // insert_overwrite will cut the intervals it overlaps with and inserts this new interval in the gap, trimming intervals on the edge to account for this new interval
    let _ = map.insert_overwrite(ii(1030, 1040), psm3.clone());
    let mut iter = map.iter();

    assert_eq!(iter.next(), Some((&ii(0, 1024), &psm)));
    assert_eq!(iter.next(), Some((&ii(1025, 1029), &psm2)));
    assert_eq!(iter.next(), Some((&ii(1030, 1040), &psm3))); // inserted interval
    assert_eq!(iter.next(), Some((&ii(1041, 1042), &psm2)));
    assert_eq!(iter.next(), Some((&ii(1043, 1050), &psm)));

    Ok(())
}
