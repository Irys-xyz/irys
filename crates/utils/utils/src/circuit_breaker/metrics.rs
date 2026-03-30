use std::fmt::Debug;

#[cfg(test)]
use proptest::prelude::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CircuitBreakerMetrics<K>
where
    K: Debug + Clone,
{
    pub total_count: usize,
    pub closed_count: usize,
    pub open_count: usize,
    pub half_open_count: usize,
    pub open_keys: Vec<K>,
}

impl<K> Default for CircuitBreakerMetrics<K>
where
    K: Debug + Clone,
{
    fn default() -> Self {
        Self {
            total_count: 0,
            closed_count: 0,
            open_count: 0,
            half_open_count: 0,
            open_keys: Vec::new(),
        }
    }
}

impl<K> CircuitBreakerMetrics<K>
where
    K: Debug + Clone,
{
    #[must_use]
    pub fn has_open_circuits(&self) -> bool {
        self.open_count > 0
    }

    #[must_use]
    pub fn has_recovering_circuits(&self) -> bool {
        self.half_open_count > 0
    }

    #[inline]
    #[must_use]
    pub fn health_ratio(&self) -> f64 {
        if self.total_count == 0 {
            return 1.0;
        }
        // usize-to-f64: no From/TryFrom exists in std; precision loss is acceptable
        // for a monitoring ratio where both values are bounded by circuit breaker capacity
        (self.closed_count as f64) / (self.total_count as f64)
    }

    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "Circuit Breakers: {} total ({} closed, {} open, {} half-open)",
            self.total_count, self.closed_count, self.open_count, self.half_open_count
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    proptest! {
        #[test]
        fn health_ratio_bounded_zero_to_one(
            closed in 0_usize..=1000,
            open in 0_usize..=1000,
            half_open in 0_usize..=1000,
        ) {
            let total = closed + open + half_open;
            let metrics: CircuitBreakerMetrics<u32> = CircuitBreakerMetrics {
                total_count: total,
                closed_count: closed,
                open_count: open,
                half_open_count: half_open,
                open_keys: Vec::new(),
            };

            let ratio = metrics.health_ratio();
            prop_assert!(ratio >= 0.0, "ratio must be >= 0.0, got {}", ratio);
            prop_assert!(ratio <= 1.0, "ratio must be <= 1.0, got {}", ratio);
        }

        #[test]
        fn health_ratio_one_when_all_closed(total in 1_usize..=1000) {
            let metrics: CircuitBreakerMetrics<u32> = CircuitBreakerMetrics {
                total_count: total,
                closed_count: total,
                open_count: 0,
                half_open_count: 0,
                open_keys: Vec::new(),
            };
            let ratio = metrics.health_ratio();
            prop_assert!((ratio - 1.0).abs() < f64::EPSILON);
        }

        #[test]
        fn health_ratio_zero_when_none_closed(total in 1_usize..=1000) {
            let metrics: CircuitBreakerMetrics<u32> = CircuitBreakerMetrics {
                total_count: total,
                closed_count: 0,
                open_count: total,
                half_open_count: 0,
                open_keys: Vec::new(),
            };
            let ratio = metrics.health_ratio();
            prop_assert!(ratio.abs() < f64::EPSILON);
        }
    }

    #[test]
    fn health_ratio_returns_one_for_empty() {
        let metrics: CircuitBreakerMetrics<u32> = CircuitBreakerMetrics::default();
        assert!((metrics.health_ratio() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn has_open_circuits_reflects_open_count() {
        let mut metrics: CircuitBreakerMetrics<u32> = CircuitBreakerMetrics::default();
        assert!(!metrics.has_open_circuits());
        metrics.open_count = 1;
        assert!(metrics.has_open_circuits());
    }

    #[test]
    fn has_recovering_circuits_reflects_half_open_count() {
        let mut metrics: CircuitBreakerMetrics<u32> = CircuitBreakerMetrics::default();
        assert!(!metrics.has_recovering_circuits());
        metrics.half_open_count = 1;
        assert!(metrics.has_recovering_circuits());
    }
}
