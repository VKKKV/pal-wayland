// SPDX-License-Identifier: GPL-3.0-or-later
//! Bounded, opt-in host timings: build with `profiling` and set PAL_PROFILE=1.
//! No guest data, per-instruction timers, GPU waits, or retained sample vectors.
//! Histograms are cumulative logarithmic microsecond buckets. Percentiles are
//! bucket upper bounds, NOT exact quantiles. Upload/submit/present measure CPU
//! calls only, not device completion, compositor delivery or physical latency.
use std::{
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

const REPORT_EVERY: u64 = 120;
const BUCKETS: usize = 66; // zero, 1..2^63 us, then all representable Durations
const NAMES: [&str; 12] = [
    "tick",
    "frame",
    "guest_execution",
    "config_reload",
    "frame_decode",
    "swapchain_acquire",
    "gpu_cpu_upload",
    "queue_submit",
    "present_cpu",
    "presentation_interval",
    "softbuffer_cpu_scale",
    "softbuffer_cpu_copy",
];

#[derive(Clone, Copy)]
struct Totals {
    count: u64,
    total: Duration,
    maximum: Duration,
    histogram: [u64; BUCKETS],
    until_report: u64,
}
impl Totals {
    const EMPTY: Self = Self {
        count: 0,
        total: Duration::ZERO,
        maximum: Duration::ZERO,
        histogram: [0; BUCKETS],
        until_report: REPORT_EVERY,
    };
    fn record(&mut self, elapsed: Duration) -> bool {
        let micros = elapsed.as_nanos().div_ceil(1_000);
        let bucket = if micros == 0 {
            0
        } else if micros > (1u128 << 63) {
            BUCKETS - 1
        } else if micros == 1 {
            1
        } else {
            (micros - 1).ilog2() as usize + 2
        };
        self.count = self.count.saturating_add(1);
        self.total = self.total.saturating_add(elapsed);
        self.maximum = self.maximum.max(elapsed);
        self.histogram[bucket] = self.histogram[bucket].saturating_add(1);
        self.until_report -= 1;
        if self.until_report == 0 {
            self.until_report = REPORT_EVERY;
            true
        } else {
            false
        }
    }
    fn percentile_upper_us(&self, percentile: u8) -> u128 {
        if self.count == 0 {
            return 0;
        }
        let rank = (u128::from(self.count) * u128::from(percentile)).div_ceil(100);
        let mut seen = 0u128;
        for (bucket, count) in self.histogram.iter().enumerate() {
            seen = seen.saturating_add(u128::from(*count));
            if seen >= rank {
                return bucket_upper_us(bucket);
            }
        }
        bucket_upper_us(BUCKETS - 1)
    }
}
fn bucket_upper_us(bucket: usize) -> u128 {
    match bucket {
        0 => 0,
        65 => Duration::MAX.as_nanos().div_ceil(1_000),
        _ => 1u128 << (bucket - 1),
    }
}

static ENABLED: OnceLock<bool> = OnceLock::new();
static TOTALS: Mutex<[Totals; NAMES.len()]> = Mutex::new([Totals::EMPTY; NAMES.len()]);
fn enabled() -> bool {
    *ENABLED.get_or_init(|| {
        std::env::var_os("PAL_PROFILE").as_deref() == Some(std::ffi::OsStr::new("1"))
    })
}
fn record(index: usize, elapsed: Duration) {
    let snapshot = {
        // Profiling must not turn a poisoned diagnostic lock into a game fault.
        let mut totals = TOTALS.lock().unwrap_or_else(|e| e.into_inner());
        let t = &mut totals[index];
        if t.record(elapsed) { Some(*t) } else { None }
    };
    // Release collector lock before the potentially blocking stderr write.
    if let Some(t) = snapshot {
        eprintln!(
            "profile span={} count={} total_us={} max_us={} p50_bucket_upper_us={} p95_bucket_upper_us={} p99_bucket_upper_us={} quantiles=log2_bucket_upper_bound scope=host_cpu_not_gpu_completion",
            NAMES[index],
            t.count,
            t.total.as_micros(),
            t.maximum.as_micros(),
            t.percentile_upper_us(50),
            t.percentile_upper_us(95),
            t.percentile_upper_us(99),
        );
    }
}

pub struct Span {
    start: Option<Instant>,
    index: usize,
}
impl Span {
    pub fn new(name: &'static str) -> Self {
        // Opt-out never samples a clock, acquires the collector lock or even
        // looks up a stage. The build feature removes instrumentation entirely.
        if !enabled() {
            return Self {
                start: None,
                index: 0,
            };
        }
        // Keep the original execution spelling for existing generated callers.
        let name = if name == "execution" {
            "guest_execution"
        } else {
            name
        };
        let index = NAMES.iter().position(|n| *n == name);
        Self {
            start: index.map(|_| Instant::now()),
            index: index.unwrap_or(0),
        }
    }
}
impl Drop for Span {
    fn drop(&mut self) {
        if let Some(start) = self.start {
            record(self.index, start.elapsed());
        }
    }
}

/// Per-display successful present-call cadence. A skipped/failed draw neither
/// samples the clock nor moves the previous timestamp. First success seeds only.
/// These are CPU present-return intervals, not display refresh timestamps.
#[derive(Default)]
pub struct PresentationClock {
    previous: Option<Instant>,
}
impl PresentationClock {
    pub fn observe(&mut self, presented: bool) {
        if presented
            && enabled()
            && let Some(elapsed) = self.presented_at(Instant::now())
        {
            record(9, elapsed);
        }
    }
    fn presented_at(&mut self, now: Instant) -> Option<Duration> {
        self.previous
            .replace(now)
            .map(|previous| now.saturating_duration_since(previous))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bucket_boundaries_and_tail_ranks() {
        let mut t = Totals::EMPTY;
        assert_eq!(t.percentile_upper_us(99), 0);
        for _ in 0..50 {
            t.record(Duration::from_nanos(1));
        }
        for _ in 0..45 {
            t.record(Duration::from_micros(3));
        }
        for _ in 0..4 {
            t.record(Duration::from_micros(17));
        }
        t.record(Duration::from_micros(1_025));
        assert_eq!(t.count, 100);
        assert_eq!(t.percentile_upper_us(50), 1);
        assert_eq!(t.percentile_upper_us(95), 4);
        assert_eq!(t.percentile_upper_us(99), 32);
        assert_eq!(t.maximum, Duration::from_micros(1_025));
        for us in [0, 1, 2, 3, 4, 8, 9, 1 << 63, (1 << 63) + 1, u64::MAX] {
            let mut one = Totals::EMPTY;
            one.record(Duration::from_micros(us));
            assert!(one.percentile_upper_us(99) >= u128::from(us));
        }
    }
    #[test]
    fn saturation_is_bounded_and_reporting_does_not_stop_or_flood() {
        let mut t = Totals::EMPTY;
        t.count = u64::MAX;
        t.total = Duration::MAX;
        t.histogram[65] = u64::MAX;
        for sample in 1..=240 {
            assert_eq!(t.record(Duration::MAX), sample % 120 == 0);
        }
        assert_eq!(t.count, u64::MAX);
        assert_eq!(t.total, Duration::MAX);
        assert_eq!(t.histogram[65], u64::MAX);
        assert_eq!(t.percentile_upper_us(99), bucket_upper_us(65));
    }
    #[test]
    fn successful_present_intervals_seed_once_and_preserve_gaps() {
        let mut clock = PresentationClock::default();
        let first = Instant::now();
        assert_eq!(clock.presented_at(first), None);
        clock.observe(false); // timeout, zero-size or failed present: no advance
        assert_eq!(clock.previous, Some(first));
        assert_eq!(
            clock.presented_at(first + Duration::from_millis(35)),
            Some(Duration::from_millis(35))
        );
        assert_eq!(
            clock.presented_at(first + Duration::from_millis(51)),
            Some(Duration::from_millis(16))
        );
    }
}
