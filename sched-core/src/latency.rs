//! Per-thread latency accounting.
//!
//! Counters are deliberately simple: count, sum, maximum and a fixed log2
//! histogram. Percentiles are reported as the upper bound of the bucket that
//! contains the requested rank, so a p99 of `6,000,000 ns` means the observed
//! p99 is at most six milliseconds. The caller must label these numbers as
//! QEMU-TCG diagnostics when they come from an emulator.

pub const BUCKETS: usize = 64;

#[derive(Clone, Copy)]
pub struct LatencyStats {
    count: u64,
    sum_ns: u64,
    max_ns: u64,
    buckets: [u64; BUCKETS],
}

impl Default for LatencyStats {
    fn default() -> Self {
        Self::new()
    }
}

impl LatencyStats {
    pub const fn new() -> Self {
        Self {
            count: 0,
            sum_ns: 0,
            max_ns: 0,
            buckets: [0; BUCKETS],
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    pub fn record(&mut self, sample_ns: u64) {
        self.count = self.count.saturating_add(1);
        self.sum_ns = self.sum_ns.saturating_add(sample_ns);
        if sample_ns > self.max_ns {
            self.max_ns = sample_ns;
        }
        self.buckets[bucket_of(sample_ns)] =
            self.buckets[bucket_of(sample_ns)].saturating_add(1);
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    pub fn sum_ns(&self) -> u64 {
        self.sum_ns
    }

    pub fn max_ns(&self) -> u64 {
        self.max_ns
    }

    pub fn mean_ns(&self) -> u64 {
        if self.count == 0 {
            0
        } else {
            self.sum_ns / self.count
        }
    }

    /// Upper bound of the bucket containing the `percent`-th sample.
    ///
    /// `percent` is clamped to 1..=100. An empty histogram returns zero.
    pub fn percentile_upper_ns(&self, percent: u32) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let percent = percent.clamp(1, 100);
        let target = (self.count.saturating_mul(percent as u64) + 99) / 100;
        let mut seen = 0u64;
        for (index, count) in self.buckets.iter().enumerate() {
            if *count == 0 {
                continue;
            }
            seen = seen.saturating_add(*count);
            if seen >= target {
                return bucket_upper(index);
            }
        }
        self.max_ns
    }
}

fn bucket_of(sample_ns: u64) -> usize {
    if sample_ns <= 1 {
        0
    } else {
        (63 - sample_ns.leading_zeros()) as usize
    }
}

fn bucket_upper(index: usize) -> u64 {
    if index >= 63 {
        u64::MAX
    } else {
        1u64 << (index + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_stats_are_zero() {
        let stats = LatencyStats::new();
        assert_eq!(stats.count(), 0);
        assert_eq!(stats.mean_ns(), 0);
        assert_eq!(stats.percentile_upper_ns(50), 0);
    }

    #[test]
    fn count_sum_and_max_are_exact() {
        let mut stats = LatencyStats::new();
        for sample in [10u64, 20, 30, 40] {
            stats.record(sample);
        }
        assert_eq!(stats.count(), 4);
        assert_eq!(stats.sum_ns(), 100);
        assert_eq!(stats.max_ns(), 40);
        assert_eq!(stats.mean_ns(), 25);
    }

    #[test]
    fn percentiles_are_bucket_upper_bounds() {
        let mut stats = LatencyStats::new();
        for sample in 1_000..=10_000u64 {
            stats.record(sample);
        }
        let p50 = stats.percentile_upper_ns(50);
        let p99 = stats.percentile_upper_ns(99);
        assert!(p50 >= 5_000);
        assert!(p99 >= 9_900);
        assert!(p50 <= 8_192, "p50 bucket upper was {}", p50);
        assert!(p99 <= 16_384, "p99 bucket upper was {}", p99);
    }

    #[test]
    fn zero_and_one_share_the_first_bucket() {
        let mut stats = LatencyStats::new();
        stats.record(0);
        stats.record(1);
        assert_eq!(stats.percentile_upper_ns(50), 2);
    }

    #[test]
    fn reset_clears_everything() {
        let mut stats = LatencyStats::new();
        stats.record(u64::MAX);
        stats.reset();
        assert_eq!(stats.count(), 0);
        assert_eq!(stats.max_ns(), 0);
        assert_eq!(stats.percentile_upper_ns(99), 0);
    }
}
