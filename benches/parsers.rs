//! Compare parser CPU time and allocations on identical input before/after edits.
//! Timing is measured WITHOUT allocation instrumentation; allocation counts come
//! from one separate warmed call. No network and no private XML is checked in.
use loxinflux::{config::Config, controls, metrics::Metrics, pipeline::parse_grabber};
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
    let config = Config::parse(include_str!("../tests/fixtures/legacy_config.toml")).unwrap();
    let mut numeric = config.clone();
    numeric.general.round_floats = false;
    let metrics = Metrics::default();
    let simple = br#"{"LL":{"control":"jdev/sps/io/test/all","Code":"200","value":21.23456789123456,"output0":{"name":"AQ","nr":0,"value":21.23456789123456}}}"#;
    let mut multi = serde_json::json!({"Code":"200","value":"<v.i>","ignored":{"nested":[1,2,3]}});
    for i in 0..32 {
        multi[format!("output{i}")] = serde_json::json!({"name":format!("AQ{i}"),"nr":i,"value":i as f64+0.23456789123456,"unit":"kWh"});
    }
    let multi = serde_json::to_vec(&serde_json::json!({"LL":multi})).unwrap();
    let n = if cfg!(debug_assertions) { 10 } else { 10_000 };
    let mut rounded = numeric.general.clone();
    rounded.round_floats = true;
    measure("number / round to 5", n * 10, || {
        loxinflux::value::number(black_box(21.23456789123456), &rounded).unwrap()
    });
    measure("grabber / 2 fields", n, || {
        parse_grabber(black_box(simple), &numeric, &metrics).unwrap()
    });
    measure("grabber / 33 fields", n, || {
        parse_grabber(black_box(&multi), &numeric, &metrics).unwrap()
    });
    if let Some(path) = std::env::var_os("LOX_XML_BENCH") {
        let xml = std::fs::read(path).unwrap();
        let parsed = controls::parse(&xml, &config).unwrap();
        println!(
            "XML bytes={} websocket={} grabber={}",
            xml.len(),
            parsed.websocket.len(),
            parsed.grabber.len()
        );
        measure(
            "XML / real export",
            if cfg!(debug_assertions) { 1 } else { 5 },
            || controls::parse(black_box(&xml), &config).unwrap(),
        );
    }
}
