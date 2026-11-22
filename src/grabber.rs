//! Selective JSON deserialization, following the relay's borrowed-data approach.
//! The /all envelope has a fixed schema, so it needs no learned shape cache.
//! Serde validates the entire document, including ignored members and escapes;
//! float_roundtrip is retained. No hand-written number scanner is involved.
use crate::{
    config::Config,
    metrics::{self, Metrics},
    value::{self, Value},
};
use anyhow::{Result, ensure};
use serde::{
    Deserialize, Deserializer,
    de::{MapAccess, SeqAccess, Visitor},
};
use std::{borrow::Cow, collections::BTreeMap, fmt};

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Key<'a>(Cow<'a, str>);
impl<'de> Deserialize<'de> for Key<'de> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct KeyVisitor;
        impl<'de> Visitor<'de> for KeyVisitor {
            type Value = Key<'de>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a JSON key")
            }
            fn visit_borrowed_str<E: serde::de::Error>(
                self,
                v: &'de str,
            ) -> Result<Self::Value, E> {
                Ok(Key(Cow::Borrowed(v)))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(Key(Cow::Owned(v.into())))
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(Key(Cow::Owned(v)))
            }
        }
        d.deserialize_str(KeyVisitor)
    }
}

#[derive(Default)]
enum Scalar<'a> {
    Number(f64),
    Text(Cow<'a, str>),
    #[default]
    Invalid,
}
impl Scalar<'_> {
    fn normalize(self, config: &Config) -> Option<Value> {
        match self {
            Self::Number(n) => value::number(n, &config.general),
            Self::Text(Cow::Borrowed(s)) => value::text(s, &config.general, &config.processing),
            Self::Text(Cow::Owned(s)) => value::text_owned(s, &config.general, &config.processing),
            Self::Invalid => None,
        }
    }
}
impl<'de> Deserialize<'de> for Scalar<'de> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct ScalarVisitor;
        impl<'de> Visitor<'de> for ScalarVisitor {
            type Value = Scalar<'de>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a sensor value")
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(Scalar::Number(v as f64))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(Scalar::Number(v as f64))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> {
                Ok(Scalar::Number(v))
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(Scalar::Number(u8::from(v) as f64))
            }
            fn visit_borrowed_str<E: serde::de::Error>(
                self,
                v: &'de str,
            ) -> Result<Self::Value, E> {
                Ok(Scalar::Text(Cow::Borrowed(v)))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(Scalar::Text(Cow::Owned(v.into())))
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(Scalar::Text(Cow::Owned(v)))
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(Scalar::Invalid)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                while a.next_element::<Skipped>()?.is_some() {}
                Ok(Scalar::Invalid)
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                while a.next_entry::<Skipped, Skipped>()?.is_some() {}
                Ok(Scalar::Invalid)
            }
        }
        d.deserialize_any(ScalarVisitor)
    }
}

// Non-object output/envelope values have no fields, as with Value::get().
macro_rules! empty_scalars {
    () => {
        fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self::Value, E> {
            Ok(Default::default())
        }
        fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self::Value, E> {
            Ok(Default::default())
        }
        fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self::Value, E> {
            Ok(Default::default())
        }
        fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self::Value, E> {
            Ok(Default::default())
        }
        fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self::Value, E> {
            Ok(Default::default())
        }
        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(Default::default())
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
            while a.next_element::<Skipped>()?.is_some() {}
            Ok(Default::default())
        }
    };
}
// IgnoredAny only checks JSON syntax: ignored strings can contain invalid UTF-8
// or unpaired surrogates, ignored numbers can overflow, and nested data bypasses
// the normal recursion limit. Deserialize without retaining a DOM, but keep the
// same string, number and depth validation as serde_json::Value.
#[derive(Default)]
struct Skipped;
impl<'de> Deserialize<'de> for Skipped {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct SkipVisitor;
        impl<'de> Visitor<'de> for SkipVisitor {
            type Value = Skipped;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a valid JSON value")
            }
            empty_scalars!();
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                while a.next_entry::<Skipped, Skipped>()?.is_some() {}
                Ok(Skipped)
            }
        }
        d.deserialize_any(SkipVisitor)
    }
}
#[derive(Default)]
struct Output<'a> {
    name: Option<Cow<'a, str>>,
    nr: Option<serde_json::Value>,
    value: Option<Scalar<'a>>,
}
impl<'de> Deserialize<'de> for Output<'de> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct OutputVisitor;
        impl<'de> Visitor<'de> for OutputVisitor {
            type Value = Output<'de>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an output")
            }
            empty_scalars!();
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                let mut out = Output::default();
                while let Some(key) = a.next_key::<Key>()? {
                    match key.0.as_ref() {
                        "name" => {
                            out.name = match a.next_value::<Scalar>()? {
                                Scalar::Text(s) => Some(s),
                                _ => None,
                            }
                        }
                        "nr" => out.nr = Some(a.next_value()?),
                        "value" => out.value = Some(a.next_value()?),
                        _ => {
                            a.next_value::<Skipped>()?;
                        }
                    }
                }
                Ok(out)
            }
        }
        d.deserialize_any(OutputVisitor)
    }
}
// Preserve integer-vs-float status semantics (200.0 is not integer 200),
// borrowing the common "200" string instead of allocating a JSON Value.
#[derive(Default)]
enum Status<'a> {
    Text(Cow<'a, str>),
    Number(serde_json::Number),
    #[default]
    Invalid,
}
impl Status<'_> {
    fn check(&self) -> Result<()> {
        match self {
            Self::Text(s) if s == "200" => Ok(()),
            Self::Number(n) if n.as_u64() == Some(200) => Ok(()),
            Self::Text(s) => anyhow::bail!("Miniserver LL status {s}"),
            Self::Number(n) => anyhow::bail!("Miniserver LL status {n}"),
            Self::Invalid => anyhow::bail!("Invalid Miniserver LL status"),
        }
    }
}
impl<'de> Deserialize<'de> for Status<'de> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct StatusVisitor;
        impl<'de> Visitor<'de> for StatusVisitor {
            type Value = Status<'de>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a response status")
            }
            fn visit_borrowed_str<E: serde::de::Error>(
                self,
                s: &'de str,
            ) -> Result<Self::Value, E> {
                Ok(Status::Text(Cow::Borrowed(s)))
            }
            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<Self::Value, E> {
                Ok(Status::Text(Cow::Owned(s.into())))
            }
            fn visit_string<E: serde::de::Error>(self, s: String) -> Result<Self::Value, E> {
                Ok(Status::Text(Cow::Owned(s)))
            }
            fn visit_i64<E: serde::de::Error>(self, n: i64) -> Result<Self::Value, E> {
                Ok(Status::Number(n.into()))
            }
            fn visit_u64<E: serde::de::Error>(self, n: u64) -> Result<Self::Value, E> {
                Ok(Status::Number(n.into()))
            }
            fn visit_f64<E: serde::de::Error>(self, n: f64) -> Result<Self::Value, E> {
                Ok(serde_json::Number::from_f64(n)
                    .map(Status::Number)
                    .unwrap_or_default())
            }
            fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(Status::Invalid)
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(Status::Invalid)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                while a.next_element::<Skipped>()?.is_some() {}
                Ok(Status::Invalid)
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                while a.next_entry::<Skipped, Skipped>()?.is_some() {}
                Ok(Status::Invalid)
            }
        }
        d.deserialize_any(StatusVisitor)
    }
}
#[derive(Default)]
struct Envelope<'a> {
    control: Option<Scalar<'a>>,
    upper_code: Option<Status<'a>>,
    lower_code: Option<Status<'a>>,
    value: Option<Scalar<'a>>,
    outputs: BTreeMap<Key<'a>, Output<'a>>,
}
impl<'de> Envelope<'de> {
    fn field<A: MapAccess<'de>>(&mut self, key: Key<'de>, a: &mut A) -> Result<(), A::Error> {
        match key.0.as_ref() {
            "control" => self.control = Some(a.next_value()?),
            "Code" => self.upper_code = Some(a.next_value()?),
            "code" => self.lower_code = Some(a.next_value()?),
            "value" => self.value = Some(a.next_value()?),
            k if k.starts_with("output") => {
                self.outputs.insert(key, a.next_value()?);
            }
            _ => {
                a.next_value::<Skipped>()?;
            }
        }
        Ok(())
    }
}
impl<'de> Deserialize<'de> for Envelope<'de> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct EnvelopeVisitor;
        impl<'de> Visitor<'de> for EnvelopeVisitor {
            type Value = Envelope<'de>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a Loxone envelope")
            }
            empty_scalars!();
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                let mut out = Envelope::default();
                while let Some(key) = a.next_key::<Key>()? {
                    out.field(key, &mut a)?;
                }
                Ok(out)
            }
        }
        d.deserialize_any(EnvelopeVisitor)
    }
}
// Retain only the selected envelope. LL supersedes root fields regardless of
// ordering; duplicate LL keys still use the last value. All ignored root data
// and nested LL values undergo the same strict string/number/depth validation.
#[derive(Default)]
struct Document<'a> {
    has_ll: bool,
    envelope: Envelope<'a>,
}
impl<'de> Deserialize<'de> for Document<'de> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct DocumentVisitor;
        impl<'de> Visitor<'de> for DocumentVisitor {
            type Value = Document<'de>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a Loxone response")
            }
            empty_scalars!();
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                let mut out = Document::default();
                while let Some(key) = a.next_key::<Key>()? {
                    if key.0 == "LL" {
                        out.envelope = a.next_value()?;
                        out.has_ll = true;
                    } else if out.has_ll {
                        a.next_value::<Skipped>()?;
                    } else {
                        out.envelope.field(key, &mut a)?;
                    }
                }
                Ok(out)
            }
        }
        d.deserialize_any(DocumentVisitor)
    }
}

#[derive(Debug)]
pub(crate) struct InvalidGrabberJson;
impl std::fmt::Display for InvalidGrabberJson {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Invalid grabber JSON (unrepairable response)")
    }
}
impl std::error::Error for InvalidGrabberJson {}

pub(crate) fn parse(
    payload: &[u8],
    config: &Config,
    metrics: &Metrics,
    require_ll: bool,
    expected_uuid: Option<&str>,
) -> Result<BTreeMap<String, Value>> {
    match serde_json::from_slice::<Document>(payload) {
        Ok(document) => parse_document(document, config, metrics, require_ll, expected_uuid),
        Err(_) => parse_repaired(payload, config, metrics, require_ll, expected_uuid),
    }
}

#[cold]
#[inline(never)]
fn parse_repaired(
    payload: &[u8],
    config: &Config,
    metrics: &Metrics,
    require_ll: bool,
    expected_uuid: Option<&str>,
) -> Result<BTreeMap<String, Value>> {
    let repaired = crate::json_repair::repair(payload).ok_or(InvalidGrabberJson)?;
    let document = serde_json::from_slice(&repaired).map_err(|_| InvalidGrabberJson)?;
    let fields = parse_document(document, config, metrics, require_ll, expected_uuid)?;
    if metrics
        .grabber_json_repaired
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        == 0
    {
        tracing::warn!(
            "Repaired malformed Miniserver JSON string value; tracking further repairs in grabber_json_repaired_total"
        );
    }
    Ok(fields)
}

fn parse_document(
    root: Document<'_>,
    config: &Config,
    metrics: &Metrics,
    require_ll: bool,
    expected_uuid: Option<&str>,
) -> Result<BTreeMap<String, Value>> {
    ensure!(!require_ll || root.has_ll, "Missing grabber LL response");
    let document = root.envelope;
    if let Some(code) = document
        .upper_code
        .as_ref()
        .or(document.lower_code.as_ref())
    {
        code.check()?;
    }
    if let Some(uuid) = expected_uuid {
        check_control(document.control, uuid)?;
    }
    let mut out = BTreeMap::new();
    let mut insert = |key: String, raw: Scalar| {
        metrics::inc(&metrics.values_received);
        if let Some(value) = raw.normalize(config) {
            out.insert(key, value);
        } else {
            metrics::inc(&metrics.values_invalid);
        }
    };
    if let Some(raw) = document.value {
        insert("Default".into(), raw);
    }
    for (_, output) in document.outputs {
        if let Some(raw) = output.value {
            let name = output
                .name
                .filter(|s| !s.is_empty())
                .map(Cow::into_owned)
                .or_else(|| {
                    output.nr.filter(|n| !n.is_null()).map(|n| match n {
                        serde_json::Value::String(s) => s,
                        other => other.to_string(),
                    })
                })
                .unwrap_or_else(|| "Subdefault".into());
            insert(name, raw);
        }
    }
    if config.processing.drop_redundant_default
        && let Some(Value::Number(n)) = out.get("Default")
        && out
            .iter()
            .any(|(key, value)| key != "Default" && value == &Value::Number(*n))
    {
        out.remove("Default");
        metrics::inc(&metrics.values_filtered);
    }
    Ok(out)
}

fn check_control(control: Option<Scalar<'_>>, expected_uuid: &str) -> Result<()> {
    let Some(control) = control else {
        // Some firmware omits control. Correlation then relies on upstream FIFO.
        return Ok(());
    };
    let Scalar::Text(control) = control else {
        anyhow::bail!("Invalid grabber response control");
    };
    let control = control
        .trim()
        .trim_end_matches('\0')
        .trim_start_matches('/');
    let control = control.strip_prefix('j').unwrap_or(control);
    if control.starts_with("dev/sys/enc/") {
        // The client owns the session keys and encrypted wire-command match.
        return Ok(());
    }
    // The upstream plaintext fallback only compares the command verb. Check
    // the actual target too, in the existing JSON pass, without another DOM.
    let path = control
        .strip_prefix("dev/sps/io/")
        .or_else(|| control.strip_prefix("dev/sps/ios/"));
    let target = path
        .and_then(|p| p.strip_suffix("/all"))
        .and_then(|p| p.rsplit('/').next());
    ensure!(
        target.is_some_and(|uuid| crate::config::normalize_uuid(uuid) == expected_uuid),
        "Grabber response control does not match the requested UUID"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repairing_a_string_never_bypasses_status_or_target_checks() {
        let raw = include_str!(
            "../tests/fixtures/miniserver_real/malformed_unescaped_quotes_in_output_value.raw"
        );
        let config = Config::default();
        for (payload, uuid) in [
            (
                raw.replace("\"Code\": \"200\"", "\"Code\": \"403\""),
                "00000000-0000-0001-ffff000000000001",
            ),
            (raw.to_owned(), "00000000-0000-0002-ffff000000000001"),
        ] {
            let metrics = Metrics::default();
            assert!(parse(payload.as_bytes(), &config, &metrics, true, Some(uuid)).is_err());
            assert_eq!(metrics::read(&metrics.values_received), 0);
            assert_eq!(metrics::read(&metrics.grabber_json_repaired), 0);
        }
    }
    #[test]
    fn plaintext_poll_control_checks_the_target_and_accepts_firmware_spellings() {
        let uuid = "01234567-89ab-cdef-0123456789abcdef";
        for control in [
            format!("jdev/sps/io/{uuid}/all"),
            format!(" /dev/sps/io/{uuid}/all\0"),
            format!("/jdev/sps/ios/hash/{uuid}/all"),
            format!("dev/sps/ios/{uuid}/all"),
            "jdev/sys/enc/opaque".into(),
        ] {
            check_control(Some(Scalar::Text(control.into())), uuid).unwrap();
        }
        for control in [
            "jdev/sps/io/wrong/all",
            "jdev/sys/getkey",
            "",
            "jdev/sps/io/wrong/state",
        ] {
            assert!(check_control(Some(Scalar::Text(control.into())), uuid).is_err());
        }
        assert!(check_control(Some(Scalar::Number(1.0)), uuid).is_err());
        assert!(check_control(None, uuid).is_ok());
    }
}
