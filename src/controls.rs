use crate::{
    config::{Config, normalize_uuid},
    protocol,
};
use anyhow::{Result, ensure};
use loxwebsocket::{LoxoneUuid, UuidMap};
use quick_xml::{
    Reader,
    events::{BytesStart, Event},
};
use regex::Regex;
use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, LazyLock},
};

#[derive(Clone, Debug)]
pub struct Route {
    pub prefix: Arc<str>,
    keys: Arc<FieldKeys>,
}
/// One shared set per distinct field, rather than three fat pointers per route.
#[derive(Debug)]
pub struct FieldKeys {
    pub field: Arc<str>,
    pub numeric_key: Arc<str>,
    pub text_key: Arc<str>,
}
impl std::ops::Deref for Route {
    type Target = FieldKeys;
    fn deref(&self) -> &FieldKeys {
        &self.keys
    }
}
#[derive(Clone, Debug)]
pub struct PollControl {
    pub uuid: LoxoneUuid,
    pub uuid_text: String,
    pub secured: bool,
    pub prefix: Arc<str>,
}
#[derive(Clone, Default, Debug)]
pub struct Controls {
    pub websocket: UuidMap<Route>,
    pub grabber: UuidMap<PollControl>,
    pub total: usize,
}

#[derive(Default)]
struct RawControl<'a> {
    attrs: HashMap<&'static str, Cow<'a, str>>,
    io: HashMap<&'static str, Cow<'a, str>>,
    seen_io: bool,
    seen_display: bool,
    unit: String,
    children: Vec<(String, String)>,
}
fn attrs<'a>(
    e: &BytesStart<'_>,
    xml: &'a str,
    wanted: &[&'static str],
) -> HashMap<&'static str, Cow<'a, str>> {
    let mut out = HashMap::new();
    // Real Loxone exports contain duplicate attributes: keep the first like
    // the legacy parser. Normalize XML attribute whitespace before unescaping.
    for a in e.attributes().with_checks(false).flatten() {
        let Some(&key) = wanted.iter().find(|key| key.as_bytes() == a.key.as_ref()) else {
            continue;
        };
        let std::collections::hash_map::Entry::Vacant(slot) = out.entry(key) else {
            continue; // First duplicate wins without decoding the later value.
        };
        // Most attributes need neither whitespace normalization nor unescaping.
        // Borrow while inspecting; allocate only the value retained by a route.
        // Reborrow through checked offsets into the source XML: attributes()
        // ties its borrow to the temporary event, while the source outlives all
        // RawControls. Owned/non-source event buffers safely fall back to a copy.
        let bytes = a.value.as_ref();
        let raw = bytes
            .as_ptr()
            .addr()
            .checked_sub(xml.as_ptr().addr())
            .and_then(|start| {
                start
                    .checked_add(bytes.len())
                    .and_then(|end| xml.get(start..end))
            })
            .map(Cow::Borrowed)
            .unwrap_or_else(|| Cow::Owned(String::from_utf8_lossy(bytes).into_owned()));
        let raw = if raw.contains(['\r', '\n', '\t']) {
            std::borrow::Cow::Owned(raw.replace("\r\n", " ").replace(['\r', '\n', '\t'], " "))
        } else {
            raw
        };
        let val = if raw.contains('&') {
            match quick_xml::escape::unescape(&raw) {
                Ok(decoded) => Cow::Owned(decoded.into_owned()),
                Err(_) => raw,
            }
        } else {
            raw
        };
        slot.insert(val);
    }
    out
}
fn get<'a>(attrs: &'a HashMap<&'static str, Cow<'_, str>>, k: &str) -> &'a str {
    attrs.get(k).map(Cow::as_ref).unwrap_or("")
}
fn flag(s: &str) -> bool {
    let s = s.trim();
    s == "1" || s.eq_ignore_ascii_case("true") || s.eq_ignore_ascii_case("yes")
}
fn strip_markup(s: &str) -> String {
    // Same non-greedy, non-DOTALL rule as Python's re.sub(r"<.*?>", "", s).
    // Unmatched angle brackets are literal units, not an instruction to discard
    // the rest of the string. Most units contain no markup at all.
    if !s.contains('<') {
        return s.trim().into();
    }
    static MARKUP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<.*?>").unwrap());
    let mut unit = MARKUP.replace_all(s, "").into_owned();
    // Keep the replacement buffer: trim() followed by into() would allocate
    // a second string for every formatted unit in a configuration export.
    unit.truncate(unit.trim_end().len());
    let leading = unit.len() - unit.trim_start().len();
    if leading > 0 {
        unit.drain(..leading);
    }
    unit
}

pub fn parse(xml: &[u8], config: &Config) -> Result<Controls> {
    let xml = String::from_utf8_lossy(xml);
    let mut reader = Reader::from_str(&xml);
    reader.config_mut().check_end_names = false;
    reader.config_mut().allow_unmatched_ends = true;
    reader.config_mut().check_comments = false;
    let mut records = Vec::<RawControl>::new();
    let mut stack = Vec::<(Vec<u8>, Option<usize>)>::with_capacity(32);
    let mut saw_root = false;
    loop {
        let event = reader.read_event();
        let empty = matches!(&event, Ok(Event::Empty(_)));
        match event {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                saw_root = true;
                let name = e.name();
                let parent = stack.last().and_then(|(_, idx)| *idx);
                let mut current = None;
                if name.as_ref() == b"C" {
                    let a = attrs(
                        &e,
                        &xml,
                        &["Type", "U", "Title", "Desc", "StatsType", "Analog", "linkC"],
                    );
                    if a.contains_key("Type") {
                        current = Some(records.len());
                        records.push(RawControl {
                            attrs: a,
                            ..Default::default()
                        });
                    }
                } else if let Some(idx) = parent {
                    match name.as_ref() {
                        b"IoData" if !records[idx].seen_io => {
                            records[idx].seen_io = true;
                            records[idx].io = attrs(&e, &xml, &["Cr", "Pr", "Visu", "VisuPwd"]);
                        }
                        b"Display" if !records[idx].seen_display => {
                            records[idx].seen_display = true;
                            records[idx].unit =
                                strip_markup(get(&attrs(&e, &xml, &["Unit"]), "Unit"));
                        }
                        b"Co" => {
                            let a = attrs(&e, &xml, &["U", "K"]);
                            records[idx]
                                .children
                                .push((normalize_uuid(get(&a, "U")), get(&a, "K").into()));
                        }
                        _ => {}
                    }
                }
                if !empty {
                    stack.push((name.as_ref().to_vec(), current));
                }
            }
            Ok(Event::End(e)) => {
                if let Some(at) = stack
                    .iter()
                    .rposition(|(n, _)| n.as_slice() == e.name().as_ref())
                {
                    stack.truncate(at);
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%error, "Recovering configuration XML parsed before malformed element");
                break;
            }
        }
    }
    ensure!(saw_root, "Configuration payload contains no XML document");
    let names: HashMap<_, _> = records
        .iter()
        .filter(|r| matches!(get(&r.attrs, "Type"), "Category" | "Place"))
        .map(|r| {
            (
                normalize_uuid(get(&r.attrs, "U")),
                get(&r.attrs, "Title").to_owned(),
            )
        })
        .collect();
    let links: HashSet<_> = records
        .iter()
        .filter(|r| {
            flag(get(&r.io, "Visu"))
                && !config
                    .filters
                    .type_blacklist
                    .contains(&get(&r.attrs, "Type").to_uppercase())
        })
        .flat_map(|r| get(&r.attrs, "linkC").split(',').map(normalize_uuid))
        .collect();
    let mut out = Controls::default();
    let mut field_keys = HashMap::<Arc<str>, Arc<FieldKeys>>::new();
    for r in records {
        let kind = get(&r.attrs, "Type").to_uppercase();
        if config.filters.type_blacklist.contains(&kind) {
            continue;
        }
        let uid = normalize_uuid(get(&r.attrs, "U"));
        let Ok(uuid) = LoxoneUuid::parse(&uid) else {
            continue;
        };
        let title = get(&r.attrs, "Title");
        if title.is_empty() {
            continue;
        }
        let mut tags = BTreeMap::from([
            ("name".into(), title.into()),
            ("description".into(), get(&r.attrs, "Desc").into()),
            ("uuid".into(), uid.clone()),
            (
                "statstype".into(),
                get(&r.attrs, "StatsType")
                    .parse::<i64>()
                    .unwrap_or(0)
                    .to_string(),
            ),
            (
                "analog".into(),
                if get(&r.attrs, "Analog") == "true" {
                    "1"
                } else {
                    "0"
                }
                .into(),
            ),
            ("type".into(), get(&r.attrs, "Type").into()),
            ("unit".into(), r.unit.clone()),
            (
                "category".into(),
                names
                    .get(&normalize_uuid(get(&r.io, "Cr")))
                    .cloned()
                    .unwrap_or_default(),
            ),
            (
                "room".into(),
                names
                    .get(&normalize_uuid(get(&r.io, "Pr")))
                    .cloned()
                    .unwrap_or_default(),
            ),
            ("visu".into(), get(&r.io, "Visu").into()),
            ("application".into(), "loxInFlux".into()),
        ]);
        out.total += 1;
        let visible = flag(get(&r.io, "Visu")) || links.contains(&uid);
        if !visible && kind != "VIRTUALTEXTIN" && config.filters.grabber.includes(&kind, &uid) {
            tags.insert("source".into(), "grabber".into());
            out.grabber.insert(
                uuid,
                PollControl {
                    uuid,
                    uuid_text: uid.clone(),
                    secured: flag(get(&r.io, "VisuPwd")),
                    prefix: protocol::prefix(title, &tags).into(),
                },
            );
        }
        tags.insert("source".into(), "websocket".into());
        // Permit unexpected WebSocket updates for known non-visualized controls,
        // but never bypass configured filters (the Python fallback did).
        if config.filters.websocket.includes(&kind, &uid) {
            out.websocket.insert(
                uuid,
                route(title, &tags, "Default", config, &mut field_keys),
            );
            for (child, key) in &r.children {
                let Ok(child_uuid) = LoxoneUuid::parse(child) else {
                    continue;
                };
                if !key.is_empty() && config.filters.websocket.includes(&kind, child) {
                    tags.insert("subuuid".into(), child.clone());
                    out.websocket.insert(
                        child_uuid,
                        route(title, &tags, key, config, &mut field_keys),
                    );
                }
                out.total += 1;
            }
        }
    }
    Ok(out)
}
fn route(
    title: &str,
    tags: &BTreeMap<String, String>,
    field: &str,
    c: &Config,
    keys: &mut HashMap<Arc<str>, Arc<FieldKeys>>,
) -> Route {
    if !keys.contains_key(field) {
        let field: Arc<str> = field.into();
        let numeric = protocol::field_key(&field, false, c.processing.separate_text_fields);
        let numeric: Arc<str> = if numeric == field.as_ref() {
            field.clone()
        } else {
            numeric.into()
        };
        let text = if c.processing.separate_text_fields {
            protocol::field_key(&field, true, true).into()
        } else {
            numeric.clone()
        };
        keys.insert(
            field.clone(),
            Arc::new(FieldKeys {
                field,
                numeric_key: numeric,
                text_key: text,
            }),
        );
    }
    Route {
        prefix: protocol::prefix(title, tags).into(),
        keys: keys.get(field).unwrap().clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const P: &str = "17a05a15-0243-d451-ffff68510c67e724";
    const S: &str = "17a05a15-0243-d451-ffff68510c67e725";
    #[test]
    fn direct_children_keep_the_first_even_when_empty_like_lxml_find() {
        for first in [
            r#"<IoData Visu="false" VisuPwd="true"/><Display Unit="first"/>"#,
            "<IoData/><Display/>",
        ] {
            let xml = format!(
                r#"<R><C Type="Sensor" U="{P}" Title="Test">{first}<IoData Visu="true"/><Display Unit="wrong"/></C></R>"#
            );
            let controls = parse(xml.as_bytes(), &Config::default()).unwrap();
            let id = LoxoneUuid::parse(P).unwrap();
            assert!(controls.grabber.contains_key(&id));
            assert_eq!(controls.grabber[&id].secured, first.contains("true"));
            assert!(!controls.grabber[&id].prefix.contains("wrong"));
        }
    }
    #[test]
    fn unit_markup_matches_legacy_non_greedy_regex() {
        for (input, expected) in [
            ("<v.1> °C", "°C"),
            ("m<unclosed", "m<unclosed"),
            ("Pa>m", "Pa>m"),
            ("<a\nb> °C", "<a\nb> °C"),
            ("<<v.1>> m", "> m"),
            ("a<x>b<y>c", "abc"),
        ] {
            assert_eq!(strip_markup(input), expected, "{input}");
        }
    }
    fn xml(visu: &str, kind: &str) -> String {
        format!(
            r#"<Root><C Type="Place" U="r" Title="Kitchen"/><C Type="{kind}" U="{P}" Title="Temp" Analog="true"><IoData Visu="{visu}" Pr="r"/><Display Unit="&lt;v.1&gt; °C"/><Co U="{S}" K="AQ"/></C></Root>"#
        )
    }
    #[test]
    fn metadata_fields_and_visibility_are_preserved() {
        let c = parse(xml("false", "Sensor").as_bytes(), &Config::default()).unwrap();
        assert_eq!(c.grabber.len(), 1);
        assert_eq!(c.websocket.len(), 2);
        let s = &c.websocket[&LoxoneUuid::parse(S).unwrap()];
        assert_eq!(&*s.field, "AQ");
        assert!(s.prefix.contains("room=Kitchen"));
        assert!(s.prefix.contains("unit=°C"));
        assert!(s.prefix.contains(&format!("uuid={P}")));
        assert!(s.prefix.contains(&format!("subuuid={S}")));
        assert_eq!(
            parse(xml("true", "Sensor").as_bytes(), &Config::default())
                .unwrap()
                .grabber
                .len(),
            0
        );
    }
    #[test]
    fn bom_does_not_change_self_closing_element_detection() {
        let raw = xml("true", "Sensor");
        let plain = parse(raw.as_bytes(), &Config::default()).unwrap();
        let bom = parse(format!("\u{feff}{raw}").as_bytes(), &Config::default()).unwrap();
        assert_eq!(plain.websocket.len(), bom.websocket.len());
        assert_eq!(plain.grabber.len(), bom.grabber.len());
        for (id, route) in plain.websocket {
            assert_eq!(route.prefix, bom.websocket[&id].prefix);
        }
    }
    #[test]
    fn filters_also_apply_to_dynamic_websocket_routes_and_children() {
        let c = Config::parse(&format!("[filters.websocket]\nuuid_blacklist=['{P}']")).unwrap();
        assert!(
            parse(xml("true", "Sensor").as_bytes(), &c)
                .unwrap()
                .websocket
                .is_empty()
        );
    }
    #[test]
    fn virtual_text_input_is_never_polled_because_all_would_write_to_it() {
        let c = parse(xml("false", "VirtualTextIn").as_bytes(), &Config::default()).unwrap();
        assert!(c.grabber.is_empty());
        assert_eq!(c.websocket.len(), 2);
    }
    #[test]
    fn duplicate_attributes_keep_first_and_truncated_xml_recovers() {
        let s = xml("false", "Sensor")
            .replace("Title=\"Temp\"", "Title=\"First\" Title=\"Second\"")
            .replace("</Root>", "");
        let c = parse(s.as_bytes(), &Config::default()).unwrap();
        assert!(c.websocket.values().all(|r| r.prefix.starts_with("First,")));
    }
    #[test]
    fn linked_control_promoted_after_all_controls_are_parsed() {
        let s = format!(
            r#"<Root><C Type="Sensor" U="{P}" Title="Parent" linkC="{S}"><IoData Visu="1"/></C><C Type="Sensor" U="{S}" Title="Child"/></Root>"#
        );
        assert!(
            parse(s.as_bytes(), &Config::default())
                .unwrap()
                .grabber
                .is_empty()
        );
    }
    #[test]
    fn missing_iodata_and_false_visu_password_never_enable_secured_polling() {
        let cfg = Config::default();
        let no_io = format!(r#"<Root><C Type="Sensor" U="{P}" Title="No IO"/></Root>"#);
        let controls = parse(no_io.as_bytes(), &cfg).unwrap();
        assert!(!controls.grabber[&LoxoneUuid::parse(P).unwrap()].secured);
        for (text, enabled) in [
            ("", false),
            ("false", false),
            ("0", false),
            ("no", false),
            ("1", true),
            ("true", true),
            (" YES ", true),
        ] {
            let raw = xml("false", "Sensor").replace(
                "Visu=\"false\"",
                &format!("Visu=\"false\" VisuPwd=\"{text}\""),
            );
            let controls = parse(raw.as_bytes(), &cfg).unwrap();
            assert_eq!(
                controls.grabber[&LoxoneUuid::parse(P).unwrap()].secured,
                enabled,
                "VisuPwd={text}"
            );
            assert_eq!(
                parse(xml(text, "Sensor").as_bytes(), &cfg)
                    .unwrap()
                    .grabber
                    .is_empty(),
                enabled,
                "Visu={text}"
            );
        }
    }
    #[test]
    fn selected_attributes_keep_xml_whitespace_entities_and_first_duplicate_semantics() {
        let raw = format!(
            "<R><C Type=\"Sensor\" Type=\"Other\" U=\"{P}\" Title=\"A\tB\r\nC&#10;D\" Ignored=\"&unknown;\"><IoData Visu=\"false\" Visu=\"true\"/></C></R>"
        );
        let controls = parse(raw.as_bytes(), &Config::default()).unwrap();
        assert_eq!(controls.grabber.len(), 1);
        let route = &controls.websocket[&LoxoneUuid::parse(P).unwrap()];
        assert!(route.prefix.contains("type=Sensor"));
        // Compare via the serializer so literal vs entity whitespace is checked
        // independently of line protocol's own escaping rules.
        let mut tags = std::collections::BTreeMap::new();
        tags.insert("name".into(), "A B C\nD".into());
        let expected = crate::protocol::prefix("A B C\nD", &tags);
        assert!(
            route
                .prefix
                .starts_with(expected.split(',').next().unwrap())
        );
    }
}
