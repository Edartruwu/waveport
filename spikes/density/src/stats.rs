//! Measurement helpers: percentiles, thread CPU time and resident memory.

use std::time::Duration;

/// A bag of samples. The spike's runs are short, so it keeps every sample
/// and sorts once at the end; capacity is reserved up front so that
/// recording never allocates inside a measured section.
#[derive(Debug)]
pub struct Samples {
    values: Vec<u64>,
    dropped: u64,
}

impl Samples {
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            values: Vec::with_capacity(capacity),
            dropped: 0,
        }
    }

    pub fn record(&mut self, value: u64) {
        if self.values.len() < self.values.capacity() {
            self.values.push(value);
        } else {
            self.dropped += 1;
        }
    }

    pub fn record_duration(&mut self, value: Duration) {
        self.record(u64::try_from(value.as_nanos()).unwrap_or(u64::MAX));
    }

    pub fn clear(&mut self) {
        self.values.clear();
        self.dropped = 0;
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Samples that did not fit and were not recorded.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Sorts the samples and returns the summary.
    pub fn summary(&mut self) -> Summary {
        self.values.sort_unstable();
        let at = |q: f64| -> u64 {
            if self.values.is_empty() {
                return 0;
            }
            let rank = ((self.values.len() - 1) as f64 * q).round() as usize;
            self.values[rank.min(self.values.len() - 1)]
        };
        let sum: u128 = self.values.iter().map(|&v| u128::from(v)).sum();
        Summary {
            count: self.values.len() as u64,
            mean: if self.values.is_empty() {
                0
            } else {
                u64::try_from(sum / self.values.len() as u128).unwrap_or(u64::MAX)
            },
            p50: at(0.50),
            p90: at(0.90),
            p99: at(0.99),
            p999: at(0.999),
            max: self.values.last().copied().unwrap_or(0),
        }
    }
}

/// Percentiles of a sample set, in the unit the samples were recorded in.
#[derive(Clone, Copy, Debug, Default)]
pub struct Summary {
    pub count: u64,
    pub mean: u64,
    pub p50: u64,
    pub p90: u64,
    pub p99: u64,
    pub p999: u64,
    pub max: u64,
}

impl Summary {
    /// The summary as a JSON object, with values divided by `scale`.
    #[must_use]
    pub fn json(&self, scale: f64) -> String {
        let f = |v: u64| v as f64 / scale;
        format!(
            "{{\"count\":{},\"mean\":{:.3},\"p50\":{:.3},\"p90\":{:.3},\"p99\":{:.3},\"p999\":{:.3},\"max\":{:.3}}}",
            self.count,
            f(self.mean),
            f(self.p50),
            f(self.p90),
            f(self.p99),
            f(self.p999),
            f(self.max)
        )
    }
}

/// CPU time the calling thread has spent running, and time it spent runnable
/// but waiting for a CPU, both in nanoseconds. Linux only.
///
/// The second number is what tells a clean run from one where something else
/// was scheduled on the pinned core.
#[must_use]
pub fn thread_sched() -> (u64, u64) {
    let Ok(text) = std::fs::read_to_string("/proc/thread-self/schedstat") else {
        return (0, 0);
    };
    let mut fields = text.split_ascii_whitespace();
    let run = fields.next().and_then(|f| f.parse().ok()).unwrap_or(0);
    let wait = fields.next().and_then(|f| f.parse().ok()).unwrap_or(0);
    (run, wait)
}

/// Resident set size of the process in bytes. Linux only.
#[must_use]
pub fn rss_bytes() -> u64 {
    let Ok(text) = std::fs::read_to_string("/proc/self/statm") else {
        return 0;
    };
    let pages: u64 = text
        .split_ascii_whitespace()
        .nth(1)
        .and_then(|f| f.parse().ok())
        .unwrap_or(0);
    pages * 4096
}

/// A fixed amount of integer work, timed. Run before and after a measurement:
/// if the two differ, the core changed speed during the run (thermal
/// throttling or a frequency change) and the run is not comparable.
#[must_use]
pub fn calibrate() -> Duration {
    let start = std::time::Instant::now();
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut sum = 0_u64;
    for _ in 0..40_000_000_u32 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        sum = sum.wrapping_add(state);
    }
    std::hint::black_box(sum);
    start.elapsed()
}

/// A small deterministic generator, so that every run sees the same load.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self(seed ^ 0x9e37_79b9_7f4a_7c15)
    }

    pub fn next(&mut self) -> u64 {
        // SplitMix64.
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A value in `0..bound`.
    pub fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 { 0 } else { self.next() % bound }
    }
}

/// A log-scale histogram with eight buckets per power of two. It never
/// grows, so recording into it does not disturb a memory measurement, and
/// a percentile read from it is at most 12.5% above the true value.
#[derive(Clone)]
pub struct Histogram {
    counts: Vec<u64>,
    total: u64,
    sum: u128,
    max: u64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

impl Histogram {
    const BUCKETS: usize = 62 * 8;

    #[must_use]
    pub fn new() -> Self {
        Self {
            counts: vec![0; Self::BUCKETS],
            total: 0,
            sum: 0,
            max: 0,
        }
    }

    fn index(value: u64) -> usize {
        if value < 8 {
            return value as usize;
        }
        let top = 63 - value.leading_zeros();
        let sub = (value >> (top - 3)) & 7;
        ((top - 2) * 8) as usize + sub as usize
    }

    /// The largest value that lands in bucket `index`.
    fn upper(index: usize) -> u64 {
        if index < 8 {
            return index as u64;
        }
        let top = (index / 8 + 2) as u32;
        let sub = (index % 8) as u64;
        (((8 + sub + 1) << (top - 3)) - 1) as u64
    }

    pub fn record(&mut self, value: u64) {
        self.counts[Self::index(value).min(Self::BUCKETS - 1)] += 1;
        self.total += 1;
        self.sum += u128::from(value);
        self.max = self.max.max(value);
    }

    pub fn record_duration(&mut self, value: Duration) {
        self.record(u64::try_from(value.as_nanos()).unwrap_or(u64::MAX));
    }

    pub fn clear(&mut self) {
        self.counts.fill(0);
        self.total = 0;
        self.sum = 0;
        self.max = 0;
    }

    pub fn merge(&mut self, other: &Self) {
        for (mine, theirs) in self.counts.iter_mut().zip(&other.counts) {
            *mine += theirs;
        }
        self.total += other.total;
        self.sum += other.sum;
        self.max = self.max.max(other.max);
    }

    #[must_use]
    pub fn count(&self) -> u64 {
        self.total
    }

    /// The sum of everything recorded.
    #[must_use]
    pub fn sum(&self) -> u128 {
        self.sum
    }

    /// How many recorded values were above `limit`, to bucket precision.
    #[must_use]
    pub fn above(&self, limit: u64) -> u64 {
        self.counts[(Self::index(limit) + 1).min(Self::BUCKETS)..]
            .iter()
            .sum()
    }

    #[must_use]
    pub fn quantile(&self, q: f64) -> u64 {
        if self.total == 0 {
            return 0;
        }
        let rank = ((self.total - 1) as f64 * q).round() as u64;
        let mut seen = 0;
        for (index, &count) in self.counts.iter().enumerate() {
            seen += count;
            if seen > rank {
                return Self::upper(index).min(self.max);
            }
        }
        self.max
    }

    #[must_use]
    pub fn summary(&self) -> Summary {
        Summary {
            count: self.total,
            mean: if self.total == 0 {
                0
            } else {
                u64::try_from(self.sum / u128::from(self.total)).unwrap_or(u64::MAX)
            },
            p50: self.quantile(0.50),
            p90: self.quantile(0.90),
            p99: self.quantile(0.99),
            p999: self.quantile(0.999),
            max: self.max,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Histogram;

    #[test]
    fn buckets_are_contiguous_and_bounded() {
        let mut previous = 0;
        for value in (0..200_u64).chain([1_000, 4_095, 4_096, 123_456_789, u64::MAX / 4]) {
            let index = Histogram::index(value);
            assert!(index >= previous || value < 200, "{value}");
            assert!(Histogram::upper(index) >= value, "{value}");
            if index > 0 {
                assert!(Histogram::upper(index - 1) < value, "{value}");
            }
            if value < 200 {
                previous = index;
            }
        }
    }

    #[test]
    fn quantiles_come_from_the_recorded_values() {
        let mut histogram = Histogram::new();
        for value in 1..=1_000_u64 {
            histogram.record(value * 1_000);
        }
        let p50 = histogram.quantile(0.5);
        assert!((500_000..=570_000).contains(&p50), "{p50}");
        assert_eq!(histogram.quantile(1.0), 1_000_000);
        assert_eq!(histogram.count(), 1_000);
        assert!(histogram.above(900_000) <= 100);
    }
}
