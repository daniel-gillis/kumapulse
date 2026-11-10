# Contributing

Thanks for looking. This project optimises for three things, in order:

1. **It must not break someone's dashboards.** A metric that silently changes
   meaning is worse than a missing metric.
2. **It must stay small.** Four direct dependencies is a feature. A pull request
   that adds a crate to save twenty lines will usually be declined.
3. **It must be obvious to operate.** One binary, environment variables, no
   config file, no state on disk.

## Before opening a pull request

```console
$ cargo fmt --all
$ cargo clippy --all-targets -- -D warnings
$ cargo test
$ cargo deny check      # if you touched dependencies
```

All four run in CI on Linux, macOS and Windows.

## Adding a metric

- Follow the `kumapulse_` prefix and add it to the table in `README.md`.
- Add a unit test in `src/metrics.rs` that asserts the rendered line — including
  label escaping if the value can contain quotes or newlines.
- Do not add unbounded labels. Anything derived from user content (monitor names,
  URLs) is already high-cardinality; do not multiply it.
- If a value can be unknown, omit the series rather than emitting a zero. A zero
  reads as "measured as zero", which is a different claim.

## Adding a data source

Implement `collect()` returning a `Snapshot` (`src/model.rs`) and map storage
quirks into that model. Keep the rules from `src/model.rs` intact:

- pending and maintenance beats are excluded from uptime ratios
- an unrecognised status is surfaced as `Unknown`, never coerced to `Down`
- timestamps are epoch milliseconds internally

## Reporting a bug

Include: the Kuma version, the source (`sqlite` or `api`), the output of
`/metrics` around the problem, and the `kumapulse_build_info` line.
