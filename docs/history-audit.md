# loxInFlux commit and PR regression audit

Audited 2026-10-30 against old main at
[`0a62da1`](https://github.com/Jakob-Gliwa/loxInFlux/commit/0a62da1a).
The GitHub main history and local history agree: **32 commits**. Also inspected
all **five PRs**, the diffs of the unmerged numeric PRs, all **three issues**,
**15 issue/PR comments**, the old regression suite and the local Cursor error
discussion. There were no inline PR review comments. Dependency-only PRs #4/#7
change Python dependencies and do not supply Rust application fixes.

## Gaps found and corrected

| Finding before this audit | Historical evidence | Correction and regression coverage |
| --- | --- | --- |
| HTTP redirects were disabled; a 302 could become a misleading “No sps configuration” error. | [`613de19`](https://github.com/Jakob-Gliwa/loxInFlux/commit/613de19705120ab312aa8c0e0c2ddae278d62973) explicitly follows redirects on the filesystem API. | Follow at most ten redirects. Real HTTP tests cover version/list/download/app redirects, same-origin Basic auth, redirect loops and removal of credentials on an origin change. |
| HTTP-200 LL failures in filesystem/app responses lost their real status. Only version/poll responses were checked. | [`2e1c1ff`](https://github.com/Jakob-Gliwa/loxInFlux/commit/2e1c1ff5) / [PR #8](https://github.com/Jakob-Gliwa/loxInFlux/pull/8) fixed misleading archive errors for rejected downloads. | Check JSON and XML LL envelopes before interpreting the response. Tests inject 403 at fslist, fsget and LoxAPP3, checking both status and endpoint in the error. Response values and credentials are not included. |
| The new heartbeat could revive a disappeared/invalid grabber field from cached observations; an invalid WebSocket reading could also leave an old good value alive. | The stale-field problem was fixed for cached Point templates in [`f73ac37`](https://github.com/Jakob-Gliwa/loxInFlux/commit/f73ac37161b7e7883ff5774b06fbee3a32b8902d). The Rust heartbeat introduced another route to the same symptom. | Reconcile heartbeat fields with each complete `/all` snapshot; clear invalid WebSocket observations. Tests cover missing fields, placeholders, JSON null, NaN and recovery to the same valid value. These tests failed before the fix. |
| Execd emitted ordinary stderr log lines without the legacy severity prefixes. | `TelegrafFormatter`, retained in [`b6ea287`](https://github.com/Jakob-Gliwa/loxInFlux/commit/b6ea2871de0ffecb4586500240e0636dfca02ea3) and covered by the old logger tests. | Restore `E!`, `W!`, `I!`, `D!`, `T!` in execd mode. All five levels are checked through a real tracing subscriber. |

Telegraf treats unprefixed execd stderr as errors, so the last finding caused
false error logs even when the bridge was healthy.
[Telegraf execd contract](https://github.com/influxdata/telegraf/blob/master/plugins/inputs/execd/README.md).

The HTTP and heartbeat cases were reproduced with failing tests before changing
their implementation. No live Miniserver or database was used.

## Previously solved bugs already carried over

| Old fix | Rust behavior and verification |
| --- | --- |
| [`3dd34fa`](https://github.com/Jakob-Gliwa/loxInFlux/commit/3dd34faeb908c93fa8884e90dc2b967ea6cc6f13): polling `VirtualTextIn` writes `all` into the control. | Excluded from polling; eligible WebSocket routes remain. Dedicated regression test. |
| [`5bbf642`](https://github.com/Jakob-Gliwa/loxInFlux/commit/5bbf6423b5eab15ec6da4049ab5f429427edcb2f), [issue #1](https://github.com/Jakob-Gliwa/loxInFlux/issues/1): configured Miniserver port ignored. | Custom ports used by HTTP and WebSocket; 80/443 semantics retained. URL unit tests and actual HTTP/WebSocket integration tests use nonstandard ports. |
| `5bbf642`: malformed Loxone 16 XML. | Duplicate attributes keep the first value, BOM and missing closing tags handled. Fixture regressions plus prior exact metadata comparison of three real exports. General libxml2 recovery equivalence is not claimed. |
| [`08f039a`](https://github.com/Jakob-Gliwa/loxInFlux/commit/08f039aba80f0c354c4efb021c0490800e4f1ced), [issue #2](https://github.com/Jakob-Gliwa/loxInFlux/issues/2): missing IoData crashes; strings `false` interpreted as true. | Missing IoData gives empty defaults. Only `1`/`true`/`yes` enable Visu/VisuPwd. Added explicit cases for absent IoData and true/false flag forms. |
| [`2a4a8ba`](https://github.com/Jakob-Gliwa/loxInFlux/commit/2a4a8ba643da44efe9dc060c1143a9d905135a6a): efficient LZ4 decoding. | Library block/frame decoding, size/CRC validation, bounded allocations; roundtrip/corruption/truncation tests. |
| [`a673601`](https://github.com/Jakob-Gliwa/loxInFlux/commit/a6736017ff522a7ef6787bb3fbe13d8dacf2fa21), `613de19`: missing await and wrong username field in freshness checks. | Explicit awaited HTTP version check using `miniserver.user`; startup cache and repeated-load tests. |
| `613de19`: FTP disabled on modern firmware. | HTTP fslist/fsget exclusively; full-access permission requirement documented. |
| `2e1c1ff`: firmware 17 picks newer bare LoxCC; filename version rollover. | Content magic determines ZIP/LoxCC decoding; choose numeric timestamp first, numeric version second; fetch LoxAPP3 separately when needed. Unit and actual HTTP tests. |
| `2e1c1ff`: linkC promotion inside discovery loop. | Two-pass discovery/promotion, with global blacklist respected. Existing link and real XML parity tests. |
| `f73ac37`: untracked write tasks, interleaved TCP drains. | One writer awaits each batch; bounded input queue. Overflow is visible and fatal. See the backpressure qualification below. |
| `f73ac37`: TCP writes lack newline; execd unnecessary buffering. | Every point is newline framed; recv-many batches contain immediately available points without waiting to fill. TCP socket and cancellable stdout tests. |
| `f73ac37`: TCP retry off-by-one and zero retries incorrectly doing no work. | Positive limits mean exactly that number of attempts; zero/negative unlimited. Added exhausted-limit and recovery-after-failure tests for both nonpositive forms. |
| `f73ac37`: secured poll sends byte-string UUID / wrong client API. | Plain UUID command, serialized fresh salt/hash/response path. Real encrypted VisuPwd integration test. |
| `f73ac37`: incorrect MQTT callback signatures. | No Python callback adapter; typed rumqttc event loop, tested CONNECT/publish flush/DISCONNECT. |
| `f73ac37`: import side effects, premature startup failure hangs, shutdown cancels unrelated tasks. | Explicit Rust construction, failure propagation and owned task cancellation; bridge lifecycle integration tests. No module import initialization. |
| [`5715be4`](https://github.com/Jakob-Gliwa/loxInFlux/commit/5715be4743ce21068007775caafe3e7a735b7d1d), [`df859d7`](https://github.com/Jakob-Gliwa/loxInFlux/commit/df859d74): SIGILL and smoke bootstrap drift. | No pygixml or required AVX2/`target-cpu=native`; CI runs the real suite on amd64/arm64/macOS. Container checks now also run on native amd64 and arm64. Remote CI execution remains pending. |

## Processing policy from PR #5

[PR #5](https://github.com/Jakob-Gliwa/loxInFlux/pull/5) was **closed without a
merge**, because the owner explicitly wanted unit extraction to remain in
Telegraf. [PR #6](https://github.com/Jakob-Gliwa/loxInFlux/pull/6) remains **open**;
the owner agreed that numeric values must always be floats. Its diff also
contains PR #5's unit parser. These are different decisions, not two merged fixes.

The new Rust defaults interpret numeric-with-unit strings in accordance with
the supplied 30-day analysis. This is a deliberate behavior change, even though
old TOML files load unchanged. Existing installations that already process raw
values in Telegraf can use:

```toml
[processing]
only_on_change = false
parse_numeric_units = false
drop_empty = false
drop_placeholders = false
separate_text_fields = false
```

This preserves raw string values and their field names. Genuine numbers still
become floats and retain decimals; `int(float)` truncation is never restored.
A regression test checks this combination. Keeping mixed raw field types
requires the existing Telegraf processors to normalize them before InfluxDB.

The old tests asserting that integer strings produce integers are deliberately
not ported: that would lock in the bug highlighted by PR #6 and the Cursor chat.
Rust tests instead check consistent float line protocol and 10,000 finite f64
bit patterns through JSON and serialization.

Existing Influx data retains its previous field types. The
[PR #6 discussion](https://github.com/Jakob-Gliwa/loxInFlux/pull/6#issuecomment-4989686946)
explicitly confirms that correcting the writer does not repair old shards. The
fresh-bucket/history-migration requirement in the migration guide still applies.

## Remaining qualifications

- **Backpressure differs at the client boundary:** Python could await the writer
  inside async callbacks. The pinned Rust client's callbacks are synchronous.
  Rust awaits output in one task and bounds memory, but sustained overload can
  exhaust the queue and stop the process with counted data loss. This is not a
  claim of lossless backpressure or exactly-once delivery.
- **Release publishing has no Rust equivalent yet.**
  [`eec697f`](https://github.com/Jakob-Gliwa/loxInFlux/commit/eec697f2184fe1661281297594db86b4d0b3b369)
  added tag/version validation, all-test publication gates, prerelease protection
  for `latest`, and release retraction on failed gates. Current Rust CI gates
  container builds on all tests and does not publish. Any future publication
  workflow must retain those four rules and both architecture manifests.
- [Issue #3](https://github.com/Jakob-Gliwa/loxInFlux/issues/3) also reported an
  absent arm64 manifest and continuing malformed JSON errors. The comments do
  **not** establish a final diagnosis/fix for that JSON symptom. Rust creates a
  missing cache directory and has real encrypted-client tests, but no claim is
  made that those reproduce that reporter's particular device responses.
- Live firmware/device validation, sustained CPU/RSS/throughput measurements and
  an Influx history migration remain separate from this offline regression audit.

## Reproduction

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo build --locked --release --bin loxinflux
```

The new HTTP checks are in `tests/loader_regressions.rs`; heartbeat, normalization,
logging, control flags and retry checks live next to their implementation.
Validation passed: **51 tests** (43 unit/transport + 4 bridge + 4 HTTP), formatting,
Clippy with warnings denied, and the optimized release build.

The [second relay audit pass](relay-audit.md#second-pass-lifecycle-boundaries-and-parser-equivalence)
rechecks both project histories and extends the stale-field and cache lessons to
failed polling responses and damaged version-matching caches. Its validation
record supersedes the test count above.
