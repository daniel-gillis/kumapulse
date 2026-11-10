//! Domain model shared by every data source.
//!
//! Uptime Kuma stores a heartbeat `status` as an integer. The mapping is fixed by
//! Kuma's own constants, so it is mirrored here rather than being a stringly
//! typed guess:
//!
//! | value | meaning      |
//! |-------|--------------|
//! | 0     | down         |
//! | 1     | up           |
//! | 2     | pending      |
//! | 3     | maintenance  |

use std::time::{Duration, SystemTime};

/// One monitored endpoint as Uptime Kuma defines it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Monitor {
    pub id: u64,
    pub name: String,
    /// Kuma monitor type: http, tcp, ping, keyword, json-query, dns, push, …
    pub kind: String,
    /// Target: a URL, hostname, or a synthetic description for push monitors.
    pub target: String,
    /// Status-page group / parent, empty when the monitor is not grouped.
    pub group: String,
    pub active: bool,
}

/// A single heartbeat sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Beat {
    pub status: Status,
    /// Round-trip time in milliseconds. Kuma writes 0 for monitors that do not
    /// measure latency (push, and some DNS types).
    pub ping_ms: f64,
    /// When the beat was recorded, in milliseconds since the Unix epoch (Kuma's format).
    pub at_ms: i64,
}

/// Heartbeat status, mirroring Kuma's integer constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Down,
    Up,
    Pending,
    Maintenance,
    /// A value this exporter does not know about. Kept distinct so a future Kuma
    /// release cannot silently look like "down" and page someone.
    Unknown(i64),
}

impl Status {
    #[must_use]
    pub const fn from_i64(value: i64) -> Self {
        match value {
            0 => Self::Down,
            1 => Self::Up,
            2 => Self::Pending,
            3 => Self::Maintenance,
            other => Self::Unknown(other),
        }
    }

    /// Numeric status for the `kumapulse_monitor_status` gauge.
    #[must_use]
    pub const fn code(self) -> i64 {
        match self {
            Self::Down => 0,
            Self::Up => 1,
            Self::Pending => 2,
            Self::Maintenance => 3,
            Self::Unknown(v) => v,
        }
    }

    /// Only up/down beats count towards an uptime ratio: pending and maintenance
    /// states are not evidence of availability or of an outage.
    #[must_use]
    pub const fn counts_for_uptime(self) -> bool {
        matches!(self, Self::Up | Self::Down)
    }
}

/// Everything known about one monitor at scrape time.
#[derive(Debug, Clone)]
pub struct MonitorState {
    pub monitor: Monitor,
    /// Most recent heartbeat, if the monitor has ever run.
    pub last: Option<Beat>,
    /// Heartbeats inside the retention window, newest last.
    pub beats: Vec<Beat>,
    /// Certificate expiry (epoch seconds) when the source exposes it.
    pub tls_expiry: Option<i64>,
}

/// One consistent view of the whole instance.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub monitors: Vec<MonitorState>,
    pub scraped_at: SystemTime,
    pub source: &'static str,
}

impl Snapshot {
    #[must_use]
    pub fn counts(&self) -> Counts {
        let mut counts = Counts::default();
        for state in &self.monitors {
            counts.monitors += 1;
            match state.last.as_ref().map(|b| b.status) {
                Some(Status::Up) => counts.up += 1,
                Some(Status::Down) => counts.down += 1,
                Some(Status::Pending) => counts.pending += 1,
                Some(Status::Maintenance) => counts.maintenance += 1,
                Some(Status::Unknown(_)) | None => counts.unknown += 1,
            }
        }
        counts
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub monitors: usize,
    pub up: usize,
    pub down: usize,
    pub pending: usize,
    pub maintenance: usize,
    pub unknown: usize,
}

/// Aggregates over one window of heartbeats.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Window {
    pub uptime_ratio: Option<f64>,
    pub heartbeats_up: usize,
    pub heartbeats_down: usize,
    /// p50 / p90 / p99 of the round-trip time, over beats that measured one.
    pub p50_ms: Option<f64>,
    pub p90_ms: Option<f64>,
    pub p99_ms: Option<f64>,
}

impl Window {
    /// Aggregate `beats` restricted to the last `window`.
    #[must_use]
    pub fn of(beats: &[Beat], now_ms: i64, window: Duration) -> Self {
        let cutoff = now_ms - i64::try_from(window.as_millis()).unwrap_or(i64::MAX);
        let mut up = 0usize;
        let mut down = 0usize;
        let mut pings: Vec<f64> = Vec::new();
        for beat in beats.iter().filter(|b| b.at_ms >= cutoff) {
            if beat.status.counts_for_uptime() {
                if beat.status == Status::Up {
                    up += 1;
                } else {
                    down += 1;
                }
            }
            if beat.ping_ms > 0.0 {
                pings.push(beat.ping_ms);
            }
        }
        let total = up + down;
        pings.sort_by(f64::total_cmp);
        Self {
            uptime_ratio: (total > 0).then(|| up as f64 / total as f64),
            heartbeats_up: up,
            heartbeats_down: down,
            p50_ms: quantile(&pings, 0.50),
            p90_ms: quantile(&pings, 0.90),
            p99_ms: quantile(&pings, 0.99),
        }
    }
}

/// Nearest-rank quantile over already-sorted samples.
fn quantile(sorted: &[f64], q: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (q * sorted.len() as f64).ceil() as usize;
    let index = rank.saturating_sub(1).min(sorted.len() - 1);
    Some(sorted[index])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn beat(status: Status, ping_ms: f64, at_ms: i64) -> Beat {
        Beat {
            status,
            ping_ms,
            at_ms,
        }
    }

    #[test]
    fn unknown_status_is_not_treated_as_down() {
        assert_eq!(Status::from_i64(99), Status::Unknown(99));
        assert!(!Status::Unknown(99).counts_for_uptime());
    }

    #[test]
    fn uptime_ratio_ignores_pending_and_maintenance() {
        let beats = vec![
            beat(Status::Up, 10.0, 1_000),
            beat(Status::Down, 10.0, 2_000),
            beat(Status::Pending, 0.0, 3_000),
            beat(Status::Maintenance, 0.0, 4_000),
        ];
        let window = Window::of(&beats, 5_000, Duration::from_secs(60));
        assert_eq!(window.uptime_ratio, Some(0.5));
        assert_eq!(window.heartbeats_up, 1);
        assert_eq!(window.heartbeats_down, 1);
    }

    #[test]
    fn window_excludes_old_beats() {
        let now = 100_000_000;
        let beats = vec![
            beat(Status::Up, 10.0, now - 90_000), // outside 60s by a wide margin
            beat(Status::Up, 20.0, now - 1_000),
        ];
        let window = Window::of(&beats, now, Duration::from_secs(60));
        assert_eq!(window.heartbeats_up, 1);
        assert_eq!(window.p50_ms, Some(20.0));
    }

    #[test]
    fn quantiles_use_nearest_rank() {
        let sorted = [1.0, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(quantile(&sorted, 0.50), Some(3.0));
        assert_eq!(quantile(&sorted, 0.90), Some(5.0));
        assert_eq!(quantile(&sorted, 0.99), Some(5.0));
        assert_eq!(quantile(&[], 0.5), None);
    }

    #[test]
    fn monitors_without_beats_are_counted_as_unknown() {
        let snapshot = Snapshot {
            monitors: vec![MonitorState {
                monitor: Monitor {
                    id: 1,
                    name: "new".into(),
                    kind: "http".into(),
                    target: "https://example.test".into(),
                    group: String::new(),
                    active: true,
                },
                last: None,
                beats: Vec::new(),
                tls_expiry: None,
            }],
            scraped_at: SystemTime::now(),
            source: "test",
        };
        assert_eq!(snapshot.counts().unknown, 1);
    }
}
