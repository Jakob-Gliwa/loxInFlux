# Migration and source audit

Reference implementation: local loxInFlux 0.3.0 source, its test suite and
`cursor_error_writing_metrics_to_influxd.md`. The supplied 30-day analysis was
used to identify field precision/type conflicts, invalid placeholder traffic,
and redundant periodic writes; its database counts were not independently
queried again.

Reviewed upstream changes:

- [PR #5](https://github.com/Jakob-Gliwa/loxInFlux/pull/5): parse numeric unit strings without treating dates/times as metrics.
- [PR #6](https://github.com/Jakob-Gliwa/loxInFlux/pull/6): always use floats, fixing both truncation and numeric type conflicts.
- [PR #8](https://github.com/Jakob-Gliwa/loxInFlux/pull/8): firmware 17 bare LoxCC payloads, HTTP filesystem access, cache and writer/lifecycle fixes.

The subsequent [complete history audit](history-audit.md) covers all 32 main
commits, all five PRs, three issues and their 15 comments. PR #5 was **closed
without merging by design**, and PR #6 is **still open**; their descriptions
must not be mistaken for changes already present in the old release.

| Existing behavior | Rust implementation |
| --- | --- |
| Existing TOML tables, CLI and Docker paths | Supported; original template is an unmodified test fixture |
| Numeric WebSocket events | Reused pinned loxwebsocket client; full f64 precision |
| Non-visualized controls | Bounded concurrent `all` request/response polling |
| VisuPwd controls | Serialized salt/hash/request sequence, awaits the actual response |
| Type/UUID filters and parent filters | Supported, including unexpected WebSocket events |
| Linked visible controls | Promoted after discovery; blacklist semantics preserved |
| XML duplicate attributes/BOM/recovery | Streaming parser, first duplicate wins, recover parsed prefix |
| ZIP and bare LoxCC | Magic-based detection, LZ4 block/frame, CRC and size validation |
| LoxAPP3 JSON | ZIP member or separate `/data/LoxAPP3.json` download |
| Config cache and reconnect refresh | Atomic combined XML/JSON cache; version check plus periodic refresh |
| UDP/TCP/MQTT/execd | All supported; batching, framed writes and bounded retries |
| Rounding | Same setting; optional decimal rounding, never integer truncation |
| Graceful shutdown | Stop producers, close WebSocket, drain output with deadline |
| HTTP redirects and HTTP-200 LL errors | Bounded redirects; JSON/XML status checked before decoding |
| Execd log severities | Legacy stderr prefixes retained; stdout reserved for metrics |
| Health and metrics | HTTP readiness/liveness and Prometheus, no separate sidecar |

The cache format is new and is downloaded once on migration; existing Python cache
files remain untouched. New caches bind to Miniserver address and user. JSON/XML
are written in one atomic file to prevent torn pairs after a crash. TOML connection
passwords are not added to that cache. Token authentication/refresh/reconnect remain owned
by the WebSocket client; credentials come from the existing TOML configuration.

The Rust scanner tolerates the malformed XML cases in the existing fixtures
(duplicate attributes, BOM, missing closing tags). It is not a reimplementation
of every possible libxml2 recovery rule; unusual corrupt exports still require
comparison against the real file. Previously ignored entries at the top level and directly under `telegraf` or
`filters` are accepted with a deprecation warning, have no effect, and will be
rejected in a future version. Other unknown fields and invalid supported values
remain errors. This compatibility policy supersedes the strict rejection policy
recorded in the October audits.

## Rollout

1. Copy the existing config unchanged and run `--check-config`.
2. Use a fresh Influx bucket or a separate Telegraf output for the pilot; compare
   numeric values, UUID/field counts and field types before replacing production.
3. Check AQ decimals, text fields, `/health`, queue depth, dropped values and
   Telegraf write errors. The existing production process is not modified here.
4. Enable change-only writes with a 3600-second heartbeat when desired. Adapt
   graph fill/rate windows to sparse observations; reduction depends on the data.
5. Telegraf remains the boundary for an optional VictoriaMetrics/VictoriaLogs
   migration. Genuine text remains available under `__text__*`. Database history
   conversion and VictoriaLogs routing are separate work and are not performed
   by the bridge.

## Reuse provenance

`src/archive.rs` follows the LoxCC header/CRC/LZ4 design in loxMqttRelay commit
`54dacfbb74d1d3cafb2cab98674186c37526f6a4`; it uses a maintained ZIP reader with
restricted features and bounded output. XML recovery follows the same project's
streaming approach, expanded for loxInFlux metadata and control selection.

`tests/support/loxwebsocket/` is copied from loxwebsocketclient-rs commit
`cfce7bd65626902b8823a2c4dcc322e78b98eedb` (MIT, Jakob Gliwa), extended with
configuration files and poll responses. Its real RSA/AES/token/visu handshake
makes the bridge tests exercise the actual client rather than mocking it out.
The upstream MIT notice is retained in the root LICENSE.

The subsequent [relay audit](relay-audit.md) reviews the standalone Rust port
through 0.5.0, ports selective XML/JSON reading and recovery fixes, and measures
them against the initial implementation. Runtime uses loxwebsocket **0.3.2**;
the fake-server files retain the original 0.3.1 provenance described above.

The pinned client does not emit `ConnectionClosed` for every transport failure
(notably unexpected EOF/read timeout). A one-second state observer compensates:
readiness drops and dedup observations are invalidated before reconnect snapshots
arrive. Client 0.3.2 also invokes the raw-payload hook during authentication, so
early event tables share a fresh timestamp per payload. The second relay audit
removed the obsolete fallback that read the clocks for every early record.

The [second audit pass](relay-audit.md#second-pass-lifecycle-boundaries-and-parser-equivalence)
also covers failed-poll heartbeat invalidation, damaged-cache recovery, strict
JSON validation without a DOM and allocation-free decimal rounding. Endpoint
validation applies to the selected output transport; unused MQTT/TCP settings do
not prevent an otherwise valid legacy configuration from loading.

The [final cross-repository audit](final-audit.md) additionally corrects legacy
metadata escaping, first-direct-child XML semantics, unit markup and idle MQTT
worker failure. Polling validates plaintext response targets in the existing
JSON pass and rejects detected client correlation failures. Opaque encrypted
responses still depend on the client's own correlation behavior; its coarse
fallback is documented in the audit.

Cancellation during initial WebSocket connection retains ownership of the
handshake until it completes or fails. An unclaimed successful client then
requests token/session cleanup. This is asynchronous and does not guarantee
immediate revocation during an unresponsive handshake or after process exit.

## November data-quality and allocation update

The [November report](optimization-2026-11-04.md) supersedes the earlier blanket
statement that any failed grabber response makes readiness fail. An isolated
unrepairable JSON response now reports degraded data quality and clears its
cached readings; transport/authentication/correlation failures and an entirely
unreadable cycle still fail readiness. The strict parser remains the first path;
bounded repairs are limited to sensor string values and undergo full validation.
No rounding, UUID-filter or field-type policy is changed by these optimizations.
