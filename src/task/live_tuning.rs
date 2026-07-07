//! Live pipeline tuning shared by MPD and HLS inputs.

use std::time::Duration;

pub const SHORT_SEG_THRESHOLD_S: f64 = 4.0;
pub const AGGREGATE_TARGET_S: f64 = 8.0;

const DEFAULT_PUBLISH_DELAY_SEGMENTS: usize = 1;
const DEFAULT_SHORT_PUBLISH_DELAY_SEGMENTS: usize = 1;
const MIN_INITIAL_PUBLISHED_SEGMENTS: usize = 3;
const MIN_REFRESH_WAIT: Duration = Duration::from_millis(250);

pub fn is_short_segment(duration_secs: f64) -> bool {
    duration_secs.is_finite() && duration_secs > 0.0 && duration_secs < SHORT_SEG_THRESHOLD_S
}

pub fn publish_delay_segments(is_live: bool, short_source: bool) -> usize {
    if !is_live {
        return 0;
    }
    let (name, default) = if short_source {
        (
            "MPD_HLS_SHORT_PUBLISH_DELAY_SEGMENTS",
            DEFAULT_SHORT_PUBLISH_DELAY_SEGMENTS,
        )
    } else {
        (
            "MPD_HLS_PUBLISH_DELAY_SEGMENTS",
            DEFAULT_PUBLISH_DELAY_SEGMENTS,
        )
    };
    env_usize(name).unwrap_or(default).max(1)
}

pub fn initial_live_backfill_segments(segment_duration_secs: f64, publish_delay: usize) -> u64 {
    let group_size = if is_short_segment(segment_duration_secs) {
        (AGGREGATE_TARGET_S / segment_duration_secs).ceil() as usize
    } else {
        1
    }
    .max(1);
    let wanted = group_size.saturating_mul(publish_delay + MIN_INITIAL_PUBLISHED_SEGMENTS);
    wanted.max(4) as u64
}

pub fn hls_refresh_wait(target_duration_secs: f64, elapsed: Duration) -> Duration {
    let base = if target_duration_secs.is_finite() && target_duration_secs > 0.0 {
        (target_duration_secs * 0.5).clamp(1.0, 6.0)
    } else {
        3.0
    };
    remaining_wait(base, elapsed)
}

pub fn media_refresh_wait(segment_duration_secs: f64, elapsed: Duration) -> Duration {
    let base = if is_short_segment(segment_duration_secs) {
        (segment_duration_secs * 0.5).clamp(1.0, 6.0)
    } else if segment_duration_secs.is_finite() && segment_duration_secs > 0.0 {
        segment_duration_secs.clamp(1.0, 10.0)
    } else {
        4.0
    };
    remaining_wait(base, elapsed)
}

fn remaining_wait(base_secs: f64, elapsed: Duration) -> Duration {
    let base = Duration::from_secs_f64(base_secs);
    base.checked_sub(elapsed).unwrap_or(MIN_REFRESH_WAIT)
}

fn env_usize(name: &str) -> Option<usize> {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
}
