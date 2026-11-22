# Allocation optimizations and malformed Miniserver responses

Implemented on 2026-11-04. This report covers the changes after the October
repository audits and the configuration deprecation-warning change.

## Miniserver JSON and readiness

The supplied corpus contains twelve raw responses. Eleven are valid JSON. The
EMS-ESP `wwcircmode` response contains a complete JSON object inside a string
without escaping its quotes. Its status text is recovered exactly as
`{"cmd":"wwcircmode","data":"off","id":158}`; its numeric Default field is
retained too. The supplied raw files and manifest are unchanged.

The ordinary path still uses the selective, strict Serde parser. Only a parsing
failure enters the separate cold repair path. Repairs are limited to string
values named `value` in the root, `LL`, or its `output*` objects:

- A complete valid JSON object/array inside the outer quotes can be quoted
  correctly. Its original text, including whitespace and internal escapes,
  remains the sensor value.
- Literal control characters inside a sensor string can be escaped.
- The fallback accepts at most 1 MiB of input, 128 levels of nesting and 256
  repairs. The complete repaired response must pass strict deserialization.

The fallback does not guess arbitrary quotes, missing structure, damaged numeric
tokens, status codes or response identity. Normal LL status and target UUID
validation also runs after repair. No numeric rounding or float conversion policy
was changed; `serde_json` retains `float_roundtrip`.

Successful repairs increment `loxinflux_grabber_json_repaired_total`; the
first repair also emits a warning. An unreadable response increments
`loxinflux_grabber_data_errors_total` and the existing grabber error counter.
Raw payloads are not logged. The failed control's last observation is invalidated
through the ordered input queue, preventing stale heartbeat output.

Readiness distinguishes data quality from operational failure:

| Completed polling cycle | Readiness, if other dependencies are healthy |
| --- | --- |
| Valid or successfully repaired responses | HTTP 200 |
| At least one readable response and an isolated unreadable JSON response | HTTP 200, `degraded: true` |
| Every polled response is unreadable | HTTP 503 |
| Transport, authentication, status or correlation failure | HTTP 503 |

`loxinflux_grabber_unreadable_controls` and the corresponding `/health` JSON
field show the count from the most recently completed cycle. A later clean cycle
clears degradation. An empty polling set does not fail readiness.

## Changes retained

- WebSocket text classification borrows the incoming bytes. Actual text reuses
  the uniquely owned `Bytes` allocation as its final `String`; shared backing
  remains safe and is copied when necessary. Numeric events still allocate
  nothing in the warmed pipeline.
- Numeric-unit parsing uses the same regex and f64 conversion, with reusable
  thread-local capture slots. Ordinary words skip the unit regex. Capture storage
  initializes once per participating thread and holds no sensor text.
- Grabber state reuses existing field keys. Line-protocol escaping appends
  directly into the batch instead of constructing intermediate strings.
- Discovery interns identical field keys and removes the redundant UUID from
  each Route, since the map already owns that key. Route shrinks from 80 to 64
  bytes; Value and Input sizes remain unchanged.
- XML attributes borrow the source export when normalization/unescaping is
  unnecessary. Only retained metadata is owned. Checked source offsets keep the
  borrow safe without unsafe code; transformed attributes use owned strings.
- MQTT publishes share the batch allocation via `Bytes`. The writer retains it
  through retries and recovers it for reuse after the publish is flushed. This
  removes one payload-sized copy; rumqttc's packet serialization and network
  buffers still exist.

These changes move ownership through the pipeline; they do not make the complete
WebSocket/TLS/JSON/MQTT stack zero-copy. The current WebSocket callback supplies
borrowed text, so its initial copy into the async queue remains necessary.

## Measurement method

The baseline is the source snapshot immediately before this task, including the
configuration warning fix. Both variants use the same locked dependencies and
release profile on macOS ARM64, rustc 1.99.1: optimization level 3, fat LTO, one
codegen unit, without `target-cpu=native`. Before and after binaries are built
ahead of time, then run sequentially in five pairs with alternating order.
Compilation and tests do not run concurrently with these measurements.

Parser, hot-path and MQTT results report the median of five run medians, each
using seven timed samples after warmup. Allocation counters are disabled during
timing and enabled for a separate warmed operation. The allocator wrapper still
checks its disabled flag in both variants. Pipeline runs process two million
values each; their table reports the median of five runs. These are local
microbenchmark observations, not statistical proof of equal performance or a
measurement of production CPU utilization.

The separate allocation probe continuously tracks requested live heap bytes.
Its timings are not used. Its retained/peak figures exclude the already loaded
XML buffer, allocator bookkeeping and process RSS. XML benchmarks construct the
complete discovery metadata from the 2,605,334-byte export (7,981 WebSocket routes
and 838 grabber controls); they exclude download, ZIP/LoxCC decoding and TLS.

## Results

Times below are microseconds per operation unless stated otherwise. `dedup`
means unchanged values are suppressed; those cases serialize no new output
after warmup. Plain text includes the callback's initial byte copy. Poll cases
include parsing and engine processing, excluding the upstream response copy.

| Workload | Before | After | Allocations before → after |
| --- | ---: | ---: | ---: |
| Plain text, no dedup | 0.118 | 0.072 | 3 → 1 |
| Plain text, dedup | 0.087 | 0.043 | 3 → 1 |
| Text with numeric unit, no dedup | 0.266 | 0.250 | 2 → 1 |
| Text with numeric unit, dedup | 0.238 | 0.223 | 2 → 1 |
| Two-field poll, no dedup | 0.533 | 0.454 | 12 → 6 |
| Two-field poll, dedup | 0.399 | 0.392 | 8 → 6 |
| Strict JSON parser, 2 fields | 0.369 | 0.365 | 6 → 6 |
| Strict JSON parser, 33 fields | 6.471 | 6.476 | 45 → 45 |
| XML discovery, milliseconds | 21.632 | 14.976 | 470,165 → 177,750 |
| MQTT, 256-byte test text | 15.630 | 15.488 | 5 → 4 |
| MQTT, 16-KiB test text | 36.832 | 36.894 | 5 → 4 |
| MQTT, 256-KiB test text | 157.920 | 156.642 | 5 → 4 |

Plain text takes about 39% less time without dedup and 51% less with it. The
two-field polling path improves about 15% without dedup. XML discovery takes
31% less time and makes 62% fewer allocations. These are individual workloads,
not a predicted percentage reduction in total process CPU usage.

Strict JSON parsing and MQTT latency remain within run-to-run variation. For
example, the five 33-field JSON medians span 6.372–6.551 µs before and
6.391–6.601 µs after; 16-KiB MQTT spans 36.151–37.786 µs before and
36.438–37.373 µs after. The initial unit-text candidate showed a possible small
slowdown; reusing regex capture storage removed it and eliminated an allocation.

The larger numeric pipeline remains at the previous performance level:

| Numeric workload, 4,096 routes | Before, ns/value | After, ns/value |
| --- | ---: | ---: |
| Full f64 precision, changing values | 46.3 | 45.8 |
| Round to five decimals, changing values | 102.4 | 100.0 |
| Suppress unchanged values | 10.1 | 10.0 |

The smaller single-route numeric probe reports 33 → 34 ns without dedup and
8 → 8 ns with dedup, with overlapping sample ranges and 1 ns output resolution.
This is recorded rather than claimed as a speedup; the representative 4,096-route
benchmark shows no sustained regression. Numeric processing remains at zero
warmed allocations.

| Requested heap in the XML probe | Before, bytes | After, bytes |
| --- | ---: | ---: |
| Retained discovery data | 4,654,090 | 3,747,074 |
| Additional Engine initialization | 2,684,043 | 2,421,923 |
| Discovery + Engine | 7,338,133 | 6,168,997 |
| Peak additional heap during XML parse | 6,334,010 | 5,335,754 |

The combined retained heap decreases by 1,169,136 bytes (15.9%). Sharing field
keys reduces their distinct allocations from 23,943 to 976. Total requested
allocation bytes during XML discovery decrease from 25,841,997 to 21,441,264;
this cumulative figure is different from retained or peak heap.

For a 256-KiB MQTT test text, requested bytes allocated during a warmed write
drop from 262,430 to 286. At 16 KiB they drop from 16,670 to 286. This is one
avoided payload copy, not the total transport memory footprint.

Raw records: [paired timings](benchmarks/2026-11-04/before-after.txt),
[heap before](benchmarks/2026-11-04/heap-before.txt),
[heap after](benchmarks/2026-11-04/heap-after.txt),
[XML parity](benchmarks/2026-11-04/xml-parity.txt), and
[frame experiment](benchmarks/2026-11-04/frame-experiment.txt).

## Frame batching experiment

`benches/frame_queue.rs` compares per-record numeric queue entries with a copied
raw frame passed once through the queue and decoded in the consumer. In the
initial experiment, 256-record frames improved from 15.172 to 9.718 µs. However,
with one or sixteen records, the frame variant introduces a heap allocation
where the warmed record variant has none. It also needs a new queue budget based
on records/bytes to preserve the current bound.

The experiment therefore remains outside production. Its benchmark excludes
production lifecycle/correlation handling and metrics, so it is not evidence
for a drop-in replacement. An owned-frame client API and equivalent queue and
failure semantics would be needed before reconsidering it.

## Verification and limits

The regression suite covers the real twelve-response corpus, exact recovered
status content, valid-response equivalence, ambiguous/oversized/deeply nested
invalid JSON, status and identity rejection after repair, degraded readiness,
total polling failure, recovery and suppression of stale heartbeat data. It
also retains precision, legacy configuration, encrypted WebSocket, HTTP,
UDP/TCP/MQTT and shutdown checks.

Offline Python/Rust discovery comparisons on all three real XML exports have
zero missing, extra or differing routes, prefixes, selections or security flags.
No live Miniserver or database was queried during this task.

All 84 tests pass: 68 library unit/transport tests, one CLI deprecation-warning
test, nine complete bridge integration tests and six HTTP loader regressions.
Formatting, Clippy with warnings denied, and the native release build pass.

The Docker build could not start because the local Docker daemon was unavailable
(`docker.sock` absent). The October Docker smoke results are historical; this
revision has not been validated as a container. Long-duration CPU/RSS and live
deployment measurements remain outside these local benchmark results.

## Reproduction

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo bench --locked --bench pipeline
LOX_XML_BENCH=/path/to/export.xml cargo bench --locked --bench parsers
cargo bench --locked --bench hotpaths
cargo bench --locked --bench mqtt
cargo bench --locked --bench frame_queue
cargo run --locked --release --example allocation_probe -- /path/to/export.xml
```

MQTT uses a local loopback broker and measures write-through-flush latency;
allocation counts cover the client and synthetic broker tasks. The ordinary
parser benchmarks require no network. The legacy schema comparison command is
documented in [performance.md](performance.md#real-xml-parity).
