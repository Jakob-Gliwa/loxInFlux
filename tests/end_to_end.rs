//! Real bridge + actual encrypted WebSocket client against the user's existing
//! fake Miniserver, extended with filesystem API responses. No hardware needed.
#[path = "support/loxwebsocket/mod.rs"]
mod common;
use common::{FakeConfig, FakeMiniserver, TEST_PASSWORD, TEST_USER, TEST_VISU_PASSWORD};
use loxtotelegraf::{
    app,
    config::{Config, Protocol},
};
use std::time::Duration;
use tokio::{
    net::{TcpListener, UdpSocket},
    time::timeout,
};
use tokio_util::sync::CancellationToken;

fn loxcc(xml: &[u8]) -> Vec<u8> {
    let body = lz4_flex::block::compress(xml);
    let mut out = Vec::new();
    for n in [
        0xaabbcceeu32,
        body.len() as u32,
        xml.len() as u32,
        crc32fast::hash(xml),
    ] {
        out.extend(n.to_le_bytes());
    }
    out.extend(body);
    out
}
async fn setup(secured: bool) -> (FakeMiniserver, Config, tempfile::TempDir, UdpSocket, u16) {
    setup_extra(secured, false).await
}
async fn setup_extra(
    secured: bool,
    extra: bool,
) -> (FakeMiniserver, Config, tempfile::TempDir, UdpSocket, u16) {
    let id = common::uuid(0xa1);
    let poll = common::uuid(0xa3);
    let mut xml = format!(
        r#"<Root><C Type="Sensor" U="{id}" Title="Temperature"><IoData Visu="true"/></C><C Type="Sensor" U="{poll}" Title="Polled"><IoData Visu="false" VisuPwd="{secured}"/></C></Root>"#
    );
    if extra {
        xml = xml.replace(
            "</Root>",
            &format!(
                r#"<C Type="Sensor" U="{}" Title="Second"><IoData Visu="false"/></C></Root>"#,
                common::uuid(0xa4)
            ),
        );
    }
    let mut fake_cfg = FakeConfig {
        tables_before_enable_ack: true,
        poll_value: Some("9.4°C".into()),
        ..Default::default()
    };
    fake_cfg.files.insert(
        "jdev/sps/LoxAPPversion3".into(),
        br#"{"LL":{"Code":"200","value":"2026-10-30 12:00:00"}}"#.to_vec(),
    );
    fake_cfg.files.insert(
        "dev/fslist/prog/".into(),
        b"sps_0272_20261030120000.LoxCC".to_vec(),
    );
    fake_cfg.files.insert(
        "dev/fsget/prog/sps_0272_20261030120000.LoxCC".into(),
        loxcc(xml.as_bytes()),
    );
    fake_cfg.files.insert(
        "data/LoxAPP3.json".into(),
        br#"{"lastModified":"2026-10-30 12:00:00","controls":{}}"#.to_vec(),
    );
    let fake = FakeMiniserver::start(fake_cfg).await;
    let output = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let reservation = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.miniserver.host = "127.0.0.1".into();
    config.miniserver.port = fake.port();
    config.miniserver.user = TEST_USER.into();
    config.miniserver.password = TEST_PASSWORD.into();
    config.miniserver.visu_password = Some(TEST_VISU_PASSWORD.into());
    config.miniserver.max_reconnect_attempts = 1;
    config.paths.data_dir = dir.path().to_string_lossy().into_owned();
    config.telegraf.host = "127.0.0.1".into();
    config.telegraf.port = output.local_addr().unwrap().port();
    config.telegraf.protocol = Protocol::Udp;
    config.general.grabber_interval = 1;
    config.general.config_refresh_interval = 1;
    config.monitoring.listen = format!("127.0.0.1:{port}");
    config.processing.only_on_change = true;
    (fake, config, dir, output, port)
}
async fn receive_until(socket: &UdpSocket, needle: &str) -> String {
    timeout(Duration::from_secs(20), async {
        let mut seen = String::new();
        let mut buf = [0; 65536];
        loop {
            let n = socket.recv(&mut buf).await.unwrap();
            seen.push_str(std::str::from_utf8(&buf[..n]).unwrap());
            if seen.contains(needle) {
                return seen;
            }
        }
    })
    .await
    .expect("expected output before timeout")
}

#[tokio::test]
async fn encrypted_websocket_polling_health_cache_and_shutdown_work_together() {
    let (fake, config, dir, output, port) = setup(false).await;
    let stop = CancellationToken::new();
    let task = tokio::spawn(app::run(config, stop.clone()));
    let first = receive_until(&output, "Default=9.4").await;
    assert!(
        first.contains("Default=21.5"),
        "initial table before enable acknowledgement was lost: {first}"
    );
    let session = fake.state.session(0).await;
    session.send_message(
        2,
        &common::value_payload(&[(common::uuid(0xa1), 21.23456789123456)]),
    );
    let actual = receive_until(&output, "Default=21.23456789123456").await;
    assert!(actual.contains("source=websocket"));
    let client = reqwest::Client::new();
    let health = client
        .get(format!("http://127.0.0.1:{port}/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(health.status(), 200);
    let metrics = client
        .get(format!("http://127.0.0.1:{port}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(metrics.contains("loxtotelegraf_values_dropped_total 0"));
    assert!(dir.path().join("configuration-cache.json").exists());
    stop.cancel();
    timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        fake.state
            .commands()
            .iter()
            .any(|c| c.cmd.starts_with("jdev/sys/killtoken/"))
    );
}
#[tokio::test]
async fn visu_password_secured_polling_awaits_the_actual_answer() {
    let (_fake, config, _dir, output, _) = setup(true).await;
    let stop = CancellationToken::new();
    let task = tokio::spawn(app::run(config, stop.clone()));
    let actual = receive_until(&output, "Default=9.4").await;
    assert!(actual.contains("source=grabber"));
    stop.cancel();
    timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn reconnect_resets_dedup_and_delivers_same_initial_value_again() {
    common::init_tracing();
    let (fake, config, _dir, output, port) = setup(false).await;
    let stop = CancellationToken::new();
    let task = tokio::spawn(app::run(config, stop.clone()));
    // The initial table intentionally precedes the authentication completion;
    // a successful poll proves that the first session is fully established.
    receive_until(&output, "Default=9.4").await;
    fake.state.session(0).await.abort();
    timeout(Duration::from_secs(3), async {
        let http = reqwest::Client::new();
        loop {
            if http
                .get(format!("http://127.0.0.1:{port}/health"))
                .send()
                .await
                .unwrap()
                .status()
                == 503
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("EOF must make readiness unhealthy");
    let second = receive_until(&output, "Default=21.5").await;
    assert!(second.contains("source=websocket"));
    assert!(fake.state.session_count() >= 2);
    stop.cancel();
    timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn restart_reuses_version_checked_atomic_configuration_cache() {
    let (fake, config, _dir, output, _) = setup(false).await;
    for _ in 0..2 {
        let stop = CancellationToken::new();
        let task = tokio::spawn(app::run(config.clone(), stop.clone()));
        receive_until(&output, "Default=9.4").await;
        stop.cancel();
        timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
    assert_eq!(
        fake.state
            .http_gets()
            .iter()
            .filter(|p| p.as_str() == "dev/fslist/prog/")
            .count(),
        1
    );
}

#[tokio::test]
async fn failed_poll_stops_its_heartbeat_while_websocket_values_remain_live() {
    let (fake, mut config, _dir, output, port) = setup(false).await;
    config.processing.heartbeat_interval = 1;
    let stop = CancellationToken::new();
    let task = tokio::spawn(app::run(config, stop.clone()));
    receive_until(&output, "Default=9.4").await;
    fake.state.set_poll_refusal(403);
    timeout(Duration::from_secs(5), async {
        let http = reqwest::Client::new();
        loop {
            let response: serde_json::Value = http
                .get(format!("http://127.0.0.1:{port}/health"))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if response["grabber_failed"] == true {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let mut buf = [0; 65536];
    while output.try_recv(&mut buf).is_ok() {}
    let mut ws_heartbeats = 0;
    let observed = timeout(Duration::from_millis(2200), async {
        loop {
            let n = output.recv(&mut buf).await.unwrap();
            let point = std::str::from_utf8(&buf[..n]).unwrap();
            assert!(
                !point.contains("source=grabber"),
                "failed poll was advertised as fresh: {point}"
            );
            if point.contains("source=websocket") {
                ws_heartbeats += 1;
            }
        }
    })
    .await;
    assert!(observed.is_err());
    assert!(ws_heartbeats > 0, "unaffected websocket heartbeat stopped");
    fake.state.set_poll_refusal(0);
    receive_until(&output, "Default=9.4").await;
    stop.cancel();
    timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn mismatched_poll_response_never_becomes_another_controls_measurement() {
    for secured in [false, true] {
        let (fake, config, _dir, output, port) = setup(secured).await;
        fake.state.set_poll_mismatch(true);
        let stop = CancellationToken::new();
        let task = tokio::spawn(app::run(config, stop.clone()));
        let observed = timeout(Duration::from_secs(2), async {
            let mut buf = [0; 65536];
            loop {
                let n = output.recv(&mut buf).await.unwrap();
                let point = std::str::from_utf8(&buf[..n]).unwrap();
                assert!(
                    !point.contains("source=grabber"),
                    "mismatched response was written: {point}"
                );
            }
        })
        .await;
        assert!(observed.is_err());
        let health = reqwest::get(format!("http://127.0.0.1:{port}/health"))
            .await
            .unwrap();
        assert_eq!(health.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
        let body: serde_json::Value = health.json().await.unwrap();
        assert_eq!(body["grabber_failed"], true);
        fake.state.set_poll_mismatch(false);
        receive_until(&output, "Default=9.4").await;
        stop.cancel();
        timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn cancelling_startup_still_releases_the_authenticated_session() {
    let (fake, config, _dir, _output, _) = setup(false).await;
    fake.state.pause_enable_reply();
    let stop = CancellationToken::new();
    let task = tokio::spawn(app::run(config, stop.clone()));
    fake.state
        .wait_until(5, "enable request before cancellation", |s| {
            s.iter()
                .filter_map(common::Entry::as_command)
                .any(|c| c.cmd == "jdev/sps/enablebinstatusupdate")
        })
        .await;
    stop.cancel();
    timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    fake.state.resume_enable_reply();
    fake.state
        .wait_until(5, "cancelled startup must close its session", |s| {
            s.iter()
                .any(|e| matches!(e, common::Entry::SessionClosed { .. }))
        })
        .await;
    assert_eq!(
        fake.state.killed_tokens().len(),
        1,
        "cancelled startup must release its token"
    );
}

async fn wait_health(
    port: u16,
    check: impl Fn(&serde_json::Value) -> bool,
) -> (reqwest::StatusCode, serde_json::Value) {
    timeout(Duration::from_secs(8), async {
        let client = reqwest::Client::new();
        loop {
            if let Ok(response) = client
                .get(format!("http://127.0.0.1:{port}/health"))
                .send()
                .await
            {
                let status = response.status();
                let body: serde_json::Value = response.json().await.unwrap();
                if check(&body) {
                    return (status, body);
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn real_malformed_status_is_exported_and_health_stays_ready() {
    let (fake, config, _dir, output, port) = setup(false).await;
    let raw =
        include_str!("fixtures/miniserver_real/malformed_unescaped_quotes_in_output_value.raw")
            .replace(
                "00000000-0000-0001-ffff000000000001",
                &common::uuid(0xa3).to_string(),
            );
    fake.state
        .set_poll_payload(common::uuid(0xa3).to_string(), Some(raw.into_bytes()));
    let stop = CancellationToken::new();
    let task = tokio::spawn(app::run(config, stop.clone()));
    let point = receive_until(&output, "wwcircmode").await;
    assert!(point.contains("__text__Status="));
    assert!(point.contains("Default=1924984032.0"));
    let (status, _) = wait_health(port, |b| b["status"] == "ok").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let metrics = reqwest::get(format!("http://127.0.0.1:{port}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(metrics.contains("grabber_json_repaired_total 1"));
    stop.cancel();
    timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn unreadable_one_of_two_controls_degrades_without_stale_heartbeat_and_recovers() {
    let (fake, mut config, _dir, output, port) = setup_extra(false, true).await;
    config.processing.heartbeat_interval = 1;
    let stop = CancellationToken::new();
    let task = tokio::spawn(app::run(config, stop.clone()));
    receive_until(&output, "Polled,").await;
    fake.state.set_poll_payload(
        common::uuid(0xa3).to_string(),
        Some(b"unrepairable".to_vec()),
    );
    let (status, body) = wait_health(port, |b| b["grabber_unreadable_controls"] == 1).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["degraded"], true);
    let mut buf = [0; 65536];
    while output.try_recv(&mut buf).is_ok() {}
    assert!(
        timeout(Duration::from_millis(1200), async {
            loop {
                let n = output.recv(&mut buf).await.unwrap();
                let point = std::str::from_utf8(&buf[..n]).unwrap();
                assert!(
                    !point.contains("Polled,"),
                    "stale failed-control heartbeat: {point}"
                );
            }
        })
        .await
        .is_err()
    );
    fake.state.set_poll_payload(
        common::uuid(0xa4).to_string(),
        Some(b"unrepairable".to_vec()),
    );
    let (status, _) = wait_health(port, |b| b["grabber_unreadable_controls"] == 2).await;
    assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE);
    fake.state
        .set_poll_payload(common::uuid(0xa3).to_string(), None);
    fake.state
        .set_poll_payload(common::uuid(0xa4).to_string(), None);
    receive_until(&output, "Polled,").await;
    let (status, _) = wait_health(port, |b| b["grabber_unreadable_controls"] == 0).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    stop.cancel();
    timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
