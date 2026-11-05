use axum::{
    Json, Router,
    extract::State,
    http::{StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use serde_json::json;
use std::{
    fmt::Write,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

pub fn timestamp_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64
}
pub fn inc(n: &AtomicU64) {
    n.fetch_add(1, Ordering::Relaxed);
}
pub fn add(n: &AtomicU64, value: u64) {
    n.fetch_add(value, Ordering::Relaxed);
}
pub fn read(n: &AtomicU64) -> u64 {
    n.load(Ordering::Relaxed)
}

pub struct Metrics {
    pub started: Instant,
    pub config_ready: AtomicBool,
    pub websocket_connected: AtomicBool,
    pub output_ready: AtomicBool,
    pub shutting_down: AtomicBool,
    pub values_received: AtomicU64,
    pub values_written: AtomicU64,
    pub points_written: AtomicU64,
    pub bytes_written: AtomicU64,
    pub values_filtered: AtomicU64,
    pub values_invalid: AtomicU64,
    pub values_unchanged: AtomicU64,
    pub values_dropped: AtomicU64,
    pub heartbeats: AtomicU64,
    pub write_errors: AtomicU64,
    pub parse_errors: AtomicU64,
    pub grabber_requests: AtomicU64,
    pub grabber_errors: AtomicU64,
    pub grabber_json_repaired: AtomicU64,
    pub grabber_data_errors: AtomicU64,
    pub grabber_unreadable_controls: AtomicU64,
    pub config_errors: AtomicU64,
    pub config_reloads: AtomicU64,
    pub reconnects: AtomicU64,
    pub queue_depth: AtomicU64,
    pub queue_capacity: AtomicU64,
    pub controls_websocket: AtomicU64,
    pub controls_grabber: AtomicU64,
    pub last_received_ns: AtomicU64,
    pub last_written_ns: AtomicU64,
    pub last_grab_success_ns: AtomicU64,
    pub grabber_failed: AtomicBool,
}
impl Default for Metrics {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            config_ready: false.into(),
            websocket_connected: false.into(),
            output_ready: false.into(),
            shutting_down: false.into(),
            values_received: 0.into(),
            values_written: 0.into(),
            points_written: 0.into(),
            bytes_written: 0.into(),
            values_filtered: 0.into(),
            values_invalid: 0.into(),
            values_unchanged: 0.into(),
            values_dropped: 0.into(),
            heartbeats: 0.into(),
            write_errors: 0.into(),
            parse_errors: 0.into(),
            grabber_requests: 0.into(),
            grabber_errors: 0.into(),
            grabber_json_repaired: 0.into(),
            grabber_data_errors: 0.into(),
            grabber_unreadable_controls: 0.into(),
            config_errors: 0.into(),
            config_reloads: 0.into(),
            reconnects: 0.into(),
            queue_depth: 0.into(),
            queue_capacity: 0.into(),
            controls_websocket: 0.into(),
            controls_grabber: 0.into(),
            last_received_ns: 0.into(),
            last_written_ns: 0.into(),
            last_grab_success_ns: 0.into(),
            grabber_failed: false.into(),
        }
    }
}
impl Metrics {
    pub fn healthy(&self) -> bool {
        self.config_ready.load(Ordering::Relaxed)
            && self.websocket_connected.load(Ordering::Relaxed)
            && self.output_ready.load(Ordering::Relaxed)
            && !self.shutting_down.load(Ordering::Relaxed)
            && !self.grabber_failed.load(Ordering::Relaxed)
            && read(&self.values_dropped) == 0
    }
    pub fn prometheus(&self) -> String {
        let mut out = String::with_capacity(6000);
        for (name, help, value) in [
            (
                "grabber_json_repaired",
                "Responses repaired after strict JSON parsing failed",
                read(&self.grabber_json_repaired),
            ),
            (
                "grabber_data_errors",
                "Unrepairable JSON responses",
                read(&self.grabber_data_errors),
            ),
            (
                "values_received",
                "Input values before filtering",
                read(&self.values_received),
            ),
            (
                "values_written",
                "Fields handed to the configured output",
                read(&self.values_written),
            ),
            (
                "points_written",
                "Line protocol points handed to output",
                read(&self.points_written),
            ),
            (
                "bytes_written",
                "Line protocol payload bytes handed to output",
                read(&self.bytes_written),
            ),
            (
                "values_filtered",
                "Values excluded by control filters or unknown UUIDs",
                read(&self.values_filtered),
            ),
            (
                "values_invalid",
                "Empty placeholder nonfinite or unsupported values",
                read(&self.values_invalid),
            ),
            (
                "values_unchanged",
                "Unchanged values suppressed",
                read(&self.values_unchanged),
            ),
            (
                "values_dropped",
                "Values lost due to bounded queue overflow or rejected output",
                read(&self.values_dropped),
            ),
            (
                "heartbeats",
                "Unchanged fields reemitted as a heartbeat",
                read(&self.heartbeats),
            ),
            (
                "write_errors",
                "Output transport errors including retried errors",
                read(&self.write_errors),
            ),
            (
                "parse_errors",
                "Malformed grabber or text payloads",
                read(&self.parse_errors),
            ),
            (
                "grabber_requests",
                "Miniserver all requests",
                read(&self.grabber_requests),
            ),
            (
                "grabber_errors",
                "Failed grabber requests",
                read(&self.grabber_errors),
            ),
            (
                "config_errors",
                "Failed configuration refreshes",
                read(&self.config_errors),
            ),
            (
                "config_reloads",
                "Successful configuration loads",
                read(&self.config_reloads),
            ),
            (
                "reconnects",
                "WebSocket reconnections",
                read(&self.reconnects),
            ),
        ] {
            let _ = writeln!(
                out,
                "# HELP loxinflux_{name}_total {help}\n# TYPE loxinflux_{name}_total counter\nloxinflux_{name}_total {value}"
            );
        }
        for (name, value) in [
            ("ready", u64::from(self.healthy())),
            (
                "grabber_unreadable_controls",
                read(&self.grabber_unreadable_controls),
            ),
            (
                "websocket_connected",
                u64::from(self.websocket_connected.load(Ordering::Relaxed)),
            ),
            (
                "output_ready",
                u64::from(self.output_ready.load(Ordering::Relaxed)),
            ),
            ("queue_depth", read(&self.queue_depth)),
            ("queue_capacity", read(&self.queue_capacity)),
            ("controls_websocket", read(&self.controls_websocket)),
            ("controls_grabber", read(&self.controls_grabber)),
        ] {
            let _ = writeln!(
                out,
                "# TYPE loxinflux_{name} gauge\nloxinflux_{name} {value}"
            );
        }
        for (name, value) in [
            ("uptime_seconds", self.started.elapsed().as_secs_f64()),
            (
                "last_received_timestamp_seconds",
                read(&self.last_received_ns) as f64 / 1e9,
            ),
            (
                "last_written_timestamp_seconds",
                read(&self.last_written_ns) as f64 / 1e9,
            ),
            (
                "last_grab_success_timestamp_seconds",
                read(&self.last_grab_success_ns) as f64 / 1e9,
            ),
        ] {
            let _ = writeln!(
                out,
                "# TYPE loxinflux_{name} gauge\nloxinflux_{name} {value}"
            );
        }
        let _ = writeln!(
            out,
            "# TYPE loxinflux_build_info gauge\nloxinflux_build_info{{version=\"{}\"}} 1",
            env!("CARGO_PKG_VERSION")
        );
        out
    }
}
pub fn router(metrics: Arc<Metrics>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(health))
        .route("/live", get(|| async { StatusCode::OK }))
        .route("/metrics", get(prometheus))
        .with_state(metrics)
}
async fn health(State(m): State<Arc<Metrics>>) -> impl IntoResponse {
    let ready = m.healthy();
    (
        if ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        Json(json!({
            "status":if ready {"ok"} else {"degraded"},"config_ready":m.config_ready.load(Ordering::Relaxed),
            "websocket_connected":m.websocket_connected.load(Ordering::Relaxed),"output_ready":m.output_ready.load(Ordering::Relaxed),
            "grabber_failed":m.grabber_failed.load(Ordering::Relaxed),
            "degraded":read(&m.grabber_unreadable_controls)>0,
            "grabber_unreadable_controls":read(&m.grabber_unreadable_controls),"dropped_values":read(&m.values_dropped),
            "queue_depth":read(&m.queue_depth),"shutting_down":m.shutting_down.load(Ordering::Relaxed)
        })),
    )
}
async fn prometheus(State(m): State<Arc<Metrics>>) -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        m.prometheus(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;
    #[tokio::test]
    async fn readiness_tracks_dependencies_without_requiring_value_changes() {
        let m = Arc::new(Metrics::default());
        let app = router(m.clone());
        let request = || {
            axum::http::Request::builder()
                .uri("/health")
                .body(axum::body::Body::empty())
                .unwrap()
        };
        assert_eq!(app.clone().oneshot(request()).await.unwrap().status(), 503);
        m.config_ready.store(true, Ordering::Relaxed);
        m.websocket_connected.store(true, Ordering::Relaxed);
        m.output_ready.store(true, Ordering::Relaxed);
        assert_eq!(app.clone().oneshot(request()).await.unwrap().status(), 200);
        inc(&m.values_dropped);
        assert_eq!(app.oneshot(request()).await.unwrap().status(), 503);
    }
    #[tokio::test]
    async fn metrics_are_prometheus_text() {
        use http_body_util::BodyExt;
        let response = router(Arc::new(Metrics::default()))
            .oneshot(
                axum::http::Request::builder()
                    .uri("/metrics")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            response.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .contains("version=0.0.4")
        );
        let text = String::from_utf8(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        assert!(text.contains("loxinflux_values_received_total 0"));
    }
}
