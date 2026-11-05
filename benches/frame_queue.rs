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
    // Experiment using today's borrowed raw-payload API. A production change
    // would also need weighted queue limits and lifecycle/correlation tests.
    let id = loxwebsocket::LoxoneUuid::from_bytes([1; 16]);
    let xml = format!(
        r#"<Root><C Type="Sensor" U="{id}" Title="Temperature"><IoData Visu="true"/></C></Root>"#
    );
    let cfg = Config::default();
    let controls = Arc::new(controls::parse(xml.as_bytes(), &cfg).unwrap());
    let now = Instant::now();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = rt.enter();
    for count in [1, 16, 256] {
        let mut payload = Vec::new();
        for _ in 0..count {
            payload.extend([1; 16]);
            payload.extend(21.2f64.to_le_bytes());
        }
        for frames in [false, true] {
            let mut engine =
                Engine::new(cfg.clone(), Arc::new(Metrics::default()), controls.clone());
            let mut batch = Batch {
                data: String::with_capacity(131072),
                ..Default::default()
            };
            let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
            measure(
                &format!("queue count={count} frames={frames}"),
                if cfg!(debug_assertions) { 10 } else { 10000 },
                || {
                    batch.clear();
                    if frames {
                        tx.try_send(Input::Grabber {
                            uuid: id,
                            payload: Bytes::copy_from_slice(&payload),
                            timestamp: 42,
                            received: now,
                        })
                        .unwrap();
                    } else {
                        loxwebsocket::proto::value::walk_values(&payload, |uuid, value| {
                            tx.try_send(Input::Number {
                                uuid: *uuid,
                                value,
                                timestamp: 42,
                                received: now,
                            })
                            .unwrap();
                        });
                    }
                    while let Ok(event) = rx.try_recv() {
                        if let Input::Grabber { payload, .. } = event {
                            loxwebsocket::proto::value::walk_values(&payload, |uuid, value| {
                                engine
                                    .process(
                                        Input::Number {
                                            uuid: *uuid,
                                            value,
                                            timestamp: 42,
                                            received: now,
                                        },
                                        &mut batch,
                                    )
                                    .unwrap();
                            });
                        } else {
                            engine.process(event, &mut batch).unwrap();
                        }
                    }
                    black_box(batch.data.len());
                },
            );
        }
    }
}
