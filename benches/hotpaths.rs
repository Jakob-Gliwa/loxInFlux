//! Compare parser CPU time and allocations on identical input before/after edits.
//! Timing is measured WITHOUT allocation instrumentation; allocation counts come
//! from one separate warmed call. No network and no private XML is checked in.
use bytes::Bytes;
use loxinflux::{
    config::Config,
    controls,
    metrics::Metrics,
    pipeline::{Batch, Engine, Input},
};
use std::sync::Arc;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    hint::black_box,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::Instant,
};
struct Meter;
static TRACK: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
fn record(size: usize) {
    if TRACK.load(Ordering::Relaxed) {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(size, Ordering::Relaxed);
    }
}
// SAFETY: every operation delegates unchanged to the system allocator. Only
// counters are added; no pointer, layout, lifetime or allocation is altered.
unsafe impl GlobalAlloc for Meter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record(size);
        unsafe { System.realloc(ptr, layout, size) }
    }
}
#[global_allocator]
static ALLOCATOR: Meter = Meter;
fn measure<T>(label: &str, iterations: usize, mut operation: impl FnMut() -> T) {
    for _ in 0..3 {
        drop(black_box(operation()));
    }
    ALLOCS.store(0, Ordering::Relaxed);
    BYTES.store(0, Ordering::Relaxed);
    TRACK.store(true, Ordering::Relaxed);
    drop(black_box(operation()));
    TRACK.store(false, Ordering::Relaxed);
    let allocs = ALLOCS.load(Ordering::Relaxed);
    let bytes = BYTES.load(Ordering::Relaxed);
    let mut samples = Vec::new();
    for _ in 0..7 {
        let start = Instant::now();
        for _ in 0..iterations {
            drop(black_box(operation()));
        }
        samples.push(start.elapsed().as_secs_f64() * 1e6 / iterations as f64);
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "{label}: median_us={:.3} min_us={:.3} max_us={:.3} allocations={allocs} allocated_bytes={bytes}",
        samples[3], samples[0], samples[6]
    );
}
fn main() {
    let id = loxwebsocket::LoxoneUuid::parse("00000000-0000-0001-ffff000000000001").unwrap();
    let xml = format!(
        r#"<Root><C Type="Sensor" U="{id}" Title="Temperature"><IoData Visu="false"/></C></Root>"#
    );
    let n = if cfg!(debug_assertions) { 10 } else { 20_000 };
    let now = Instant::now();
    for dedup in [false, true] {
        let mut cfg = Config::default();
        cfg.processing.only_on_change = dedup;
        let routes = controls::parse(xml.as_bytes(), &cfg).unwrap();
        let mut engine = Engine::new(cfg, Arc::new(Metrics::default()), Arc::new(routes));
        let mut batch = Batch {
            data: String::with_capacity(32768),
            ..Default::default()
        };
        measure(&format!("number dedup={dedup}"), n, || {
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
            black_box(batch.data.len());
        });
        for text in ["ordinary text status", "21.23456789°C"] {
            measure(
                &format!(
                    "text {} dedup={dedup}",
                    if text.starts_with('2') {
                        "unit"
                    } else {
                        "plain"
                    }
                ),
                n,
                || {
                    batch.clear();
                    engine
                        .process(
                            Input::Text {
                                uuid: id,
                                value: Bytes::copy_from_slice(black_box(text.as_bytes())),
                                timestamp: 42,
                                received: now,
                            },
                            &mut batch,
                        )
                        .unwrap();
                    black_box(batch.data.len());
                },
            );
        }
        let payload = Bytes::from_static(
            br#"{"LL":{"Code":"200","value":21.2,"output0":{"name":"AQ","value":21.2}}}"#,
        );
        measure(&format!("poll 2 fields dedup={dedup}"), n, || {
            batch.clear();
            engine
                .process(
                    Input::Grabber {
                        uuid: id,
                        payload: payload.clone(),
                        timestamp: 42,
                        received: now,
                    },
                    &mut batch,
                )
                .unwrap();
            black_box(batch.data.len());
        });
    }
}
