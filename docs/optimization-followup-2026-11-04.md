# Second allocation pass, 2026-11-04

This pass starts from the completed [first November optimization](optimization-2026-11-04.md),
including malformed JSON recovery and the configuration warning policy. Its
numbers are incremental improvements over that version, not over Python or the
original Rust port. Dependencies and Cargo.lock are unchanged.

## Retained changes

1. **Compact routing.** A route holds its prefix and one `Arc<FieldKeys>` shared
   by all occurrences of that field. The previous three field-name handles
   occupied 48 bytes per route despite sharing the underlying strings. Route
   shrinks from 64 to 24 bytes. The real export uses 488 distinct field sets.
2. **History only when needed.** Engine shares the immutable discovery snapshot.
   With `only_on_change=false`, it routes and serializes directly, without a
   second pair of routing maps or cached last values. With change detection
   enabled, the mutable observations still preserve deduplication, heartbeats
   and refresh semantics. `Engine::new` now accepts `Arc<Controls>` so callers
   can share the actual snapshot instead of cloning its maps.
3. **Smaller observations.** The unused WebSocket `seen` timestamp is removed.
   Grabber freshness is recorded once per complete successful control snapshot,
   including unchanged snapshots. Per-field emission times remain independent.
   Failed polls and session resets invalidate observations as before.
4. **Fewer JSON objects.** The parser retains only the selected response envelope
   instead of allocating a boxed nested document. The common string status
   `"200"` is borrowed. Integer 200 and floating-point 200.0 retain their previous
   distinct validation behavior. Last duplicate LL keys win, root fields cannot
   overwrite an LL envelope, and ignored data remains strictly validated.
5. **Remove repeated UTF-8 validation.** Three small unsafe conversions reuse
   bytes whose validity is already established: the text path validates the
   same immutable Bytes first; the MQTT payload originates from a String and
   remains immutable through publishing and retries. Network text still passes
   UTF-8 validation. There are no unchecked frame lifetimes, raw pointer writes,
   unchecked indexing or modified float conversions. Each unsafe operation has
   its local invariant documented. Shared slices and Unicode batch restoration
   have explicit regression coverage; Miri was not run.
6. **Stream cache writes.** Serde writes through a 64-KiB BufWriter on Tokio's
   blocking pool. This avoids constructing a second complete escaped cache
   document and avoids doing its serialization on the WebSocket runtime thread.
   Flush and close precede the atomic rename. The JSON cache format is unchanged.

The two BTreeMaps and owned returned field names in the grabber parser remain.
This pass reduces allocations; it does not claim an entirely zero-copy parser
or transport stack. Frame batching, dense indexed state arrays and split metadata
prefixes are not introduced. The previous frame experiment added allocations for
small frames and still needs an ownership API and equivalent queue byte/record
limits. The compact route change captures substantial memory savings without
requiring that larger redesign.

## Method

Both variants use rustc 1.99.1 on macOS ARM64, the same locked dependencies and
the existing release profile: optimization level 3, fat LTO, one codegen unit.
The baseline is rebuilt in a separate target directory; its 64-byte Route and
six-allocation parser result are verified before comparison. Source hashes and
the compiler version are in [provenance.json](benchmarks/2026-11-04-followup/provenance.json).

Five paired runs alternate before/after order. All binaries are built before
timing; no compilation or other test run overlaps the measurements. Parser,
hot-path, MQTT and bridge figures are medians of the five run medians, each of
which uses seven timed samples after warmup. The numeric pipeline uses five
two-million-value runs over 4,096 routes. Allocation counts come from separate
warmed operations, with counting disabled during timing. The heap probe runs
separately and measures requested live heap, not allocator bookkeeping or RSS.

The new bridge benchmark uses the real WebSocket reader, application queue,
engine and TCP writer with a fake Miniserver and TCP sink on loopback. Startup
and authentication finish before timing. Each sample waits for every expected
output point. It includes fake-server, scheduler, socket and sink costs and does
not use TLS. Both dedup modes receive changing numeric values. These are local
latency/throughput measurements, not production CPU utilization measurements.

## CPU and allocations

| Workload | Before | After | Interpretation |
| --- | ---: | ---: | --- |
| Numeric updates, no dedup | 46.3 ns/value | 42.3 ns/value | 8.6% less time |
| Numeric updates with decimal rounding, no dedup | 100.9 ns/value | 97.8 ns/value | 3.1% less time |
| Unchanged numeric updates suppressed | 9.9 ns/value | 9.3 ns/value | Small improvement |
| Changing numeric updates, dedup enabled | 45.4 ns/value | 45.4 ns/value | Same median |
| Changing rounded updates, dedup enabled | 100.2 ns/value | 100.5 ns/value | Within run variation |
| Two-field poll, parse + engine, no dedup | 451 ns | 354 ns | 21.5% less time |
| Two-field poll, parse + engine, dedup enabled | 389 ns | 343 ns | 11.8% less time |
| Two-field JSON parse alone | 369 ns | 337 ns | Allocations 6 → 4 |
| 33-field JSON parse alone | 6.351 µs | 6.279 µs | Allocations 45 → 43 |
| Plain text, dedup enabled | 42 ns | 39 ns | One allocation in both versions |
| XML discovery | 14.834 ms | 14.682 ms | Essentially unchanged |
| MQTT, 256-byte test text | 15.735 µs | 15.829 µs | Within run variation |
| MQTT, 16-KiB test text | 37.210 µs | 36.821 µs | Essentially unchanged |
| MQTT, 256-KiB test text | 160.185 µs | 151.347 µs | 5.5% less time |

Numeric processing continues to allocate nothing after warmup. XML discovery
does allocate 488 extra shared field-set objects: 177,750 → 178,238 allocations
(+0.27%). Its cumulative requested allocation bytes decrease from 21,441,264 to
20,112,880, and its retained memory falls substantially. No decimal rounding,
field-type, filtering or precision policy was changed to obtain these results.

| Full bridge, records per frame | Before, no dedup | After, no dedup | Before, dedup | After, dedup |
| --- | ---: | ---: | ---: | ---: |
| 1 | 19,636 ns/value | 19,182 ns/value | 19,675 ns/value | 19,266 ns/value |
| 16 | 1,418 ns/value | 1,410 ns/value | 1,422 ns/value | 1,397 ns/value |
| 256 | 257 ns/value | 249 ns/value | 259 ns/value | 254 ns/value |

These full-path changes are small, around 0.6–3.1%, and the samples have overlapping
ranges. The result supports retaining the changes with no evident full-path
regression; it is not a claim of a statistically established production speedup.
The larger local parser improvements should not be projected onto total CPU usage.

## Retained heap and cache serialization

Measured with the same 2,605,334-byte XML export, containing 7,981 WebSocket routes
and 838 grabber controls. Both probes include the Arc discovery snapshot and use
the same unchanged legacy TOML fixture.

| Requested live heap | Before, bytes | After, bytes |
| --- | ---: | ---: |
| Discovery | 3,747,162 | 3,123,034 |
| Additional Engine, dedup enabled | 2,421,675 | 1,520,555 |
| Additional Engine, dedup disabled | 2,421,675 | 15,625 |
| Discovery + Engine, dedup enabled | 6,168,837 | 4,643,589 |
| Discovery + Engine, dedup disabled | 6,168,837 | 3,138,659 |
| Peak additional heap during discovery | 5,335,754 | 4,367,690 |

Combined retained memory falls **24.7% with dedup** and **49.1% without dedup**.
The probe initializes the engine before live sensor text/state arrives; it
excludes the loaded XML buffer, transport buffers, queue contents and process RSS.

The separate cache benchmark compares the old serialize-to-Vec/write strategy
against streaming, on the same real XML and synthetic minimal app metadata. It
verifies the decoded result after every strategy. Three runs give median run
medians of 39.519 ms for the old strategy and 32.964 ms for the 64-KiB buffer;
filesystem variation is large, so no precise CPU speedup is claimed. A 256-KiB
buffer did not give a consistent advantage over 64 KiB and was not selected.

Per write, the old strategy requests 8,388,480 cumulative allocation bytes across
16 allocations/reallocations; streaming requests 65,536 bytes in one allocation.
Those cumulative allocation bytes are not peak RSS or the total application heap.

## Validation

- 86 tests pass: 70 library tests, one CLI warning test, nine complete bridge
  integration tests and six HTTP loader regressions. Bench harnesses also execute
  in the all-targets check, including the full bridge with both dedup modes.
- New checks cover unchanged and changed route refreshes in both modes,
  freshness of unchanged poll snapshots, status/envelope edge cases, Unicode
  shared input slices and exact MQTT batch restoration.
- Existing 10,000-pattern f64 differential tests and the twelve real Miniserver
  responses pass. All twelve raw files and the manifest remain unchanged.
- Python/Rust comparisons on all three real XML exports report zero missing,
  extra or differing routes and metadata.
- Formatting, Clippy with warnings denied, native release build and the legacy
  configuration CLI check pass.
- The local Docker socket is absent, so this revision was not built or tested as
  a container. No live Miniserver or database was queried.

Raw results: [paired runs](benchmarks/2026-11-04-followup/before-after.txt),
[heap before](benchmarks/2026-11-04-followup/heap-before.txt),
[heap after](benchmarks/2026-11-04-followup/heap-after.txt),
[cache strategies](benchmarks/2026-11-04-followup/cache.txt),
[XML parity](benchmarks/2026-11-04-followup/xml-parity.txt).

The existing benchmark commands remain valid. Additional checks:

```sh
cargo bench --locked --bench bridge
LOX_XML_BENCH=/path/to/export.xml cargo bench --locked --bench cache
```

The bridge benchmark requires local loopback sockets. The cache benchmark writes
only inside its own temporary directory and does not modify the supplied export.
