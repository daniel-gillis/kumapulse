//! Read Uptime Kuma's SQLite database directly.
//!
//! This is the high-fidelity source: every monitor (not just the ones on a
//! public status page), full heartbeat history, and TLS expiry where Kuma
//! records it.
//!
//! The database is opened **read-only** and never written to. Kuma runs SQLite
//! in WAL mode, which means the exporter needs read access to the directory
//! holding `kuma.db` *and* its `-wal`/`-shm` siblings — mount the whole
//! directory, not just the file.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use rusqlite::{Connection, OpenFlags};

use crate::error::SourceError;
use crate::model::{Beat, Monitor, MonitorState, Snapshot, Status};

/// How much heartbeat history is loaded for the longest uptime window.
const RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

pub struct SqliteSource {
    path: PathBuf,
}

impl SqliteSource {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Read monitors, their heartbeats and certificate expiry in one snapshot.
    pub fn collect(&self) -> Result<Snapshot, SourceError> {
        // URI form so the connection is opened read-only: an exporter must never
        // be able to modify the thing it monitors.
        let uri = format!("file:{}?mode=ro", self.path.display());
        let conn = Connection::open_with_flags(
            uri,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| SourceError::Database(format!("{}: {e}", self.path.display())))?;
        let _ = conn.busy_timeout(Duration::from_secs(5));

        let groups = load_groups(&conn).unwrap_or_default();
        let monitors = load_monitors(&conn, &groups)?;
        let now_ms = now_millis();
        let beats = load_beats(&conn, now_ms - millis(RETENTION))?;

        let mut by_monitor: HashMap<u64, Vec<Beat>> = beats;
        let mut states: Vec<MonitorState> = Vec::with_capacity(monitors.len());
        for (monitor, tls_expiry) in monitors {
            let mut monitor_beats = by_monitor.remove(&monitor.id).unwrap_or_default();
            monitor_beats.sort_by_key(|b| b.at_ms);
            let last = monitor_beats.last().copied();
            states.push(MonitorState {
                monitor,
                last,
                beats: monitor_beats,
                tls_expiry,
            });
        }

        Ok(Snapshot {
            monitors: states,
            scraped_at: SystemTime::now(),
            source: "sqlite",
        })
    }
}

/// Monitor groups, keyed by id. Absent on older Kuma schemas.
fn load_groups(conn: &Connection) -> Result<HashMap<i64, String>, SourceError> {
    let mut out = HashMap::new();
    let Ok(mut stmt) = conn.prepare("SELECT id, name FROM monitor_group") else {
        return Ok(out);
    };
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|e| SourceError::Database(format!("monitor_group: {e}")))?;
    for row in rows.flatten() {
        out.insert(row.0, row.1);
    }
    Ok(out)
}

/// Columns that differ between Kuma releases are read defensively.
fn load_monitors(
    conn: &Connection,
    groups: &HashMap<i64, String>,
) -> Result<Vec<(Monitor, Option<i64>)>, SourceError> {
    let mut stmt = conn
        .prepare("SELECT * FROM monitor")
        .map_err(|e| SourceError::Database(format!("monitor: {e}")))?;
    // rusqlite 0.41 returns an iterator here rather than a slice.
    let columns: Vec<String> = stmt.column_names().map(str::to_owned).collect();
    let index = |name: &str| columns.iter().position(|c| c == name);

    let (Some(id_i), Some(name_i)) = (index("id"), index("name")) else {
        return Err(SourceError::Database(
            "monitor table has no id/name column".into(),
        ));
    };
    let kind_i = index("type");
    let url_i = index("url");
    let host_i = index("hostname");
    let active_i = index("active");
    let parent_i = index("parent");
    let group_i = index("monitor_group_id");
    let tls_i = index("tls_info");

    let mut rows = stmt
        .query([])
        .map_err(|e| SourceError::Database(format!("monitor: {e}")))?;
    let mut out = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| SourceError::Database(format!("monitor: {e}")))?
    {
        let id: i64 = row.get(id_i).unwrap_or(0);
        let name: String = row.get(name_i).unwrap_or_default();
        let kind: String = kind_i
            .and_then(|i| row.get(i).ok())
            .unwrap_or_else(|| "unknown".into());
        let url: String = url_i.and_then(|i| row.get(i).ok()).unwrap_or_default();
        let hostname: String = host_i.and_then(|i| row.get(i).ok()).unwrap_or_default();
        let active: i64 = active_i.and_then(|i| row.get(i).ok()).unwrap_or(1);
        let parent: i64 = parent_i.and_then(|i| row.get(i).ok()).unwrap_or(0);
        let group_id: i64 = group_i.and_then(|i| row.get(i).ok()).unwrap_or(0);
        let tls_raw: Option<String> = tls_i.and_then(|i| row.get(i).ok());

        // HTTP-ish monitors carry a URL; raw tcp/ping carry a hostname.
        let target = if url.trim().is_empty() { hostname } else { url };
        let group = groups
            .get(&group_id)
            .or_else(|| groups.get(&parent))
            .cloned()
            .unwrap_or_default();

        out.push((
            Monitor {
                id: u64::try_from(id).unwrap_or(0),
                name,
                kind,
                target,
                group,
                active: active != 0,
            },
            tls_expiry(tls_raw.as_deref()),
        ));
    }
    Ok(out)
}

/// Heartbeats inside the retention window, grouped by monitor id.
///
/// Kuma's `heartbeat.time` is a **TEXT** datetime in recent releases
/// (`2026-11-10 02:10:46.319`, UTC) and an INTEGER epoch-millisecond value in
/// older ones. Both are handled: the column type is detected once and the
/// comparison is made in the column's own type, which keeps the filter
/// index-friendly instead of loading millions of rows to filter in Rust.
fn load_beats(conn: &Connection, since_ms: i64) -> Result<HashMap<u64, Vec<Beat>>, SourceError> {
    let storage: String = conn
        .query_row("SELECT typeof(time) FROM heartbeat LIMIT 1", [], |row| {
            row.get(0)
        })
        .unwrap_or_else(|_| "integer".to_string());

    let sql = "SELECT monitor_id, status, ping, time FROM heartbeat WHERE time >= ?1";
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| SourceError::Database(format!("heartbeat: {e}")))?;

    let map_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<(i64, i64, f64, i64)> {
        let monitor_id: i64 = row.get(0)?;
        let status: i64 = row.get(1).unwrap_or(-1);
        let ping: Option<f64> = row.get(2).ok();
        // Try the integer form first, then the text form.
        let at_ms = match row.get::<_, i64>(3) {
            Ok(value) => value,
            Err(_) => row
                .get::<_, String>(3)
                .ok()
                .and_then(|text| parse_kuma_timestamp(&text))
                .unwrap_or(0),
        };
        Ok((monitor_id, status, ping.unwrap_or(0.0), at_ms))
    };

    let rows = if storage == "text" {
        stmt.query_map([format_kuma_timestamp(since_ms)], map_row)
    } else {
        stmt.query_map([since_ms], map_row)
    }
    .map_err(|e| SourceError::Database(format!("heartbeat: {e}")))?;

    let mut out: HashMap<u64, Vec<Beat>> = HashMap::new();
    for (monitor_id, status, ping_ms, at_ms) in rows.flatten() {
        if let Ok(id) = u64::try_from(monitor_id) {
            out.entry(id).or_default().push(Beat {
                status: Status::from_i64(status),
                ping_ms,
                at_ms,
            });
        }
    }
    Ok(out)
}

/// Parse a Kuma heartbeat timestamp into epoch milliseconds.
///
/// Accepts `YYYY-MM-DD HH:MM:SS[.fff]` (UTC, what Kuma writes), and the RFC 3339
/// form with `T`/`Z` or an offset, which appears in certificate blobs.
#[must_use]
pub fn parse_kuma_timestamp(input: &str) -> Option<i64> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let normalised = trimmed.replace(' ', "T");
    parse_rfc3339_millis(&normalised)
}

/// Render epoch milliseconds in Kuma's TEXT format so SQL comparisons match.
#[must_use]
pub fn format_kuma_timestamp(epoch_ms: i64) -> String {
    let (days, rem) = (
        epoch_ms.div_euclid(86_400_000),
        epoch_ms.rem_euclid(86_400_000),
    );
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second, millis) = (
        rem / 3_600_000,
        (rem % 3_600_000) / 60_000,
        (rem % 60_000) / 1000,
        rem % 1000,
    );
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}.{millis:03}")
}

/// Inverse of `days_from_civil`.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Kuma stores certificate details as a JSON blob per monitor.
fn tls_expiry(raw: Option<&str>) -> Option<i64> {
    let value: serde_json::Value = serde_json::from_str(raw?).ok()?;
    // Shape seen in Kuma 2.x: {"certInfo":{"validTill":"..."},"tlsInfo":{...}}
    let till = value.pointer("/certInfo/validTill")?.as_str()?;
    parse_rfc3339_millis(till).map(|ms| ms / 1000)
}

/// Minimal RFC 3339 → epoch **milliseconds**, so a timestamp library is not required.
fn parse_rfc3339_millis(input: &str) -> Option<i64> {
    // Accepts 2026-11-09T12:00:00Z or with an offset like +02:00.
    let (date, rest) = input.split_once('T')?;
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    let time = rest.trim_end_matches('Z');
    let (hms, offset) = match time.split_once('+').or_else(|| time.split_once('-')) {
        Some((hms, off)) => (hms, Some(off)),
        None => (time, None),
    };
    let mut t = hms.split(':');
    let hour: i64 = t.next()?.parse().ok()?;
    let minute: i64 = t.next()?.parse().ok()?;
    let seconds_token = t.next()?;
    let (second, millis): (i64, i64) = match seconds_token.split_once('.') {
        Some((sec, frac)) => {
            // 1 digit = tenths, 2 = hundredths, 3+ = milliseconds.
            let digits: String = frac.chars().take(3).collect();
            (sec.parse().ok()?, format!("{digits:0<3}").parse().ok()?)
        }
        None => (seconds_token.parse().ok()?, 0),
    };

    let days = days_from_civil(year, month, day);
    let mut epoch_ms =
        days * 86_400_000 + hour * 3_600_000 + minute * 60_000 + second * 1000 + millis;
    if let Some(off) = offset {
        let (sign, rest) = off.split_at(1);
        let mut parts = rest.split(':');
        let oh: i64 = parts.next()?.parse().ok()?;
        let om: i64 = parts.next().unwrap_or("0").parse().ok()?;
        let shift = oh * 3_600_000 + om * 60_000;
        epoch_ms += if sign == "-" { shift } else { -shift };
    }
    Some(epoch_ms)
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn millis(window: Duration) -> i64 {
    i64::try_from(window.as_millis()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_kuma_text_timestamps() {
        // What Kuma actually writes: UTC, space separator, milliseconds.
        assert_eq!(
            parse_kuma_timestamp("2026-11-09 12:00:00.000"),
            Some(1_794_225_600_000)
        );
        assert_eq!(
            parse_kuma_timestamp("2026-11-09 12:00:00.1"),
            Some(1_794_225_600_100)
        );
        assert_eq!(
            parse_kuma_timestamp("2026-11-09 12:00:00.123"),
            Some(1_794_225_600_123)
        );
    }

    #[test]
    fn parses_rfc3339_with_offset() {
        assert_eq!(
            parse_kuma_timestamp("2026-11-09T14:00:00+02:00"),
            Some(1_794_225_600_000)
        );
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_kuma_timestamp("not a timestamp"), None);
        assert_eq!(parse_kuma_timestamp(""), None);
    }

    #[test]
    fn formats_back_to_kuma_text() {
        assert_eq!(
            format_kuma_timestamp(1_794_225_600_123),
            "2026-11-09 12:00:00.123"
        );
        for sample in [0_i64, 1_000, 1_794_225_600_000, 1_800_000_000_999] {
            let text = format_kuma_timestamp(sample);
            assert_eq!(
                parse_kuma_timestamp(&text),
                Some(sample),
                "roundtrip {text}"
            );
        }
    }

    #[test]
    fn tls_expiry_reads_kuma_shape() {
        let raw = r#"{"certInfo":{"validTill":"2026-11-09T12:00:00Z"}}"#;
        assert_eq!(tls_expiry(Some(raw)), Some(1_794_225_600));
    }

    #[test]
    fn missing_tls_info_is_none() {
        assert_eq!(tls_expiry(None), None);
        assert_eq!(tls_expiry(Some("{}")), None);
    }
}
