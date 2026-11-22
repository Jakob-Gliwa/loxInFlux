# Migration and source audit

## Upgrading from 0.3 to 0.5

loxInFlux 0.5.0 replaces the Python implementation (0.3.x) with a Rust binary.
The configuration loads unchanged; the written data does not. The list of
breaking changes is in the [README](../README.md#breaking-changes-in-050).

1. **Pin the old version first.** Change `acidcliff/loxinflux:latest` to
   `acidcliff/loxinflux:0.3` in your compose file so the production writer does
   not switch on its own.
2. **Check the configuration** with the new image:
   `docker run --rm -v "$PWD/config:/app/config:ro" acidcliff/loxinflux:0.5 --check-config`.
   Remove the entries that are reported as deprecated.
3. **Run a pilot next to production.** Start a second container with a copy of
   the configuration that writes to a fresh bucket or a separate Telegraf
   output. Compare numeric values, UUID and field counts and field types. Check
   AQ decimals, `__text__*` fields, `/health`, queue depth, dropped values and
   Telegraf write errors. Both versions can poll the same Miniserver; the 0.5
   pilot needs no data directory.
4. **Decide what happens to the history.** Every numeric field is a float from
   0.5 on. Fields that 0.3 stored as integers or strings in the same bucket
   reject the new points with `field type conflict`. Either
   - migrate the history into a new bucket with
     [loxInFlux-migration-helper](https://github.com/Jakob-Gliwa/loxInFlux-migration-helper)
     (recommended). It applies the same conversions as 0.5, verifies the copy
     series by series, catches up the time since the copy and renames the
     buckets, so consumers keep their bucket name; tokens bound to the old
     bucket need replacing. Its
     [switchover guide](https://github.com/Jakob-Gliwa/loxInFlux-migration-helper/blob/main/docs/switchover.md)
     covers this step and step 5. It can also copy values that InfluxDB still
     holds on disk but no query returns (`recover`), or
   - switch production to a fresh bucket and query the old one for history, or
   - convert the old field types to float in place before switching. The
     [InfluxDB v2 migration script](https://gist.github.com/BetaWayOfLife/69eefad3cf5a0e97bc3251c03fc6d898)
     from [PR #6](https://github.com/Jakob-Gliwa/loxInFlux/pull/6) reads,
     coerces, deletes and rewrites one month at a time, and defaults to a dry
     run. Take a backup and stop loxInFlux while it runs.
5. **Switch production** to `:0.5`, keep the `/app/config` mount, drop the
   `/app/data` mount and the `[paths]` section, and optionally map port 9108 for
   health and metrics.
6. **Optionally enable change-only writes** (`processing.only_on_change = true`
   with `heartbeat_interval = 3600`). Adapt graph fill and rate windows to the
   sparser data.
7. **Delete the old configuration copies.** 0.3 wrote one `sps_*.xml` per
   Miniserver program version plus `LoxAPP3.json` into the data directory and
   never removed them; 0.5 experimental builds wrote `configuration-cache.json`.
   They contain the complete Miniserver program, including users and
   credentials. 0.5 neither reads nor deletes them. Remove them once a rollback
   is no longer planned; 0.3 downloads its configuration again if they are gone.

**Rollback:** go back to `acidcliff/loxinflux:0.3.0` and restore the `/app/data`
mount. Float points that 0.5 already wrote stay in the bucket and will in turn
conflict with 0.3's integer writes, which is another reason to pilot against a
separate bucket.

## Source audit

Reference implementation: local loxInFlux 0.3.0 (Python) source, its test suite and
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
| Config cache and reconnect refresh | No disk cache; periodic and reconnect version checks over the encrypted WebSocket, HTTP download only when the version changed |
| UDP/TCP/MQTT/execd | All supported; batching, framed writes and bounded retries |
| Rounding | Same setting; optional decimal rounding, never integer truncation |
| Graceful shutdown | Stop producers, close WebSocket, drain output with deadline |
| HTTP redirects and HTTP-200 LL errors | Bounded redirects; JSON/XML status checked before decoding |
| Execd log severities | Legacy stderr prefixes retained; stdout reserved for metrics |
| Health and metrics | HTTP readiness/liveness and Prometheus, no separate sidecar |

The Miniserver configuration is held in memory only and downloaded on every
start; existing Python cache files remain untouched. Token
authentication/refresh/reconnect remain owned by the WebSocket client;
credentials come from the existing TOML configuration.

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
