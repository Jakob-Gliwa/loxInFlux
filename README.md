# loxToTelegraf

Rust rewrite of [loxInFlux](https://github.com/Jakob-Gliwa/loxInFlux), using
[loxwebsocket](https://github.com/Jakob-Gliwa/loxwebsocketclient-rs) pinned to
release **0.3.2** (Rust 1.99+). Collects visualized states and polls
non-visualized controls, then forwards Influx line protocol to Telegraf through
UDP, TCP, MQTT 3.1.1/QoS 0, or execd/stdout. No Python runtime.

## Start

The [relay history audit](docs/relay-audit.md) records the XML/JSON optimizations,
regression fixes and decisions carried over from loxMqttRelay's Rust branches.
The [final cross-repository comparison](docs/final-audit.md) adds a detailed
contract matrix, further lifecycle/parser regressions and remaining client API
limitations.

```sh
cp config/default_config.toml config/config.toml
# Edit Miniserver and Telegraf credentials/addresses.
cargo build --release --locked
target/release/loxtotelegraf --config-dir config --check-config
target/release/loxtotelegraf --config-dir config --data-dir data
```

An existing loxInFlux `config.toml` can be used directly. All existing tables and
documented keys are supported, as are `--config-dir`, `--data-dir`, `--log-level`
and the Docker environment variables `CONFIG_DIR`, `DATA_DIR`, `LOG_LEVEL`.
Legacy entries that Python ignored at the top level, directly under `[telegraf]`
or directly under `[filters]` now produce a deprecation warning and are ignored.
This includes `telegraf.round_floats` and `telegraf.round_decimals`; they do not
activate rounding. Remove these entries: accepting them will no longer be supported
in a future version. Effective rounding settings remain `general.round_floats`
and `general.rounding_precision`. Invalid supported settings still fail validation.

CLI arguments override environment variables; the data directory then overrides
`[paths].data_dir`. `--config-file` also accepts an explicit file. Logs go to stderr.

```sh
docker compose build
docker compose up -d
```

The existing `/app/config` and `/app/data` mounts remain usable. The Docker image
also provides `loxInFlux` as a binary alias. Configuration HTTP endpoints require
a Miniserver user with full access, as in loxInFlux 0.3.0. Port 443 selects HTTPS;
other ports select HTTP. `miniserver.tls_verify = false` preserves the existing
self-signed certificate behavior; set it to true for normal certificate validation.

## Precision and compatibility

Numbers stay `f64` through parsing, normalization and serialization, including
whole numbers. `21.2` never becomes `21`, and `42` is written as `42.0`, never `42i`.
Without rounding, the shortest decimal representation round-trips to the same
binary float. Existing `round_floats` settings still apply; the new template turns
rounding off to preserve the received precision.

The XML control title remains the measurement name. Existing tags are preserved:
`name`, `description`, `uuid`, `subuuid`, `statstype`, `analog`, `type`, `unit`,
`category`, `room`, `visu`, `source`, `application=loxInFlux`. Numeric fields remain
`Default`, `AQ`, `Q`, or the output/state name. Empty tags are omitted as before.

Intentional corrections:

- Numeric strings and number/unit strings become floats on both paths. Dates,
  times, comma decimals and sentences beginning with a number remain text.
- Empty values, `<v.…>` placeholders, unsupported structured values and nonfinite
  numbers are discarded and counted.
- Genuine strings use a separate field, e.g. `__text__Default` or `__text__TQ`.
  This prevents collisions when different controls with the same measurement
  title produce different value types. Numeric names starting with `__text__` or
  `__number__` are escaped into the reserved `__number__` namespace.
- WebSocket filters also apply to dynamically discovered state updates. The
  old fallback could bypass them. Parent filters also apply to their children.
- WebSocket text states (type 3) are handled as well as numeric states (type 2).
- Every point has a receive timestamp in nanoseconds, including grabber points
  without additional outputs. Multi-output grabber responses share a timestamp.

The corresponding switches in `[processing]` can preserve legacy text/empty
field behavior when needed. The numeric truncation bug is deliberately never
reintroduced. `VirtualTextIn` is never polled: its `all` command would write text.
Daytimer/weather tables were not exported by loxInFlux and are not mapped here.

In the old project's PR #5, unit extraction was deliberately left to Telegraf.
If your existing Telegraf processors already own this step, use
`processing.parse_numeric_units = false`. To preserve raw string field names,
empty strings and placeholders as well, see the explicit compatibility settings
in [the history audit](docs/history-audit.md#processing-policy-from-pr-5).

With `protocol = "execd"`, metrics go to stdout and logs go to stderr with
Telegraf's `E!`/`W!`/`I!`/`D!`/`T!` severity prefixes.

**Existing InfluxDB field types do not change when the application is fixed.**
Use a fresh bucket for the first precision-correct run, or plan a separate
history/schema migration. Writing a float into an old integer/string field can
still be rejected. No historical data is rewritten by this application.

## Change-only writes and heartbeat

Add to an existing configuration:

```toml
[processing]
only_on_change = true
heartbeat_interval = 3600
```

Without this table, existing configs retain write-every-observation behavior.
The new template enables change-only writes. Comparison happens per UUID,
source and field after optional rounding. A timer reemits unchanged values at
the heartbeat interval only while the WebSocket session is healthy. Polling
fields additionally need a recent observation (within two polling intervals plus
one request timeout). Disconnection clears cached observations. Set the heartbeat
interval to zero to disable reemission.

Polling still happens at `general.grabber_interval`: suppressing writes reduces
database volume, not requests to the Miniserver. Visualized controls remain the
way to get immediate events without polling. `drop_redundant_default = true`
optionally drops a numeric Default only when another numeric output has exactly
the same value; disabled by default because equality alone does not prove that
two outputs represent the same quantity.

## Health and metrics

Default address: `0.0.0.0:9108`, configurable in `[monitoring]`.

| Endpoint | Meaning |
| --- | --- |
| `/health`, `/ready` | 200 when dependencies work; isolated unreadable poll responses report `degraded: true`. Connection/authentication errors or a completely unreadable polling cycle return 503. |
| `/live` | HTTP server is running, including during startup |
| `/metrics` | Prometheus text exposition |

Malformed Miniserver JSON is parsed strictly first. A bounded fallback can repair
unescaped complete JSON objects/arrays inside sensor strings (for example an
EMS-ESP status string) and literal control characters in those strings. The
repaired document is validated again; status and target UUID checks still apply.
Ambiguous quotes, broken structure and invalid numbers are not guessed.

An unrecoverable individual response clears that control's cached observations
and increments `loxtotelegraf_grabber_data_errors_total`. Other controls continue;
`/health` includes `degraded` and `grabber_unreadable_controls`. If every poll is
unreadable, readiness fails. Authentication, transport and correlation errors
continue to fail readiness even when another control responds. Successful later
cycles clear the current degradation. Repairs are counted separately in
`loxtotelegraf_grabber_json_repaired_total`; neither metric includes raw payloads.

For Uptime Kuma, use an HTTP monitor pointing at `http://HOST:9108/health` with
expected status 200. No sensor changes are required to stay healthy. Docker
healthchecks invoke `loxtotelegraf --healthcheck`; no curl is required. If monitoring
is disabled, disable the container healthcheck as well.

Counters include received/written/filtered/invalid/unchanged/dropped values,
written points and bytes, heartbeat fields, output errors, grabber requests and
errors, configuration reloads and WebSocket reconnects. Gauges expose queue
depth/capacity, control counts, readiness, uptime and last receive/write times.
Metrics have no UUID/name labels, avoiding per-control cardinality.

`values_written_total` counts fields successfully handed to the transport, not
database commits. UDP cannot confirm a receiver exists; TCP and MQTT QoS 0
cannot confirm storage in InfluxDB. MQTT counts a write after the client flushes
the publish to its socket. Monitor Telegraf/database errors separately.

## Performance and delivery

A single Tokio I/O runtime avoids a worker thread per event. Static line-protocol
prefixes and field keys are precomputed. Numeric lookup uses the client's binary
UUID map. Values traverse a bounded channel without JSON conversion, UUID string
formatting, or spawning per-value tasks. The consumer drains available events in
batches and reuses its serialization buffer. It does not wait to fill a batch.
Configuration download/decompression/parsing is outside the measurement hot path.

TCP/stdout/MQTT batch writes; UDP packs complete newline-terminated points into
datagrams capped by `udp_max_bytes`. A single oversize UDP point fails explicitly;
use TCP or a larger configured limit for large text/output sets. Transport errors
retain and retry the batch with its original timestamps. A partial TCP write may
therefore be replayed. The queue is in memory and is not a persistent delivery log.

The pinned client's callbacks are synchronous. If the bounded queue fills, the
application counts the loss and stops with an error rather than continuing with
an unreported gap. This is a deliberate, observable limit, not a lossless-delivery
claim. Increase `queue_capacity`, improve the output, or lower volume for sustained
overload. SIGTERM/SIGINT stop producers and drain the queue, bounded by
`shutdown_timeout`. Execd uses nonblocking pipe I/O on Unix so a full stdout pipe
does not prevent process shutdown.

Existing configs without `udp_max_bytes` retain the maximum UDP payload of 65507
bytes. The new template uses 1400 bytes to limit fragmentation; choose TCP for
large multi-output controls. UDP batching cannot make UDP delivery reliable.

For Docker builds behind a private CA, the Dockerfile accepts an optional
BuildKit secret without embedding certificates in the image or disabling TLS:
`docker build --secret id=build_ca,src=/path/to/ca-bundle.pem -t loxtotelegraf:local .`.

## Validation

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo bench --locked --bench pipeline
```

Tests include actual encrypted WebSocket authentication against a fake Miniserver,
secured polling, reconnection, UDP/TCP/MQTT delivery, health, malformed containers,
precision regressions, filtering and deduplication. See [migration notes](docs/migration.md)
for the source audit and [performance notes](docs/performance.md) for measurements.
The tests do not connect to production hardware or databases.

The [November optimization report](docs/optimization-2026-11-04.md) records paired
CPU/allocation measurements, retained-heap measurements, the real Miniserver
fixture results and the frame-batching experiment. Reproduce additional probes:

```sh
cargo bench --locked --bench hotpaths
cargo bench --locked --bench mqtt # local loopback broker
cargo bench --locked --bench frame_queue # experiment, not the production path
cargo run --locked --release --example allocation_probe -- /path/to/export.xml
```

The [second allocation pass](docs/optimization-followup-2026-11-04.md) adds compact
shared routes, history-free processing when deduplication is disabled, fewer
JSON allocations and streaming cache writes. It includes five paired runs and a
full WebSocket-to-TCP loopback benchmark:

```sh
cargo bench --locked --bench bridge
LOX_XML_BENCH=/path/to/export.xml cargo bench --locked --bench cache
```

