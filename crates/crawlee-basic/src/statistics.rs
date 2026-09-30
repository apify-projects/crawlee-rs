//! Crawl statistics. Field names of [`FinalStatistics`] follow `FinalStatistics` of Crawlee for JS.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::Serialize;

/// Live counters, updated concurrently by request tasks.
#[derive(Debug)]
pub struct Statistics {
    started_at: Mutex<Option<Instant>>,
    finished_at: Mutex<Option<Instant>>,
    succeeded: AtomicU64,
    failed: AtomicU64,
    retries: AtomicU64,
    skipped: AtomicU64,
    succeeded_millis: AtomicU64,
    failed_millis: AtomicU64,
    min_millis: AtomicU64,
    max_millis: AtomicU64,
    detail: Mutex<Detail>,
}

#[derive(Debug, Default)]
struct Detail {
    /// `retry_histogram[n]` = requests that finished after `n` retries.
    retry_histogram: Vec<u64>,
    status_codes: BTreeMap<u16, u64>,
    errors: BTreeMap<String, u64>,
    retry_errors: BTreeMap<String, u64>,
}

impl Default for Statistics {
    fn default() -> Self {
        Statistics {
            started_at: Mutex::new(None),
            finished_at: Mutex::new(None),
            succeeded: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            retries: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            succeeded_millis: AtomicU64::new(0),
            failed_millis: AtomicU64::new(0),
            min_millis: AtomicU64::new(u64::MAX),
            max_millis: AtomicU64::new(0),
            detail: Mutex::new(Detail::default()),
        }
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Groups errors by their message with digits masked, so `timeout after 1234ms` and
/// `timeout after 99ms` land in the same bucket.
fn error_group(message: &str) -> String {
    let first_line = message.lines().next().unwrap_or_default();
    let mut out = String::with_capacity(first_line.len().min(200));
    let mut last_was_digit = false;
    for c in first_line.chars().take(200) {
        if c.is_ascii_digit() {
            if !last_was_digit {
                out.push('_');
            }
            last_was_digit = true;
        } else {
            out.push(c);
            last_was_digit = false;
        }
    }
    out
}

impl Statistics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn start(&self) {
        *self.started_at.lock() = Some(Instant::now());
        *self.finished_at.lock() = None;
    }

    pub fn finish(&self) {
        *self.finished_at.lock() = Some(Instant::now());
    }

    pub fn record_success(&self, duration: Duration, retry_count: u32) {
        self.succeeded.fetch_add(1, Ordering::Relaxed);
        self.record_duration(duration, &self.succeeded_millis);
        self.record_retries(retry_count);
    }

    pub fn record_failure(&self, duration: Duration, retry_count: u32) {
        self.failed.fetch_add(1, Ordering::Relaxed);
        self.record_duration(duration, &self.failed_millis);
        self.record_retries(retry_count);
    }

    pub fn record_skipped(&self) {
        self.skipped.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_retry(&self, message: &str) {
        self.retries.fetch_add(1, Ordering::Relaxed);
        *self.detail.lock().retry_errors.entry(error_group(message)).or_default() += 1;
    }

    pub fn record_error(&self, message: &str) {
        *self.detail.lock().errors.entry(error_group(message)).or_default() += 1;
    }

    pub fn record_status_code(&self, status: u16) {
        *self.detail.lock().status_codes.entry(status).or_default() += 1;
    }

    fn record_duration(&self, duration: Duration, total: &AtomicU64) {
        let ms = millis(duration);
        total.fetch_add(ms, Ordering::Relaxed);
        self.min_millis.fetch_min(ms, Ordering::Relaxed);
        self.max_millis.fetch_max(ms, Ordering::Relaxed);
    }

    fn record_retries(&self, retry_count: u32) {
        let mut detail = self.detail.lock();
        let index = retry_count as usize;
        if detail.retry_histogram.len() <= index {
            detail.retry_histogram.resize(index + 1, 0);
        }
        detail.retry_histogram[index] += 1;
    }

    pub fn requests_succeeded(&self) -> u64 {
        self.succeeded.load(Ordering::Relaxed)
    }

    pub fn requests_failed(&self) -> u64 {
        self.failed.load(Ordering::Relaxed)
    }

    /// Requests finished either way (skipped ones included).
    pub fn requests_finished(&self) -> u64 {
        self.requests_succeeded() + self.requests_failed() + self.skipped.load(Ordering::Relaxed)
    }

    pub fn snapshot(&self) -> FinalStatistics {
        let succeeded = self.requests_succeeded();
        let failed = self.requests_failed();
        let runtime = match (*self.started_at.lock(), *self.finished_at.lock()) {
            (Some(start), Some(end)) => end.duration_since(start),
            (Some(start), None) => start.elapsed(),
            _ => Duration::ZERO,
        };
        let minutes = runtime.as_secs_f64() / 60.0;
        let per_minute = |count: u64| if minutes > 0.0 { (count as f64 / minutes).round() as u64 } else { 0 };
        let avg = |total: &AtomicU64, count: u64| if count > 0 { total.load(Ordering::Relaxed) / count } else { 0 };
        let detail = self.detail.lock();

        FinalStatistics {
            requests_succeeded: succeeded,
            requests_failed: failed,
            requests_skipped: self.skipped.load(Ordering::Relaxed),
            requests_total: succeeded + failed,
            requests_retries: self.retries.load(Ordering::Relaxed),
            request_avg_succeeded_duration_millis: avg(&self.succeeded_millis, succeeded),
            request_avg_failed_duration_millis: avg(&self.failed_millis, failed),
            request_min_duration_millis: if succeeded + failed > 0 {
                self.min_millis.load(Ordering::Relaxed)
            } else {
                0
            },
            request_max_duration_millis: self.max_millis.load(Ordering::Relaxed),
            requests_succeeded_per_minute: per_minute(succeeded),
            requests_failed_per_minute: per_minute(failed),
            request_total_duration_millis: self.succeeded_millis.load(Ordering::Relaxed)
                + self.failed_millis.load(Ordering::Relaxed),
            crawler_runtime_millis: millis(runtime),
            retry_histogram: detail.retry_histogram.clone(),
            status_codes: detail.status_codes.clone(),
            errors: detail.errors.clone(),
            retry_errors: detail.retry_errors.clone(),
        }
    }
}

/// Statistics returned by `run()`.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FinalStatistics {
    pub requests_succeeded: u64,
    pub requests_failed: u64,
    pub requests_skipped: u64,
    pub requests_total: u64,
    pub requests_retries: u64,
    pub request_avg_succeeded_duration_millis: u64,
    pub request_avg_failed_duration_millis: u64,
    pub request_min_duration_millis: u64,
    pub request_max_duration_millis: u64,
    pub requests_succeeded_per_minute: u64,
    pub requests_failed_per_minute: u64,
    pub request_total_duration_millis: u64,
    pub crawler_runtime_millis: u64,
    pub retry_histogram: Vec<u64>,
    pub status_codes: BTreeMap<u16, u64>,
    pub errors: BTreeMap<String, u64>,
    pub retry_errors: BTreeMap<String, u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_groups() {
        let stats = Statistics::new();
        stats.start();
        stats.record_success(Duration::from_millis(100), 0);
        stats.record_success(Duration::from_millis(300), 2);
        stats.record_failure(Duration::from_millis(50), 3);
        stats.record_error("timed out after 1234 ms");
        stats.record_error("timed out after 99 ms");
        stats.record_status_code(404);
        stats.finish();

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.requests_succeeded, 2);
        assert_eq!(snapshot.requests_total, 3);
        assert_eq!(snapshot.request_avg_succeeded_duration_millis, 200);
        assert_eq!(snapshot.request_min_duration_millis, 50);
        assert_eq!(snapshot.retry_histogram, vec![1, 0, 1, 1]);
        assert_eq!(snapshot.errors.get("timed out after _ ms"), Some(&2));
        assert_eq!(snapshot.status_codes.get(&404), Some(&1));
    }
}
