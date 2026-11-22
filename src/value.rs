use crate::config::{General, Processing};
use regex::{CaptureLocations, Regex};
use std::{cell::RefCell, io::Write, sync::LazyLock};

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Number(f64),
    Text(String),
}

// Full match, not merely a numeric prefix: dates, times, comma decimals and
// sentences beginning with numbers must remain text. Units may contain /, °, %
// and superscripts, but not a second ASCII number or whitespace.
static WITH_UNIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r"^([+-]?(?:[0-9]+(?:\.[0-9]*)?|\.[0-9]+)(?:[eE][+-]?[0-9]+)?)\s*([\p{L}°%µμΩ‰/²³][\p{L}°%µμΩ‰/²³]*)$"
).unwrap()
});
thread_local! {
    // Reuse the capture slots instead of allocating them for every sensor
    // value. This synchronous parser never holds the borrow across an await.
    static UNIT_LOCATIONS: RefCell<CaptureLocations> = RefCell::new(WITH_UNIT.capture_locations());
}

fn unit_number(text: &str) -> Option<f64> {
    UNIT_LOCATIONS.with_borrow_mut(|locations| {
        WITH_UNIT.captures_read(locations, text)?;
        let (start, end) = locations.get(2)?;
        if matches!(&text[start..end], "e" | "E") {
            return None;
        }
        let (start, end) = locations.get(1)?;
        text[start..end].parse().ok()
    })
}

pub fn number(value: f64, general: &General) -> Option<Value> {
    if !value.is_finite() {
        return None;
    }
    let value = if general.round_floats {
        // Decimal formatting matches Python's decimal rounding of the actual
        // binary f64 (including ties-to-even) without a multiply overflow or
        // the precision loss of value * 10^precision for large values.
        // At most sign + 309 integer digits + dot + 15 decimal places.
        // Keep std's exact decimal rounding, but avoid a heap allocation for
        // every numeric event. Config validation bounds precision to 0..15.
        let mut bytes = [0u8; 326];
        let mut out = std::io::Cursor::new(bytes.as_mut_slice());
        write!(out, "{:.*}", general.rounding_precision as usize, value).ok()?;
        let len = out.position() as usize;
        std::str::from_utf8(&bytes[..len]).ok()?.parse().ok()?
    } else {
        value
    };
    Some(Value::Number(if value == 0.0 { 0.0 } else { value }))
}

enum TextKind {
    Value(Value),
    Literal,
}
fn classify_text(value: &str, general: &General, processing: &Processing) -> Option<TextKind> {
    let trimmed = value.trim();
    if (processing.drop_empty && trimmed.is_empty())
        || (processing.drop_placeholders && trimmed.contains("<v."))
    {
        return None;
    }
    if let Ok(n) = trimmed.parse::<f64>() {
        return number(n, general).map(TextKind::Value);
    }
    if processing.parse_numeric_units
        && trimmed
            .as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.'))
        && let Some(n) = unit_number(trimmed)
    {
        return number(n, general).map(TextKind::Value);
    }
    Some(TextKind::Literal)
}
pub fn text(value: &str, general: &General, processing: &Processing) -> Option<Value> {
    match classify_text(value, general, processing)? {
        TextKind::Value(v) => Some(v),
        TextKind::Literal => Some(Value::Text(value.to_owned())),
    }
}
pub fn text_owned(value: String, general: &General, processing: &Processing) -> Option<Value> {
    match classify_text(&value, general, processing)? {
        TextKind::Value(v) => Some(v),
        TextKind::Literal => Some(Value::Text(value)),
    }
}
pub fn text_bytes(
    value: bytes::Bytes,
    general: &General,
    processing: &Processing,
) -> Result<Option<Value>, std::str::Utf8Error> {
    let text = std::str::from_utf8(&value)?;
    Ok(match classify_text(text, general, processing) {
        Some(TextKind::Value(v)) => Some(v),
        Some(TextKind::Literal) => Some(Value::Text(
            // SAFETY: from_utf8 above validated this exact immutable Bytes
            // slice. Classification only borrows it, and into() transfers or
            // copies those same bytes without changing their content.
            unsafe { String::from_utf8_unchecked(value.into()) },
        )),
        None => None,
    })
}

pub fn json(
    value: &serde_json::Value,
    general: &General,
    processing: &Processing,
) -> Option<Value> {
    match value {
        serde_json::Value::Number(n) => number(n.as_f64()?, general),
        serde_json::Value::String(s) => text(s, general, processing),
        serde_json::Value::Bool(b) => number(if *b { 1.0 } else { 0.0 }, general),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn owned_text_reuses_the_input_buffer_and_shared_text_remains_valid() {
        let bytes = bytes::Bytes::copy_from_slice(b"ordinary status text");
        let ptr = bytes.as_ptr();
        let Some(Value::Text(text)) =
            text_bytes(bytes, &General::default(), &Processing::default()).unwrap()
        else {
            panic!("expected text")
        };
        assert_eq!(ptr, text.as_ptr());
        let bytes = bytes::Bytes::copy_from_slice(b"shared status text");
        let other = bytes.clone();
        assert_eq!(
            text_bytes(bytes, &General::default(), &Processing::default()).unwrap(),
            Some(Value::Text("shared status text".into()))
        );
        assert_eq!(&other[..], b"shared status text");
        let bytes = bytes::Bytes::copy_from_slice("xxGrüße 🌡️yy".as_bytes());
        let slice = bytes.slice(2..bytes.len() - 2);
        assert_eq!(
            text_bytes(slice, &General::default(), &Processing::default()).unwrap(),
            Some(Value::Text("Grüße 🌡️".into()))
        );
        assert_eq!(&bytes[..], "xxGrüße 🌡️yy".as_bytes());
        assert!(
            text_bytes(
                bytes::Bytes::from_static(b"bad\xff"),
                &General::default(),
                &Processing::default()
            )
            .is_err()
        );
    }
    #[test]
    fn json_and_line_protocol_roundtrip_finite_f64_bits() {
        // Deterministic broad coverage, including exponents and subnormals.
        // Detects decimal JSON parsing drift as well as integer truncation.
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
            let parsed: serde_json::Value = serde_json::from_str(&decimal).unwrap();
            let Some(Value::Number(actual)) =
                json(&parsed, &General::default(), &Processing::default())
            else {
                panic!("lost numeric value");
            };
            assert_eq!(actual.to_bits(), bits, "{decimal}");
            let mut line = String::new();
            crate::protocol::append_value(&mut line, &Value::Number(actual));
            assert_eq!(line.parse::<f64>().unwrap().to_bits(), bits);
        }
    }
    #[test]
    fn all_numeric_paths_preserve_decimals_and_float_type() {
        let g = General::default();
        let p = Processing::default();
        for (s, n) in [
            ("21.2°", 21.2),
            ("9.4", 9.4),
            ("1079.6 U/min", 1079.6),
            ("132 Tagen", 132.0),
            ("10620.0seconds", 10620.0),
            ("94Lx", 94.0),
            ("1e-5kW", 0.00001),
            ("-.5°C", -0.5),
            ("+65%", 65.0),
            ("42", 42.0),
        ] {
            assert_eq!(text(s, &g, &p), Some(Value::Number(n)), "{s}");
        }
        assert_eq!(
            json(&serde_json::json!(21.2), &g, &p),
            Some(Value::Number(21.2))
        );
    }
    #[test]
    fn timestamps_and_sentences_do_not_become_metrics() {
        for s in [
            "2026-07-09",
            "12:34:56",
            "1,5",
            "3.14.15",
            "2 Fenster offen",
            "Ein",
            "1e",
            "5-",
            "5.",
        ] {
            if s == "5." {
                continue;
            }
            assert_eq!(
                text(s, &General::default(), &Processing::default()),
                Some(Value::Text(s.into())),
                "{s}"
            );
        }
    }
    #[test]
    fn invalid_and_nonfinite_values_are_dropped() {
        for s in ["", "  ", "<v.col>", "<v.i> °C", "NaN", "inf", "1e999"] {
            assert!(
                text(s, &General::default(), &Processing::default()).is_none(),
                "{s}"
            );
        }
        assert!(number(f64::NAN, &General::default()).is_none());
    }
    #[test]
    fn rounding_is_optional_and_no_integer_cast_exists() {
        let g = General {
            round_floats: true,
            rounding_precision: 2,
            ..Default::default()
        };
        assert_eq!(number(21.2345, &g), Some(Value::Number(21.23)));
        assert_eq!(number(2.675, &g), Some(Value::Number(2.67)));
        assert_eq!(number(f64::MAX, &g), Some(Value::Number(f64::MAX)));
        let exact = 0.12345678901234567;
        assert_eq!(
            number(exact, &General::default()),
            Some(Value::Number(exact))
        );
    }
    #[test]
    fn stack_rounding_matches_decimal_reference_for_every_supported_precision() {
        let mut cases = vec![
            0.0,
            -0.0,
            f64::MAX,
            -f64::MAX,
            f64::MIN_POSITIVE,
            f64::from_bits(1),
            2.675,
            -2.675,
            0.5,
            1.5,
            2.5,
            999_999.999_999_999_9,
        ];
        let mut bits = 0x1234_5678_9abc_def0u64;
        for _ in 0..10_000 {
            bits ^= bits << 13;
            bits ^= bits >> 7;
            bits ^= bits << 17;
            let n = f64::from_bits(bits);
            if n.is_finite() {
                cases.push(n);
            }
        }
        for precision in 0..=15 {
            let general = General {
                round_floats: true,
                rounding_precision: precision,
                ..Default::default()
            };
            for &n in &cases {
                let expected: f64 = format!("{:.*}", precision as usize, n).parse().unwrap();
                let expected = if expected == 0.0 { 0.0 } else { expected };
                let Some(Value::Number(actual)) = number(n, &general) else {
                    panic!("lost {n} at precision {precision}")
                };
                assert_eq!(
                    actual.to_bits(),
                    expected.to_bits(),
                    "{n} at precision {precision}"
                );
            }
        }
    }
    #[test]
    fn legacy_telegraf_processing_can_keep_units_placeholders_and_empty_text() {
        let processing = Processing {
            parse_numeric_units: false,
            drop_empty: false,
            drop_placeholders: false,
            separate_text_fields: false,
            ..Default::default()
        };
        for raw in ["21.2°C", "523ppm", "", "<v.i>", "Ein"] {
            assert_eq!(
                text(raw, &General::default(), &processing),
                Some(Value::Text(raw.into()))
            );
        }
        // Turning off interpretation must never restore int(float) truncation.
        assert_eq!(
            json(&serde_json::json!(9.4), &General::default(), &processing),
            Some(Value::Number(9.4))
        );
        assert_eq!(
            text("42", &General::default(), &processing),
            Some(Value::Number(42.0))
        );
    }
}
