//! Line protocol construction. Static tags are escaped once during discovery.
use crate::value::Value;
use std::collections::BTreeMap;

pub fn escape(s: &str, equals: bool) -> String {
    let mut out = String::with_capacity(s.len());
    append_escaped(&mut out, s, equals);
    out
}

fn append_escaped(out: &mut String, s: &str, equals: bool) {
    for c in s.chars() {
        match c {
            ' ' | ',' => {
                out.push('\\');
                out.push(c);
            }
            '=' if equals => out.push_str("\\="),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
}

pub fn prefix(measurement: &str, tags: &BTreeMap<String, String>) -> String {
    let mut out = escape(measurement, false);
    for (key, value) in tags {
        if !value.is_empty() {
            out.push(',');
            append_escaped(&mut out, key, true);
            out.push('=');
            append_escaped(&mut out, value, true);
            // Match influxdb-client's _escape_tag_value: without this guard
            // a final backslash can consume the following tag/field boundary.
            if value.ends_with('\\') {
                out.push(' ');
            }
        }
    }
    out.push(' ');
    out
}

pub fn field_key(name: &str, text: bool, separate: bool) -> String {
    let mut out = String::with_capacity(name.len() + 10);
    append_field_key(&mut out, name, text, separate);
    out
}

pub fn append_field_key(out: &mut String, name: &str, text: bool, separate: bool) {
    if separate && text {
        out.push_str("__text__");
    } else if separate && (name.starts_with("__text__") || name.starts_with("__number__")) {
        out.push_str("__number__");
    }
    append_escaped(out, name, true);
}

pub fn append_value(out: &mut String, value: &Value) {
    match value {
        Value::Number(n) => {
            out.push_str(ryu::Buffer::new().format_finite(*n));
        }
        Value::Text(s) => {
            out.push('"');
            for c in s.chars() {
                match c {
                    '\\' => out.push_str("\\\\"),
                    '"' => out.push_str("\\\""),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    c => out.push(c),
                }
            }
            out.push('"');
        }
    }
}
pub fn finish(out: &mut String, timestamp: u64) {
    out.push(' ');
    out.push_str(itoa::Buffer::new().format(timestamp));
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metadata_matches_legacy_writer_for_tabs_unicode_and_terminal_backslashes() {
        #[derive(serde::Deserialize)]
        struct Case {
            measurement: String,
            tags: BTreeMap<String, String>,
            field: String,
            line: String,
        }
        let cases: Vec<Case> =
            serde_json::from_str(include_str!("../tests/fixtures/legacy_line_protocol.json"))
                .unwrap();
        for case in cases {
            let mut actual = prefix(&case.measurement, &case.tags);
            actual.push_str(&field_key(&case.field, false, false));
            actual.push('=');
            append_value(&mut actual, &Value::Number(1.25));
            finish(&mut actual, 42);
            assert_eq!(actual, case.line);
        }
    }
    #[test]
    fn line_protocol_escaping_and_types() {
        let mut tags = BTreeMap::new();
        tags.insert("room".into(), "A, B=C".into());
        let mut out = prefix("Living room", &tags);
        out.push_str("AQ=");
        append_value(&mut out, &Value::Number(21.2));
        finish(&mut out, 42);
        assert_eq!(out, "Living\\ room,room=A\\,\\ B\\=C AQ=21.2 42\n");
        out.clear();
        append_value(&mut out, &Value::Text("a\"b\\c\nd".into()));
        assert_eq!(out, "\"a\\\"b\\\\c\\nd\"");
    }
    #[test]
    fn whole_numbers_are_never_influx_integers() {
        let mut out = String::new();
        append_value(&mut out, &Value::Number(42.0));
        assert_eq!(out, "42.0");
    }
    #[test]
    fn text_namespace_cannot_collide_with_numbers() {
        assert_eq!(field_key("Default", true, true), "__text__Default");
        assert_eq!(
            field_key("__text__Default", false, true),
            "__number____text__Default"
        );
        assert_eq!(
            field_key("__number____text__Default", false, true),
            "__number____number____text__Default"
        );
    }
}
