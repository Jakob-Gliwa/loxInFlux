# Changelog

## 0.5.0

Rust rewrite of loxInFlux. **Breaking:** the data written to InfluxDB changes.
Details in the [README](README.md#breaking-changes-in-050).

### Breaking changes

- Every number is written as a float (`42.0`, never `42i`). Fields that 0.3
  stored as integers or strings reject the new values with
  `field type conflict` in the same InfluxDB shard.
- Numbers with units (`523ppm`, `19.8°`, `-331 W`) are written as floats.
- Text goes into its own field `__text__<field>`; empty values and Loxone
  placeholders such as `<v.i>` are dropped and counted.
- The image is a single static binary `FROM scratch`: no Python, no shell, no
  `docker-entrypoint.sh`.
- Nothing is stored on disk: the Miniserver configuration stays in memory, the
  `/app/data` mount and `[paths]` are no longer used.
- `telegraf.round_floats` and `telegraf.round_decimals` log a deprecation
  warning.

### Migrating the history

Existing buckets written by 0.3 need their history moved to the new schema
before 0.5 writes into them.
[loxInFlux-migration-helper](https://github.com/Jakob-Gliwa/loxInFlux-migration-helper)
does this: it copies the bucket with the same conversions as 0.5, verifies it,
catches up and swaps the bucket names. Follow its
[switchover guide](https://github.com/Jakob-Gliwa/loxInFlux-migration-helper/blob/main/docs/switchover.md).
Alternatives (fresh bucket, in-place script) are in
[docs/migration.md](docs/migration.md#upgrading-from-03-to-05). If you are not
ready, pin `acidcliff/loxinflux:0.3`.

### New

- Health and metrics endpoint on port 9108 (`/health`, `/metrics`) with a
  Docker healthcheck.
- `--check-config` validates a configuration without starting.
- `[processing]` options for unit parsing, empty values, placeholders and
  separate text fields.
- The configuration version is checked over the encrypted WebSocket session
  instead of HTTP with Basic auth.
- Configurable rate limit for polling commands (`general.grabber_rate_limit`);
  firmware 17.3 blocks clients that send commands in a burst.
- Logs show the cause, the target and the recovery of failures.

## 0.3.0

Miniserver firmware 17 support, configuration download over HTTP instead of
FTP, test suite and release gates. See the
[release notes](https://github.com/Jakob-Gliwa/loxInFlux/releases/tag/v0.3.0).
