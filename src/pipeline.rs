use crate::{
    config::Config,
    controls::{Controls, PollControl, Route},
    metrics::{self, Metrics},
    protocol,
    value::{self, Value},
};
use anyhow::{Context, Result};
use bytes::Bytes;
use loxwebsocket::{LoxoneUuid, UuidMap};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

#[derive(Debug)]
pub enum Input {
    Number {
        uuid: LoxoneUuid,
        value: f64,
        timestamp: u64,
        received: Instant,
    },
    Text {
        uuid: LoxoneUuid,
        value: Bytes,
        timestamp: u64,
        received: Instant,
    },
    Grabber {
        uuid: LoxoneUuid,
        payload: Bytes,
        timestamp: u64,
        received: Instant,
    },
    ParsedGrabber {
        uuid: LoxoneUuid,
        fields: BTreeMap<String, Value>,
        timestamp: u64,
        received: Instant,
    },
    GrabberFailed {
        uuid: LoxoneUuid,
    },
    Refresh(Arc<Controls>),
    SessionReset,
}
#[derive(Default)]
pub struct Batch {
    pub data: String,
    pub points: u64,
    pub fields: u64,
}
impl Batch {
    pub fn clear(&mut self) {
        self.data.clear();
        self.points = 0;
        self.fields = 0;
    }
}
#[derive(Default)]
struct Last {
    value: Option<Value>,
    emitted: Option<Instant>,
}
impl Last {
    fn accept(&mut self, value: Value, now: Instant, config: &Config, metrics: &Metrics) -> bool {
        let same = self.value.as_ref() == Some(&value);
        if config.processing.only_on_change && same {
            let due = config.processing.heartbeat_interval > 0
                && self.emitted.is_some_and(|t| {
                    now.saturating_duration_since(t)
                        >= Duration::from_secs(config.processing.heartbeat_interval)
                });
            if !due {
                metrics::inc(&metrics.values_unchanged);
                return false;
            }
            metrics::inc(&metrics.heartbeats);
        }
        self.value = Some(value);
        self.emitted = Some(now);
        true
    }
}
struct WsState {
    route: Route,
    last: Last,
}
struct GrabState {
    control: PollControl,
    fields: HashMap<String, Last>,
    seen: Option<Instant>,
}
pub struct Engine {
    config: Config,
    metrics: Arc<Metrics>,
    controls: Arc<Controls>,
    websocket: UuidMap<WsState>,
    grabber: UuidMap<GrabState>,
}
impl Engine {
    pub fn new(config: Config, metrics: Arc<Metrics>, controls: Arc<Controls>) -> Self {
        let mut out = Self {
            config,
            metrics,
            controls: controls.clone(),
            websocket: UuidMap::default(),
            grabber: UuidMap::default(),
        };
        out.refresh(controls);
        out
    }
    pub fn refresh(&mut self, controls: Arc<Controls>) {
        self.controls = controls;
        // With change detection disabled no observation history is needed.
        // The immutable discovery snapshot already provides all routing data.
        if !self.config.processing.only_on_change {
            return;
        }
        let controls = &self.controls;
        let mut old_ws = std::mem::take(&mut self.websocket);
        self.websocket = controls
            .websocket
            .iter()
            .map(|(id, route)| {
                let last = old_ws
                    .remove(id)
                    .filter(|s| s.route.prefix == route.prefix && s.route.field == route.field)
                    .map(|s| s.last)
                    .unwrap_or_default();
                (
                    *id,
                    WsState {
                        route: route.clone(),
                        last,
                    },
                )
            })
            .collect();
        let mut old_grab = std::mem::take(&mut self.grabber);
        self.grabber = controls
            .grabber
            .iter()
            .map(|(id, c)| {
                let (fields, seen) = old_grab
                    .remove(id)
                    .filter(|s| s.control.prefix == c.prefix)
                    .map(|s| (s.fields, s.seen))
                    .unwrap_or_default();
                (
                    *id,
                    GrabState {
                        control: c.clone(),
                        fields,
                        seen,
                    },
                )
            })
            .collect();
    }
    pub fn process(&mut self, input: Input, batch: &mut Batch) -> Result<()> {
        match input {
            Input::Number {
                uuid,
                value,
                timestamp,
                received,
            } => self.websocket_value(
                uuid,
                value::number(value, &self.config.general),
                timestamp,
                received,
                batch,
            ),
            Input::Text {
                uuid,
                value,
                timestamp,
                received,
            } => {
                let normalized =
                    match value::text_bytes(value, &self.config.general, &self.config.processing) {
                        Ok(value) => value,
                        Err(error) => {
                            if let Some(state) = self.websocket.get_mut(&uuid) {
                                state.last = Last::default();
                            }
                            metrics::inc(&self.metrics.values_invalid);
                            return Err(error).context("WebSocket text is not UTF-8");
                        }
                    };
                self.websocket_value(uuid, normalized, timestamp, received, batch);
            }
            Input::Grabber {
                uuid,
                payload,
                timestamp,
                received,
            } => self.grabber_value(uuid, &payload, timestamp, received, batch)?,
            Input::ParsedGrabber {
                uuid,
                fields,
                timestamp,
                received,
            } => self.grabber_fields(uuid, fields, timestamp, received, batch),
            Input::GrabberFailed { uuid } => self.invalidate_grabber(uuid),
            Input::Refresh(controls) => self.refresh(controls),
            Input::SessionReset => self.invalidate_observations(),
        }
        Ok(())
    }
    fn websocket_value(
        &mut self,
        uuid: LoxoneUuid,
        value: Option<Value>,
        timestamp: u64,
        now: Instant,
        batch: &mut Batch,
    ) {
        if !self.config.processing.only_on_change {
            let Some(route) = self.controls.websocket.get(&uuid) else {
                metrics::inc(&self.metrics.values_filtered);
                return;
            };
            if let Some(value) = value {
                emit_ws_value(route, &value, timestamp, batch);
            } else {
                metrics::inc(&self.metrics.values_invalid);
            }
            return;
        }
        let Some(state) = self.websocket.get_mut(&uuid) else {
            metrics::inc(&self.metrics.values_filtered);
            return;
        };
        let Some(value) = value else {
            metrics::inc(&self.metrics.values_invalid);
            // Invalid is a new observation, not permission to keep advertising
            // the last good reading as fresh through heartbeat/dedup.
            state.last = Last::default();
            return;
        };
        if state.last.accept(value, now, &self.config, &self.metrics) {
            emit_ws(state, timestamp, batch);
        }
    }
    fn grabber_value(
        &mut self,
        uuid: LoxoneUuid,
        payload: &[u8],
        timestamp: u64,
        now: Instant,
        batch: &mut Batch,
    ) -> Result<()> {
        if !self.controls.grabber.contains_key(&uuid) {
            metrics::inc(&self.metrics.values_filtered);
            return Ok(());
        }
        let fields = match parse_grabber(payload, &self.config, &self.metrics) {
            Ok(fields) => fields,
            Err(error) => {
                self.invalidate_grabber(uuid);
                return Err(error);
            }
        };
        self.grabber_fields(uuid, fields, timestamp, now, batch);
        Ok(())
    }
    fn invalidate_grabber(&mut self, uuid: LoxoneUuid) {
        if let Some(state) = self.grabber.get_mut(&uuid) {
            state.fields.clear();
            state.seen = None;
        }
    }
    fn grabber_fields(
        &mut self,
        uuid: LoxoneUuid,
        fields: BTreeMap<String, Value>,
        timestamp: u64,
        now: Instant,
        batch: &mut Batch,
    ) {
        if !self.config.processing.only_on_change {
            let Some(control) = self.controls.grabber.get(&uuid) else {
                metrics::inc(&self.metrics.values_filtered);
                return;
            };
            if fields.is_empty() {
                return;
            }
            batch.data.push_str(&control.prefix);
            for (i, (key, value)) in fields.into_iter().enumerate() {
                if i > 0 {
                    batch.data.push(',');
                }
                emit_field(
                    &key,
                    &value,
                    self.config.processing.separate_text_fields,
                    batch,
                );
            }
            protocol::finish(&mut batch.data, timestamp);
            batch.points += 1;
            return;
        }
        let Some(state) = self.grabber.get_mut(&uuid) else {
            metrics::inc(&self.metrics.values_filtered);
            return;
        };
        // /all is a complete snapshot. Fields omitted or reported invalid must
        // also disappear from the heartbeat cache (legacy Point-poisoning fix).
        state.fields.retain(|key, _| fields.contains_key(key));
        state.seen = Some(now);
        let mut wrote = false;
        for (key, value) in fields {
            if !state.fields.contains_key(key.as_str()) {
                state.fields.insert(key.clone(), Last::default());
            }
            let last = state.fields.get_mut(key.as_str()).unwrap();
            if !last.accept(value, now, &self.config, &self.metrics) {
                continue;
            }
            if !wrote {
                batch.data.push_str(&state.control.prefix);
                wrote = true;
            } else {
                batch.data.push(',');
            }
            let value = last.value.as_ref().unwrap();
            emit_field(
                &key,
                value,
                self.config.processing.separate_text_fields,
                batch,
            );
        }
        if wrote {
            protocol::finish(&mut batch.data, timestamp);
            batch.points += 1;
        }
    }
    /// Heartbeat only while the Miniserver session is healthy. Polling fields
    /// additionally require a recent successful observation; stale cached
    /// readings are never made fresh during a failed grabber cycle.
    pub fn heartbeat(&mut self, now: Instant, timestamp: u64, batch: &mut Batch) {
        let p = &self.config.processing;
        if !p.only_on_change
            || p.heartbeat_interval == 0
            || !self.metrics.websocket_connected.load(Ordering::Relaxed)
        {
            return;
        }
        let interval = Duration::from_secs(p.heartbeat_interval);
        for state in self.websocket.values_mut() {
            if state
                .last
                .emitted
                .is_some_and(|t| now.saturating_duration_since(t) >= interval)
            {
                emit_ws(state, timestamp, batch);
                state.last.emitted = Some(now);
                metrics::inc(&self.metrics.heartbeats);
            }
        }
        let fresh = Duration::from_secs(
            self.config
                .general
                .grabber_interval
                .saturating_mul(2)
                .saturating_add(self.config.miniserver.request_timeout),
        );
        for state in self.grabber.values_mut() {
            if !state
                .seen
                .is_some_and(|t| now.saturating_duration_since(t) <= fresh)
            {
                continue;
            }
            let mut wrote = false;
            for (key, last) in &mut state.fields {
                if last
                    .emitted
                    .is_some_and(|t| now.saturating_duration_since(t) >= interval)
                {
                    if !wrote {
                        batch.data.push_str(&state.control.prefix);
                        wrote = true;
                    } else {
                        batch.data.push(',');
                    }
                    let value = last.value.as_ref().unwrap();
                    protocol::append_field_key(
                        &mut batch.data,
                        key,
                        matches!(value, Value::Text(_)),
                        p.separate_text_fields,
                    );
                    batch.data.push('=');
                    protocol::append_value(&mut batch.data, value);
                    batch.fields += 1;
                    last.emitted = Some(now);
                    metrics::inc(&self.metrics.heartbeats);
                }
            }
            if wrote {
                protocol::finish(&mut batch.data, timestamp);
                batch.points += 1;
            }
        }
    }
    pub fn invalidate_observations(&mut self) {
        // Called across reconnections: a prior session's unchanged readings
        // cannot be assumed to describe the new program/current device state.
        for state in self.websocket.values_mut() {
            state.last = Last::default();
        }
        for state in self.grabber.values_mut() {
            state.fields.clear();
            state.seen = None;
        }
    }
}
fn emit_ws(state: &WsState, timestamp: u64, batch: &mut Batch) {
    let Some(value) = state.last.value.as_ref() else {
        return;
    };
    emit_ws_value(&state.route, value, timestamp, batch);
}
fn emit_field(key: &str, value: &Value, separate: bool, batch: &mut Batch) {
    protocol::append_field_key(
        &mut batch.data,
        key,
        matches!(value, Value::Text(_)),
        separate,
    );
    batch.data.push('=');
    protocol::append_value(&mut batch.data, value);
    batch.fields += 1;
}
fn emit_ws_value(route: &Route, value: &Value, timestamp: u64, batch: &mut Batch) {
    batch.data.push_str(&route.prefix);
    batch.data.push_str(if matches!(value, Value::Text(_)) {
        &route.text_key
    } else {
        &route.numeric_key
    });
    batch.data.push('=');
    protocol::append_value(&mut batch.data, value);
    protocol::finish(&mut batch.data, timestamp);
    batch.points += 1;
    batch.fields += 1;
}

pub fn parse_grabber(
    payload: &[u8],
    config: &Config,
    metrics: &Metrics,
) -> Result<BTreeMap<String, Value>> {
    crate::grabber::parse(payload, config, metrics, false, None)
}

// Kept only as a differential oracle for the selective parser. It exercises
// the former production DOM route, including duplicate keys and field ordering.
#[cfg(test)]
fn parse_grabber_reference(
    payload: &[u8],
    config: &Config,
    metrics: &Metrics,
) -> Result<BTreeMap<String, Value>> {
    let document: serde_json::Value =
        serde_json::from_slice(payload).context("Invalid grabber JSON")?;
    let ll = document.get("LL").unwrap_or(&document);
    crate::miniserver::check_status(ll)?;
    let mut out = BTreeMap::new();
    if let Some(v) = ll.get("value") {
        insert_field("Default".into(), v, &mut out, config, metrics);
    }
    if let Some(obj) = ll.as_object() {
        for (key, output) in obj {
            if !key.starts_with("output") {
                continue;
            }
            if let Some(v) = output.get("value") {
                let name = output
                    .get("name")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .or_else(|| {
                        output.get("nr").filter(|n| !n.is_null()).map(|n| {
                            n.as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| n.to_string())
                        })
                    })
                    .unwrap_or_else(|| "Subdefault".into());
                insert_field(name, v, &mut out, config, metrics);
            }
        }
    }
    if config.processing.drop_redundant_default
        && let Some(Value::Number(n)) = out.get("Default")
        && out
            .iter()
            .any(|(k, v)| k != "Default" && v == &Value::Number(*n))
    {
        out.remove("Default");
        metrics::inc(&metrics.values_filtered);
    }
    Ok(out)
}
#[cfg(test)]
fn insert_field(
    key: String,
    raw: &serde_json::Value,
    out: &mut BTreeMap<String, Value>,
    config: &Config,
    metrics: &Metrics,
) {
    metrics::inc(&metrics.values_received);
    if let Some(v) = value::json(raw, &config.general, &config.processing) {
        out.insert(key, v);
    } else {
        metrics::inc(&metrics.values_invalid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn real_miniserver_corpus_preserves_valid_responses_and_recovers_status_text() {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/miniserver_real");
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
        let config = Config::default();
        for case in manifest.as_array().unwrap() {
            let payload = std::fs::read(dir.join(case["file"].as_str().unwrap())).unwrap();
            let metrics = Metrics::default();
            let parsed = parse_grabber(&payload, &config, &metrics).unwrap();
            if case["strict_json_valid"] == true {
                assert_eq!(
                    parsed,
                    parse_grabber_reference(&payload, &config, &Metrics::default()).unwrap()
                );
                assert_eq!(metrics::read(&metrics.grabber_json_repaired), 0);
            } else {
                assert_eq!(
                    parsed.get("Status"),
                    Some(&Value::Text(
                        r#"{"cmd":"wwcircmode","data":"off","id":158}"#.into()
                    ))
                );
                assert_eq!(parsed.get("Default"), Some(&Value::Number(1924984032.0)));
                assert_eq!(metrics::read(&metrics.grabber_json_repaired), 1);
            }
        }
    }
    const UUID: &str = "17a05a15-0243-d451-ffff68510c67e724";
    fn setup(change: bool) -> (Engine, Arc<Metrics>, LoxoneUuid) {
        let mut config = Config::default();
        config.processing.only_on_change = change;
        config.processing.heartbeat_interval = 60;
        let xml = format!(r#"<Root><C Type="Sensor" U="{UUID}" Title="Temp"/></Root>"#);
        let controls = crate::controls::parse(xml.as_bytes(), &config).unwrap();
        let metrics = Arc::new(Metrics::default());
        metrics.websocket_connected.store(true, Ordering::Relaxed);
        (
            Engine::new(config, metrics.clone(), Arc::new(controls)),
            metrics,
            LoxoneUuid::parse(UUID).unwrap(),
        )
    }
    #[test]
    fn refresh_preserves_same_routes_and_resets_changed_routes_in_both_modes() {
        for dedup in [false, true] {
            let (mut engine, _, uuid) = setup(dedup);
            let now = Instant::now();
            let mut batch = Batch::default();
            let number = || Input::Number {
                uuid,
                value: 21.2,
                timestamp: 42,
                received: now,
            };
            engine.process(number(), &mut batch).unwrap();
            batch.clear();
            for (title, expected) in [("Temp", !dedup), ("Renamed", true)] {
                let xml = format!(r#"<Root><C Type="Sensor" U="{UUID}" Title="{title}"/></Root>"#);
                let controls =
                    Arc::new(crate::controls::parse(xml.as_bytes(), &engine.config).unwrap());
                engine
                    .process(Input::Refresh(controls), &mut batch)
                    .unwrap();
                engine.process(number(), &mut batch).unwrap();
                assert_eq!(batch.points, u64::from(expected));
                if expected {
                    assert!(batch.data.starts_with(title));
                }
                batch.clear();
            }
            engine.process(Input::SessionReset, &mut batch).unwrap();
            engine.process(number(), &mut batch).unwrap();
            assert_eq!(batch.points, 1);
            batch.clear();
            engine
                .process(Input::Refresh(Arc::new(Controls::default())), &mut batch)
                .unwrap();
            engine.process(number(), &mut batch).unwrap();
            assert_eq!(batch.points, 0);
        }
    }
    #[test]
    fn unchanged_poll_refreshes_snapshot_freshness_before_heartbeat() {
        let (mut engine, _, uuid) = setup(true);
        engine.config.general.grabber_interval = 1;
        engine.config.miniserver.request_timeout = 1;
        let now = Instant::now();
        let mut batch = Batch::default();
        for seconds in [0, 59] {
            engine
                .process(
                    Input::ParsedGrabber {
                        uuid,
                        fields: BTreeMap::from([("Default".into(), Value::Number(21.2))]),
                        timestamp: 42,
                        received: now + Duration::from_secs(seconds),
                    },
                    &mut batch,
                )
                .unwrap();
            assert_eq!(batch.points, u64::from(seconds == 0));
            batch.clear();
        }
        engine.heartbeat(now + Duration::from_secs(60), 43, &mut batch);
        assert_eq!(batch.points, 1);
        batch.clear();
        engine.heartbeat(now + Duration::from_secs(120), 44, &mut batch);
        assert_eq!(batch.points, 0);
    }
    #[test]
    fn ws_precision_is_roundtrip_and_same_value_deduplicates() {
        let (mut e, m, uuid) = setup(true);
        let mut b = Batch::default();
        let now = Instant::now();
        for _ in 0..2 {
            e.process(
                Input::Number {
                    uuid,
                    value: 21.23456789123456,
                    timestamp: 42,
                    received: now,
                },
                &mut b,
            )
            .unwrap();
        }
        assert_eq!(b.points, 1);
        assert!(b.data.ends_with("Default=21.23456789123456 42\n"));
        assert_eq!(metrics::read(&m.values_unchanged), 1);
        b.clear();
        e.heartbeat(now + Duration::from_secs(60), 43, &mut b);
        assert_eq!(b.points, 1);
        m.websocket_connected.store(false, Ordering::Relaxed);
        b.clear();
        e.heartbeat(now + Duration::from_secs(120), 44, &mut b);
        assert_eq!(b.points, 0);
    }
    #[test]
    fn grabber_handles_sparse_outputs_and_never_accumulates_old_fields() {
        let (mut e, _, uuid) = setup(false);
        let mut b = Batch::default();
        let now = Instant::now();
        let payload=Bytes::from_static(r#"{"LL":{"Code":"200","value":"21.2°","output7":{"name":"AQ","value":21.234},"output8":{"name":"API","value":""}}}"#.as_bytes());
        e.process(
            Input::Grabber {
                uuid,
                payload,
                timestamp: 42,
                received: now,
            },
            &mut b,
        )
        .unwrap();
        assert!(b.data.ends_with("AQ=21.234,Default=21.2 42\n"));
        b.clear();
        e.process(
            Input::Grabber {
                uuid,
                payload: Bytes::from_static(br#"{"LL":{"value":"message"}}"#),
                timestamp: 43,
                received: now,
            },
            &mut b,
        )
        .unwrap();
        assert!(b.data.ends_with("__text__Default=\"message\" 43\n"));
        assert!(!b.data.contains("AQ="));
    }
    #[test]
    fn default_is_preserved_if_raw_output_has_different_precision() {
        let mut c = Config::default();
        c.processing.drop_redundant_default = true;
        let m = Metrics::default();
        let f = parse_grabber(
            r#"{"LL":{"value":"21.2°","output0":{"name":"AQ","value":21.234}}}"#.as_bytes(),
            &c,
            &m,
        )
        .unwrap();
        assert_eq!(f.len(), 2);
    }
    #[test]
    fn polluted_types_and_invalid_markers_never_enter_numeric_fields() {
        let c = Config::default();
        let m = Metrics::default();
        let f = parse_grabber(
            br#"{"LL":{"value":"<v.col>","output0":{"name":"AQ","value":9.4}}}"#,
            &c,
            &m,
        )
        .unwrap();
        assert_eq!(f.len(), 1);
        assert_eq!(f["AQ"], Value::Number(9.4));
    }
    #[test]
    fn stale_poll_values_do_not_receive_heartbeats() {
        let (mut e, _, uuid) = setup(true);
        let now = Instant::now();
        let mut b = Batch::default();
        e.process(
            Input::Grabber {
                uuid,
                payload: Bytes::from_static(br#"{"LL":{"value":4.2}}"#),
                timestamp: 42,
                received: now,
            },
            &mut b,
        )
        .unwrap();
        b.clear();
        e.heartbeat(now + Duration::from_secs(1000), 43, &mut b);
        assert_eq!(b.points, 0);
    }
    #[test]
    fn heartbeat_does_not_resurrect_removed_or_invalid_grabber_fields() {
        for next in [
            br#"{"LL":{"value":4.2}}"#.as_slice(),
            br#"{"LL":{"value":4.2,"output0":{"name":"AQ","value":"<v.i>"}}}"#,
            br#"{"LL":{"value":4.2,"output0":{"name":"AQ","value":null}}}"#,
        ] {
            let (mut engine, _, uuid) = setup(true);
            let now = Instant::now();
            let mut batch = Batch::default();
            for (offset, payload) in [
                (
                    0,
                    br#"{"LL":{"value":4.2,"output0":{"name":"AQ","value":9.4}}}"#.as_slice(),
                ),
                (1, next),
            ] {
                engine
                    .process(
                        Input::Grabber {
                            uuid,
                            payload: Bytes::copy_from_slice(payload),
                            timestamp: 42 + offset,
                            received: now + Duration::from_secs(offset),
                        },
                        &mut batch,
                    )
                    .unwrap();
            }
            batch.clear();
            engine.heartbeat(now + Duration::from_secs(60), 100, &mut batch);
            assert!(batch.data.ends_with("Default=4.2 100\n"));
            assert!(
                !batch.data.contains("AQ="),
                "stale field revived: {}",
                batch.data
            );
        }
    }
    #[test]
    fn invalid_websocket_value_clears_heartbeat_and_dedup_observation() {
        for invalid in [None, Some(Bytes::from_static(b"<v.i>"))] {
            let (mut engine, _, uuid) = setup(true);
            let now = Instant::now();
            let mut batch = Batch::default();
            engine
                .process(
                    Input::Number {
                        uuid,
                        value: 9.4,
                        timestamp: 1,
                        received: now,
                    },
                    &mut batch,
                )
                .unwrap();
            let event = match invalid {
                Some(value) => Input::Text {
                    uuid,
                    value,
                    timestamp: 2,
                    received: now,
                },
                None => Input::Number {
                    uuid,
                    value: f64::NAN,
                    timestamp: 2,
                    received: now,
                },
            };
            engine.process(event, &mut batch).unwrap();
            batch.clear();
            engine.heartbeat(now + Duration::from_secs(60), 3, &mut batch);
            assert_eq!(
                batch.points, 0,
                "invalid reading revived as a fresh heartbeat"
            );
            engine
                .process(
                    Input::Number {
                        uuid,
                        value: 9.4,
                        timestamp: 4,
                        received: now + Duration::from_secs(61),
                    },
                    &mut batch,
                )
                .unwrap();
            assert_eq!(
                batch.points, 1,
                "recovered reading must not be deduplicated"
            );
        }
    }
    #[test]
    fn selective_json_matches_dom_for_layout_changes_escapes_duplicates_and_errors() {
        let corpus = [
            r#"{"value":1,"LL":{"value":2},"value":3,"Code":403}"#,
            r#"{"LL":{"LL":{"value":999},"value":2},"output0":{"value":3}}"#,
            r#"{"LL":{"value":1},"value":"bad\uD800"}"#,
            r#"{"LL":{"value":1},"LL":null,"value":3}"#,
            r#"{"LL":{"Code":"\u003200","value":1}}"#,
            r#"{"LL":{"Code":200.0,"value":1}}"#,
            r#"{"LL":{"Code":2e2,"value":1}}"#,
            r#"{"LL":{"Code":-200,"value":1}}"#,
            r#"{"LL":{"Code":true,"value":1}}"#,
            r#"{"LL":{"Code":[200],"value":1}}"#,
            r#"{"LL":{"Code":{"value":200},"value":1}}"#,
            r#"{"LL":{"Code":"200","value":9.4,"output0":{"name":"AQ","value":21.234}}}"#,
            r#"{"LL":{"output2":{"value":2,"name":"same"},"value":1,"output10":{"name":"same","value":10}}}"#,
            r#"{"LL":{"value":"21.2°C","output7":{"name":"A\u0051","value":9.4},"unused":[null,{"x":"escaped\ntext"}]}}"#,
            r#"{"LL":{"value":1,"value":2,"output0":{"name":"old","name":"new","value":1,"value":3},"output0":{"nr":0,"value":4}}}"#,
            r#"{"LL":{"value":1},"LL":{"value":2}}"#,
            r#"{"value":true,"output0":{"nr":false,"value":false},"output1":{"nr":[1,2],"value":"text"}}"#,
            r#"{"LL":{"Code":"403","code":"200","value":42}}"#,
            r#"{"LL":{"Code":null,"code":200,"value":42}}"#,
            r#"{"LL":{"code":200,"value":null,"output0":{"name":false,"value":{}},"output9":{"nr":null,"value":[]}}}"#,
            r#"{"LL":{"value":"<v.i>","output0":{"name":"","value":""},"output1":false,"output2":{"name":"missing"}}}"#,
            r#"{"LL":{"value":1e400}}"#,
            r#"{"LL":{"value":-0,"output0":{"value":18446744073709551616}}}"#,
            r#"{"LL":{"value":"line\nwith\tquotes\""}}"#,
            r#"{"LL":{"value":1}} trailing"#,
            r#"{"LL":{"value":1,"output0":{"value":2,}}}"#,
            r#"{"LL":{"value":01}}"#,
            r#"{"LL":{"value":1},"unused":"bad\x"}"#,
            r#"{"LL":null}"#,
            r#"{"LL":[]} 没"#,
            r#"[1,2,3]"#,
            "null",
            "42",
            r#""text""#,
        ];
        for round in [false, true] {
            let mut config = Config::default();
            config.general.round_floats = round;
            for payload in corpus {
                let old_metrics = Metrics::default();
                let new_metrics = Metrics::default();
                let old = parse_grabber_reference(payload.as_bytes(), &config, &old_metrics);
                let new = parse_grabber(payload.as_bytes(), &config, &new_metrics);
                assert_eq!(
                    old.is_ok(),
                    new.is_ok(),
                    "{payload}: old={old:?} new={new:?}"
                );
                if let (Ok(old), Ok(new)) = (old, new) {
                    assert_eq!(old, new, "{payload}");
                }
                assert_eq!(
                    metrics::read(&old_metrics.values_received),
                    metrics::read(&new_metrics.values_received),
                    "{payload}"
                );
                assert_eq!(
                    metrics::read(&old_metrics.values_invalid),
                    metrics::read(&new_metrics.values_invalid),
                    "{payload}"
                );
            }
        }
    }
    #[test]
    fn selective_json_preserves_ten_thousand_f64_bit_patterns() {
        let config = Config::default();
        let metrics = Metrics::default();
        let mut bits = 0x1234_5678_9abc_def0u64;
        for _ in 0..10_000 {
            bits ^= bits << 13;
            bits ^= bits >> 7;
            bits ^= bits << 17;
            let n = f64::from_bits(bits);
            if !n.is_finite() || n == 0.0 {
                continue;
            }
            let decimal = ryu::Buffer::new().format_finite(n).to_owned();
            let payload = format!(
                r#"{{"LL":{{"value":{decimal},"output0":{{"name":"AQ","value":{decimal}}}}}}}"#
            );
            let parsed = parse_grabber(payload.as_bytes(), &config, &metrics).unwrap();
            for key in ["Default", "AQ"] {
                let Value::Number(actual) = parsed[key] else {
                    panic!("missing number")
                };
                assert_eq!(actual.to_bits(), bits, "{key}: {decimal}");
            }
        }
    }
    #[test]
    fn selective_json_validates_ignored_content_like_the_dom() {
        let mut corpus = vec![
            br#"{"LL":{"value":1,"ignored":1e400}}"#.to_vec(),
            br#"{"LL":{"value":1,"ignored":"\uD800"}}"#.to_vec(),
            b"{\"LL\":{\"value\":1,\"ignored\":\"\xff\"}}".to_vec(),
            br#"{"LL":{"value":{"ignored":1e400}}}"#.to_vec(),
            br#"{"LL":{"output0":[{"bad":"\uDC00"}]}}"#.to_vec(),
        ];
        corpus.push(
            format!(
                r#"{{"LL":{{"value":1,"ignored":{}0{}}}}}"#,
                "[".repeat(150),
                "]".repeat(150)
            )
            .into_bytes(),
        );
        for payload in corpus {
            let config = Config::default();
            let old_metrics = Metrics::default();
            let new_metrics = Metrics::default();
            assert!(parse_grabber_reference(&payload, &config, &old_metrics).is_err());
            assert!(
                parse_grabber(&payload, &config, &new_metrics).is_err(),
                "accepted {:?}",
                String::from_utf8_lossy(&payload)
            );
            assert_eq!(metrics::read(&new_metrics.values_received), 0);
        }
    }
    #[test]
    fn malformed_or_rejected_poll_clears_observations_and_allows_same_value_recovery() {
        for payload in [b"broken".as_slice(), br#"{"LL":{"Code":403,"value":1}}"#] {
            let (mut engine, _, uuid) = setup(true);
            let now = Instant::now();
            let mut batch = Batch::default();
            engine
                .process(
                    Input::Grabber {
                        uuid,
                        payload: Bytes::from_static(br#"{"LL":{"value":9.4}}"#),
                        timestamp: 1,
                        received: now,
                    },
                    &mut batch,
                )
                .unwrap();
            assert!(
                engine
                    .process(
                        Input::Grabber {
                            uuid,
                            payload: Bytes::copy_from_slice(payload),
                            timestamp: 2,
                            received: now
                        },
                        &mut batch
                    )
                    .is_err()
            );
            batch.clear();
            engine.heartbeat(now + Duration::from_secs(60), 3, &mut batch);
            assert_eq!(batch.points, 0, "failed poll revived the previous value");
            engine
                .process(
                    Input::Grabber {
                        uuid,
                        payload: Bytes::from_static(br#"{"LL":{"value":9.4}}"#),
                        timestamp: 4,
                        received: now + Duration::from_secs(61),
                    },
                    &mut batch,
                )
                .unwrap();
            assert_eq!(batch.points, 1, "recovery was suppressed as unchanged");
        }
    }
    #[test]
    fn non_utf8_websocket_text_cannot_panic_or_keep_a_stale_heartbeat_alive() {
        let (mut engine, metrics, uuid) = setup(true);
        let now = Instant::now();
        let mut batch = Batch::default();
        engine
            .process(
                Input::Number {
                    uuid,
                    value: 9.4,
                    timestamp: 1,
                    received: now,
                },
                &mut batch,
            )
            .unwrap();
        let error = engine.process(
            Input::Text {
                uuid,
                value: Bytes::from_static(b"\xffbad"),
                timestamp: 2,
                received: now,
            },
            &mut batch,
        );
        assert!(error.is_err());
        assert_eq!(metrics::read(&metrics.values_invalid), 1);
        batch.clear();
        engine.heartbeat(now + Duration::from_secs(60), 3, &mut batch);
        assert_eq!(batch.points, 0);
    }
}
