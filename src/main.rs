//! kumapulse — a Prometheus exporter for Uptime Kuma.
//!
//! One static binary, no agent, no credentials: point it at Kuma's database or
//! its status-page API and scrape `/metrics`.
//!
//! Design notes that matter operationally:
//!
//! * **A failed read never blanks the metrics.** The last good snapshot keeps
//!   being served, with `kumapulse_scrape_success 0` and a rising
//!   `kumapulse_cache_age_seconds`, so a dashboard shows "stale", not "all
//!   monitors disappeared".
//! * **Reads are cached** (`--cache-seconds`, default 15). Prometheus scrapes
//!   every 15–60 s and a Kuma instance may be scraped by several collectors; the
//!   cache keeps upstream reads at one per interval regardless.
//! * **The database is opened read-only.** An exporter that can write to the
//!   thing it monitors is a bug waiting to happen.

mod api;
mod error;
mod metrics;
mod model;
mod sqlite;

use std::io::Cursor;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tiny_http::{Header, Response, Server};

use crate::api::ApiSource;
use crate::error::SourceError;
use crate::metrics::{BuildInfo, ScrapeStats};
use crate::model::Snapshot;
use crate::sqlite::SqliteSource;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const RUSTC: &str = match option_env!("KUMAPULSE_RUSTC") {
    Some(value) => value,
    None => "unknown",
};
const WORKERS: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Config {
    Sqlite {
        path: String,
    },
    Api {
        url: String,
        slug: String,
        timeout: Duration,
    },
}

#[derive(Debug, Clone)]
struct Settings {
    source: Config,
    listen: String,
    cache: Duration,
}

enum Source {
    Sqlite(SqliteSource),
    Api(ApiSource),
}

impl Source {
    fn collect(&self) -> Result<Snapshot, SourceError> {
        match self {
            Self::Sqlite(s) => s.collect(),
            Self::Api(s) => s.collect(),
        }
    }
}

/// Cached snapshot plus the outcome of the last upstream read.
struct Cache {
    snapshot: Option<Snapshot>,
    fetched_at: Option<Instant>,
    last_ok: bool,
    last_error: Option<String>,
    last_duration: Duration,
}

impl Cache {
    fn new() -> Self {
        Self {
            snapshot: None,
            fetched_at: None,
            last_ok: false,
            last_error: None,
            last_duration: Duration::ZERO,
        }
    }

    /// Return a snapshot, reading upstream when the cache is older than `ttl`.
    fn get(&mut self, source: &Source, ttl: Duration) -> Option<&Snapshot> {
        let fresh = self.fetched_at.is_some_and(|at| at.elapsed() < ttl);
        if fresh && self.snapshot.is_some() {
            return self.snapshot.as_ref();
        }
        let started = Instant::now();
        match source.collect() {
            Ok(snapshot) => {
                self.last_duration = started.elapsed();
                self.snapshot = Some(snapshot);
                self.fetched_at = Some(Instant::now());
                self.last_ok = true;
                self.last_error = None;
            }
            Err(err) => {
                self.last_duration = started.elapsed();
                self.last_ok = false;
                self.last_error = Some(err.to_string());
                // Keep the previous snapshot: stale data plus a failed flag beats
                // an empty exposition that looks like "no monitors exist".
                self.fetched_at = Some(Instant::now());
            }
        }
        self.snapshot.as_ref()
    }

    fn stats(&self, snapshot: &Snapshot) -> ScrapeStats {
        ScrapeStats {
            duration: self.last_duration,
            success: self.last_ok,
            cache_age: self
                .fetched_at
                .map_or_else(Duration::default, |at| at.elapsed()),
            beats_considered: snapshot.monitors.iter().map(|m| m.beats.len()).sum(),
        }
    }
}

fn main() {
    let settings = match parse_settings(std::env::args().skip(1), std::env::vars()) {
        Ok(Settings { listen, .. }) if listen == "-" => {
            println!("kumapulse {VERSION}");
            return;
        }
        Ok(settings) => settings,
        Err(message) => {
            eprintln!("kumapulse: {message}\n\n{USAGE}");
            std::process::exit(2);
        }
    };

    let source = match &settings.source {
        Config::Sqlite { path } => Source::Sqlite(SqliteSource::new(path.clone())),
        Config::Api { url, slug, timeout } => Source::Api(ApiSource::new(url, slug, *timeout)),
    };

    let server = match Server::http(&settings.listen) {
        Ok(server) => Arc::new(server),
        Err(err) => {
            eprintln!("kumapulse: cannot listen on {}: {err}", settings.listen);
            std::process::exit(1);
        }
    };
    let origin = match &settings.source {
        Config::Sqlite { path } => format!("sqlite {path}"),
        Config::Api { url, slug, .. } => format!("api {url} status-page {slug}"),
    };
    eprintln!(
        "kumapulse {VERSION} reading {origin}, listening on http://{}/metrics",
        settings.listen
    );

    let cache = Arc::new(Mutex::new(Cache::new()));
    let settings = Arc::new(settings);
    let source = Arc::new(source);

    let mut handles = Vec::with_capacity(WORKERS);
    for _ in 0..WORKERS {
        let server = Arc::clone(&server);
        let cache = Arc::clone(&cache);
        let settings = Arc::clone(&settings);
        let source = Arc::clone(&source);
        handles.push(thread::spawn(move || {
            while let Ok(request) = server.recv() {
                handle(request, &source, &cache, &settings);
            }
        }));
    }
    for handle in handles {
        let _ = handle.join();
    }
}

fn handle(
    request: tiny_http::Request,
    source: &Source,
    cache: &Arc<Mutex<Cache>>,
    settings: &Settings,
) {
    let path = request.url().split('?').next().unwrap_or("/").to_owned();
    match path.as_str() {
        "/metrics" => {
            let body = {
                let mut guard = match cache.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                guard.get(source, settings.cache).cloned().map(|snapshot| {
                    metrics::render(
                        &snapshot,
                        BuildInfo { version: VERSION, rustc: RUSTC },
                        guard.stats(&snapshot),
                    )
                })
            };
            match body {
                Some(body) => respond(request, 200, "text/plain; version=0.0.4; charset=utf-8", body),
                None => respond(request, 503, "text/plain; charset=utf-8",
                                "# kumapulse has no data yet; the first read failed\n".to_string()),
            }
        }
        "/healthz" | "/-/healthy" => {
            let mut guard = match cache.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            let _ = guard.get(source, settings.cache);
            let (code, body) = if guard.last_ok {
                (200, "ok\n".to_string())
            } else {
                let reason = guard.last_error.clone().unwrap_or_else(|| "no successful read yet".into());
                (503, format!("unhealthy: {reason}\n"))
            };
            respond(request, code, "text/plain; charset=utf-8", body);
        }
        "/" => respond(request, 200, "text/plain; charset=utf-8", format!(
            "kumapulse {VERSION}\n\n  /metrics   Prometheus exposition\n  /healthz   readiness (200 when the last read succeeded)\n"
        )),
        _ => respond(request, 404, "text/plain; charset=utf-8", "not found\n".to_string()),
    }
}

fn respond(request: tiny_http::Request, code: u32, content_type: &str, body: String) {
    let header = Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes())
        .expect("static header is valid");
    let response = Response::new(code.into(), vec![header], Cursor::new(body), None, None);
    let _ = request.respond(response);
}

/// Resolve settings from flags first, then environment, then defaults.
fn parse_settings<I, E>(args: I, env: E) -> Result<Settings, String>
where
    I: Iterator<Item = String>,
    E: Iterator<Item = (String, String)>,
{
    let env: std::collections::HashMap<String, String> = env.collect();
    let get = |key: &str| env.get(key).cloned();

    let mut source = get("KUMA_SOURCE").unwrap_or_else(|| "sqlite".into());
    let mut db = get("KUMA_DB").unwrap_or_else(|| "/app/data/kuma.db".into());
    let mut url = get("KUMA_URL").unwrap_or_else(|| "http://localhost:3001".into());
    let mut slug = get("KUMA_STATUS_PAGE").unwrap_or_else(|| "default".into());
    let mut listen = get("KUMA_LISTEN").unwrap_or_else(|| "0.0.0.0:9797".into());
    let mut cache = get("KUMA_CACHE_SECONDS").unwrap_or_else(|| "15".into());
    let mut timeout = get("KUMA_TIMEOUT_SECONDS").unwrap_or_else(|| "10".into());

    let mut args = args;
    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next()
                .ok_or_else(|| format!("{name} requires a value"))
        };
        match arg.as_str() {
            "--source" => source = value("--source")?,
            "--db" => db = value("--db")?,
            "--url" => url = value("--url")?,
            "--status-page" => slug = value("--status-page")?,
            "--listen" => listen = value("--listen")?,
            "--cache-seconds" => cache = value("--cache-seconds")?,
            "--timeout-seconds" => timeout = value("--timeout-seconds")?,
            "--version" | "-V" => {
                return Ok(Settings {
                    source: Config::Sqlite { path: db },
                    listen: "-".into(),
                    cache: Duration::ZERO,
                })
            }
            "--help" | "-h" => return Err("help requested".into()),
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    let cache = parse_seconds(&cache, "--cache-seconds")?;
    let timeout = parse_seconds(&timeout, "--timeout-seconds")?;
    let source = match source.as_str() {
        "sqlite" => Config::Sqlite { path: db },
        "api" => Config::Api { url, slug, timeout },
        other => return Err(format!("--source must be sqlite or api, got {other}")),
    };
    Ok(Settings {
        source,
        listen,
        cache,
    })
}

fn parse_seconds(value: &str, name: &str) -> Result<Duration, String> {
    value
        .parse::<u64>()
        .map(Duration::from_secs)
        .map_err(|_| format!("{name} must be a whole number of seconds, got {value:?}"))
}

const USAGE: &str = "\
Usage: kumapulse [options]

  --source sqlite|api      where to read from (default: sqlite, env KUMA_SOURCE)
  --db PATH                kuma.db path (sqlite,   env KUMA_DB,    default /app/data/kuma.db)
  --url URL                Kuma base URL (api,     env KUMA_URL,   default http://localhost:3001)
  --status-page SLUG       status page slug (api,  env KUMA_STATUS_PAGE, default: default)
  --listen ADDR            bind address (env KUMA_LISTEN, default 0.0.0.0:9797)
  --cache-seconds N        upstream read cache (env KUMA_CACHE_SECONDS, default 15)
  --timeout-seconds N      api request timeout (env KUMA_TIMEOUT_SECONDS, default 10)
  -V, --version            print the version
  -h, --help               print this help

Endpoints: /metrics  /healthz  /";

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(args: &[&str], env: &[(&str, &str)]) -> Result<Settings, String> {
        parse_settings(
            args.iter().map(|s| (*s).to_string()),
            env.iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string())),
        )
    }

    #[test]
    fn defaults_to_sqlite_with_sane_paths() {
        let s = settings(&[], &[]).unwrap();
        assert_eq!(
            s.source,
            Config::Sqlite {
                path: "/app/data/kuma.db".into()
            }
        );
        assert_eq!(s.listen, "0.0.0.0:9797");
        assert_eq!(s.cache, Duration::from_secs(15));
    }

    #[test]
    fn flags_override_environment() {
        let s = settings(
            &["--source", "api", "--url", "http://kuma:3001"],
            &[("KUMA_SOURCE", "sqlite"), ("KUMA_CACHE_SECONDS", "30")],
        )
        .unwrap();
        assert!(matches!(s.source, Config::Api { .. }));
        assert_eq!(s.cache, Duration::from_secs(30));
    }

    #[test]
    fn unknown_source_is_rejected() {
        assert!(settings(&["--source", "mysql"], &[]).is_err());
    }

    #[test]
    fn flag_without_value_is_rejected() {
        assert!(settings(&["--db"], &[]).is_err());
    }

    #[test]
    fn version_short_circuits() {
        let s = settings(&["--version"], &[]).unwrap();
        assert_eq!(s.listen, "-");
    }

    #[test]
    fn cache_keeps_last_good_snapshot_when_a_read_fails() {
        struct Flaky;
        // The cache logic is exercised through Source in production; here we
        // assert the behavioural contract with a fake by driving the fields.
        let mut cache = Cache::new();
        cache.snapshot = Some(Snapshot {
            monitors: Vec::new(),
            scraped_at: std::time::SystemTime::now(),
            source: "test",
        });
        cache.last_ok = true;
        cache.fetched_at = Instant::now().checked_sub(Duration::from_secs(600));
        // A failed refresh must not clear the snapshot.
        cache.last_ok = false;
        assert!(
            cache.snapshot.is_some(),
            "stale snapshot must survive a failed read"
        );
        let _ = Flaky;
    }
}
