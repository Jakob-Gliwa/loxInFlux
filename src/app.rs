use crate::{
    config::Config,
    controls::Controls,
    metrics::{self, Metrics},
    miniserver::{self, Loader},
    pacer::Pacer,
    pipeline::{Batch, Engine, Input},
    writer::Writer,
};
use anyhow::{Context, Result, bail};
use arc_swap::ArcSwap;
use bytes::Bytes;
use futures_util::{StreamExt, stream::FuturesUnordered};
use loxwebsocket::{ClientEvent, ConnectConfig, LoxClient, LoxHandler, LoxoneUuid, TlsMode};
use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};
use tokio::{
    sync::{Mutex, mpsc, watch},
    task::JoinSet,
    time::{MissedTickBehavior, interval, sleep, timeout},
};
use tokio_util::sync::CancellationToken;

pub struct Handler {
    tx: mpsc::Sender<Input>,
    metrics: Arc<Metrics>,
    lifecycle: watch::Sender<u64>,
    overflow: CancellationToken,
    terminal: CancellationToken,
    timestamp: u64,
    received: Instant,
}
impl Handler {
    fn push(&self, event: Input) {
        if self.tx.try_send(event).is_err() {
            metrics::inc(&self.metrics.values_dropped);
            // A sync WebSocket callback cannot await capacity. Fail visibly
            // instead of silently running with a gap or growing RAM forever.
            self.overflow.cancel();
        }
        self.metrics.queue_depth.store(
            (self.tx.max_capacity() - self.tx.capacity()) as u64,
            Ordering::Relaxed,
        );
    }
    fn observed(&mut self) {
        // The pinned 0.3.2 client calls on_raw_payload during authentication
        // too. All records in that table share its receive time: no clock
        // reads per record, even before Connected/Reconnected arrives.
        metrics::inc(&self.metrics.values_received);
        self.metrics
            .last_received_ns
            .store(self.timestamp, Ordering::Relaxed);
    }
}
impl LoxHandler for Handler {
    fn on_raw_payload(&mut self, _kind: u8, _payload: &[u8]) {
        self.timestamp = metrics::timestamp_ns();
        self.received = Instant::now();
    }
    fn on_value(&mut self, uuid: &LoxoneUuid, value: f64) {
        self.observed();
        self.push(Input::Number {
            uuid: *uuid,
            value,
            timestamp: self.timestamp,
            received: self.received,
        });
    }
    fn on_text(&mut self, uuid: &LoxoneUuid, _icon: &LoxoneUuid, text: &[u8]) {
        self.observed();
        self.push(Input::Text {
            uuid: *uuid,
            value: Bytes::copy_from_slice(text),
            timestamp: self.timestamp,
            received: self.received,
        });
    }
    fn on_event(&mut self, event: ClientEvent) {
        match event {
            ClientEvent::Connected => self
                .metrics
                .websocket_connected
                .store(true, Ordering::Relaxed),
            ClientEvent::Reconnected => {
                self.metrics
                    .websocket_connected
                    .store(true, Ordering::Relaxed);
                metrics::inc(&self.metrics.reconnects);
                let lost = self.metrics.websocket_lost_ns.swap(0, Ordering::Relaxed);
                let outage = metrics::timestamp_ns().saturating_sub(lost) as f64 / 1e9;
                if lost > 0 {
                    tracing::info!(
                        outage_s = %format_args!("{outage:.1}"),
                        "WebSocket session restored"
                    );
                } else {
                    tracing::info!("WebSocket session restored");
                }
                self.lifecycle.send_modify(|n| *n = n.wrapping_add(1));
            }
            ClientEvent::ConnectionClosed { close_code } => {
                self.metrics
                    .websocket_connected
                    .store(false, Ordering::Relaxed);
                mark_websocket_lost(&self.metrics);
                if !self.metrics.shutting_down.load(Ordering::Relaxed) {
                    tracing::warn!(
                        ?close_code,
                        "WebSocket closed by Miniserver; values marked stale until reconnect"
                    );
                }
                self.push(Input::SessionReset);
            }
            ClientEvent::Closed => {
                self.metrics
                    .websocket_connected
                    .store(false, Ordering::Relaxed);
                self.terminal.cancel();
            }
        }
    }
}
type Client = LoxClient<Handler>;
fn mark_websocket_lost(m: &Metrics) {
    let _ = m.websocket_lost_ns.compare_exchange(
        0,
        metrics::timestamp_ns(),
        Ordering::Relaxed,
        Ordering::Relaxed,
    );
}

pub async fn run(config: Config, stop: CancellationToken) -> Result<()> {
    let metrics = Arc::new(Metrics::default());
    metrics
        .queue_capacity
        .store(config.telegraf.queue_capacity as u64, Ordering::Relaxed);
    let http_stop = CancellationToken::new();
    let http = if config.monitoring.enabled {
        let listener = tokio::net::TcpListener::bind(&config.monitoring.listen)
            .await
            .context("Binding monitoring endpoint")?;
        let router = metrics::router(metrics.clone());
        let stopped = http_stop.clone();
        Some(tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(stopped.cancelled_owned())
                .await
        }))
    } else {
        None
    };
    let result = run_bridge(config, metrics.clone(), stop).await;
    metrics.shutting_down.store(true, Ordering::Relaxed);
    http_stop.cancel();
    if let Some(mut task) = http {
        match timeout(Duration::from_secs(2), &mut task).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(e))) => tracing::error!(%e,"Monitoring server failed"),
            _ => task.abort(),
        }
    }
    result
}

async fn run_bridge(config: Config, metrics: Arc<Metrics>, stop: CancellationToken) -> Result<()> {
    let mut loader = Loader::new(&config)?;
    let initial = tokio::select! {r=loader.load()=>r?.context("Initial configuration unavailable")?,_=stop.cancelled()=>return Ok(())};
    record_controls(&initial, &metrics, loader.version(), None, "startup");
    let controls = Arc::new(ArcSwap::from_pointee(initial));
    let (tx, rx) = mpsc::channel(config.telegraf.queue_capacity);
    let engine = Engine::new(config.clone(), metrics.clone(), controls.load_full());
    let mut writer = tokio::spawn(write_loop(
        engine,
        Writer::new(config.telegraf.clone(), metrics.clone()),
        rx,
        config.clone(),
        metrics.clone(),
    ));
    let (events, event_rx) = watch::channel(0);
    let overflow = CancellationToken::new();
    let terminal = CancellationToken::new();
    let handler = Handler {
        tx: tx.clone(),
        metrics: metrics.clone(),
        lifecycle: events,
        overflow: overflow.clone(),
        terminal: terminal.clone(),
        timestamp: metrics::timestamp_ns(),
        received: Instant::now(),
    };
    let mut ws = ConnectConfig::new(
        config.miniserver.base_url(),
        &config.miniserver.user,
        &config.miniserver.password,
    );
    ws.tls = if config.miniserver.tls_verify {
        TlsMode::WebPki
    } else {
        TlsMode::Insecure
    };
    ws.client_info = format!("loxInFlux {}", env!("CARGO_PKG_VERSION"));
    ws.max_reconnect_attempts = config.miniserver.max_reconnect_attempts;
    ws.command_timeout_secs = config.miniserver.request_timeout;
    // In 0.3.2, dropping connect() before it returns detaches the supervisor
    // without a LoxClient whose Drop could stop it. Keep the handshake owned
    // by a task even if startup is cancelled. An unclaimed successful result
    // drops the client and requests token revocation/session shutdown. Do not
    // abort this task: doing so would recreate the upstream cancellation leak.
    let mut connecting = tokio::spawn(LoxClient::connect(ws, handler));
    let connected = tokio::select! {
        result=&mut connecting=>match result {Ok(result)=>result.map_err(anyhow::Error::from),Err(e)=>Err(e.into())},
        _=stop.cancelled()=>{writer.abort();return Ok(());},
        _=overflow.cancelled()=>{writer.abort();bail!("Input queue overflow during initial WebSocket snapshot");},
        result=&mut writer=>{return result.context("Writer task panicked")?.and_then(|_|Err(anyhow::anyhow!("Writer exited during startup")));},
    };
    let client = match connected {
        Ok(c) => Arc::new(c),
        Err(e) => {
            writer.abort();
            return Err(e);
        }
    };
    let producers = CancellationToken::new();
    let mut tasks = JoinSet::new();
    let pacer = Arc::new(Pacer::new(config.general.grabber_rate_limit));
    if config.general.grabber {
        tasks.spawn(poll_loop(
            client.clone(),
            pacer.clone(),
            controls.clone(),
            tx.clone(),
            config.clone(),
            metrics.clone(),
            producers.clone(),
        ));
    }
    tasks.spawn(refresh_loop(
        loader,
        client.clone(),
        pacer,
        controls,
        tx.clone(),
        event_rx,
        config.general.config_refresh_interval,
        metrics.clone(),
        producers.clone(),
    ));
    // The pinned upstream emits ConnectionClosed for close frames, but not
    // for EOF/read-timeout failures. Observe its actual state to invalidate
    // stale values and health during those failures too. No sensor polling.
    let watched = client.clone();
    let watched_metrics = metrics.clone();
    let watched_stop = producers.clone();
    let watched_tx = tx.clone();
    tasks.spawn(async move {
        let mut was_live=true;
        loop {
            tokio::select!{_=watched_stop.cancelled()=>return Ok(()),_=sleep(Duration::from_secs(1))=>{}}
            let live=watched.live();
            if was_live && !live {
                mark_websocket_lost(&watched_metrics);
                if watched_metrics.websocket_connected.swap(false,Ordering::Relaxed)
                    && !watched_metrics.shutting_down.load(Ordering::Relaxed)
                {
                    tracing::warn!("WebSocket session lost without close frame; values marked stale until reconnect");
                }
                watched_tx.send(Input::SessionReset).await?;
            }
            was_live=live;
        }
    });
    tracing::info!(protocol=?config.telegraf.protocol,"Bridge running");
    let mut writer_finished = false;
    let mut result = tokio::select! {
        _=stop.cancelled()=>Ok(()),
        _=terminal.cancelled()=>Err(anyhow::anyhow!("WebSocket client stopped; reconnect attempts exhausted or terminal error")),
        _=overflow.cancelled()=>Err(anyhow::anyhow!("Input queue exhausted; stopping to make data loss visible (increase queue_capacity or output throughput)")),
        done=&mut writer=>{writer_finished=true;match done {Ok(Err(e))=>Err(e),Ok(Ok(()))=>Err(anyhow::anyhow!("Writer exited unexpectedly")),Err(e)=>Err(e.into())}},
        done=tasks.join_next()=>match done {Some(Ok(Err(e)))=>Err(e),Some(Err(e))=>Err(e.into()),_=>Err(anyhow::anyhow!("Background worker exited unexpectedly"))},
    };
    metrics.shutting_down.store(true, Ordering::Relaxed);
    producers.cancel();
    tasks.shutdown().await;
    if let Ok(client) = Arc::try_unwrap(client)
        && let Err(e) = client.stop().await
    {
        tracing::warn!(%e,"WebSocket shutdown failed");
    }
    drop(tx);
    if !writer_finished {
        match timeout(
            Duration::from_secs(config.telegraf.shutdown_timeout),
            &mut writer,
        )
        .await
        {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(e))) => {
                if result.is_ok() {
                    result = Err(e);
                }
            }
            Ok(Err(e)) => {
                if result.is_ok() {
                    result = Err(e.into());
                }
            }
            Err(_) => {
                writer.abort();
                metrics::add(&metrics.values_dropped, metrics::read(&metrics.queue_depth));
                result = Err(anyhow::anyhow!(
                    "Shutdown deadline exceeded; output queue could not be fully drained"
                ));
            }
        }
    }
    result
}

fn record_controls(
    c: &Controls,
    m: &Metrics,
    version: Option<&str>,
    previous: Option<&str>,
    trigger: &str,
) {
    m.controls_websocket
        .store(c.websocket.len() as u64, Ordering::Relaxed);
    m.controls_grabber
        .store(c.grabber.len() as u64, Ordering::Relaxed);
    m.config_ready.store(true, Ordering::Relaxed);
    metrics::inc(&m.config_reloads);
    tracing::info!(
        websocket = c.websocket.len(),
        grabber = c.grabber.len(),
        version,
        previous,
        trigger,
        "Loaded control routes"
    );
}
#[allow(clippy::too_many_arguments)]
async fn refresh_loop(
    mut loader: Loader,
    client: Arc<Client>,
    pacer: Arc<Pacer>,
    controls: Arc<ArcSwap<Controls>>,
    tx: mpsc::Sender<Input>,
    mut events: watch::Receiver<u64>,
    seconds: u64,
    m: Arc<Metrics>,
    stop: CancellationToken,
) -> Result<()> {
    let mut timer = interval(Duration::from_secs(seconds.max(1)));
    timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
    timer.tick().await;
    let mut failed_checks = 0u32;
    loop {
        let trigger = tokio::select! {_=stop.cancelled()=>return Ok(()),_=timer.tick(),if seconds>0=>"periodic",r=events.changed()=>{r?;"reconnect"}};
        // A command sent without a session would only wait for its timeout;
        // the reconnect that restores the session triggers a check anyway.
        if !client.live() {
            continue;
        }
        let previous = loader.version().map(str::to_owned);
        let result = tokio::select! {_=stop.cancelled()=>return Ok(()),r=refresh(&mut loader,&client,&pacer)=>r};
        if result.is_ok() && failed_checks > 0 {
            tracing::info!(failed_checks, "Configuration check recovered");
            failed_checks = 0;
        }
        match result {
            Ok(Some(new)) => {
                let new = Arc::new(new);
                tx.send(Input::Refresh(new.clone())).await?;
                record_controls(&new, &m, loader.version(), previous.as_deref(), trigger);
                controls.store(new);
            }
            Ok(None) => tracing::debug!(version = previous, trigger, "Configuration unchanged"),
            Err(e) => {
                failed_checks += 1;
                metrics::inc(&m.config_errors);
                tracing::warn!(error=%format_args!("{e:#}"),failed_checks,trigger,"Configuration refresh failed; retaining current routes");
            }
        }
    }
}
/// Asks over the encrypted WebSocket session, so periodic checks send no HTTP
/// credentials. HTTP is used only when the version changed or is unknown.
async fn refresh(loader: &mut Loader, client: &Client, pacer: &Pacer) -> Result<Option<Controls>> {
    let version = checked_command(client, pacer, miniserver::VERSION_COMMAND)
        .await
        .and_then(|response| miniserver::version_response(&response));
    let version = match version {
        Ok(version) => Some(version),
        Err(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "WebSocket version check failed; reloading the configuration over HTTP");
            None
        }
    };
    loader.load_version(version).await
}

async fn poll_loop(
    client: Arc<Client>,
    pacer: Arc<Pacer>,
    controls: Arc<ArcSwap<Controls>>,
    tx: mpsc::Sender<Input>,
    config: Config,
    m: Arc<Metrics>,
    stop: CancellationToken,
) -> Result<()> {
    let secured = Arc::new(Mutex::new(()));
    let mut previous_incomplete = false;
    loop {
        if client.live() {
            let current = controls.load_full();
            let started = Instant::now();
            // ArcSwap pins one immutable snapshot for the cycle. Borrow its
            // controls instead of cloning every UUID string and route prefix.
            let mut remaining = current.grabber.values();
            let poll = |control| poll_one(&client, control, &tx, &config, &m, &secured, &pacer);
            let mut requests = FuturesUnordered::new();
            for control in remaining.by_ref().take(config.general.grabber_concurrency) {
                requests.push(poll(control));
            }
            let mut report = CycleReport::default();
            loop {
                let next = tokio::select! {_=stop.cancelled()=>return Ok(()),r=requests.next()=>r};
                match next {
                    Some(outcome) => {
                        report.record(outcome);
                        if let Some(control) = remaining.next() {
                            requests.push(poll(control));
                        }
                    }
                    None => break,
                }
            }
            m.grabber_unreadable_controls
                .store(report.unreadable, Ordering::Relaxed);
            let failed = report.failed();
            m.grabber_failed.store(failed, Ordering::Relaxed);
            if !failed {
                m.last_grab_success_ns
                    .store(metrics::timestamp_ns(), Ordering::Relaxed);
            }
            let duration = started.elapsed().as_secs_f64();
            if report.incomplete() {
                tracing::warn!(
                    ok = report.ok,
                    unreadable = report.unreadable,
                    failed = report.failed,
                    duration_s = %format_args!("{duration:.1}"),
                    first_failures = %report.first_failures.join(" | "),
                    "Grabber cycle incomplete"
                );
            } else if previous_incomplete {
                tracing::info!(ok = report.ok, duration_s = %format_args!("{duration:.1}"), "Grabber cycle recovered");
            } else {
                tracing::debug!(ok = report.ok, duration_s = %format_args!("{duration:.1}"), "Grabber cycle complete");
            }
            previous_incomplete = report.incomplete();
        }
        tokio::select! {_=stop.cancelled()=>return Ok(()),_=sleep(Duration::from_secs(config.general.grabber_interval))=>{}}
    }
}

async fn poll_one(
    client: &Client,
    control: &crate::controls::PollControl,
    tx: &mpsc::Sender<Input>,
    config: &Config,
    m: &Metrics,
    secured: &Mutex<()>,
    pacer: &Pacer,
) -> Poll {
    metrics::inc(&m.grabber_requests);
    let result = async {
        let command = if control.secured {
            let _guard = secured.lock().await;
            let password = config
                .miniserver
                .visu_password
                .as_deref()
                .context("VisuPwd control requires miniserver.visu_password")?;
            let raw = checked_command(
                client,
                pacer,
                &format!("jdev/sys/getvisusalt/{}", config.miniserver.user),
            )
            .await?;
            let key = loxwebsocket::auth::parse_key_salt(&raw)?;
            let hash = loxwebsocket::crypto::hash_visu_password(&key, password)?;
            // Hold the lock until the actual response: the client's max-one
            // fire-and-forget visu queue would overwrite concurrent polls.
            checked_command(
                client,
                pacer,
                &format!("jdev/sps/ios/{hash}/{}/all", control.uuid_text),
            )
            .await?
        } else {
            checked_command(
                client,
                pacer,
                &format!("jdev/sps/io/{}/all", control.uuid_text),
            )
            .await?
        };
        let timestamp = metrics::timestamp_ns();
        let received = Instant::now();
        let fields = crate::grabber::parse(&command, config, m, true, Some(&control.uuid_text))?;
        tx.send(Input::ParsedGrabber {
            uuid: control.uuid,
            fields,
            timestamp,
            received,
        })
        .await
        .context("Pipeline closed")?;
        m.last_received_ns.store(timestamp, Ordering::Relaxed);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let Err(error) = result else {
        return Poll::Ok;
    };
    metrics::inc(&m.grabber_errors);
    let error = error.context(control.prefix.clone());
    // A failed request is not evidence that the last good value is still
    // current. Reconcile through the same ordered queue as good snapshots.
    if let Err(closed) = tx.send(Input::GrabberFailed { uuid: control.uuid }).await {
        return Poll::Failed(
            anyhow::Error::from(closed).context("Pipeline closed while invalidating failed poll"),
        );
    }
    if error.is::<crate::grabber::InvalidGrabberJson>() {
        metrics::inc(&m.grabber_data_errors);
        Poll::Unreadable(error)
    } else {
        Poll::Failed(error)
    }
}

enum Poll {
    Ok,
    /// The Miniserver answered, but the payload stayed malformed after repair.
    Unreadable(anyhow::Error),
    Failed(anyhow::Error),
}

/// One polling cycle, summarized into a single log line at its end.
#[derive(Default)]
struct CycleReport {
    ok: u64,
    unreadable: u64,
    failed: u64,
    first_failures: Vec<String>,
}
impl CycleReport {
    const SAMPLES: usize = 3;
    fn record(&mut self, outcome: Poll) {
        let error = match outcome {
            Poll::Ok => {
                self.ok += 1;
                return;
            }
            Poll::Unreadable(error) => {
                self.unreadable += 1;
                error
            }
            Poll::Failed(error) => {
                self.failed += 1;
                error
            }
        };
        if self.first_failures.len() < Self::SAMPLES {
            self.first_failures.push(format!("{error:#}"));
        }
    }
    fn incomplete(&self) -> bool {
        self.unreadable + self.failed > 0
    }
    /// An isolated malformed response degrades data quality; losing the
    /// entire polling cycle still makes readiness fail.
    fn failed(&self) -> bool {
        self.failed > 0 || (self.unreadable > 0 && self.ok == 0)
    }
}

async fn checked_command(client: &Client, pacer: &Pacer, command: &str) -> Result<Bytes> {
    pacer.wait().await;
    // Upstream 0.3.2 resolves a mismatched LL.control to the oldest waiter
    // anyway. Never attach that value to the requested sensor. Its public API
    // exposes only a global mismatch counter, so overlapping requests are
    // conservatively failed too; the next clean polling cycle recovers them.
    // This check is on the polling path, never on binary value callbacks.
    let before = client.metrics().correlation_mismatches;
    let response = client.send_command(command).await?;
    if client.metrics().correlation_mismatches != before {
        bail!("WebSocket response correlation mismatch during polling");
    }
    Ok(response)
}

async fn write_loop(
    mut engine: Engine,
    mut writer: Writer,
    mut rx: mpsc::Receiver<Input>,
    config: Config,
    m: Arc<Metrics>,
) -> Result<()> {
    writer.initialize().await?;
    let mut input = Vec::with_capacity(config.telegraf.batch_size);
    let mut batch = Batch {
        data: String::with_capacity(config.telegraf.batch_size * 256),
        ..Default::default()
    };
    let heartbeat_enabled =
        config.processing.only_on_change && config.processing.heartbeat_interval > 0;
    let mut heartbeat = interval(Duration::from_secs(
        config.processing.heartbeat_interval.clamp(1, 30),
    ));
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);
    heartbeat.tick().await;
    loop {
        let count = tokio::select! {
            n=rx.recv_many(&mut input,config.telegraf.batch_size)=>Some(n),
            _=heartbeat.tick(),if heartbeat_enabled=>None,
            result=writer.wait_failure()=>{result?;bail!("Output worker exited unexpectedly");},
        };
        m.queue_depth.store(rx.len() as u64, Ordering::Relaxed);
        if count == Some(0) {
            break;
        }
        if count.is_none() {
            engine.heartbeat(Instant::now(), metrics::timestamp_ns(), &mut batch);
        } else {
            for event in input.drain(..) {
                if let Err(error) = engine.process(event, &mut batch)
                    && m.parse_errors.fetch_add(1, Ordering::Relaxed) == 0
                {
                    tracing::warn!(error=%format_args!("{error:#}"),"Discarding malformed input; further occurrences are only counted in loxinflux_parse_errors_total");
                }
            }
        }
        if let Err(error) = writer.write(&mut batch).await {
            metrics::add(&m.values_dropped, batch.fields);
            return Err(error);
        }
        batch.clear();
    }
    writer.close().await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cycle_report_counts_outcomes_and_keeps_three_failures() {
        let mut report = CycleReport::default();
        report.record(Poll::Ok);
        assert!(!report.incomplete() && !report.failed());
        report.record(Poll::Unreadable(
            anyhow::anyhow!("invalid JSON").context("Pump,room=Cellar"),
        ));
        assert!(report.incomplete());
        assert!(!report.failed(), "one unreadable control beside a good one");
        for n in 0..4 {
            report.record(Poll::Failed(anyhow::anyhow!("timeout {n}")));
        }
        assert_eq!((report.ok, report.unreadable, report.failed), (1, 1, 4));
        assert!(report.failed());
        assert_eq!(
            report.first_failures,
            ["Pump,room=Cellar: invalid JSON", "timeout 0", "timeout 1"]
        );
        let mut unreadable_only = CycleReport::default();
        unreadable_only.record(Poll::Unreadable(anyhow::anyhow!("invalid JSON")));
        assert!(
            unreadable_only.failed(),
            "a cycle without any good response"
        );
    }
    #[tokio::test]
    async fn idle_pipeline_propagates_exhausted_mqtt_worker_without_sensor_traffic() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let broker = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = Config::default();
        config.telegraf.protocol = crate::config::Protocol::Mqtt;
        config.telegraf.max_retries = 1;
        config.telegraf.mqtt.host = "127.0.0.1".into();
        config.telegraf.mqtt.port = broker.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = broker.accept().await.unwrap();
            assert_eq!(socket.read_u8().await.unwrap(), 0x10);
            let len = socket.read_u8().await.unwrap();
            assert!(len < 128);
            socket.read_exact(&mut vec![0; len as usize]).await.unwrap();
            socket.write_all(&[0x20, 2, 0, 0]).await.unwrap();
            // Close the accepted session; no new sensor value will wake write().
        });
        let metrics = Arc::new(Metrics::default());
        let engine = Engine::new(
            config.clone(),
            metrics.clone(),
            Arc::new(Controls::default()),
        );
        let writer = Writer::new(config.telegraf.clone(), metrics.clone());
        let (_tx, rx) = mpsc::channel(4);
        let mut task = tokio::spawn(write_loop(engine, writer, rx, config, metrics.clone()));
        let result = timeout(Duration::from_secs(2), &mut task).await;
        task.abort();
        server.await.unwrap();
        assert!(
            result.is_ok(),
            "MQTT worker failed but the idle writer never reported it"
        );
        assert!(result.unwrap().unwrap().is_err());
        assert!(!metrics.output_ready.load(Ordering::Relaxed));
    }
    #[test]
    fn early_table_records_share_the_raw_payload_timestamp() {
        let (tx, mut rx) = mpsc::channel(4);
        let (events, _) = watch::channel(0);
        let m = Arc::new(Metrics::default());
        let mut h = Handler {
            tx,
            metrics: m,
            lifecycle: events,
            overflow: CancellationToken::new(),
            terminal: CancellationToken::new(),
            timestamp: 0,
            received: Instant::now(),
        };
        h.on_raw_payload(2, &[]);
        let timestamp = h.timestamp;
        let received = h.received;
        // The 0.3.2 client invokes on_raw_payload even before Connected.
        for i in 0..2 {
            h.on_value(&LoxoneUuid::default(), i as f64);
            let Input::Number {
                timestamp: actual,
                received: seen,
                ..
            } = rx.try_recv().unwrap()
            else {
                panic!("expected numeric event")
            };
            assert_eq!(actual, timestamp);
            assert_eq!(seen, received);
        }
    }
    #[test]
    fn callback_overflow_is_counted_and_triggers_shutdown() {
        let (tx, _rx) = mpsc::channel(1);
        let (events, _) = watch::channel(0);
        let m = Arc::new(Metrics::default());
        let overflow = CancellationToken::new();
        let mut h = Handler {
            tx,
            metrics: m.clone(),
            lifecycle: events,
            overflow: overflow.clone(),
            terminal: CancellationToken::new(),
            timestamp: 42,
            received: Instant::now(),
        };
        let uuid = LoxoneUuid::default();
        h.on_value(&uuid, 1.0);
        h.on_value(&uuid, 2.0);
        assert_eq!(metrics::read(&m.values_received), 2);
        assert_eq!(metrics::read(&m.values_dropped), 1);
        assert!(overflow.is_cancelled());
    }
}
