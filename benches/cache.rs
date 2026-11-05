//! Compare cache serialization strategies on the same XML export and disk target.
//! Timing is measured WITHOUT allocation instrumentation; allocation counts come
//! from one separate warmed call. No network and no private XML is checked in.
use std::io::{BufWriter, Write};
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

#[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
struct Cache {
    origin: String,
    username: String,
    xml: String,
    app: serde_json::Value,
}
fn main() {
    let Some(path) = std::env::var_os("LOX_XML_BENCH") else {
        return;
    };
    let cache = Cache {
        origin: "http://localhost".into(),
        username: "fixture".into(),
        xml: std::fs::read_to_string(path).unwrap(),
        app: serde_json::json!({"lastModified":"bench","controls":{}}),
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.json");
    let n = if cfg!(debug_assertions) { 1 } else { 10 };
    for capacity in [0, 65536, 262144] {
        measure(&format!("cache buffer={capacity}"), n, || {
            if capacity == 0 {
                std::fs::write(&path, serde_json::to_vec(&cache).unwrap()).unwrap();
            } else {
                let mut out =
                    BufWriter::with_capacity(capacity, std::fs::File::create(&path).unwrap());
                serde_json::to_writer(&mut out, &cache).unwrap();
                out.flush().unwrap();
            }
        });
        let actual: Cache = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(actual, cache);
    }
}
