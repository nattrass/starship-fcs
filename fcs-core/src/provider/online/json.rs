//! Just enough JSON to talk to a chat completions endpoint, hand-rolled so
//! the `online` feature stays dependency-free.
//!
//! This is not a general-purpose library and does not want to be. It encodes
//! the small request bodies the adapters build, and it decodes responses far
//! enough to pull one string out of a known shape. Everything it parses is
//! **untrusted**: a response body is model output plus whatever a network
//! path did to it, so the parser is written to fail rather than to cope —
//! bounded nesting depth, no recovery, no partial values, and non-finite
//! numbers rejected outright, in the same spirit as the wire
//! [`protocol`](crate::protocol). A failure here becomes a
//! [`ProviderError`](crate::provider::ProviderError), which the watchdog
//! turns into a tick flown by the autopilot.

use std::fmt;

/// How deeply nested a document may be before the parser gives up. Bounded so
/// a hostile response cannot exhaust the stack; far deeper than any real
/// completions payload.
const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Value>),
    /// Key/value pairs in document order. A `Vec` rather than a map: order is
    /// preserved exactly as written or read, so an encoded body is
    /// byte-stable and a decoded one carries no reordering of its own.
    Object(Vec<(String, Value)>),
}

impl Value {
    pub fn object(pairs: Vec<(&str, Value)>) -> Value {
        Value::Object(
            pairs
                .into_iter()
                .map(|(key, value)| (key.to_string(), value))
                .collect(),
        )
    }

    pub fn array(items: Vec<Value>) -> Value {
        Value::Array(items)
    }

    /// The value under `key`, if this is an object that has one.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(pairs) => pairs
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }

    /// Follows a chain of object keys, e.g. `path(["error", "message"])`.
    pub fn path(&self, keys: &[&str]) -> Option<&Value> {
        let mut current = self;
        for key in keys {
            current = current.get(key)?;
        }
        Some(current)
    }

    pub fn encode(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            Value::Null => out.push_str("null"),
            Value::Bool(true) => out.push_str("true"),
            Value::Bool(false) => out.push_str("false"),
            // JSON has no spelling for a non-finite number, and this codebase
            // treats one as the absence of data rather than something to
            // approximate.
            Value::Number(value) if !value.is_finite() => out.push_str("null"),
            Value::Number(value) => {
                // Integral values are written without a fractional part, so a
                // field an API declares as an integer (`max_tokens`) is not
                // sent as `16000.0`.
                if value.fract() == 0.0 && value.abs() < 1e15 {
                    out.push_str(&format!("{}", *value as i64));
                } else {
                    out.push_str(&format!("{value}"));
                }
            }
            Value::String(text) => write_string(text, out),
            Value::Array(items) => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Value::Object(pairs) => {
                out.push('{');
                for (index, (key, value)) in pairs.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    write_string(key, out);
                    out.push(':');
                    value.write(out);
                }
                out.push('}');
            }
        }
    }
}

impl From<&str> for Value {
    fn from(text: &str) -> Self {
        Value::String(text.to_string())
    }
}

impl From<String> for Value {
    fn from(text: String) -> Self {
        Value::String(text)
    }
}

impl From<f64> for Value {
    fn from(value: f64) -> Self {
        Value::Number(value)
    }
}

impl From<u32> for Value {
    fn from(value: u32) -> Self {
        Value::Number(value as f64)
    }
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Value::Bool(value)
    }
}

fn write_string(text: &str, out: &mut String) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            control if control < ' ' => {
                out.push_str(&format!("\\u{:04x}", control as u32));
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

#[derive(Debug, Clone, PartialEq)]
pub enum JsonError {
    UnexpectedEnd,
    Unexpected {
        at: usize,
        found: char,
    },
    InvalidEscape {
        at: usize,
    },
    InvalidNumber {
        at: usize,
    },
    /// A number that parsed but is `NaN` or an infinity — refused for the
    /// same reason the wire protocol refuses one.
    NonFiniteNumber {
        at: usize,
    },
    TooDeep {
        at: usize,
    },
    TrailingData {
        at: usize,
    },
}

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JsonError::UnexpectedEnd => write!(f, "unexpected end of JSON"),
            JsonError::Unexpected { at, found } => {
                write!(f, "unexpected {found:?} at byte {at}")
            }
            JsonError::InvalidEscape { at } => write!(f, "invalid escape at {at}"),
            JsonError::InvalidNumber { at } => write!(f, "invalid number at {at}"),
            JsonError::NonFiniteNumber { at } => write!(f, "non-finite number at {at}"),
            JsonError::TooDeep { at } => write!(f, "nesting deeper than {MAX_DEPTH} at {at}"),
            JsonError::TrailingData { at } => write!(f, "trailing data at {at}"),
        }
    }
}

impl std::error::Error for JsonError {}

/// Parses one complete JSON document. Trailing content after the value is an
/// error rather than something to ignore.
pub fn parse(text: &str) -> Result<Value, JsonError> {
    let chars: Vec<char> = text.chars().collect();
    let mut parser = Parser { chars, at: 0 };

    parser.skip_whitespace();
    let value = parser.value(0)?;
    parser.skip_whitespace();

    if parser.at < parser.chars.len() {
        return Err(JsonError::TrailingData { at: parser.at });
    }
    Ok(value)
}

struct Parser {
    chars: Vec<char>,
    at: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn next(&mut self) -> Option<char> {
        let character = self.peek()?;
        self.at += 1;
        Some(character)
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t' | '\n' | '\r')) {
            self.at += 1;
        }
    }

    fn expect(&mut self, expected: char) -> Result<(), JsonError> {
        match self.next() {
            Some(found) if found == expected => Ok(()),
            Some(found) => Err(JsonError::Unexpected {
                at: self.at - 1,
                found,
            }),
            None => Err(JsonError::UnexpectedEnd),
        }
    }

    fn literal(&mut self, word: &str, value: Value) -> Result<Value, JsonError> {
        for expected in word.chars() {
            self.expect(expected)?;
        }
        Ok(value)
    }

    fn value(&mut self, depth: usize) -> Result<Value, JsonError> {
        if depth > MAX_DEPTH {
            return Err(JsonError::TooDeep { at: self.at });
        }

        match self.peek().ok_or(JsonError::UnexpectedEnd)? {
            'n' => self.literal("null", Value::Null),
            't' => self.literal("true", Value::Bool(true)),
            'f' => self.literal("false", Value::Bool(false)),
            '"' => Ok(Value::String(self.string()?)),
            '[' => self.array(depth),
            '{' => self.object(depth),
            found if found == '-' || found.is_ascii_digit() => self.number(),
            found => Err(JsonError::Unexpected { at: self.at, found }),
        }
    }

    fn array(&mut self, depth: usize) -> Result<Value, JsonError> {
        self.expect('[')?;
        let mut items = Vec::new();

        self.skip_whitespace();
        if self.peek() == Some(']') {
            self.at += 1;
            return Ok(Value::Array(items));
        }

        loop {
            self.skip_whitespace();
            items.push(self.value(depth + 1)?);
            self.skip_whitespace();

            match self.next().ok_or(JsonError::UnexpectedEnd)? {
                ',' => continue,
                ']' => return Ok(Value::Array(items)),
                found => {
                    return Err(JsonError::Unexpected {
                        at: self.at - 1,
                        found,
                    })
                }
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Value, JsonError> {
        self.expect('{')?;
        let mut pairs = Vec::new();

        self.skip_whitespace();
        if self.peek() == Some('}') {
            self.at += 1;
            return Ok(Value::Object(pairs));
        }

        loop {
            self.skip_whitespace();
            let key = self.string()?;
            self.skip_whitespace();
            self.expect(':')?;
            self.skip_whitespace();
            let value = self.value(depth + 1)?;
            pairs.push((key, value));
            self.skip_whitespace();

            match self.next().ok_or(JsonError::UnexpectedEnd)? {
                ',' => continue,
                '}' => return Ok(Value::Object(pairs)),
                found => {
                    return Err(JsonError::Unexpected {
                        at: self.at - 1,
                        found,
                    })
                }
            }
        }
    }

    fn string(&mut self) -> Result<String, JsonError> {
        self.expect('"')?;
        let mut out = String::new();

        loop {
            match self.next().ok_or(JsonError::UnexpectedEnd)? {
                '"' => return Ok(out),
                '\\' => out.push(self.escape()?),
                control if control < ' ' => {
                    return Err(JsonError::Unexpected {
                        at: self.at - 1,
                        found: control,
                    })
                }
                other => out.push(other),
            }
        }
    }

    fn escape(&mut self) -> Result<char, JsonError> {
        let at = self.at;
        match self.next().ok_or(JsonError::UnexpectedEnd)? {
            '"' => Ok('"'),
            '\\' => Ok('\\'),
            '/' => Ok('/'),
            'b' => Ok('\u{8}'),
            'f' => Ok('\u{c}'),
            'n' => Ok('\n'),
            'r' => Ok('\r'),
            't' => Ok('\t'),
            'u' => self.unicode_escape(at),
            _ => Err(JsonError::InvalidEscape { at }),
        }
    }

    /// `\uXXXX`, including the surrogate pair a character outside the basic
    /// plane is written as.
    fn unicode_escape(&mut self, at: usize) -> Result<char, JsonError> {
        let first = self.hex4(at)?;

        // A high surrogate is only half a character; the low half must follow.
        if (0xD800..0xDC00).contains(&first) {
            if self.next() != Some('\\') || self.next() != Some('u') {
                return Err(JsonError::InvalidEscape { at });
            }
            let second = self.hex4(at)?;
            if !(0xDC00..0xE000).contains(&second) {
                return Err(JsonError::InvalidEscape { at });
            }
            let combined = 0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00);
            return char::from_u32(combined).ok_or(JsonError::InvalidEscape { at });
        }

        char::from_u32(first).ok_or(JsonError::InvalidEscape { at })
    }

    fn hex4(&mut self, at: usize) -> Result<u32, JsonError> {
        let mut value = 0u32;
        for _ in 0..4 {
            let digit = self.next().ok_or(JsonError::UnexpectedEnd)?;
            let digit = digit.to_digit(16).ok_or(JsonError::InvalidEscape { at })?;
            value = value * 16 + digit;
        }
        Ok(value)
    }

    fn number(&mut self) -> Result<Value, JsonError> {
        let start = self.at;
        if self.peek() == Some('-') {
            self.at += 1;
        }
        while matches!(self.peek(), Some(c) if c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))
        {
            self.at += 1;
        }

        let token: String = self.chars[start..self.at].iter().collect();
        let value: f64 = token
            .parse()
            .map_err(|_| JsonError::InvalidNumber { at: start })?;
        if !value.is_finite() {
            return Err(JsonError::NonFiniteNumber { at: start });
        }
        Ok(Value::Number(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_the_shape_a_chat_request_body_needs() {
        let body = Value::object(vec![
            ("model", "claude-opus-5".into()),
            ("max_tokens", 16000u32.into()),
            (
                "messages",
                Value::array(vec![Value::object(vec![
                    ("role", "user".into()),
                    ("content", "hello".into()),
                ])]),
            ),
        ]);

        assert_eq!(
            body.encode(),
            r#"{"model":"claude-opus-5","max_tokens":16000,"messages":[{"role":"user","content":"hello"}]}"#
        );
    }

    /// An integer field an API declares as an integer must not go out with a
    /// fractional part.
    #[test]
    fn integral_numbers_encode_without_a_decimal_point() {
        assert_eq!(Value::Number(16000.0).encode(), "16000");
        assert_eq!(Value::Number(-3.0).encode(), "-3");
        assert_eq!(Value::Number(0.25).encode(), "0.25");
    }

    /// A prompt carries newlines and quotes on every single turn — this is
    /// the escape path the adapters actually depend on.
    #[test]
    fn strings_escape_the_characters_a_rendered_prompt_contains() {
        let encoded = Value::String("SAY: \"all\tnominal\"\nDO: x\\y".into()).encode();
        assert_eq!(encoded, r#""SAY: \"all\tnominal\"\nDO: x\\y""#);
    }

    #[test]
    fn control_characters_encode_as_unicode_escapes() {
        // Built rather than written out, so the expectation itself cannot be
        // mangled by whatever is editing this file.
        let expected = format!("\"{}u0001\"", '\\');
        assert_eq!(Value::String('\u{1}'.to_string()).encode(), expected);
    }

    /// JSON cannot spell a non-finite number, and this codebase does not
    /// invent a spelling for one.
    #[test]
    fn non_finite_numbers_encode_as_null_rather_than_an_invented_spelling() {
        assert_eq!(Value::Number(f64::NAN).encode(), "null");
        assert_eq!(Value::Number(f64::INFINITY).encode(), "null");
    }

    #[test]
    fn parses_the_shape_an_anthropic_response_has() {
        let value = parse(
            r#"{"id":"msg_1","content":[{"type":"text","text":"SAY: all nominal"}],"stop_reason":"end_turn"}"#,
        )
        .unwrap();

        let blocks = value.get("content").unwrap().as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].get("type").unwrap().as_str(), Some("text"));
        assert_eq!(
            blocks[0].get("text").unwrap().as_str(),
            Some("SAY: all nominal")
        );
        assert_eq!(value.get("stop_reason").unwrap().as_str(), Some("end_turn"));
    }

    #[test]
    fn path_walks_a_chain_of_keys() {
        let value = parse(r#"{"error":{"type":"api_error","message":"overloaded"}}"#).unwrap();
        assert_eq!(
            value.path(&["error", "message"]).unwrap().as_str(),
            Some("overloaded")
        );
        assert_eq!(value.path(&["error", "missing"]), None);
        assert_eq!(value.path(&["nope", "message"]), None);
    }

    #[test]
    fn parses_every_scalar_kind() {
        assert_eq!(parse("null").unwrap(), Value::Null);
        assert_eq!(parse("true").unwrap(), Value::Bool(true));
        assert_eq!(parse("false").unwrap(), Value::Bool(false));
        assert_eq!(parse("-12.5e2").unwrap(), Value::Number(-1250.0));
        assert_eq!(parse(r#""hi""#).unwrap(), Value::String("hi".into()));
    }

    #[test]
    fn parses_empty_containers_and_whitespace() {
        assert_eq!(parse("  [ ]  ").unwrap(), Value::Array(Vec::new()));
        assert_eq!(parse("\n{\t}\r\n").unwrap(), Value::Object(Vec::new()));
    }

    #[test]
    fn parses_every_string_escape() {
        let value = parse(r#""\" \\ \/ \b \f \n \r \t A""#).unwrap();
        assert_eq!(value.as_str(), Some("\" \\ / \u{8} \u{c} \n \r \t A"));
    }

    /// A model that answers with an emoji is answering with a surrogate pair
    /// on the wire.
    #[test]
    fn parses_a_surrogate_pair_into_one_character() {
        assert_eq!(parse(r#""🚀""#).unwrap().as_str(), Some("🚀"));
    }

    #[test]
    fn a_lone_or_mismatched_surrogate_is_refused() {
        for text in [r#""\ud83d""#, r#""\ud83dA""#, r#""\ud83dx""#] {
            assert!(parse(text).is_err(), "{text} should not parse");
        }
    }

    #[test]
    fn encoding_then_parsing_round_trips_a_document() {
        let original = Value::object(vec![
            ("text", "line one\nline \"two\"".into()),
            ("n", 42u32.into()),
            ("flag", true.into()),
            ("list", Value::array(vec![Value::Null, 1.5.into()])),
        ]);

        assert_eq!(parse(&original.encode()).unwrap(), original);
    }

    #[test]
    fn malformed_documents_are_refused_rather_than_guessed_at() {
        for text in [
            "",
            "{",
            "[1,]",
            "{\"a\"}",
            "{\"a\":}",
            "tru",
            "\"unterminated",
            "01x",
        ] {
            assert!(parse(text).is_err(), "{text:?} should not parse");
        }
    }

    #[test]
    fn trailing_content_after_the_document_is_an_error() {
        assert_eq!(parse("{} garbage"), Err(JsonError::TrailingData { at: 3 }));
    }

    /// A number that overflows to infinity is not data, the same way a `NaN`
    /// on the command wire is not.
    #[test]
    fn a_number_that_overflows_to_infinity_is_refused() {
        assert_eq!(parse("1e999"), Err(JsonError::NonFiniteNumber { at: 0 }));
    }

    /// A hostile response must not be able to exhaust the stack.
    #[test]
    fn nesting_past_the_depth_limit_is_refused_rather_than_overflowing_the_stack() {
        let deep = format!("{}{}", "[".repeat(500), "]".repeat(500));
        assert!(matches!(parse(&deep), Err(JsonError::TooDeep { .. })));
    }

    #[test]
    fn a_raw_control_character_inside_a_string_is_refused() {
        assert!(parse("\"a\nb\"").is_err());
    }

    #[test]
    fn parsing_the_same_document_twice_yields_the_same_value() {
        let text = r#"{"content":[{"type":"text","text":"hi"}]}"#;
        assert_eq!(parse(text), parse(text));
    }
}
