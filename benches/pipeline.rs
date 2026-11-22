//! CPU-only: routing, conversion, deduplication and serialization. No network.
use loxinflux::{
    config::Config,
    controls,
    metrics::Metrics,
    pipeline::{Batch, Engine, Input},
};
use loxwebsocket::LoxoneUuid;
use std::{hint::black_box, sync::Arc, time::Instant};
fn run(name: &str, round: bool, changes: bool, dedup: bool) {
    let mut config = Config::default();
    config.general.round_floats = round;
    config.processing.only_on_change = dedup;
    let ids: Vec<_> = (0..4096u64)
        .map(|i| {
            let mut b = [0; 16];
            b[..8].copy_from_slice(&i.to_le_bytes());
            LoxoneUuid::from_bytes(b)
        })
        .collect();
    let mut xml = String::from("<Root>");
    for id in &ids {
        xml.push_str(&format!(r#"<C Type="InfoOnlyAnalog" U="{id}" Title="Temperature living room" Desc="Sensor" Analog="true"><IoData Visu="true"/></C>"#));
    }
    xml.push_str("</Root>");
    let controls = Arc::new(controls::parse(xml.as_bytes(), &config).unwrap());
    let mut engine = Engine::new(config, Arc::new(Metrics::default()), controls.clone());
    let mut batch = Batch::default();
    let now = Instant::now();
    for id in &ids {
        engine
            .process(
                Input::Number {
                    uuid: *id,
                    value: 21.23456789,
                    timestamp: 42,
                    received: now,
                },
                &mut batch,
            )
            .unwrap();
    }
    batch.clear();
    let n = if cfg!(debug_assertions) {
        10_000
    } else {
        2_000_000
    };
    let start = Instant::now();
    let mut bytes = 0usize;
    for i in 0..n {
        let value = if changes {
            21.0 + (i % 1000) as f64 / 1000.0
        } else {
            21.23456789
        };
        engine
            .process(
                Input::Number {
                    uuid: ids[i % ids.len()],
                    value: black_box(value),
                    timestamp: 42,
                    received: now,
                },
                &mut batch,
            )
            .unwrap();
        if i % 512 == 511 {
            bytes += black_box(batch.data.len());
            batch.clear();
        }
    }
    bytes += black_box(batch.data.len());
    let elapsed = start.elapsed().as_secs_f64();
    println!(
        "{name}: {n} values in {elapsed:.3}s; {:.0} values/s; {:.1} ns/value; {:.1} MiB/s serialized",
        n as f64 / elapsed,
        elapsed * 1e9 / n as f64,
        bytes as f64 / elapsed / 1048576.0
    );
}
fn main() {
    run("f64 / all changes", false, true, false);
    run("round to 5 / all changes", true, true, false);
    run("f64 / unchanged suppression", false, false, true);
    run("f64 / changes with dedup", false, true, true);
    run("round to 5 / changes with dedup", true, true, true);
}
