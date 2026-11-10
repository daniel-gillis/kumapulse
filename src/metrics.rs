//! Prometheus text exposition.
//!
//! The format is written by hand rather than pulled from a client library: it is
//! a few hundred lines of escaping, and it keeps the dependency graph (and the
//! binary) small. Output is deterministic — families are emitted in a fixed
//! order and monitors sorted by id — so a diff between two scrapes shows only
//! real changes.

use std::fmt::Write as _;
use std::time::{Duration, SystemTime};

use crate::model::{MonitorState, Snapshot, Status, Window};

/// How long a heartbeat counts towards each uptime window.
pub const WINDOWS: [(&str, Duration); 3] = [
    ("24h", Duration::from_secs(24 * 60 * 60)),
    ("7d", Duration::from_secs(7 * 24 * 60 * 60)),
    ("30d", Duration::from_secs(30 * 24 * 60 * 60)),
];

const HELP_PREFIX: &str = "Prometheus exporter for Uptime Kuma.";

/// Build metadata, compiled in.
#[derive(Debug, Clone, Copy)]
pub struct BuildInfo {
    pub version: &'static str,
    pub rustc: &'static str,
}

/// Per-scrape diagnostics.
#[derive(Debug, Clone, Copy)]
pub struct ScrapeStats {
    pub duration: Duration,
    pub success: bool,
    pub cache_age: Duration,
    pub beats_considered: usize,
}

/// Render the whole exposition.
#[must_use]
pub fn render(snapshot: &Snapshot, build: BuildInfo, stats: ScrapeStats) -> String {
    let mut out = String::with_capacity(4096 + snapshot.monitors.len() * 320);
    let now_ms = i64::try_from(
        snapshot
            .scraped_at
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis()),
    )
    .unwrap_or(i64::MAX);
    let mut monitors: Vec<&MonitorState> = snapshot.monitors.iter().collect();
    monitors.sort_by_key(|m| m.monitor.id);

    family(
        &mut out,
        "kumapulse_build_info",
        "gauge",
        &format!("{HELP_PREFIX} Build information."),
    );
    let _ = writeln!(
        out,
        "kumapulse_build_info{{version=\"{}\",rustc=\"{}\"}} 1",
        escape(build.version),
        escape(build.rustc)
    );

    family(
        &mut out,
        "kumapulse_source_info",
        "gauge",
        "Data source the exporter is currently reading.",
    );
    let _ = writeln!(
        out,
        "kumapulse_source_info{{source=\"{}\"}} 1",
        escape(snapshot.source)
    );

    family(
        &mut out,
        "kumapulse_scrape_success",
        "gauge",
        "Whether the last upstream read succeeded.",
    );
    let _ = writeln!(out, "kumapulse_scrape_success {}", u8::from(stats.success));

    family(
        &mut out,
        "kumapulse_scrape_duration_seconds",
        "gauge",
        "Time spent reading and aggregating the last scrape.",
    );
    let _ = writeln!(
        out,
        "kumapulse_scrape_duration_seconds {:.6}",
        stats.duration.as_secs_f64()
    );

    family(
        &mut out,
        "kumapulse_cache_age_seconds",
        "gauge",
        "Age of the snapshot being served; 0 on a fresh read.",
    );
    let _ = writeln!(
        out,
        "kumapulse_cache_age_seconds {:.3}",
        stats.cache_age.as_secs_f64()
    );

    family(
        &mut out,
        "kumapulse_monitors_total",
        "gauge",
        "Monitors known to the exporter.",
    );
    let _ = writeln!(out, "kumapulse_monitors_total {}", snapshot.monitors.len());

    let counts = snapshot.counts();
    family(
        &mut out,
        "kumapulse_monitors_by_status",
        "gauge",
        "Monitors by the outcome of their latest heartbeat.",
    );
    for (status, value) in [
        ("up", counts.up),
        ("down", counts.down),
        ("pending", counts.pending),
        ("maintenance", counts.maintenance),
        ("unknown", counts.unknown),
    ] {
        let _ = writeln!(
            out,
            "kumapulse_monitors_by_status{{status=\"{status}\"}} {value}"
        );
    }

    family(
        &mut out,
        "kumapulse_heartbeats_considered",
        "gauge",
        "Heartbeats held in memory for window aggregation.",
    );
    let _ = writeln!(
        out,
        "kumapulse_heartbeats_considered {}",
        stats.beats_considered
    );

    // ── per monitor ────────────────────────────────────────────────────────
    family(
        &mut out,
        "kumapulse_monitor_up",
        "gauge",
        &format!("{HELP_PREFIX} 1 when the latest heartbeat succeeded, 0 otherwise."),
    );
    for m in &monitors {
        let label_set = labels(m);
        let up = u8::from(matches!(m.last.map(|b| b.status), Some(Status::Up)));
        let _ = writeln!(out, "kumapulse_monitor_up{{{label_set}}} {up}");
    }

    family(
        &mut out,
        "kumapulse_monitor_status",
        "gauge",
        "Latest heartbeat status: 0=down, 1=up, 2=pending, 3=maintenance.",
    );
    for m in &monitors {
        if let Some(beat) = m.last {
            let _ = writeln!(
                out,
                "kumapulse_monitor_status{{{}}} {}",
                labels(m),
                beat.status.code()
            );
        }
    }

    family(
        &mut out,
        "kumapulse_monitor_active",
        "gauge",
        "1 when the monitor is enabled in Uptime Kuma.",
    );
    for m in &monitors {
        let _ = writeln!(
            out,
            "kumapulse_monitor_active{{{}}} {}",
            labels(m),
            u8::from(m.monitor.active)
        );
    }

    family(
        &mut out,
        "kumapulse_monitor_response_seconds",
        "gauge",
        "Round-trip time of the latest heartbeat, in seconds.",
    );
    for m in &monitors {
        if let Some(beat) = m.last {
            if beat.ping_ms > 0.0 {
                let _ = writeln!(
                    out,
                    "kumapulse_monitor_response_seconds{{{}}} {:.6}",
                    labels(m),
                    beat.ping_ms / 1000.0
                );
            }
        }
    }

    family(
        &mut out,
        "kumapulse_monitor_last_check_timestamp_seconds",
        "gauge",
        "Unix timestamp of the latest heartbeat.",
    );
    for m in &monitors {
        if let Some(beat) = m.last {
            let _ = writeln!(
                out,
                "kumapulse_monitor_last_check_timestamp_seconds{{{}}} {:.3}",
                labels(m),
                beat.at_ms as f64 / 1000.0
            );
        }
    }

    family(
        &mut out,
        "kumapulse_monitor_heartbeat_age_seconds",
        "gauge",
        "Seconds since the latest heartbeat; rising means checks have stalled.",
    );
    for m in &monitors {
        if let Some(beat) = m.last {
            let age = (now_ms - beat.at_ms).max(0) as f64 / 1000.0;
            let _ = writeln!(
                out,
                "kumapulse_monitor_heartbeat_age_seconds{{{}}} {age:.3}",
                labels(m)
            );
        }
    }

    family(
        &mut out,
        "kumapulse_monitor_tls_expiry_timestamp_seconds",
        "gauge",
        "Unix timestamp when the monitored certificate expires, when known.",
    );
    for m in &monitors {
        if let Some(expiry) = m.tls_expiry {
            let _ = writeln!(
                out,
                "kumapulse_monitor_tls_expiry_timestamp_seconds{{{}}} {expiry}",
                labels(m)
            );
        }
    }

    for (window_name, window_len) in WINDOWS {
        let aggregated: Vec<(&MonitorState, Window)> = monitors
            .iter()
            .map(|m| (*m, Window::of(&m.beats, now_ms, window_len)))
            .collect();

        family(&mut out, "kumapulse_monitor_uptime_ratio", "gauge",
               "Share of up heartbeats in the window (1 = always up). Pending and maintenance beats are excluded.");
        for (m, w) in &aggregated {
            if let Some(ratio) = w.uptime_ratio {
                let _ = writeln!(
                    out,
                    "kumapulse_monitor_uptime_ratio{{{},window=\"{window_name}\"}} {ratio:.6}",
                    labels(m)
                );
            }
        }

        family(
            &mut out,
            "kumapulse_monitor_heartbeats",
            "gauge",
            "Heartbeats by outcome inside the window.",
        );
        for (m, w) in &aggregated {
            for (outcome, count) in &[("up", w.heartbeats_up), ("down", w.heartbeats_down)] {
                let _ = writeln!(out,
                    "kumapulse_monitor_heartbeats{{{},window=\"{window_name}\",status=\"{outcome}\"}} {count}",
                    labels(m));
            }
        }

        family(
            &mut out,
            "kumapulse_monitor_response_seconds_quantile",
            "gauge",
            "Round-trip time quantiles over the window, in seconds.",
        );
        for (m, w) in &aggregated {
            for (quantile, value) in [("0.5", w.p50_ms), ("0.9", w.p90_ms), ("0.99", w.p99_ms)] {
                if let Some(ms) = value {
                    let _ = writeln!(out,
                        "kumapulse_monitor_response_seconds_quantile{{{},window=\"{window_name}\",quantile=\"{quantile}\"}} {:.6}",
                        labels(m), ms / 1000.0);
                }
            }
        }
    }

    out
}

fn family(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {}", escape_help(help));
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

/// Label set shared by every per-monitor series.
fn labels(m: &MonitorState) -> String {
    format!(
        "monitor=\"{}\",id=\"{}\",type=\"{}\",url=\"{}\",group=\"{}\"",
        escape(&m.monitor.name),
        m.monitor.id,
        escape(&m.monitor.kind),
        escape(&m.monitor.target),
        escape(&m.monitor.group),
    )
}

/// Prometheus label-value escaping: backslash, quote and newline only.
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(ch),
        }
    }
    out
}

fn escape_help(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Beat, Monitor};

    fn state(id: u64, name: &str, status: Status, ping_ms: f64) -> MonitorState {
        MonitorState {
            monitor: Monitor {
                id,
                name: name.into(),
                kind: "http".into(),
                target: "https://example.test/health".into(),
                group: "core".into(),
                active: true,
            },
            last: Some(Beat {
                status,
                ping_ms,
                at_ms: 1_700_000_000_000,
            }),
            beats: vec![Beat {
                status,
                ping_ms,
                at_ms: 1_700_000_000_000,
            }],
            tls_expiry: None,
        }
    }

    fn render_with(monitors: Vec<MonitorState>) -> String {
        let snapshot = Snapshot {
            monitors,
            scraped_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_001),
            source: "sqlite",
        };
        render(
            &snapshot,
            BuildInfo {
                version: "test",
                rustc: "test",
            },
            ScrapeStats {
                duration: Duration::from_millis(3),
                success: true,
                cache_age: Duration::ZERO,
                beats_considered: 1,
            },
        )
    }

    #[test]
    fn emits_type_and_help_once_per_family() {
        let out = render_with(vec![
            state(1, "a", Status::Up, 10.0),
            state(2, "b", Status::Down, 20.0),
        ]);
        assert_eq!(out.matches("# TYPE kumapulse_monitor_up gauge").count(), 1);
        // Trailing space matters: "# HELP kumapulse_monitor_up" is also a prefix of
        // the uptime_ratio HELP line.
        assert_eq!(out.matches("# HELP kumapulse_monitor_up ").count(), 1);
        assert!(out.contains("kumapulse_monitor_up{monitor=\"a\",id=\"1\""));
        assert!(out.contains("kumapulse_monitor_up{monitor=\"b\",id=\"2\""));
    }

    #[test]
    fn escapes_quotes_backslashes_and_newlines_in_labels() {
        let out = render_with(vec![state(1, "weird \"name\"\\x\n", Status::Up, 1.0)]);
        assert!(
            out.contains(r#"monitor="weird \"name\"\\x\n""#),
            "got: {out}"
        );
        assert!(!out.contains("weird \"name\""), "raw quote leaked");
    }

    #[test]
    fn output_is_sorted_by_monitor_id() {
        let out = render_with(vec![
            state(9, "later", Status::Up, 1.0),
            state(2, "earlier", Status::Up, 1.0),
        ]);
        let first = out.find("id=\"2\"").expect("id 2 present");
        let second = out.find("id=\"9\"").expect("id 9 present");
        assert!(first < second, "monitors must be emitted in id order");
    }

    #[test]
    fn down_monitor_reports_zero_up_and_status_zero() {
        let out = render_with(vec![state(5, "db", Status::Down, 0.0)]);
        assert!(out.contains("kumapulse_monitor_up{monitor=\"db\""));
        let line = out
            .lines()
            .find(|l| l.starts_with("kumapulse_monitor_status{"))
            .unwrap();
        assert!(line.ends_with(" 0"), "down must be status 0: {line}");
    }

    #[test]
    fn uptime_windows_are_present_for_each_monitor() {
        let out = render_with(vec![state(1, "a", Status::Up, 5.0)]);
        for window in ["24h", "7d", "30d"] {
            assert!(
                out.contains(&format!("window=\"{window}\"")),
                "missing {window}"
            );
        }
        assert!(out.contains("kumapulse_monitor_uptime_ratio{"));
    }
}
