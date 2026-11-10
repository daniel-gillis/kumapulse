# Security policy

## Supported versions

Only the latest release receives fixes. The project is a single static binary
with no runtime dependencies, so upgrading is a file replacement.

## Reporting a vulnerability

Open a [private security advisory](https://github.com/daniel-gillis/kumapulse/security/advisories/new)
or email the maintainer. Please do not open a public issue for anything
exploitable.

Expect an acknowledgement within 72 hours. Fixes ship as a patch release with an
advisory describing the impact and the affected versions.

## Design commitments

These are properties of the code, not aspirations — if you find a way to break
one of them, it is a security bug:

1. **The Uptime Kuma database is opened read-only.** There is no SQL write path.
2. **No credentials are stored, sent or logged.** The exporter authenticates to
   nothing.
3. **No shell, no dynamic loading, no `unsafe`.** `#![forbid(unsafe_code)]` and a
   distroless image with no interpreter.
4. **The container runs as non-root** with a read-only root filesystem in every
   provided example.
5. **Dependency surface is deliberate.** Four direct dependencies; `cargo-deny`
   runs in CI and fails on any advisory, unknown registry, wildcard version or
   disallowed licence.

## Threat model

`kumapulse` sits next to a monitoring system. The realistic attack surface is:

- **Malicious metric content.** Monitor names come from Kuma and are attacker-
  controlled only if someone can already edit Kuma. Label values are escaped per
  the Prometheus text format (`\\`, `"`, newline) — covered by unit tests.
- **Resource exhaustion.** Exposition size is linear in monitor count; the
  heartbeat history held in memory is bounded by the 30-day retention window and
  the cache interval. A 100k-heartbeat instance produces tens of MB, not GB.
- **Denial of the exporter itself.** If the data source becomes unreadable, the
  process keeps serving cached data and reports `kumapulse_scrape_success 0`
  rather than exiting — monitoring should outlive the thing it monitors.

It is **not** in scope to protect Kuma's database from someone who can already
read it; mounting it read-only is the whole design.
