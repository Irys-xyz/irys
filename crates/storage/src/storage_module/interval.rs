use std::ops::{Deref, DerefMut};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use irys_types::{IntervalState, IntervalStateWrapped, ChunkState};
use nodit::interval::{ie, ii};
use nodit::{Interval, NoditMap};
use serde::{Deserialize, Serialize};

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

#[test]
fn interval_test() -> eyre::Result<()> {
    let mut map = NoditMap::<u32, Interval<u32>, IntervalStateWrapped>::new();
    let psm = IntervalStateWrapped::new(IntervalState {
        state: ChunkState::Unpacked,
    });
    map.insert_merge_touching_if_values_equal(ii(0, 1024), psm.clone())
        .unwrap();
    let mut psm2 = psm.clone();
    psm2.state = ChunkState::Packed;
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
        state: ChunkState::Unpacked,
    });
    map.insert_merge_touching_if_values_equal(ii(0, 1024), psm.clone())
        .unwrap();
    let mut psm2 = psm.clone();
    psm2.state = ChunkState::Packed;
    map.insert_merge_touching_if_values_equal(ii(1025, 1032), psm2.clone())
        .unwrap();
    map.insert_merge_touching_if_values_equal(ii(1033, 1042), psm2.clone())
        .unwrap();
    map.insert_merge_touching_if_values_equal(ii(1043, 1050), psm.clone())
        .unwrap();
    let psm3 = IntervalStateWrapped::new(IntervalState {
        state: ChunkState::Data,
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
