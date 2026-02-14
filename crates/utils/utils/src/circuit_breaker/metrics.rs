use std::fmt::Debug;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
        self.closed_count as f64 / self.total_count as f64
    }

    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "Circuit Breakers: {} total ({} closed, {} open, {} half-open)",
            self.total_count, self.closed_count, self.open_count, self.half_open_count
        )
    }
}
