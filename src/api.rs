//! Read Uptime Kuma through its public status-page API.
//!
//! No file access and no credentials: the exporter asks Kuma for the same JSON
//! its status pages use. The trade-off is coverage — only monitors published on
//! the status page are visible, and heartbeat history is whatever Kuma returns.
//! For everything, use the SQLite source.
//!
//! Point `--url` at Uptime Kuma itself, not at an auth proxy in front of it:
//! a reverse proxy will answer with a login redirect, which this reports as a
//! parse error rather than silently producing zero monitors.

use std::time::{Duration, SystemTime};

use serde_json::Value;
use ureq::Agent;

use crate::error::SourceError;
use crate::model::{Beat, Monitor, MonitorState, Snapshot, Status};

pub struct ApiSource {
    base: String,
    slug: String,
    agent: Agent,
}

impl ApiSource {
    #[must_use]
    pub fn new(base: &str, slug: &str, timeout: Duration) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(timeout)
            .user_agent(concat!("kumapulse/", env!("CARGO_PKG_VERSION")))
            .build();
        Self {
            base: base.trim_end_matches('/').to_owned(),
            slug: slug.to_owned(),
            agent,
        }
    }

    pub fn collect(&self) -> Result<Snapshot, SourceError> {
        let page = self.get(&format!("{}/api/status-page/{}", self.base, self.slug))?;
        let heartbeats = self.get(&format!(
            "{}/api/status-page/heartbeat/{}",
            self.base, self.slug
        ))?;

        let mut monitors = Vec::new();
        for group in page
            .get("publicGroupList")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let group_name = group
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            for monitor in group
                .get("monitorList")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let id = monitor.get("id").and_then(Value::as_u64).unwrap_or(0);
                let name = monitor
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let kind = monitor
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned();
                let url = monitor
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let hostname = monitor
                    .get("hostname")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let target = if url.trim().is_empty() { hostname } else { url };

                let beats = heartbeats
                    .get("heartbeatList")
                    .and_then(|list| list.get(id.to_string()))
                    .and_then(Value::as_array)
                    .map(|list| list.iter().filter_map(parse_beat).collect::<Vec<_>>())
                    .unwrap_or_default();
                let last = beats.iter().max_by_key(|b| b.at_ms).copied();

                monitors.push(MonitorState {
                    monitor: Monitor {
                        id,
                        name,
                        kind,
                        target,
                        group: group_name.clone(),
                        active: true,
                    },
                    last,
                    beats,
                    tls_expiry: None,
                });
            }
        }

        if monitors.is_empty() {
            return Err(SourceError::Parse(
                "status page returned no monitors — check --status-page slug, or point --url at Kuma rather than an auth proxy".into(),
            ));
        }

        Ok(Snapshot {
            monitors,
            scraped_at: SystemTime::now(),
            source: "api",
        })
    }

    fn get(&self, url: &str) -> Result<Value, SourceError> {
        let response = self
            .agent
            .get(url)
            .call()
            .map_err(|e| SourceError::Http(format!("{url}: {e}")))?;
        if response.status() != 200 {
            return Err(SourceError::Http(format!(
                "{url}: status {}",
                response.status()
            )));
        }
        response
            .into_json::<Value>()
            .map_err(|e| SourceError::Parse(format!("{url}: {e}")))
    }
}

fn parse_beat(value: &Value) -> Option<Beat> {
    let status = value.get("status").and_then(Value::as_i64)?;
    let ping = value.get("ping").and_then(Value::as_f64).unwrap_or(0.0);
    let at_ms = value.get("time").and_then(Value::as_i64).unwrap_or(0);
    if at_ms == 0 {
        return None; // Kuma's placeholder beats for monitors that never ran
    }
    Some(Beat {
        status: Status::from_i64(status),
        ping_ms: ping,
        at_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_status_page_payload() {
        let page: Value = serde_json::from_str(
            r#"{"config":{"title":"Homelab"},"publicGroupList":[
                 {"id":1,"name":"Core","monitorList":[
                    {"id":1,"name":"Kuma","type":"http","url":"http://kuma:3001"},
                    {"id":2,"name":"DNS","type":"ping","hostname":"1.1.1.1","url":null}
                 ]}]}"#,
        )
        .unwrap();
        let beats: Value = serde_json::from_str(
            r#"{"heartbeatList":{"1":[{"status":1,"time":1700000000000,"ping":12.5},
                                      {"status":0,"time":1700000060000,"ping":0}]},
                 "uptimeList":{"1_24":0.98}}"#,
        )
        .unwrap();

        let group = &page["publicGroupList"][0];
        assert_eq!(group["name"], "Core");
        let monitor = &group["monitorList"][1];
        assert_eq!(monitor["hostname"], "1.1.1.1");

        let parsed: Vec<Beat> = beats["heartbeatList"]["1"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(parse_beat)
            .collect();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].status, Status::Up);
        assert_eq!(parsed[1].status, Status::Down);
    }

    #[test]
    fn beats_without_time_are_skipped() {
        let value: Value = serde_json::from_str(r#"{"status":1,"time":0,"ping":1.0}"#).unwrap();
        assert!(parse_beat(&value).is_none());
    }

    #[test]
    fn url_trailing_slash_is_normalised() {
        let source = ApiSource::new("http://kuma:3001/", "default", Duration::from_secs(1));
        assert_eq!(source.base, "http://kuma:3001");
    }
}
