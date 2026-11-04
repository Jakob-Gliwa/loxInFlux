//! Bounded fallback for malformed Miniserver string values. Never used on the
//! valid-JSON fast path. No guessing of arbitrary quotes or numeric syntax.
const MAX_BYTES: usize = 1024 * 1024;
const MAX_DEPTH: usize = 128;
const MAX_REPAIRS: usize = 256;
#[derive(Clone, Copy)]
enum Site {
    Root,
    Ll,
    Output,
    Value,
    Other,
}
struct Repair<'a> {
    input: &'a [u8],
    at: usize,
    out: Vec<u8>,
    changes: usize,
}
pub fn repair(input: &[u8]) -> Option<Vec<u8>> {
    if input.len() > MAX_BYTES {
        return None;
    }
    let mut r = Repair {
        input,
        at: 0,
        out: Vec::with_capacity(input.len()),
        changes: 0,
    };
    r.value(Site::Root, 0)?;
    r.space();
    (r.at == input.len() && r.changes > 0 && r.changes <= MAX_REPAIRS).then_some(r.out)
}
impl Repair<'_> {
    fn space(&mut self) {
        while self.input.get(self.at).is_some_and(u8::is_ascii_whitespace) {
            self.copy();
        }
    }
    fn copy(&mut self) {
        self.out.push(self.input[self.at]);
        self.at += 1;
    }
    fn take(&mut self, b: u8) -> Option<()> {
        self.space();
        if self.input.get(self.at) != Some(&b) {
            return None;
        }
        self.copy();
        Some(())
    }
    fn string(&mut self, allow: bool) -> Option<()> {
        if self.input.get(self.at) != Some(&b'"') {
            return None;
        }
        let start = self.at + 1;
        // A complete valid JSON object/array between the *outer* quotes gives
        // an unambiguous boundary, even with nested objects and escaped quotes.
        if allow
            && matches!(
                self.input[start..]
                    .iter()
                    .find(|b| !b.is_ascii_whitespace()),
                Some(b'{' | b'[')
            )
        {
            let mut stream = serde_json::Deserializer::from_slice(&self.input[start..])
                .into_iter::<serde_json::Value>();
            if matches!(stream.next(), Some(Ok(_))) {
                let mut end = start + stream.byte_offset();
                while self.input.get(end).is_some_and(u8::is_ascii_whitespace) {
                    end += 1;
                }
                if self.input.get(end) == Some(&b'"') {
                    let next = self.input[end + 1..]
                        .iter()
                        .find(|b| !b.is_ascii_whitespace());
                    if matches!(next, Some(b',' | b'}' | b']')) {
                        let text = std::str::from_utf8(&self.input[start..end]).ok()?;
                        serde_json::to_writer(&mut self.out, text).ok()?;
                        self.at = end + 1;
                        self.changes += 1;
                        return Some(());
                    }
                }
            }
        }
        self.copy();
        while let Some(&b) = self.input.get(self.at) {
            match b {
                b'"' => {
                    self.copy();
                    return Some(());
                }
                b'\\' => {
                    self.copy();
                    self.input.get(self.at)?;
                    self.copy();
                }
                0..=31 if allow => {
                    const HEX: &[u8] = b"0123456789abcdef";
                    self.out.extend_from_slice(&[
                        b'\\',
                        b'u',
                        b'0',
                        b'0',
                        HEX[(b >> 4) as usize],
                        HEX[(b & 15) as usize],
                    ]);
                    self.at += 1;
                    self.changes += 1;
                }
                _ => self.copy(),
            }
            if self.changes > MAX_REPAIRS {
                return None;
            }
        }
        None
    }
    fn value(&mut self, site: Site, depth: usize) -> Option<()> {
        if depth > MAX_DEPTH || self.changes > MAX_REPAIRS {
            return None;
        }
        self.space();
        match *self.input.get(self.at)? {
            b'{' => {
                self.copy();
                self.space();
                if self.input.get(self.at) == Some(&b'}') {
                    self.copy();
                    return Some(());
                }
                loop {
                    self.space();
                    let start = self.at;
                    self.string(false)?;
                    let key: String = serde_json::from_slice(&self.input[start..self.at]).ok()?;
                    self.take(b':')?;
                    let child = match (site, key.as_str()) {
                        (Site::Root, "LL") => Site::Ll,
                        (Site::Root | Site::Ll, k) if k.starts_with("output") => Site::Output,
                        (Site::Root | Site::Ll | Site::Output, "value") => Site::Value,
                        _ => Site::Other,
                    };
                    self.value(child, depth + 1)?;
                    self.space();
                    if self.input.get(self.at) == Some(&b'}') {
                        self.copy();
                        break;
                    }
                    self.take(b',')?;
                }
            }
            b'[' => {
                self.copy();
                self.space();
                if self.input.get(self.at) == Some(&b']') {
                    self.copy();
                    return Some(());
                }
                loop {
                    self.value(Site::Other, depth + 1)?;
                    self.space();
                    if self.input.get(self.at) == Some(&b']') {
                        self.copy();
                        break;
                    }
                    self.take(b',')?;
                }
            }
            b'"' => self.string(matches!(site, Site::Value))?,
            _ => {
                let start = self.at;
                while self
                    .input
                    .get(self.at)
                    .is_some_and(|b| !b.is_ascii_whitespace() && !b",]}".contains(b))
                {
                    self.copy();
                }
                if self.at == start {
                    return None;
                }
            }
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn structured_text_and_literal_controls_are_repaired_without_changing_content() {
        for inner in [
            r#"{"cmd":"wwcircmode","data":"off","id":158}"#,
            r#"[{"nested":[1,true,null,{"text":"a\\b\"c"}]}]"#,
        ] {
            let raw = format!(r#"{{"LL":{{"Code":"200","output":{{"value":"{inner}"}}}}}}"#);
            let fixed = repair(raw.as_bytes()).unwrap();
            let value: serde_json::Value = serde_json::from_slice(&fixed).unwrap();
            assert_eq!(value["LL"]["output"]["value"], inner);
        }
        let raw = b"{\"LL\":{\"value\":\"a\nb\tc\r\x01\"}}";
        let value: serde_json::Value = serde_json::from_slice(&repair(raw).unwrap()).unwrap();
        assert_eq!(value["LL"]["value"], "a\nb\tc\r\x01");
    }
    #[test]
    fn ambiguity_structure_status_identity_and_limits_are_not_guessed() {
        for raw in [
            r#"{"LL":{"value":"say "hello" please"}}"#,
            r#"{"LL":{"Code":"{"x":1}","value":1}}"#,
            r#"{"LL":{"control":"{"x":1}","value":1}}"#,
            r#"{"LL":{"value":"{"x":1}","broken":}}"#,
            r#"{"LL":{"value":"{"x":1}",}}"#,
            r#"{"LL":{"value":"{"x":1}"}} trailing"#,
            r#"{"LL":{"value":"{"x":1e999}"}}"#,
        ] {
            assert!(
                repair(raw.as_bytes())
                    .is_none_or(
                        |fixed| serde_json::from_slice::<serde_json::Value>(&fixed).is_err()
                    ),
                "{raw}"
            );
        }
        assert!(repair(&vec![b' '; MAX_BYTES + 1]).is_none());
        let raw = format!(
            "{}0{}",
            "[".repeat(MAX_DEPTH + 2),
            "]".repeat(MAX_DEPTH + 2)
        );
        assert!(repair(raw.as_bytes()).is_none());
    }
}
