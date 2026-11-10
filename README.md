<p align="center">
  <img src="assets/logo.svg" width="96" alt="kumapulse logo">
</p>

<h1 align="center">kumapulse</h1>

**Prometheus metrics for Uptime Kuma — one static binary, no agent, no credentials.**

[![CI](https://github.com/daniel-gillis/kumapulse/actions/workflows/ci.yml/badge.svg)](https://github.com/daniel-gillis/kumapulse/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/daniel-gillis/kumapulse)](https://github.com/daniel-gillis/kumapulse/releases)
[![Licence: MIT](https://img.shields.io/badge/licence-MIT-blue.svg)](LICENSE)
[![Image size](https://img.shields.io/badge/image-~8%20MB-informational)](Dockerfile)
[![Binary](https://img.shields.io/badge/binary-3.4%20MB-informational)](#performance)
[![MSRV](https://img.shields.io/badge/MSRV-1.89-informational)](Cargo.toml)

Uptime Kuma is excellent at *checking* things and poor at *being checked*. It has
no Prometheus endpoint, so its uptime, latency and certificate data never reach
the dashboards and alerting you already run. `kumapulse` closes that gap:

- **Read-only.** It opens Kuma's database `mode=ro` and never writes to it.
- **No credentials.** No API key, no login, no agent inside Kuma.
- **No new failure mode.** A failed read serves the last good snapshot marked
  stale, instead of blanking your dashboards.
- **Tiny.** 3.3 MB static binary, ~12 MiB RSS, idle at 0% CPU.
- **Zero known CVEs.** Four dependencies, `cargo-deny` gates advisories in CI, distroless image, no shell.

```console
$ docker run -d --name kumapulse \
    -v /path/to/uptime-kuma:/kuma:ro \
    -e KUMA_DB=/kuma/kuma.db \
    -p 9797:9797 \
    ghcr.io/daniel-gillis/kumapulse:latest
$ curl -s localhost:9797/metrics | head
```

---

## Why this exists

Every "Uptime Kuma + Prometheus" search ends at the same place: an exporter that
scrapes Kuma's HTML, or a Grafana datasource plugin that logs in as you, or a
fork of Kuma itself to add a `/metrics` handler. Each has the same problem —
it couples you to a UI, holds credentials, or makes you maintain a fork of a
fast-moving upstream.

`kumapulse` treats Kuma as a data source, not a web page. It reads the two things
Kuma already publishes (its SQLite database, or its public status-page JSON) and
turns them into the metric families a commercial SaaS would sell you.

## Quickstart

### Docker (recommended)

```yaml
# docker-compose.yml
services:
  kumapulse:
    image: ghcr.io/daniel-gillis/kumapulse:latest
    restart: unless-stopped
    volumes:
      - /opt/uptime-kuma:/kuma:ro     # the whole directory, not just kuma.db
    environment:
      KUMA_DB: /kuma/kuma.db
    ports:
      - "9797:9797"
```

```console
$ docker compose up -d
$ curl -s localhost:9797/metrics | grep kumapulse_monitor_up | head -3
kumapulse_monitor_up{monitor="HP01 Proxmox",id="1",type="port",url="192.168.68.11",group=""} 1
kumapulse_monitor_up{monitor="Authentik",id="12",type="http",url="http://192.168.68.90:9000/-/health/ready/",group=""} 1
```

### Binary

```console
$ curl -sSL https://github.com/daniel-gillis/kumapulse/releases/latest/download/kumapulse-linux-amd64 \
    -o /usr/local/bin/kumapulse && chmod +x /usr/local/bin/kumapulse
$ KUMA_DB=/opt/uptime-kuma/kuma.db kumapulse
kumapulse 0.1.0 reading sqlite /opt/uptime-kuma/kuma.db, listening on http://0.0.0.0:9797/metrics
```

### Prometheus

```yaml
scrape_configs:
  - job_name: kumapulse
    static_configs:
      - targets: ["kumapulse:9797"]
```

That is the entire setup. No service discovery, no relabelling, no sidecar.

---

## Two sources, one set of metrics

| | `sqlite` (default) | `api` |
|---|---|---|
| Reads | `kuma.db` directly | `/api/status-page/*` JSON |
| Monitors | **all** of them | only those on a published status page |
| History | full heartbeat retention | what the status page returns |
| TLS expiry | ✅ (from Kuma's `tls_info`) | ❌ |
| Requires | read access to Kuma's data directory | network access to Kuma |
| Auth | none | none |

Use `sqlite` when you can mount the data directory — it is strictly richer.
Use `api` when the exporter runs somewhere else and you would rather not share
a volume.

> **Point `--url` at Uptime Kuma itself, not at a reverse proxy in front of it.**
> A proxy answers with a login redirect; `kumapulse` reports that as a parse
> error rather than silently showing zero monitors.

## Configuration

Every setting is available as a flag and an environment variable.

| Flag | Environment | Default | Meaning |
|---|---|---|---|
| `--source` | `KUMA_SOURCE` | `sqlite` | `sqlite` or `api` |
| `--db` | `KUMA_DB` | `/app/data/kuma.db` | path to `kuma.db` |
| `--url` | `KUMA_URL` | `http://localhost:3001` | Kuma base URL (api mode) |
| `--status-page` | `KUMA_STATUS_PAGE` | `default` | status-page slug (api mode) |
| `--listen` | `KUMA_LISTEN` | `0.0.0.0:9797` | bind address |
| `--cache-seconds` | `KUMA_CACHE_SECONDS` | `15` | minimum interval between upstream reads |
| `--timeout-seconds` | `KUMA_TIMEOUT_SECONDS` | `10` | API request timeout |

## Metrics

All per-monitor series carry `monitor`, `id`, `type`, `url` and `group` labels.

| Metric | Type | Meaning |
|---|---|---|
| `kumapulse_monitor_up` | gauge | 1 when the latest heartbeat succeeded |
| `kumapulse_monitor_status` | gauge | 0=down 1=up 2=pending 3=maintenance |
| `kumapulse_monitor_active` | gauge | 1 when the monitor is enabled in Kuma |
| `kumapulse_monitor_response_seconds` | gauge | round-trip time of the latest heartbeat |
| `kumapulse_monitor_last_check_timestamp_seconds` | gauge | when the latest heartbeat ran |
| `kumapulse_monitor_heartbeat_age_seconds` | gauge | rising value ⇒ checks have stalled |
| `kumapulse_monitor_uptime_ratio{window}` | gauge | up ÷ (up+down) over `24h`, `7d`, `30d` |
| `kumapulse_monitor_heartbeats{window,status}` | gauge | heartbeat counts per outcome |
| `kumapulse_monitor_response_seconds_quantile{window,quantile}` | gauge | p50/p90/p99 latency |
| `kumapulse_monitor_tls_expiry_timestamp_seconds` | gauge | certificate expiry (sqlite only) |
| `kumapulse_monitors_total` | gauge | monitors known to the exporter |
| `kumapulse_monitors_by_status{status}` | gauge | fleet-wide counts |
| `kumapulse_scrape_success` | gauge | 0 when the last upstream read failed |
| `kumapulse_scrape_duration_seconds` | gauge | time spent reading upstream |
| `kumapulse_cache_age_seconds` | gauge | age of the served snapshot |
| `kumapulse_build_info{version,rustc}` | gauge | build provenance |
| `kumapulse_source_info{source}` | gauge | which source is active |

**Semantics worth knowing:** pending and maintenance heartbeats are excluded
from uptime ratios — they are not evidence of either availability or an outage.
An unrecognised status value is surfaced as itself rather than being coerced to
"down", so a future Kuma release cannot silently page you.

## Alerts that are worth having

```yaml
groups:
  - name: kumapulse
    rules:
      - alert: MonitorDown
        expr: kumapulse_monitor_up == 0
        for: 2m
        labels: { severity: critical }
        annotations:
          summary: "{{ $labels.monitor }} is down"

      - alert: ChecksStalled
        expr: kumapulse_monitor_heartbeat_age_seconds > 300
        for: 5m
        labels: { severity: warning }
        annotations:
          summary: "{{ $labels.monitor }} has not been checked in 5 minutes"

      - alert: CertificateExpiring
        expr: (kumapulse_monitor_tls_expiry_timestamp_seconds - time()) / 86400 < 21
        for: 1h
        labels: { severity: warning }
        annotations:
          summary: "{{ $labels.monitor }} certificate expires in {{ $value | printf \"%.0f\" }} days"

      - alert: UptimeBudgetBurn
        expr: kumapulse_monitor_uptime_ratio{window="24h"} < 0.99
        for: 15m
        labels: { severity: warning }
        annotations:
          summary: "{{ $labels.monitor }} below 99% over 24h"

      - alert: ExporterStale
        expr: kumapulse_scrape_success == 0
        for: 5m
        labels: { severity: warning }
        annotations:
          summary: "kumapulse cannot read Uptime Kuma; serving cached data"
```

`ChecksStalled` is the alert Kuma itself cannot give you: a monitor that has not
run is not the same as a monitor that is up.

## Integrations

| Target | How |
|---|---|
| **Prometheus** | scrape `/metrics` — nothing else needed |
| **VictoriaMetrics / vmagent** | same exposition format |
| **Grafana Mimir / Thanos** | scrape, then remote-write as usual |
| **Grafana** | any Prometheus datasource; example dashboard in `examples/` |
| **Alertmanager** | use the rules above |
| **Kubernetes** | `examples/kubernetes.yaml` (Deployment + Service + ServiceMonitor) |
| **Docker Compose** | `examples/docker-compose.yml` |
| **systemd** | `examples/kumapulse.service`, hardened unit included |
| **Health probes** | `GET /healthz` (503 when the last read failed) |

## Performance

Measured on an Apple M-series host against a live Kuma instance with **33
monitors** and 30 days of heartbeat history:

| | Value |
|---|---|
| Release binary | **3.4 MB** (static, stripped, `opt-level="z"`, fat LTO) |
| Container image | **~8 MB** (distroless static, non-root, no shell) |
| Resident memory | **11.8 MiB** |
| CPU at idle | **0.0 %** |
| Scrape (cached) | **0.9 ms**, 58 KB exposition |
| Cold read (full history) | **76 ms** |

Scrapes are served from a cache (`--cache-seconds`, default 15 s), so ten
Prometheus instances scraping every 15 s still produce one database read per
interval.

## Security

- Database connections are opened `SQLITE_OPEN_READ_ONLY`; there is no write path
  in the codebase.
- No shell, no package manager, no root in the container.
- `#![forbid(unsafe_code)]`, four direct dependencies, `cargo-deny` gates
  advisories and licences in CI.
- See [SECURITY.md](SECURITY.md) for the disclosure process.

## Development

```console
$ cargo test          # 25 unit tests, no network, no fixtures on disk
$ cargo clippy --all-targets -- -D warnings
$ cargo fmt --all
$ cargo run -- --source sqlite --db ./fixtures/kuma.db --listen 127.0.0.1:9797
```

## Licence

MIT — see [LICENSE](LICENSE). Not affiliated with Uptime Kuma; it is a
compatible companion, not a fork.
