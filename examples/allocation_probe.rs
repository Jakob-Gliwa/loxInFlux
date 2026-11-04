//! Offline heap probe; pass a real XML export as the first argument.
//! Tracks requested live heap bytes, not RSS or allocator overhead. Never use
//! these instrumented runs for CPU timings. Prints counts, never control names.
use bytes::Bytes;
use loxinflux::{
    config::Config,
    controls,
    metrics::Metrics,
    pipeline::{Batch, Engine, Input},
};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
    },
    time::Instant,
};
struct Meter;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static CALLS: AtomicUsize = AtomicUsize::new(0);
static ENABLED: AtomicBool = AtomicBool::new(false);
fn added(n: usize) {
    let live = LIVE.fetch_add(n, Relaxed) + n;
    PEAK.fetch_max(live, Relaxed);
    if ENABLED.load(Relaxed) {
        CALLS.fetch_add(1, Relaxed);
    }
}
// SAFETY: pointer ownership and layouts are passed unchanged to System;
// instrumentation only adjusts independent allocation counters.
unsafe impl GlobalAlloc for Meter {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            added(l.size())
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            added(l.size())
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, n) };
        if !q.is_null() {
            LIVE.fetch_sub(l.size(), Relaxed);
            added(n)
        }
        q
    }
}
#[global_allocator]
static A: Meter = Meter;
fn allocations(label: &str, mut f: impl FnMut()) {
    f();
    CALLS.store(0, Relaxed);
    ENABLED.store(true, Relaxed);
    for _ in 0..1000 {
        f()
    }
    ENABLED.store(false, Relaxed);
    println!(
        "{label}: allocations/event={:.3}",
        CALLS.load(Relaxed) as f64 / 1000.0
    );
}
fn main() {
    println!(
        "Input bytes={} Route bytes={} PollControl bytes={} Value bytes={}",
        size_of::<Input>(),
        size_of::<controls::Route>(),
        size_of::<controls::PollControl>(),
        size_of::<loxinflux::value::Value>()
    );
    let config =
        Config::parse(&std::fs::read_to_string("tests/fixtures/legacy_config.toml").unwrap())
            .unwrap();
    let xml = std::fs::read(std::env::args().nth(1).unwrap()).unwrap();
    let start = LIVE.load(Relaxed);
    PEAK.store(start, Relaxed);
    let controls = Arc::new(controls::parse(&xml, &config).unwrap());
    let retained = LIVE.load(Relaxed) - start;
    let peak = PEAK.load(Relaxed) - start;
    println!(
        "routes={} grabber={} control_retained_bytes={retained} parse_peak_extra_bytes={peak}",
        controls.websocket.len(),
        controls.grabber.len()
    );
    let metrics = Arc::new(Metrics::default());
    for dedup in [false, true] {
        let base = LIVE.load(Relaxed);
        let mut config = config.clone();
        config.processing.only_on_change = dedup;
        let engine = Engine::new(config, metrics.clone(), controls.clone());
        println!(
            "engine_extra_bytes dedup={dedup}: {}",
            LIVE.load(Relaxed) - base
        );
        drop(engine);
    }
    let mut ptrs = HashSet::new();
    let mut unique = HashSet::new();
    let mut stored = 0;
    let mut distinct = 0;
    for r in controls.websocket.values() {
        for s in [&r.field, &r.numeric_key, &r.text_key] {
            if ptrs.insert(s.as_ptr()) {
                stored += s.len() + 16;
            }
            if unique.insert(s.as_ref()) {
                distinct += s.len() + 16;
            }
        }
    }
    println!(
        "field_key_allocations={} unique_field_key_strings={} estimated_arc_bytes={} estimated_interned_arc_bytes={}",
        ptrs.len(),
        unique.len(),
        stored,
        distinct
    );
    let id = *controls.websocket.keys().next().unwrap();
    let poll = *controls.grabber.keys().next().unwrap();
    let now = Instant::now();
    allocations("text callback Bytes copy alone", || {
        std::hint::black_box(Bytes::copy_from_slice(&[b'x'; 100]));
    });
    allocations("text normalization alone", || {
        std::hint::black_box(loxinflux::value::text(
            "plain text",
            &config.general,
            &config.processing,
        ));
    });
    for only_on_change in [false, true] {
        let mut config = config.clone();
        config.general.round_floats = false;
        config.processing.only_on_change = only_on_change;
        let mut engine = Engine::new(config, metrics.clone(), controls.clone());
        let mut batch = Batch {
            data: String::with_capacity(16384),
            ..Default::default()
        };
        allocations(&format!("numeric, dedup={only_on_change}"), || {
            batch.clear();
            engine
                .process(
                    Input::Number {
                        uuid: id,
                        value: 21.23456789,
                        timestamp: 42,
                        received: now,
                    },
                    &mut batch,
                )
                .unwrap();
        });
        allocations(
            &format!("100-byte text including callback copy, dedup={only_on_change}"),
            || {
                batch.clear();
                engine
                    .process(
                        Input::Text {
                            uuid: id,
                            value: Bytes::copy_from_slice(&[b'x'; 100]),
                            timestamp: 42,
                            received: now,
                        },
                        &mut batch,
                    )
                    .unwrap();
            },
        );
        let payload = Bytes::from_static(
            br#"{"LL":{"Code":"200","value":21.2,"output0":{"name":"AQ","value":21.2}}}"#,
        );
        allocations(
            &format!(
                "parse+process 2-field poll excluding upstream payload copy, dedup={only_on_change}"
            ),
            || {
                batch.clear();
                engine
                    .process(
                        Input::Grabber {
                            uuid: poll,
                            payload: payload.clone(),
                            timestamp: 42,
                            received: now,
                        },
                        &mut batch,
                    )
                    .unwrap();
            },
        );
    }
}
