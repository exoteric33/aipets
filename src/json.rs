//! Minimal JSON for editing the agents' config files. Objects keep their key order and values their
//! original text, so writing a file back only changes what was edited (and the indentation).

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Obj(Obj),
    Arr(Vec<Json>),
    /// A string, number, true, false or null, kept exactly as it was written.
    Val(String),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Obj {
    pub items: Vec<(String, Json)>,
}

impl Obj {
    pub fn new() -> Obj {
        Obj::default()
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        self.items.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Json> {
        self.items.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn set(&mut self, key: &str, value: Json) {
        match self.items.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = value,
            None => self.items.push((key.to_string(), value)),
        }
    }

    pub fn remove(&mut self, key: &str) {
        self.items.retain(|(k, _)| k != key);
    }

    pub fn text(&self, key: &str) -> Option<String> {
        self.get(key).and_then(Json::text)
    }
}

impl Json {
    pub fn str(text: &str) -> Json {
        Json::Val(quote(text))
    }

    pub fn int(n: i64) -> Json {
        Json::Val(n.to_string())
    }

    pub fn bool(b: bool) -> Json {
        Json::Val(if b { "true" } else { "false" }.to_string())
    }

    pub fn null() -> Json {
        Json::Val("null".to_string())
    }

    /// The decoded text of a string; None for anything else.
    pub fn text(&self) -> Option<String> {
        match self {
            Json::Val(raw) if raw.len() >= 2 && raw.starts_with('"') => Some(unescape(raw)),
            _ => None,
        }
    }

    pub fn obj(&self) -> Option<&Obj> {
        if let Json::Obj(o) = self { Some(o) } else { None }
    }

    pub fn obj_mut(&mut self) -> Option<&mut Obj> {
        if let Json::Obj(o) = self { Some(o) } else { None }
    }

    pub fn arr(&self) -> Option<&Vec<Json>> {
        if let Json::Arr(a) = self { Some(a) } else { None }
    }

    pub fn arr_mut(&mut self) -> Option<&mut Vec<Json>> {
        if let Json::Arr(a) = self { Some(a) } else { None }
    }
}

pub fn quote(text: &str) -> String {
    let mut s = String::with_capacity(text.len() + 2);
    s.push('"');
    for c in text.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            '\r' => s.push_str("\\r"),
            '\t' => s.push_str("\\t"),
            c if (c as u32) < 0x20 => s.push_str(&format!("\\u{:04x}", c as u32)),
            c => s.push(c),
        }
    }
    s.push('"');
    s
}

/// The text of a quoted JSON string (the quotes included in raw).
pub fn unescape(raw: &str) -> String {
    let u: Vec<u16> = raw.encode_utf16().collect();
    let mut out: Vec<u16> = Vec::with_capacity(u.len());
    let end = u.len().saturating_sub(1);
    let mut i = 1;
    while i < end {
        let c = u[i];
        if c != b'\\' as u16 || i + 1 >= end {
            out.push(c);
            i += 1;
            continue;
        }
        i += 1;
        let c = u[i];
        match c {
            0x6E => out.push(b'\n' as u16), // n
            0x72 => out.push(b'\r' as u16), // r
            0x74 => out.push(b'\t' as u16), // t
            0x62 => out.push(0x08),         // b
            0x66 => out.push(0x0C),         // f
            0x75 => {
                // u
                if i + 4 < u.len() {
                    let hex = String::from_utf16_lossy(&u[i + 1..i + 5]);
                    out.push(u16::from_str_radix(&hex, 16).unwrap_or(0xFFFD));
                    i += 4;
                }
            }
            _ => out.push(c), // \" \\ \/
        }
        i += 1;
    }
    String::from_utf16_lossy(&out)
}

pub fn parse(text: &str) -> Result<Json, String> {
    let mut p = Parser { s: text.as_bytes(), src: text, i: 0 };
    let v = p.value(0)?;
    p.skip_space();
    if p.i < p.s.len() {
        return Err(format!("unexpected text after the JSON value at {}", p.i));
    }
    Ok(v)
}

/// Two-space indented JSON with a trailing newline (the style Claude Code and Codex write).
pub fn write(value: &Json) -> String {
    let mut s = String::new();
    write_into(&mut s, value, 0);
    s.push('\n');
    s
}

fn write_into(s: &mut String, value: &Json, indent: usize) {
    match value {
        Json::Obj(o) => {
            if o.items.is_empty() {
                s.push_str("{}");
                return;
            }
            s.push_str("{\n");
            for (i, (k, v)) in o.items.iter().enumerate() {
                s.push_str(&" ".repeat(indent + 2));
                s.push_str(&quote(k));
                s.push_str(": ");
                write_into(s, v, indent + 2);
                s.push_str(if i + 1 < o.items.len() { ",\n" } else { "\n" });
            }
            s.push_str(&" ".repeat(indent));
            s.push('}');
        }
        Json::Arr(a) => {
            if a.is_empty() {
                s.push_str("[]");
                return;
            }
            s.push_str("[\n");
            for (i, v) in a.iter().enumerate() {
                s.push_str(&" ".repeat(indent + 2));
                write_into(s, v, indent + 2);
                s.push_str(if i + 1 < a.len() { ",\n" } else { "\n" });
            }
            s.push_str(&" ".repeat(indent));
            s.push(']');
        }
        Json::Val(raw) => s.push_str(raw),
    }
}

struct Parser<'a> {
    s: &'a [u8],
    src: &'a str,
    i: usize,
}

impl Parser<'_> {
    fn skip_space(&mut self) {
        while self.i < self.s.len() {
            match self.s[self.i] {
                b' ' | b'\t' | b'\r' | b'\n' => self.i += 1,
                0xEF if self.s[self.i..].starts_with(&[0xEF, 0xBB, 0xBF]) => self.i += 3, // a BOM
                _ => break,
            }
        }
    }

    fn expect(&mut self, c: u8) -> Result<(), String> {
        if self.i >= self.s.len() || self.s[self.i] != c {
            return Err(format!("expected '{}' at {}", c as char, self.i));
        }
        self.i += 1;
        Ok(())
    }

    fn value(&mut self, depth: usize) -> Result<Json, String> {
        if depth > 128 {
            return Err("JSON nesting is too deep".into());
        }
        self.skip_space();
        if self.i >= self.s.len() {
            return Err("unexpected end of JSON".into());
        }
        match self.s[self.i] {
            b'{' => {
                let mut obj = Obj::new();
                self.i += 1;
                self.skip_space();
                if self.i < self.s.len() && self.s[self.i] == b'}' {
                    self.i += 1;
                    return Ok(Json::Obj(obj));
                }
                loop {
                    self.skip_space();
                    if self.i >= self.s.len() || self.s[self.i] != b'"' {
                        return Err(format!("object key is not a string at {}", self.i));
                    }
                    let key = self.value(depth + 1)?;
                    let name = key.text().ok_or_else(|| format!("object key is not a string at {}", self.i))?;
                    self.skip_space();
                    self.expect(b':')?;
                    if obj.get(&name).is_some() {
                        return Err(format!("duplicate JSON key: {name}"));
                    }
                    let v = self.value(depth + 1)?;
                    obj.items.push((name, v));
                    self.skip_space();
                    if self.i < self.s.len() && self.s[self.i] == b',' {
                        self.i += 1;
                        continue;
                    }
                    self.expect(b'}')?;
                    return Ok(Json::Obj(obj));
                }
            }
            b'[' => {
                let mut list = Vec::new();
                self.i += 1;
                self.skip_space();
                if self.i < self.s.len() && self.s[self.i] == b']' {
                    self.i += 1;
                    return Ok(Json::Arr(list));
                }
                loop {
                    list.push(self.value(depth + 1)?);
                    self.skip_space();
                    if self.i < self.s.len() && self.s[self.i] == b',' {
                        self.i += 1;
                        continue;
                    }
                    self.expect(b']')?;
                    return Ok(Json::Arr(list));
                }
            }
            b'"' => {
                let start = self.i;
                self.i += 1;
                while self.i < self.s.len() && self.s[self.i] != b'"' {
                    let c = self.s[self.i];
                    if c < b' ' {
                        return Err(format!("unescaped control character at {}", self.i));
                    }
                    if c == b'\\' {
                        self.i += 1;
                        if self.i >= self.s.len() || !b"\"\\/bfnrtu".contains(&self.s[self.i]) {
                            return Err(format!("invalid JSON escape at {}", self.i));
                        }
                        if self.s[self.i] == b'u' {
                            for _ in 0..4 {
                                self.i += 1;
                                if self.i >= self.s.len() || !self.s[self.i].is_ascii_hexdigit() {
                                    return Err(format!("invalid unicode escape at {}", self.i));
                                }
                            }
                        }
                    }
                    self.i += 1;
                }
                if self.i >= self.s.len() {
                    return Err(format!("unterminated string at {start}"));
                }
                self.i += 1;
                Ok(Json::Val(self.src[start..self.i].to_string()))
            }
            c => {
                let start = self.i;
                while self.i < self.s.len() && !b",]} \t\r\n".contains(&self.s[self.i]) {
                    self.i += 1;
                }
                if self.i == start {
                    return Err(format!("unexpected character '{}' at {}", c as char, self.i));
                }
                let raw = &self.src[start..self.i];
                if raw != "true" && raw != "false" && raw != "null" && !is_number(raw) {
                    return Err(format!("invalid JSON value at {start}"));
                }
                Ok(Json::Val(raw.to_string()))
            }
        }
    }
}

/// -?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?
fn is_number(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    if i < b.len() && b[i] == b'-' {
        i += 1;
    }
    if i < b.len() && b[i] == b'0' {
        i += 1;
    } else if i < b.len() && (b'1'..=b'9').contains(&b[i]) {
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    } else {
        return false;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let d = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == d {
            return false;
        }
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let d = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == d {
            return false;
        }
    }
    i == b.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_keeps_order_and_raw_values() {
        let src = "{\"b\": 1.50, \"a\": [true, null, \"x\\u00e9\\n\"], \"e\": {}, \"l\": []}";
        let v = parse(src).unwrap();
        assert_eq!(
            write(&v),
            "{\n  \"b\": 1.50,\n  \"a\": [\n    true,\n    null,\n    \"x\\u00e9\\n\"\n  ],\n  \"e\": {},\n  \"l\": []\n}\n"
        );
        let a = v.obj().unwrap().get("a").unwrap().arr().unwrap();
        assert_eq!(a[2].text().unwrap(), "x\u{e9}\n");
    }

    #[test]
    fn rejects_bad_json() {
        assert!(parse("{\"a\":1,\"a\":2}").is_err());
        assert!(parse("{\"a\":01}").is_err());
        assert!(parse("[1,]").is_err());
        assert!(parse("\"a\nb\"").is_err());
        assert!(parse("{} x").is_err());
        assert!(parse("\u{feff}{}").is_ok());
    }

    #[test]
    fn surrogate_pairs_and_quotes() {
        assert_eq!(unescape("\"\\ud83d\\ude00\""), "\u{1F600}");
        assert_eq!(quote("a\"b\\c\u{1}"), "\"a\\\"b\\\\c\\u0001\"");
        assert_eq!(unescape("\"ab\\\""), "ab\\");
    }
}
