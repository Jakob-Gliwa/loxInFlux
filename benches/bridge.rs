//! Whole bridge over loopback: fake Miniserver -> real WebSocket reader ->
//! queue/engine -> TCP writer -> sink. Startup/authentication are outside timing.
//! Includes fake-server and sink costs; no TLS or production hardware.
#[path = "../tests/support/loxwebsocket/mod.rs"]
mod common;
use common::{FakeConfig, FakeMiniserver, TEST_PASSWORD, TEST_USER};
use loxtotelegraf::{
    app,
    config::{Config, Protocol},
};
use loxwebsocket::LoxoneUuid;
use std::time::{Duration, Instant};
use tokio::{io::AsyncReadExt, net::TcpListener, sync::watch, time::timeout};
use tokio_util::sync::CancellationToken;

async fn received(rx: &mut watch::Receiver<u64>, target: u64) {
    timeout(Duration::from_secs(20), async {
        while *rx.borrow_and_update() < target {
            rx.changed().await.unwrap();
        }
    })
    .await
    .expect("bridge must deliver every point");
    assert_eq!(*rx.borrow(), target);
}
async fn run(dedup: bool) {
    let ids: Vec<_> = (0..256u64)
        .map(|n| {
            let mut bytes = [0; 16];
            bytes[..8].copy_from_slice(&n.to_le_bytes());
            LoxoneUuid::from_bytes(bytes)
        })
        .collect();
    let mut xml = String::from("<Root>");
    for id in &ids {
        xml.push_str(&format!(
            r#"<C Type="Sensor" U="{id}" Title="Temperature"><IoData Visu="true"/></C>"#
        ));
    }
    xml.push_str("</Root>");
    let body = lz4_flex::block::compress(xml.as_bytes());
    let mut archive = Vec::new();
    for n in [
        0xaabbcceeu32,
        body.len() as u32,
        xml.len() as u32,
        crc32fast::hash(xml.as_bytes()),
    ] {
        archive.extend(n.to_le_bytes());
    }
    archive.extend(body);
    let mut fake_cfg = FakeConfig::default();
    fake_cfg.files.insert(
        "jdev/sps/LoxAPPversion3".into(),
        br#"{"LL":{"Code":"200","value":"bench"}}"#.to_vec(),
    );
    fake_cfg.files.insert(
        "dev/fslist/prog/".into(),
        b"sps_0272_20261030120000.LoxCC".to_vec(),
    );
    fake_cfg.files.insert(
        "dev/fsget/prog/sps_0272_20261030120000.LoxCC".into(),
        archive,
    );
    fake_cfg.files.insert(
        "data/LoxAPP3.json".into(),
        br#"{"lastModified":"bench","controls":{}}"#.to_vec(),
    );
    let fake = FakeMiniserver::start(fake_cfg).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let output_port = listener.local_addr().unwrap().port();
    let (count_tx, mut count_rx) = watch::channel(0u64);
    let sink = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0; 65536];
        loop {
            let n = socket.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            let points = buf[..n].iter().filter(|&&b| b == b'\n').count() as u64;
            count_tx.send_modify(|n| *n += points);
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.miniserver.host = "127.0.0.1".into();
    cfg.miniserver.port = fake.port();
    cfg.miniserver.user = TEST_USER.into();
    cfg.miniserver.password = TEST_PASSWORD.into();
    cfg.paths.data_dir = dir.path().to_string_lossy().into_owned();
    cfg.general.grabber = false;
    cfg.general.config_refresh_interval = 0;
    cfg.monitoring.enabled = false;
    cfg.processing.only_on_change = dedup;
    cfg.processing.heartbeat_interval = 0;
    cfg.telegraf.protocol = Protocol::Tcp;
    cfg.telegraf.host = "127.0.0.1".into();
    cfg.telegraf.port = output_port;
    let stop = CancellationToken::new();
    let shutdown = stop.clone();
    let task =
        tokio::spawn(async move { app::run(cfg, shutdown).await.expect("bridge task failed") });
    fake.state
        .wait_until(20, "enabled binary status", |s| {
            s.iter()
                .filter_map(common::Entry::as_command)
                .any(|c| c.cmd == "jdev/sps/enablebinstatusupdate")
        })
        .await;
    let session = fake.state.session(0).await;
    session.flushed().await;
    session.send_message(
        2,
        &common::value_payload(&ids.iter().map(|id| (*id, 99.0)).collect::<Vec<_>>()),
    );
    let mut total = 256;
    received(&mut count_rx, total).await;
    for size in [1, 16, 256] {
        let payloads: Vec<_> = [11.25, 11.5]
            .iter()
            .map(|v| {
                common::value_payload(&ids[..size].iter().map(|id| (*id, *v)).collect::<Vec<_>>())
            })
            .collect();
        let n = if cfg!(debug_assertions) { 4 } else { 200 };
        let mut times = Vec::new();
        for sample in 0..8 {
            let started = Instant::now();
            for i in 0..n {
                session.send_message(2, &payloads[i % 2]);
            }
            total += (n * size) as u64;
            received(&mut count_rx, total).await;
            if sample > 0 {
                times.push(started.elapsed().as_secs_f64() * 1e9 / (n * size) as f64);
            }
        }
        times.sort_by(f64::total_cmp);
        println!(
            "bridge records={size} dedup={dedup}: median_ns={:.3} min_ns={:.3} max_ns={:.3}",
            times[3], times[0], times[6]
        );
    }
    stop.cancel();
    timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap();
    sink.await.unwrap();
}
fn main() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            run(false).await;
            run(true).await;
        });
}
