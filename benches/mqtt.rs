//! Compare MQTT write time and allocations on identical input before/after edits.
//! Timing is measured WITHOUT allocation instrumentation; allocation counts come
//! from one separate warmed call. Uses only a local loopback broker.
use loxinflux::{
    config::{Protocol, Telegraf},
    metrics::Metrics,
    pipeline::Batch,
    writer::Writer,
};
use std::sync::Arc;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    hint::black_box,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::Instant,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
fn main() {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        for size in [256, 16384, 262144] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let broker = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                assert_eq!(socket.read_u8().await.unwrap(), 0x10);
                let n=socket.read_u8().await.unwrap() as usize;
                socket.read_exact(&mut vec![0;n]).await.unwrap();
                socket.write_all(&[0x20,2,0,0]).await.unwrap();
                let mut buf=[0;65536];
                while socket.read(&mut buf).await.unwrap_or(0)>0 {}
            });
            let mut cfg=Telegraf { protocol:Protocol::Mqtt,max_retries:1,..Default::default() };
            cfg.mqtt.host="127.0.0.1".into();cfg.mqtt.port=port;
            let mut writer=Writer::new(cfg,Arc::new(Metrics::default()));
            writer.initialize().await.unwrap();
            let mut batch=Batch {data:format!("m f=\"{}\" 42\n","x".repeat(size)),points:1,fields:1};
            batch.data.reserve(1024);
            for _ in 0..5 { writer.write(&mut batch).await.unwrap(); }
            ALLOCS.store(0,Ordering::Relaxed);BYTES.store(0,Ordering::Relaxed);TRACK.store(true,Ordering::Relaxed);
            writer.write(&mut batch).await.unwrap();
            TRACK.store(false,Ordering::Relaxed);
            let a=ALLOCS.load(Ordering::Relaxed);let b=BYTES.load(Ordering::Relaxed);
            let n=if cfg!(debug_assertions) { 2 } else if size>16384 { 200 } else {1000};
            let mut samples=Vec::new();
            for _ in 0..7 {
                let start=Instant::now();
                for _ in 0..n {writer.write(&mut batch).await.unwrap();black_box(batch.data.len());}
                samples.push(start.elapsed().as_secs_f64()*1e6/n as f64);
            }
            samples.sort_by(f64::total_cmp);
            println!("MQTT {size}: median_us={:.3} min_us={:.3} max_us={:.3} allocations={a} allocated_bytes={b}",samples[3],samples[0],samples[6]);
            writer.close().await.unwrap();broker.await.unwrap();
        }
    });
}
