use std::collections::VecDeque;

// Simple circular buffer implementation
#[derive(Debug, Default, Clone)]
pub struct CircularBuffer<T> {
    buffer: VecDeque<T>,
    capacity: usize,
}

impl<T> CircularBuffer<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            buffer: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn push(&mut self, item: T) {
        if self.capacity == 0 {
            return;
        }
        if self.buffer.len() >= self.capacity {
            self.buffer.pop_front();
        }
        self.buffer.push_back(item);
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.buffer.iter()
    }

    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    pub fn front(&self) -> Option<&T> {
        self.buffer.front()
    }

    pub fn back(&self) -> Option<&T> {
        self.buffer.back()
    }

    pub fn retain<F>(&mut self, mut f: F)
    where
        F: FnMut(&T) -> bool,
    {
        self.buffer.retain(|item| f(item));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn len_never_exceeds_capacity(capacity in 0_usize..64, items in proptest::collection::vec(any::<i32>(), 0..128)) {
            let mut buf = CircularBuffer::new(capacity);
            for item in &items {
                buf.push(*item);
                prop_assert!(buf.len() <= capacity);
            }
        }

        #[test]
        fn push_is_noop_for_zero_capacity(items in proptest::collection::vec(any::<i32>(), 0..64)) {
            let mut buf = CircularBuffer::new(0);
            for item in &items {
                buf.push(*item);
            }
            prop_assert!(buf.is_empty());
            prop_assert_eq!(buf.len(), 0);
        }

        #[test]
        fn back_equals_last_pushed(capacity in 1_usize..64, items in proptest::collection::vec(any::<i32>(), 1..128)) {
            let mut buf = CircularBuffer::new(capacity);
            for item in &items {
                buf.push(*item);
                prop_assert_eq!(buf.back(), Some(item));
            }
        }

        #[test]
        fn front_is_oldest_after_wrapping(capacity in 1_usize..32, items in proptest::collection::vec(any::<i32>(), 1..128)) {
            let mut buf = CircularBuffer::new(capacity);
            for item in &items {
                buf.push(*item);
            }
            if items.len() >= capacity {
                let expected = items[items.len() - capacity];
                prop_assert_eq!(buf.front(), Some(&expected));
            }
        }

        #[test]
        fn retain_never_increases_length(capacity in 1_usize..32, items in proptest::collection::vec(any::<i32>(), 0..64)) {
            let mut buf = CircularBuffer::new(capacity);
            for item in &items {
                buf.push(*item);
            }
            let len_before = buf.len();
            buf.retain(|x| *x > 0);
            prop_assert!(buf.len() <= len_before);
        }
    }
}
