//! Session-scoped adaptive fetch tuning, modelled after mpd-hls live sessions.

use std::env;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const DEFAULT_SHARED_DOWNLOAD_CONCURRENCY: usize = 10;
const DEFAULT_SHORT_SHARED_DOWNLOAD_CONCURRENCY: usize = 10;
const DEFAULT_SEGMENT_FETCH_CONCURRENCY: usize = 10;
const DEFAULT_SHORT_SEGMENT_FETCH_CONCURRENCY: usize = 10;

const DEFAULT_MIN_SHARED_DOWNLOAD_CONCURRENCY: usize = 8;
const DEFAULT_SHORT_MIN_SHARED_DOWNLOAD_CONCURRENCY: usize = 8;
const DEFAULT_MIN_SEGMENT_FETCH_CONCURRENCY: usize = 6;
const DEFAULT_SHORT_MIN_SEGMENT_FETCH_CONCURRENCY: usize = 6;

const DEFAULT_SLOW_FETCH_MS: u64 = 3_500;
const DEFAULT_FAST_FETCH_MS: u64 = 1_200;
const DEFAULT_FAST_STREAK: usize = 8;
const DEFAULT_FAILURE_STREAK: usize = 2;
const DEFAULT_SLOW_COOLDOWN_MS: u64 = 750;

pub struct AdaptiveFetchTuning {
    cfg: FetchTuningConfig,
    state: Mutex<FetchTuningState>,
    in_flight: AtomicUsize,
    notify: Notify,
}

#[derive(Debug, Clone)]
struct FetchTuningConfig {
    enabled: bool,
    shared_default: usize,
    shared_short_default: usize,
    segment_default: usize,
    segment_short_default: usize,
    min_shared: usize,
    min_shared_short: usize,
    min_segment: usize,
    min_segment_short: usize,
    slow_fetch: Duration,
    fast_fetch: Duration,
    fast_streak_to_scale_up: usize,
    failure_streak_to_scale_down: usize,
    slow_scale_down: bool,
    slow_scale_down_cooldown: Duration,
}

#[derive(Debug, Clone)]
struct FetchTuningState {
    configured: bool,
    short_source: bool,
    shared_limit: usize,
    segment_limit: usize,
    fast_streak: usize,
    failure_streak: usize,
    last_slow_scale_down: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FetchTuningSnapshot {
    pub shared_download_concurrency: usize,
    pub segment_fetch_concurrency: usize,
    pub in_flight_downloads: usize,
    pub short_source: bool,
}

#[derive(Debug, Clone, Copy)]
enum ScaleReason {
    Failure,
    SlowFetch,
}

#[derive(Debug, Clone, Copy)]
struct Limits {
    shared_base: usize,
    segment_base: usize,
    shared_min: usize,
    segment_min: usize,
}

#[derive(Debug, Clone, Copy)]
struct Adjustment {
    previous_shared: usize,
    previous_segment: usize,
    current_shared: usize,
    current_segment: usize,
    short_source: bool,
    reason: ScaleReason,
}

impl AdaptiveFetchTuning {
    pub fn new() -> Self {
        let cfg = FetchTuningConfig::from_env();
        let limits = cfg.limits(false);
        Self {
            cfg,
            state: Mutex::new(FetchTuningState {
                configured: false,
                short_source: false,
                shared_limit: limits.shared_base,
                segment_limit: limits.segment_base,
                fast_streak: 0,
                failure_streak: 0,
                last_slow_scale_down: None,
            }),
            in_flight: AtomicUsize::new(0),
            notify: Notify::new(),
        }
    }

    pub fn configure_source(&self, task_id: &str, short_source: bool) {
        let snapshot = {
            let mut state = self.state.lock().expect("fetch tuning mutex poisoned");
            if state.configured && state.short_source == short_source {
                return;
            }
            let limits = self.cfg.limits(short_source);
            state.configured = true;
            state.short_source = short_source;
            state.shared_limit = state
                .shared_limit
                .clamp(limits.shared_min, limits.shared_base);
            state.segment_limit = state
                .segment_limit
                .clamp(limits.segment_min, limits.segment_base);
            state.fast_streak = 0;
            state.failure_streak = 0;
            state.last_slow_scale_down = None;
            FetchTuningSnapshot {
                shared_download_concurrency: state.shared_limit,
                segment_fetch_concurrency: state.segment_limit,
                in_flight_downloads: self.in_flight.load(Ordering::Acquire),
                short_source,
            }
        };

        self.notify.notify_waiters();
        tracing::info!(
            task = %task_id,
            short_source = snapshot.short_source,
            shared_download_concurrency = snapshot.shared_download_concurrency,
            segment_fetch_concurrency = snapshot.segment_fetch_concurrency,
            "using adaptive fetch concurrency"
        );
    }

    pub fn segment_fetch_concurrency(&self) -> usize {
        self.snapshot().segment_fetch_concurrency.max(1)
    }

    pub fn snapshot(&self) -> FetchTuningSnapshot {
        let state = self.state.lock().expect("fetch tuning mutex poisoned");
        FetchTuningSnapshot {
            shared_download_concurrency: state.shared_limit.max(1),
            segment_fetch_concurrency: state.segment_limit.max(1),
            in_flight_downloads: self.in_flight.load(Ordering::Acquire),
            short_source: state.short_source,
        }
    }

    pub async fn acquire_shared<'a>(
        &'a self,
        cancel: &CancellationToken,
    ) -> crate::Result<AdaptiveFetchPermit<'a>> {
        loop {
            let limit = self.snapshot().shared_download_concurrency.max(1);
            let current = self.in_flight.load(Ordering::Acquire);
            if current < limit {
                if self
                    .in_flight
                    .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    return Ok(AdaptiveFetchPermit { tuning: self });
                }
                continue;
            }

            tokio::select! {
                _ = self.notify.notified() => {}
                _ = cancel.cancelled() => {
                    return Err(anyhow::anyhow!("task cancelled while waiting for fetch slot"));
                }
            }
        }
    }

    pub fn record_success(&self, task_id: &str, elapsed: Duration) {
        if !self.cfg.enabled {
            return;
        }
        if elapsed >= self.cfg.slow_fetch {
            if self.cfg.slow_scale_down {
                if let Some(adjustment) = self.scale_down(ScaleReason::SlowFetch) {
                    self.log_adjustment(task_id, adjustment, elapsed);
                }
            } else {
                let mut state = self.state.lock().expect("fetch tuning mutex poisoned");
                state.fast_streak = 0;
                state.failure_streak = 0;
            }
            return;
        }

        let maybe_adjustment = {
            let mut state = self.state.lock().expect("fetch tuning mutex poisoned");
            state.failure_streak = 0;
            if elapsed > self.cfg.fast_fetch {
                state.fast_streak = 0;
                return;
            }

            state.fast_streak = state.fast_streak.saturating_add(1);
            if state.fast_streak < self.cfg.fast_streak_to_scale_up {
                return;
            }

            let limits = self.cfg.limits(state.short_source);
            let prev_shared = state.shared_limit;
            let prev_segment = state.segment_limit;
            state.shared_limit = state.shared_limit.saturating_add(1).min(limits.shared_base);
            state.segment_limit = state
                .segment_limit
                .saturating_add(1)
                .min(limits.segment_base);
            state.fast_streak = 0;

            if state.shared_limit != prev_shared || state.segment_limit != prev_segment {
                Some(Adjustment {
                    previous_shared: prev_shared,
                    previous_segment: prev_segment,
                    current_shared: state.shared_limit,
                    current_segment: state.segment_limit,
                    short_source: state.short_source,
                    reason: ScaleReason::Failure,
                })
            } else {
                None
            }
        };

        if let Some(adjustment) = maybe_adjustment {
            self.notify.notify_waiters();
            tracing::info!(
                task = %task_id,
                short_source = adjustment.short_source,
                previous_shared_download_concurrency = adjustment.previous_shared,
                previous_segment_fetch_concurrency = adjustment.previous_segment,
                shared_download_concurrency = adjustment.current_shared,
                segment_fetch_concurrency = adjustment.current_segment,
                elapsed_ms = elapsed.as_millis() as u64,
                "adaptive fetch tuning scaled up after consecutive fast fetches"
            );
        }
    }

    pub fn record_failure(&self, task_id: &str, elapsed: Duration) {
        if !self.cfg.enabled {
            return;
        }
        let should_scale_down = {
            let mut state = self.state.lock().expect("fetch tuning mutex poisoned");
            state.fast_streak = 0;
            state.failure_streak = state.failure_streak.saturating_add(1);
            if state.failure_streak < self.cfg.failure_streak_to_scale_down {
                tracing::warn!(
                    task = %task_id,
                    failure_streak = state.failure_streak,
                    threshold = self.cfg.failure_streak_to_scale_down,
                    elapsed_ms = elapsed.as_millis() as u64,
                    "saw isolated fetch failure; keeping concurrency"
                );
                false
            } else {
                state.failure_streak = 0;
                true
            }
        };

        if should_scale_down {
            if let Some(adjustment) = self.scale_down(ScaleReason::Failure) {
                self.log_adjustment(task_id, adjustment, elapsed);
            }
        }
    }

    pub fn record_ignored_failure(&self, task_id: &str, elapsed: Duration, kind: &str) {
        if !self.cfg.enabled {
            return;
        }
        let (shared_limit, segment_limit) = {
            let mut state = self.state.lock().expect("fetch tuning mutex poisoned");
            state.fast_streak = 0;
            state.failure_streak = 0;
            (state.shared_limit, state.segment_limit)
        };
        tracing::debug!(
            task = %task_id,
            kind,
            shared_download_concurrency = shared_limit,
            segment_fetch_concurrency = segment_limit,
            elapsed_ms = elapsed.as_millis() as u64,
            "fetch failure ignored for adaptive concurrency"
        );
    }

    fn scale_down(&self, reason: ScaleReason) -> Option<Adjustment> {
        let mut state = self.state.lock().expect("fetch tuning mutex poisoned");
        let now = Instant::now();
        if matches!(reason, ScaleReason::SlowFetch) {
            if let Some(last) = state.last_slow_scale_down {
                if now.duration_since(last) < self.cfg.slow_scale_down_cooldown {
                    state.fast_streak = 0;
                    return None;
                }
            }
            state.last_slow_scale_down = Some(now);
        }

        let limits = self.cfg.limits(state.short_source);
        let prev_shared = state.shared_limit;
        let prev_segment = state.segment_limit;
        state.shared_limit = state.shared_limit.saturating_sub(1).max(limits.shared_min);
        state.segment_limit = state
            .segment_limit
            .saturating_sub(1)
            .max(limits.segment_min);
        state.fast_streak = 0;

        if state.shared_limit == prev_shared && state.segment_limit == prev_segment {
            return None;
        }

        Some(Adjustment {
            previous_shared: prev_shared,
            previous_segment: prev_segment,
            current_shared: state.shared_limit,
            current_segment: state.segment_limit,
            short_source: state.short_source,
            reason,
        })
    }

    fn log_adjustment(&self, task_id: &str, adjustment: Adjustment, elapsed: Duration) {
        match adjustment.reason {
            ScaleReason::Failure => {
                tracing::warn!(
                    task = %task_id,
                    short_source = adjustment.short_source,
                    previous_shared_download_concurrency = adjustment.previous_shared,
                    previous_segment_fetch_concurrency = adjustment.previous_segment,
                    shared_download_concurrency = adjustment.current_shared,
                    segment_fetch_concurrency = adjustment.current_segment,
                    elapsed_ms = elapsed.as_millis() as u64,
                    "adaptive fetch tuning scaled down after failure"
                );
            }
            ScaleReason::SlowFetch => {
                tracing::warn!(
                    task = %task_id,
                    short_source = adjustment.short_source,
                    previous_shared_download_concurrency = adjustment.previous_shared,
                    previous_segment_fetch_concurrency = adjustment.previous_segment,
                    shared_download_concurrency = adjustment.current_shared,
                    segment_fetch_concurrency = adjustment.current_segment,
                    elapsed_ms = elapsed.as_millis() as u64,
                    slow_ms = self.cfg.slow_fetch.as_millis() as u64,
                    "adaptive fetch tuning scaled down after slow fetch"
                );
            }
        }
    }
}

impl Default for AdaptiveFetchTuning {
    fn default() -> Self {
        Self::new()
    }
}

pub struct AdaptiveFetchPermit<'a> {
    tuning: &'a AdaptiveFetchTuning,
}

impl Drop for AdaptiveFetchPermit<'_> {
    fn drop(&mut self) {
        self.tuning.in_flight.fetch_sub(1, Ordering::AcqRel);
        self.tuning.notify.notify_one();
    }
}

impl FetchTuningConfig {
    fn from_env() -> Self {
        Self {
            enabled: env_bool("MPD_HLS_ADAPTIVE_FETCH").unwrap_or(true),
            shared_default: env_usize("MPD_HLS_SHARED_DOWNLOAD_CONCURRENCY")
                .unwrap_or(DEFAULT_SHARED_DOWNLOAD_CONCURRENCY)
                .max(1),
            shared_short_default: env_usize("MPD_HLS_SHORT_SHARED_DOWNLOAD_CONCURRENCY")
                .unwrap_or(DEFAULT_SHORT_SHARED_DOWNLOAD_CONCURRENCY)
                .max(1),
            segment_default: env_usize("MPD_HLS_SEGMENT_FETCH_CONCURRENCY")
                .unwrap_or(DEFAULT_SEGMENT_FETCH_CONCURRENCY)
                .max(1),
            segment_short_default: env_usize("MPD_HLS_SHORT_SEGMENT_FETCH_CONCURRENCY")
                .unwrap_or(DEFAULT_SHORT_SEGMENT_FETCH_CONCURRENCY)
                .max(1),
            min_shared: env_usize("MPD_HLS_MIN_SHARED_DOWNLOAD_CONCURRENCY")
                .unwrap_or(DEFAULT_MIN_SHARED_DOWNLOAD_CONCURRENCY)
                .max(1),
            min_shared_short: env_usize("MPD_HLS_SHORT_MIN_SHARED_DOWNLOAD_CONCURRENCY")
                .unwrap_or(DEFAULT_SHORT_MIN_SHARED_DOWNLOAD_CONCURRENCY)
                .max(1),
            min_segment: env_usize("MPD_HLS_MIN_SEGMENT_FETCH_CONCURRENCY")
                .unwrap_or(DEFAULT_MIN_SEGMENT_FETCH_CONCURRENCY)
                .max(1),
            min_segment_short: env_usize("MPD_HLS_SHORT_MIN_SEGMENT_FETCH_CONCURRENCY")
                .unwrap_or(DEFAULT_SHORT_MIN_SEGMENT_FETCH_CONCURRENCY)
                .max(1),
            slow_fetch: Duration::from_millis(
                env_u64("MPD_HLS_ADAPTIVE_SLOW_FETCH_MS").unwrap_or(DEFAULT_SLOW_FETCH_MS),
            ),
            fast_fetch: Duration::from_millis(
                env_u64("MPD_HLS_ADAPTIVE_FAST_FETCH_MS").unwrap_or(DEFAULT_FAST_FETCH_MS),
            ),
            fast_streak_to_scale_up: env_usize("MPD_HLS_ADAPTIVE_FAST_STREAK")
                .unwrap_or(DEFAULT_FAST_STREAK)
                .max(1),
            failure_streak_to_scale_down: env_usize("MPD_HLS_ADAPTIVE_FAILURE_STREAK")
                .unwrap_or(DEFAULT_FAILURE_STREAK)
                .max(1),
            slow_scale_down: env_bool("MPD_HLS_ADAPTIVE_SLOW_SCALE_DOWN").unwrap_or(false),
            slow_scale_down_cooldown: Duration::from_millis(
                env_u64("MPD_HLS_ADAPTIVE_SLOW_COOLDOWN_MS").unwrap_or(DEFAULT_SLOW_COOLDOWN_MS),
            ),
        }
    }

    fn limits(&self, short_source: bool) -> Limits {
        let (shared_base, segment_base, shared_min, segment_min) = if short_source {
            (
                self.shared_short_default,
                self.segment_short_default,
                self.min_shared_short,
                self.min_segment_short,
            )
        } else {
            (
                self.shared_default,
                self.segment_default,
                self.min_shared,
                self.min_segment,
            )
        };

        Limits {
            shared_base: shared_base.max(shared_min).max(1),
            segment_base: segment_base.max(segment_min).max(1),
            shared_min: shared_min.min(shared_base).max(1),
            segment_min: segment_min.min(segment_base).max(1),
        }
    }
}

fn env_usize(name: &str) -> Option<usize> {
    env::var(name).ok()?.trim().parse().ok()
}

fn env_u64(name: &str) -> Option<u64> {
    env::var(name).ok()?.trim().parse().ok()
}

fn env_bool(name: &str) -> Option<bool> {
    let value = env::var(name).ok()?;
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_source_uses_throughput_first_defaults() {
        let tuning = AdaptiveFetchTuning::new();
        tuning.configure_source("test", true);
        let snapshot = tuning.snapshot();
        assert_eq!(snapshot.shared_download_concurrency, 10);
        assert_eq!(snapshot.segment_fetch_concurrency, 10);
    }

    #[test]
    fn isolated_failure_keeps_concurrency() {
        let tuning = AdaptiveFetchTuning::new();
        tuning.configure_source("test", true);
        tuning.record_failure("test", Duration::from_millis(100));
        let snapshot = tuning.snapshot();
        assert_eq!(snapshot.shared_download_concurrency, 10);
        assert_eq!(snapshot.segment_fetch_concurrency, 10);
    }

    #[test]
    fn consecutive_failures_scale_down_to_short_minimums() {
        let tuning = AdaptiveFetchTuning::new();
        tuning.configure_source("test", true);
        for _ in 0..8 {
            tuning.record_failure("test", Duration::from_millis(100));
        }
        let snapshot = tuning.snapshot();
        assert_eq!(snapshot.shared_download_concurrency, 8);
        assert_eq!(snapshot.segment_fetch_concurrency, 6);
    }

    #[test]
    fn ignored_failure_breaks_pressure_failure_streak() {
        let tuning = AdaptiveFetchTuning::new();
        tuning.configure_source("test", false);
        tuning.record_failure("test", Duration::from_millis(100));
        tuning.record_ignored_failure("test", Duration::from_millis(100), "http_status_404");
        tuning.record_failure("test", Duration::from_millis(100));
        let snapshot = tuning.snapshot();
        assert_eq!(snapshot.shared_download_concurrency, 10);
        assert_eq!(snapshot.segment_fetch_concurrency, 10);
    }

    #[test]
    fn slow_success_does_not_scale_down_by_default() {
        let tuning = AdaptiveFetchTuning::new();
        tuning.configure_source("test", true);
        tuning.record_success("test", Duration::from_secs(30));
        let snapshot = tuning.snapshot();
        assert_eq!(snapshot.shared_download_concurrency, 10);
        assert_eq!(snapshot.segment_fetch_concurrency, 10);
    }

    #[test]
    fn consecutive_fast_fetches_recover_one_step() {
        let tuning = AdaptiveFetchTuning::new();
        tuning.configure_source("test", false);
        tuning.record_failure("test", Duration::from_millis(100));
        tuning.record_failure("test", Duration::from_millis(100));
        let reduced = tuning.snapshot();
        assert_eq!(reduced.shared_download_concurrency, 9);
        assert_eq!(reduced.segment_fetch_concurrency, 9);

        for _ in 0..DEFAULT_FAST_STREAK {
            tuning.record_success("test", Duration::from_millis(50));
        }
        let recovered = tuning.snapshot();
        assert_eq!(recovered.shared_download_concurrency, 10);
        assert_eq!(recovered.segment_fetch_concurrency, 10);
    }
}
