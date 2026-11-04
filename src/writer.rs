use crate::{
    config::{Protocol, Telegraf},
    metrics::{self, Metrics},
    pipeline::Batch,
};
use anyhow::{Context, Result, bail, ensure};
use bytes::Bytes;
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, Outgoing, QoS};
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpStream, UdpSocket},
    sync::watch,
    task::JoinHandle,
    time::{sleep, timeout},
};

enum Transport {
    Udp(UdpSocket),
    Tcp(TcpStream),
    Execd(crate::stdout::Output),
    Mqtt(MqttTransport),
}
struct MqttTransport {
    client: AsyncClient,
    task: JoinHandle<Result<()>>,
    topic: String,
    flushed: watch::Receiver<u64>,
}
impl Drop for MqttTransport {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub struct Writer {
    config: Telegraf,
    metrics: Arc<Metrics>,
    transport: Option<Transport>,
}
impl Writer {
    pub fn new(config: Telegraf, metrics: Arc<Metrics>) -> Self {
        Self {
            config,
            metrics,
            transport: None,
        }
    }
    async fn connect(&mut self) -> Result<()> {
        self.transport = Some(match self.config.protocol {
            Protocol::Udp => {
                let remote = tokio::net::lookup_host((self.config.host.as_str(), self.config.port))
                    .await?
                    .next()
                    .context("No Telegraf address")?;
                let socket = UdpSocket::bind(if remote.is_ipv4() {
                    "0.0.0.0:0"
                } else {
                    "[::]:0"
                })
                .await?;
                socket.connect(remote).await?;
                Transport::Udp(socket)
            }
            Protocol::Tcp => {
                let socket =
                    TcpStream::connect((self.config.host.as_str(), self.config.port)).await?;
                socket.set_nodelay(true)?;
                Transport::Tcp(socket)
            }
            Protocol::Execd => Transport::Execd(crate::stdout::Output::open()?),
            Protocol::Mqtt => {
                let cfg = &self.config.mqtt;
                let mut options = MqttOptions::new(&cfg.client_id, &cfg.host, cfg.port);
                options.set_keep_alive(Duration::from_secs(60));
                options.set_max_packet_size(64 * 1024, 16 * 1024 * 1024);
                if let Some(user) = cfg.username.as_ref().filter(|user| !user.is_empty()) {
                    options.set_credentials(user, cfg.password.as_deref().unwrap_or(""));
                }
                let (client, mut eventloop) = AsyncClient::new(options, 32);
                // Establish the initial session synchronously; a queue accepting
                // publishes is not proof of a healthy broker connection.
                loop {
                    if matches!(
                        eventloop.poll().await?,
                        Event::Incoming(Incoming::ConnAck(_))
                    ) {
                        break;
                    }
                }
                let metrics = self.metrics.clone();
                let max = self.config.max_retries;
                let (flushed_tx, flushed) = watch::channel(0u64);
                let task = tokio::spawn(async move {
                    let mut failures = 0u32;
                    loop {
                        match eventloop.poll().await {
                            Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                                failures = 0;
                                metrics.output_ready.store(true, Ordering::Relaxed);
                            }
                            Ok(Event::Outgoing(Outgoing::Publish(_))) => {
                                // EventLoop flushes the network before yielding
                                // this event. Merely enqueuing is not a write.
                                flushed_tx.send_modify(|n| *n = n.wrapping_add(1));
                            }
                            Ok(Event::Outgoing(Outgoing::Disconnect)) => return Ok(()),
                            Ok(_) => {}
                            Err(error) => {
                                metrics.output_ready.store(false, Ordering::Relaxed);
                                metrics::inc(&metrics.write_errors);
                                failures = failures.saturating_add(1);
                                if max > 0 && failures >= max as u32 {
                                    return Err(error.into());
                                }
                                tracing::warn!(
                                    attempt = failures,
                                    "MQTT connection failed; retrying"
                                );
                                sleep(Duration::from_secs(1u64 << failures.min(5))).await;
                            }
                        }
                    }
                });
                Transport::Mqtt(MqttTransport {
                    client,
                    task,
                    topic: cfg.topic.clone(),
                    flushed,
                })
            }
        });
        self.metrics.output_ready.store(true, Ordering::Relaxed);
        Ok(())
    }
    pub async fn initialize(&mut self) -> Result<()> {
        let mut failures = 0;
        loop {
            let result = timeout(
                Duration::from_secs(self.config.write_timeout),
                self.connect(),
            )
            .await;
            match result {
                Ok(Ok(())) => return Ok(()),
                other => {
                    self.metrics.output_ready.store(false, Ordering::Relaxed);
                    metrics::inc(&self.metrics.write_errors);
                    failures += 1;
                    if self.config.max_retries > 0 && failures >= self.config.max_retries {
                        bail!("Output connection failed after {failures} attempts: {other:?}");
                    }
                    tracing::warn!(attempt = failures, "Output connection failed; retrying");
                    sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }
    pub async fn write(&mut self, batch: &mut Batch) -> Result<()> {
        if self.config.protocol == Protocol::Mqtt && batch.points > 0 {
            // Transfer the existing allocation to the MQTT queue. Retain one
            // handle across retries, then reclaim the buffer when it is unique.
            let payload = Bytes::from(std::mem::take(&mut batch.data));
            // SAFETY: payload was just moved from a String. Bytes and all
            // published clones are immutable throughout this operation.
            let data = unsafe { std::str::from_utf8_unchecked(&payload) };
            let result = self
                .write_data(data, batch.points, batch.fields, Some(&payload))
                .await;
            // SAFETY: publishing never mutates our Bytes handle. Conversion
            // to Vec preserves the validated String's exact byte sequence.
            batch.data = unsafe { String::from_utf8_unchecked(payload.into()) };
            result
        } else {
            self.write_data(&batch.data, batch.points, batch.fields, None)
                .await
        }
    }
    async fn write_data(
        &mut self,
        data: &str,
        points: u64,
        fields: u64,
        mqtt_payload: Option<&Bytes>,
    ) -> Result<()> {
        if points == 0 {
            return Ok(());
        }
        if self.config.protocol == Protocol::Udp {
            ensure!(
                data.split_inclusive('\n')
                    .all(|s| s.len() <= self.config.udp_max_bytes),
                "A point exceeds telegraf.udp_max_bytes; increase the limit or use TCP/MQTT/execd"
            );
        }
        let mut failures = 0;
        loop {
            if self.transport.is_none() {
                self.initialize().await?;
            }
            let result = timeout(
                Duration::from_secs(self.config.write_timeout),
                send(
                    self.transport.as_mut().unwrap(),
                    data,
                    self.config.udp_max_bytes,
                    mqtt_payload,
                ),
            )
            .await;
            match result {
                Ok(Ok(())) => {
                    metrics::add(&self.metrics.points_written, points);
                    metrics::add(&self.metrics.values_written, fields);
                    metrics::add(&self.metrics.bytes_written, data.len() as u64);
                    self.metrics
                        .last_written_ns
                        .store(metrics::timestamp_ns(), Ordering::Relaxed);
                    return Ok(());
                }
                other => {
                    self.metrics.output_ready.store(false, Ordering::Relaxed);
                    metrics::inc(&self.metrics.write_errors);
                    // Keep this exact batch for retry, including timestamps. A
                    // TCP disconnect may have delivered a prefix already.
                    self.transport = None;
                    failures += 1;
                    if self.config.max_retries > 0 && failures >= self.config.max_retries {
                        bail!("Output write failed after {failures} attempts: {other:?}");
                    }
                    tracing::warn!(
                        attempt = failures,
                        "Output write failed; retaining batch and retrying"
                    );
                    sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }
    pub async fn close(&mut self) -> Result<()> {
        // Even a failed shutdown consumes the transport and makes readiness
        // false. A second close must not reuse a stopped MQTT worker/socket.
        self.metrics.output_ready.store(false, Ordering::Relaxed);
        match self.transport.take().as_mut() {
            Some(Transport::Tcp(t)) => t.shutdown().await?,
            Some(Transport::Execd(t)) => t.flush().await?,
            Some(Transport::Mqtt(t)) => {
                t.client.disconnect().await?;
                (&mut t.task).await??;
            }
            _ => {}
        }
        Ok(())
    }
    pub async fn wait_failure(&mut self) -> Result<()> {
        match self.transport.as_mut() {
            Some(Transport::Mqtt(t)) => {
                let result = (&mut t.task).await;
                self.metrics.output_ready.store(false, Ordering::Relaxed);
                result.context("MQTT worker panicked")??;
                bail!("MQTT worker exited unexpectedly")
            }
            _ => std::future::pending().await,
        }
    }
}
async fn send(
    transport: &mut Transport,
    data: &str,
    udp_max: usize,
    mqtt_payload: Option<&Bytes>,
) -> Result<()> {
    match transport {
        Transport::Udp(socket) => {
            let mut start = 0;
            let mut end = 0;
            for line in data.split_inclusive('\n') {
                if end - start + line.len() > udp_max {
                    let n = socket.send(&data.as_bytes()[start..end]).await?;
                    ensure!(n == end - start, "Partial UDP datagram");
                    start = end;
                }
                end += line.len();
            }
            if end > start {
                let n = socket.send(&data.as_bytes()[start..end]).await?;
                ensure!(n == end - start, "Partial UDP datagram");
            }
        }
        Transport::Tcp(stream) => stream.write_all(data.as_bytes()).await?,
        Transport::Execd(stdout) => {
            stdout.write_all(data.as_bytes()).await?;
            stdout.flush().await?;
        }
        Transport::Mqtt(t) => {
            ensure!(!t.task.is_finished(), "MQTT worker stopped");
            ensure!(
                data.len() + t.topic.len() + 16 < 16 * 1024 * 1024,
                "MQTT batch exceeds 16 MiB"
            );
            let before = *t.flushed.borrow_and_update();
            t.client
                .publish_bytes(
                    &t.topic,
                    QoS::AtMostOnce,
                    false,
                    mqtt_payload.expect("MQTT payload ownership").clone(),
                )
                .await?;
            while *t.flushed.borrow_and_update() == before {
                t.flushed
                    .changed()
                    .await
                    .context("MQTT worker stopped before publishing")?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    async fn packet(socket: &mut TcpStream) -> (u8, Vec<u8>) {
        let kind = socket.read_u8().await.unwrap();
        let mut size = 0;
        let mut scale = 1;
        loop {
            let b = socket.read_u8().await.unwrap();
            size += (b & 127) as usize * scale;
            if b & 128 == 0 {
                break;
            }
            scale *= 128;
        }
        let mut body = vec![0; size];
        socket.read_exact(&mut body).await.unwrap();
        (kind, body)
    }
    #[tokio::test]
    async fn mqtt_flushes_large_batch_before_reporting_written_and_disconnects() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let line = "measurement,room=Küche value=\"Grüße 🌡️\" 42\n";
        let expected = line.repeat(1000);
        let broker_expected = expected.clone();
        let broker = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (kind, _) = packet(&mut socket).await;
            assert_eq!(kind, 0x10);
            socket.write_all(&[0x20, 2, 0, 0]).await.unwrap();
            let (kind, body) = packet(&mut socket).await;
            assert_eq!(kind, 0x30);
            let topic_size = u16::from_be_bytes([body[0], body[1]]) as usize;
            assert_eq!(&body[2..2 + topic_size], b"loxone/metrics");
            assert_eq!(&body[2 + topic_size..], broker_expected.as_bytes());
            let (kind, _) = packet(&mut socket).await;
            assert_eq!(kind, 0xe0);
        });
        let mut cfg = Telegraf {
            protocol: Protocol::Mqtt,
            max_retries: 1,
            ..Default::default()
        };
        cfg.mqtt.host = "127.0.0.1".into();
        cfg.mqtt.port = port;
        let m = Arc::new(Metrics::default());
        let mut writer = Writer::new(cfg, m.clone());
        timeout(Duration::from_secs(5), async {
            writer.initialize().await.unwrap();
            let mut batch = Batch {
                data: expected.clone(),
                points: 1000,
                fields: 1000,
            };
            writer.write(&mut batch).await.unwrap();
            assert_eq!(
                batch.data, expected,
                "MQTT must restore the exact UTF-8 batch"
            );
            assert_eq!(metrics::read(&m.points_written), 1000);
            writer.close().await.unwrap();
            broker.await.unwrap();
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn failed_mqtt_close_consumes_transport_and_can_be_repeated() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (drop_tx, drop_rx) = tokio::sync::oneshot::channel();
        let broker = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            assert_eq!(packet(&mut socket).await.0, 0x10);
            socket.write_all(&[0x20, 2, 0, 0]).await.unwrap();
            drop_rx.await.unwrap();
        });
        let mut cfg = Telegraf {
            protocol: Protocol::Mqtt,
            max_retries: 1,
            ..Default::default()
        };
        cfg.mqtt.host = "127.0.0.1".into();
        cfg.mqtt.port = port;
        let m = Arc::new(Metrics::default());
        let mut writer = Writer::new(cfg, m.clone());
        timeout(Duration::from_secs(3), async {
            writer.initialize().await.unwrap();
            drop_tx.send(()).unwrap();
            broker.await.unwrap();
            while m.output_ready.load(Ordering::Relaxed) {
                tokio::task::yield_now().await;
            }
            assert!(writer.close().await.is_err());
            assert!(writer.transport.is_none());
            assert!(!m.output_ready.load(Ordering::Relaxed));
            writer.close().await.unwrap();
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn udp_batches_without_splitting_lines() {
        let listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let cfg = Telegraf {
            host: "127.0.0.1".into(),
            port: listener.local_addr().unwrap().port(),
            udp_max_bytes: 512,
            ..Default::default()
        };
        let m = Arc::new(Metrics::default());
        let mut writer = Writer::new(cfg, m.clone());
        writer.initialize().await.unwrap();
        let line = format!("m,name={} f=21.2 42\n", "x".repeat(200));
        let mut batch = Batch {
            data: line.repeat(3),
            points: 3,
            fields: 3,
        };
        writer.write(&mut batch).await.unwrap();
        let mut buf = [0; 2048];
        let n = listener.recv(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], line.repeat(2).as_bytes());
        let n = listener.recv(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], line.as_bytes());
        assert_eq!(metrics::read(&m.points_written), 3);
    }
    #[tokio::test]
    async fn tcp_frames_are_newline_terminated_and_flushed() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cfg = Telegraf {
            host: "127.0.0.1".into(),
            port: listener.local_addr().unwrap().port(),
            protocol: Protocol::Tcp,
            max_retries: 1,
            ..Default::default()
        };
        let mut writer = Writer::new(cfg, Arc::new(Metrics::default()));
        writer.initialize().await.unwrap();
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut batch = Batch {
            data: "m f=1.0 1\nm f=2.0 2\n".into(),
            points: 2,
            fields: 2,
        };
        writer.write(&mut batch).await.unwrap();
        let mut actual = vec![0; batch.data.len()];
        socket.read_exact(&mut actual).await.unwrap();
        assert_eq!(actual, batch.data.as_bytes());
    }
    #[tokio::test]
    async fn oversize_udp_is_an_error_not_a_silent_loss() {
        let cfg = Telegraf {
            udp_max_bytes: 512,
            ..Default::default()
        };
        let mut writer = Writer::new(cfg, Arc::new(Metrics::default()));
        assert!(
            writer
                .write(&mut Batch {
                    data: "x".repeat(600),
                    points: 1,
                    fields: 1
                })
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn tcp_retry_limit_is_exact_and_failure_reaches_caller() {
        let metrics = Arc::new(Metrics::default());
        let mut writer = Writer::new(
            Telegraf {
                host: "127.0.0.1".into(),
                port: 0,
                protocol: Protocol::Tcp,
                max_retries: 2,
                write_timeout: 1,
                ..Default::default()
            },
            metrics.clone(),
        );
        let result = timeout(Duration::from_secs(4), writer.initialize())
            .await
            .unwrap();
        assert!(result.is_err());
        assert_eq!(metrics::read(&metrics.write_errors), 2);
        assert!(!metrics.output_ready.load(Ordering::Relaxed));
    }
    #[tokio::test]
    async fn mqtt_empty_username_is_anonymous_and_nonempty_credentials_are_preserved() {
        for user in ["", "mqttuser"] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let broker = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let (kind, body) = packet(&mut socket).await;
                assert_eq!(kind, 0x10);
                assert_eq!(body[7] & 0xc0, if user.is_empty() { 0 } else { 0xc0 });
                let client_len = u16::from_be_bytes([body[10], body[11]]) as usize;
                let credentials = &body[12 + client_len..];
                if user.is_empty() {
                    assert!(credentials.is_empty());
                } else {
                    assert_eq!(credentials, b"\x00\x08mqttuser\x00\x06secret");
                }
                socket.write_all(&[0x20, 2, 0, 0]).await.unwrap();
                assert_eq!(packet(&mut socket).await.0, 0xe0);
            });
            let mut cfg = Telegraf {
                protocol: Protocol::Mqtt,
                max_retries: 1,
                ..Default::default()
            };
            cfg.mqtt.host = "127.0.0.1".into();
            cfg.mqtt.port = port;
            cfg.mqtt.username = Some(user.into());
            cfg.mqtt.password = Some("secret".into());
            timeout(Duration::from_secs(5), async {
                let mut writer = Writer::new(cfg, Arc::new(Metrics::default()));
                writer.initialize().await.unwrap();
                writer.close().await.unwrap();
                broker.await.unwrap();
            })
            .await
            .unwrap();
        }
    }
    #[tokio::test]
    async fn tcp_nonpositive_retry_limits_retry_until_server_recovers() {
        for max_retries in [0, -1] {
            let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = reservation.local_addr().unwrap();
            drop(reservation);
            let metrics = Arc::new(Metrics::default());
            let mut writer = Writer::new(
                Telegraf {
                    host: "127.0.0.1".into(),
                    port: addr.port(),
                    protocol: Protocol::Tcp,
                    max_retries,
                    write_timeout: 1,
                    ..Default::default()
                },
                metrics.clone(),
            );
            let task = tokio::spawn(async move {
                writer.initialize().await?;
                Ok::<_, anyhow::Error>(writer)
            });
            timeout(Duration::from_secs(3), async {
                while metrics::read(&metrics.write_errors) == 0 {
                    sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert!(!metrics.output_ready.load(Ordering::Relaxed));
            let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
            let mut writer = timeout(Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(metrics.output_ready.load(Ordering::Relaxed));
            writer.close().await.unwrap();
            drop(listener);
        }
    }
}
