# Performance and verification record

The [second 2026-11-04 pass](optimization-followup-2026-11-04.md) records the current
compact routing and history changes, a complete loopback bridge benchmark and
86 passing tests. The [first 2026-11-04 report](optimization-2026-11-04.md) records allocation
changes, five paired benchmark runs, 84 passing tests, exact real-XML parity and
tolerant handling of malformed Miniserver string values. The measurements and
Docker validation below are the historical October record; the current Docker
check could not run because the local daemon was unavailable.

Measured locally on macOS ARM64 with rustc 1.99.1, release profile (`opt-level=3`,
fat LTO, one codegen unit), 2026-10-30. No `target-cpu=native` is required for the
portable build. The original measurements below are single-run development
observations; the subsequent [relay audit](relay-audit.md#measured-results)
records repeated, paired parser measurements after optimization. Neither measures
production throughput or whole-process CPU utilization.

## Numeric processing benchmark

`cargo bench --locked --bench pipeline` processes two million updates across
4096 pre-discovered controls with a reused output buffer. It includes UUID lookup,
optional decimal rounding, change detection and line protocol serialization.

| Case | Values/s | ns/value |
| --- | ---: | ---: |
| Full f64 precision, every value changes | 18,283,869 | 54.7 |
| Round to five decimals, every value changes | 9,098,226 | 109.9 |
| Full f64 precision, unchanged values suppressed | 95,150,683 | 10.5 |

The measurement excludes WebSocket decoding, ingress channel/counters, network
syscalls, TLS, Telegraf and database persistence. The unchanged case emits no
bytes after warmup. The rounded case deliberately uses decimal round-to-nearest
instead of multiplication/casting shortcuts that can change precision.

## Real XML parity

Offline comparison against the existing Python application's installed environment
and three existing configuration exports. The original default TOML was used;
no live Miniserver or database was contacted.

The timings in this table predate the relay audit. Metadata parity was verified
again after its parser changes; see that audit for the current parser timings.

| Export | Bytes | WebSocket routes | Grabber controls | Rust parse | Python parse |
| --- | ---: | ---: | ---: | ---: | ---: |
| 2026-01-04 | 2,605,334 | 7,981 | 838 | 55.6 ms | 237 ms |
| 2025-07-08 | 2,585,086 | 7,934 | 834 | 39.3 ms | 316 ms |
| 2025-01-19 | 2,524,186 | 7,872 | 832 | 37.5 ms | 257 ms |

All three comparisons had **zero missing, extra or differing routes/metadata**,
including grabber selection and VisuPwd flags. The WebSocket comparison includes
all known controls eligible for the old dynamic-promotion path, not only the
initial visible subset. Python time includes first-use parser selection; both
times include parsing and constructing prefixes and exclude network download.
This is a compatibility check with timing observations, not an isolated XML
parser benchmark.

Reproduce with the legacy Python environment:

```sh
cargo build --release --locked --example inspect_config
/path/to/legacy/.venv/bin/python scripts/compare_legacy.py \
  /path/to/legacy tests/fixtures/legacy_config.toml /path/to/export.xml \
  ./target/release/examples/inspect_config
```

## Checks

- The [relay audit](relay-audit.md#measured-results) adds paired before/after
  parser measurements: XML 32.736 → 20.898 ms, 33-field JSON 12.795 → 6.567 µs,
  with exact metadata parity on all three exports repeated after the changes.
- The second pass removes the temporary allocation from decimal rounding:
  53 → 40 ns per call, with exact reference results at all 16 supported
  precisions. Strict JSON validation retains the prior parser throughput.
- The [final audit](final-audit.md#performance-check-after-the-final-fixes)
  repeats paired measurements after the additional compatibility fixes:
  XML 21.136 → 21.164 ms and 33-field JSON 6.457 → 6.367 µs. Timings remain
  at the preceding level; XML uses 0.25% more allocations. Real metadata parity
  remains exact on all three exports.
- 62 unit/transport tests, seven complete bridge integration tests and six HTTP
  regression tests passed (75 total after the final audit pass).
- Precision coverage includes 10,000 deterministic f64 bit patterns through
  decimal JSON parsing and line-protocol serialization, with exact bit equality.
- The complete bridge tests use the real encrypted WebSocket client and fake
  Miniserver: early initial tables, normal/VisuPwd polling, EOF reconnect,
  readiness, cache reuse, rejected mismatched poll targets, startup cancellation
  and graceful shutdown.
- UDP packing, TCP framing, MQTT publish flushing and Execd cancellation tested
  against local sockets. MQTT worker failure without sensor traffic and failed
  shutdown cleanup are covered too. Monitoring uses actual HTTP and router tests.
- Formatting and Clippy (`-D warnings`) passed.
- Cargo audit reports zero unignored vulnerabilities; the public-key-only RSA
  exception inherited from the relay is documented in `.cargo/audit.toml`.
- Linux ARM64 Docker image built and passed version/legacy-config smoke checks.

Live hardware compatibility, long-duration process CPU/RSS measurements, output
failure stress tests and performance on the deployment host remain to be measured.
The bridge does not claim exactly-once delivery or historical database migration.
