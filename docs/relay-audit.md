# loxMqttRelay Rust history and performance audit

The [final cross-repository audit](final-audit.md) adds the concluding contract
matrix, seven additional findings and the current validation record. Measurements
below remain the dated before/after observations from their respective passes.

Audited 2026-10-30 against the immutable
[`697f66d`](https://github.com/Jakob-Gliwa/loxMqttRelay/commit/697f66ddd5b610b126d5dbff770a9c963e86f83e)
snapshot (0.5.0). Enumerated 247 commits across the available histories, including
246 reachable from this main snapshot and one additional `perf` commit. Inspected
the relevant implementation changes, all six PRs, 22 non-PR issues and 38 comments;
there were no inline PR review comments. The source repository was read only.

## Branches and PRs

| Branch / PR | What it actually contains | Relevance here |
| --- | --- | --- |
| `rust-rewrite`, [`6128a4b`, PR #1](https://github.com/Jakob-Gliwa/loxMqttRelay/pull/1) | Original 2025 Rust processing extension loaded through PyO3; Python still owned the application. Merged. | Earlier cached normalization, compiled regexes and batching; not the standalone port. |
| `perf`, [`194caa6`](https://github.com/Jakob-Gliwa/loxMqttRelay/commit/194caa6f664bcb236c577f571669a655f10904b0) | Unmerged alternative using `serde_json_borrow`, borrowed DOM values and buffer reservations. | Borrowing is useful; adding another DOM and parser dependency is unnecessary for our fixed `/all` schema. |
| `perfMiniserverSync`, [PR #8](https://github.com/Jakob-Gliwa/loxMqttRelay/pull/8) | Library LZ4 block/frame decoding, checksums and declared-length checks. Merged. | Already used in our bounded archive decoder. |
| `rust-full-port`, [`697f66d`, PR #28](https://github.com/Jakob-Gliwa/loxMqttRelay/pull/28) | Entire application in Rust, native lifecycles/config/sync, current dependencies and audit gates. Merged into main on the audit date. | Primary reference for this review. Main and this branch pointed to the same commit at discovery. |
| `bugfix`, [PR #7](https://github.com/Jakob-Gliwa/loxMqttRelay/pull/7) | Non-UTF-8 MQTT payloads no longer panic. Merged. | Applied the same robustness requirement to incoming text and XML persistence. |
| `multiarch`, [PR #2](https://github.com/Jakob-Gliwa/loxMqttRelay/pull/2) | Multiarch packaging. Merged. | Our CI already defines native amd64/arm64 container and test jobs. Local container validation uses ARM64; remote CI has not been run here. |
| [PR #11](https://github.com/Jakob-Gliwa/loxMqttRelay/pull/11) | Proposed topic-to-VI HTTP map, closed without merging. | Not copied. The explicitly requested health/metrics server serves a different purpose. |

Historical branch names may be deleted after merging; the commit IDs and PRs
above pin the reviewed versions independently of subsequent branch cleanup.

## Improvements adopted

### Selective XML scanning

[`84817fa`](https://github.com/Jakob-Gliwa/loxMqttRelay/commit/84817fae708d4593930068dc50dd2a46f9982f8d)
uses a streaming scanner for one precise query, reads only needed attributes,
borrows ordinary attribute text while inspecting it, and allocates normalized
text only when necessary. Our scanner already streamed, but still decoded and
allocated **every attribute on every element** into maps.

`src/controls.rs` now reads only Type/U/Title/Desc/StatsType/Analog/linkC on C,
Cr/Pr/Visu/VisuPwd on IoData, Unit on Display and U/K on Co. Other attributes
and elements do not get attribute maps. Attribute keys are static strings,
ordinary whitespace-free values need one retained allocation, duplicate values
are skipped before decoding, and self-closing tags never allocate a stack name.
Boolean flags use ASCII case-insensitive comparisons without lowercasing copies.

The relay's precise XML semantics remain intact: first duplicate wins; literal
CRLF becomes one space; character references are unescaped after normalization;
BOM and malformed closing tags are tolerated. Our query still extracts loxInFlux
metadata and parent/child routes, not the relay's VirtualInCaption whitelist.
All three real exports were compared again with Python: **zero missing, extra
or differing routes/tags/VisuPwd flags**.

### Parse polling JSON once, without a complete DOM

[`7812555`](https://github.com/Jakob-Gliwa/loxMqttRelay/commit/781255550d16fc457583bec49c2d1833ba95ffe1)
learns recurring MQTT layouts; [`4d56cd7`](https://github.com/Jakob-Gliwa/loxMqttRelay/commit/4d56cd74983fca140e4e137afc660ccf6f57077b)
borrows input, precomputes targets and eliminates locks/caches that cannot hit.

Our previous polling path built one `serde_json::Value` to check LL status, then
built a second tree in the writer. The new `src/grabber.rs` uses Serde visitors
to read the fixed response schema directly. Keys and unescaped strings borrow
from the response while parsing; irrelevant members are consumed without making
a DOM. Fields cross the queue already parsed and normalized. The producer now
borrows controls from one ArcSwap snapshot with bounded concurrent requests;
it no longer clones every UUID string and route prefix for each polling cycle.

No learned layout cache is needed for this fixed schema. Escaped strings and
changing field order work through the same parser. Duplicate keys retain their
last value, output keys retain the prior lexical processing order, and Code/code
precedence is unchanged. Numeric parsing stays in Serde with `float_roundtrip`,
never in a custom number scanner or an integer truncation path. Differential
tests retain the old DOM implementation **only in tests** and compare results,
errors and counters for changing shapes, duplicates, Unicode/escapes, missing
fields and invalid JSON. A further 10,000 finite f64 bit patterns pass through
both Default and AQ without a changed bit.

### Robustness fixes

- **Lossy XML recovery was undone during cache persistence.** The parser accepted
  a stray UTF-8 byte, but `String::from_utf8(xml)?` then failed the entire load.
  [Issue #4](https://github.com/Jakob-Gliwa/loxMqttRelay/issues/4) and the relay's
  scanner explicitly address this class of malformed export. A real HTTP test
  reproduced the failure before the fix and now checks initial loading,
  persistence and subsequent cache reuse with identical recovered metadata.
- **Invalid WebSocket text could leave a stale heartbeat observation.** It now
  invalidates that observation, increments the invalid counter and reports an
  error without panicking. Base64 forwarding from the MQTT relay is not copied
  into the sensor schema: binary garbage is not made into a valid sensor value.
- **An empty MQTT username sent credentials instead of connecting anonymously.**
  The relay's native client already treats an empty user as absent. Our writer
  now does the same; a socket-level test checks CONNECT flags and actual payload
  bytes for both empty and nonempty credentials.
- **Archive no-panic coverage was narrower.** Following
  [`d6aa790`](https://github.com/Jakob-Gliwa/loxMqttRelay/commit/d6aa790), tests now
  walk ZIP/container truncations and header corruption through the production
  decoder, alongside the existing size/CRC/LZ4 regressions.

### Dependencies and CI

The latest relay changes
[`7e2d50f`](https://github.com/Jakob-Gliwa/loxMqttRelay/commit/7e2d50f70174f17280ccac45435336e35ac62aa6)
and [`499fb72`](https://github.com/Jakob-Gliwa/loxMqttRelay/commit/499fb72)
prompted an actual audit of **our** lockfile. Its TLS dependencies were already
current; the relay's eight old TLS advisories did not reproduce here.

Our ZIP 4.6.1 did report RUSTSEC-2026-0367/0368/0369, concerning filesystem
extraction and permissions/symlinks. We only read named entries in memory, so
those extraction APIs are not used. Nevertheless ZIP was upgraded to 9.1.3;
only the deflate backend is enabled, removing the unnecessary zopfli encoder.
We retain a maintained archive reader rather than copying the relay's custom
central-directory reader.

The runtime client is now pinned to **loxwebsocket 0.3.2**, as in relay 0.5.0;
the declared Rust minimum is correspondingly **1.99**. Encrypted-client tests
pass on the new release. The dependency graph still has one rustls and one ring.
`cargo audit` now gates container builds in CI. Its one explicit exception is
the same narrowly justified RSA Marvin advisory as the relay: production only
encrypts with a public key; private-key decryption occurs in the local fake
server tests. The exception is documented in `.cargo/audit.toml`; no blanket
advisory suppression was added. Audit reports zero unignored vulnerabilities.

## Measured results

Same macOS ARM64 host, rustc 1.99.1, optimized release builds. Three alternating
before/after runs of the compiled benchmark binaries; each run takes seven
samples, reported below as the median of the three sample medians. Allocation
counts come from a separate warmed invocation, with counting disabled for timing.

| Workload | Before | After | Speedup | Allocations before → after |
| --- | ---: | ---: | ---: | ---: |
| Grabber JSON, 2 fields | 0.581 µs | 0.381 µs | 1.52× | 17 → 6 |
| Grabber JSON, 33 fields | 12.795 µs | 6.567 µs | 1.95× | 310 → 46 |
| Real XML, 2,605,334 bytes | 32.736 ms | 20.898 ms | 1.57× | 846,129 → 468,978 |

Allocated bytes for the XML decreased from 37,244,646 to 25,772,043 per parse;
for the 33-field JSON from 29,417 to 8,968. The JSON table compares **one** parsing
call in both versions; removal of the extra production parse is additional and
is not counted again in these speedup figures. The XML route counts remain
7,981 WebSocket routes and 838 polled controls.

These measurements cover parsing and route construction, not total application
CPU, network latency or database throughput. XML runs on startup/refresh. Binary
WebSocket values already avoid JSON parsing and retain their existing direct
f64 path. Reproduction uses `benches/parsers.rs` with `LOX_XML_BENCH` set to a
local export; no private export was added to the repository.

## What was already correct or deliberately not copied

| Relay idea / fix | Decision for loxToTelegraf |
| --- | --- |
| Immutable config, precomputed routes, ArcSwap and lazy logging (`1620d68`, `4d56cd7`) | Already present. Extended borrowing in the poll loop; no per-value locks or LRU added. |
| Firmware-17 bare LoxCC, CRC/length validation, reconnect resync (`76ea033`, `73dc8ed`) | Already covered; HTTP loader uses pooled requests and preserves old routes when refresh fails. |
| Do not rewrite unchanged config after failed sync (`b382703`) | No TOML write path; cache replacement happens only after successful loading and parsing. |
| Real MQTT session state, observable publish failures (`3cfa80b`, `5fa6781`) | Already checks CONNACK and transport flush; queue acceptance alone is not success. QoS 0 still provides no broker/database acknowledgment. |
| Dependency logs need an active subscriber (`2690f19`) | Already uses tracing-subscriber, including the upstream client. |
| MQTT v5 / mqtt-glide, no command outbox | Keep MQTT 3.1.1 compatibility. This bridge exports timestamped telemetry, not actuator commands; replacing its retry policy with command dropping would change its contract. |
| RegexSet and exact control-topic matching | Relay-specific MQTT ingress. Our type/UUID filters are exact sets and there are no inbound control topics. |
| Plain HTTP hyper client for filesystem sync | Retain HTTPS/443 support and pooled reqwest. The relay's plaintext-only assumption is not copied. |
| Baseline + x86-64-v3 binaries and launcher (`ce9bc21`) | Keep the portable baseline. No x86 benchmark was run here to justify two binaries; do not impose AVX2 or `target-cpu=native` on distributed images. |

Live Miniserver validation, long-duration CPU/RSS measurements, publication
workflows and the previously documented queue-overflow behavior remain outside
this offline audit. Local validation passed: 58 distinct unit/transport/bridge/
HTTP tests, parser parity on three real exports, formatting, Clippy with warnings
denied, dependency audit with the documented RSA exception, and release
compilation. The final Linux ARM64 Docker image also passed version and unchanged
legacy-TOML smoke checks with networking disabled. Remote CI was not executed.

## Second pass: lifecycle boundaries and parser equivalence

Repeated the GitHub branch/PR checks on 2026-10-30. loxMqttRelay remains at
`697f66d` with six PRs and the unmerged `perf` branch; loxInFlux remains at
`0a62da1` with five PRs. The WebSocket repository is at
[`0517b1b`](https://github.com/Jakob-Gliwa/loxwebsocketclient-rs/commit/0517b1bc73a5b5d74ffad40e3dbf4df0dc14fb29)
(the already pinned 0.3.2 release), with no PRs. There were no new upstream
fixes to pick up. This pass revisited the existing changes and their interaction
with the bridge, rather than treating the previous audit as proof of parity.

### Further findings corrected

| Finding | Correction and evidence |
| --- | --- |
| The selective JSON parser used `IgnoredAny` for unused members. Unlike the old DOM, Serde's skip path accepts overflowing numbers, unpaired surrogate escapes and non-UTF-8 string bytes, and bypasses the normal nesting limit. | A non-retaining visitor now validates skipped values with the same scalar/depth rules as the DOM. Extended differential tests cover those cases, including invalid nested sensor values. They fail before the correction. This follows the relay's `7812555`/`4d56cd7` rule that optimized and reference paths must agree on failures as well as outputs. |
| Clearing missing fields from successful polls did not cover a whole failed poll. After malformed JSON, LL 403 or a request failure, the last good reading could still be advertised with a fresh heartbeat timestamp. | Failure now invalidates only that control's observations through the same ordered queue as successful responses. Tests cover malformed/rejected responses, recovery to the same value, and the production encrypted polling path: its heartbeat stops while an unaffected WebSocket heartbeat continues. This extends the stale-template lesson from loxInFlux `f73ac37` to our heartbeat feature. |
| A cache with a matching version and valid JSON wrapper but unusable XML aborted startup instead of falling back to download. | Cache parsing failure now falls through to the normal validated download; the in-memory version is updated only on success. A real HTTP test first reproduces the startup failure, then verifies re-download, replacement and subsequent cache reuse. A malformed remote document still fails; no previously loaded route snapshot is replaced on error. |
| The 0.3.1 compatibility fallback still read both clocks for every early table record. In 0.3.2, `on_raw_payload` is called during the handshake as well. | Use that existing once-per-payload timestamp. A failing-before regression proves that records in the initial table keep the same timestamp/Instant before `Connected`. The EOF observer remains necessary: 0.3.2 still does not emit `ConnectionClosed` for every transport failure. |
| Config validation missed blank/malformed active hostnames and MQTT's 16-bit string byte lengths/NUL constraints, yet rejected unused MQTT settings for UDP/execd. | Validate the selected transport's endpoint and MQTT fields before opening sockets; keep unused transport settings harmless. Errors name the field without repeating credentials. Tests include IPv6, multi-byte UTF-8 boundary lengths, anonymous MQTT and inactive legacy settings. The early-validation model comes from relay [`17d1357`](https://github.com/Jakob-Gliwa/loxMqttRelay/commit/17d13572091dfb43971b25bdce0307fbf571a43f) and its native config port. |

### Allocation-free optional rounding

The relay's allocation audit (`4d56cd7`) prompted a closer check of our numeric
path. Rounding formatted every reading into a temporary heap string. It now
uses a 326-byte stack buffer for the sign, at most 309 integer digits, a decimal
point and the allowed 15 fractional digits. Both versions use the same standard decimal
formatter and f64 parser; no multiply/cast shortcut or custom rounding algorithm
was introduced. Tests compare exact output bits at every precision 0..15 for
10,000 deterministic bit patterns plus maximum/subnormal/halfway/negative-zero
cases.

Three alternating release-binary runs, each with seven timing samples, on the
same ARM64 host with no concurrent compiler/test workload:

| Workload | Before this pass | After | Allocations before → after |
| --- | ---: | ---: | ---: |
| One number rounded to five places | 53 ns | 40 ns | 1 → 0 |
| Grabber JSON, 2 fields | 0.374 µs | 0.368 µs | 6 → 6 |
| Grabber JSON, 33 fields | 6.536 µs | 6.534 µs | 46 → 45 |

The rounding microbenchmark takes about **25% less time**. JSON timing remains
effectively unchanged while strict failure parity is restored. The removed JSON
allocation was the skipper's temporary nesting stack. These figures do not
measure whole-process CPU consumption; the unchanged XML parser retains the
first pass's measurements. `benches/parsers.rs` contains the reproducible cases.

Validation after this pass: **67 tests** (56 unit/transport, five encrypted bridge,
six HTTP), formatting and Clippy with warnings denied passed. The dependency
lockfile and the documented audit exception are unchanged. Release/container
checks are recorded in [performance.md](performance.md).
