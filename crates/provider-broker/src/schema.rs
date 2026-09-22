//! The closed Chat Completions request schema (design §5.3).
//!
//! This module is deliberately self-contained: a **bounded** JSON parser (depth, value count,
//! duplicate keys and per-string size are enforced *while parsing*, before an unbounded generic
//! tree can be allocated) plus the closed allow-list validation and normalization into the exact
//! object the adapter re-serializes upstream.
//!
//! The schema is intentionally narrower than every API calling itself OpenAI-compatible. Unknown
//! top-level fields, alternate generation/token-limit aliases, and remote-fetch/file-upload content
//! forms are refused rather than forwarded (design §5.3.6, §5.3.7) — an opaque vendor extension
//! could raise hidden/output generation past a broker limit even when it cannot change the HTTP
//! destination.
//!
//! Every accepted field is fixture-backed by the PB0 capture (`harness/harness-spike/opencode/
//! broker/requests/*.json`): the measured top-level key set is exactly `model`, `messages`,
//! `max_tokens`, `stream`, `stream_options`, `tools`, `tool_choice`; the measured message roles are
//! `system`, `user`, `assistant`, `tool`; and the measured tool set is the ten function tools.

use std::collections::HashSet;

use serde_json::{Map, Value};

/// Parser/validation limits (design §5.3.8). These are compile-time, not operator-tunable.
pub const MAX_JSON_DEPTH: usize = 64;
/// Maximum total JSON values (scalars, objects, arrays) accepted in one request body.
pub const MAX_JSON_VALUES: usize = 100_000;
/// Maximum bytes in a single JSON string.
pub const MAX_STRING_BYTES: usize = 1024 * 1024;
/// Maximum messages in one request.
pub const MAX_MESSAGES: usize = 4_096;
/// Maximum tools in one request.
pub const MAX_TOOLS: usize = 256;

/// The top-level field allow-list (design §5.3.2). Anything else is refused.
const ALLOWED_TOP_LEVEL: &[&str] = &[
    "model",
    "messages",
    "max_tokens",
    "stream",
    "stream_options",
    "tools",
    "tool_choice",
];

/// Whether `name` is one of the closed top-level fields. The pinned PB0 fixtures are asserted against
/// this in `tests/loopback.rs`, so the allow-list cannot silently drift from the measured shapes.
pub fn top_level_field_allowed(name: &str) -> bool {
    ALLOWED_TOP_LEVEL.contains(&name)
}

/// The message roles the pinned fixtures exercise.
pub const ALLOWED_MESSAGE_ROLES: &[&str] = &["system", "user", "assistant", "tool"];

/// Generation/token-limit aliases that must never be forwarded even though they are not in the
/// allow-list, so the refusal names the exact control (design §5.3.6). Kept separate from the
/// generic unknown-field refusal purely for a precise diagnostic.
const GENERATION_CONTROL_ALIASES: &[&str] = &[
    "max_completion_tokens",
    "max_output_tokens",
    "n",
    "best_of",
    "best_of_n",
    "num_choices",
    "candidate_count",
];

/// Why a request body was refused. Every variant is a closed, non-secret reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaError {
    /// The body was empty or consisted only of whitespace.
    Empty,
    /// Bytes remained after the top-level value.
    TrailingData,
    /// The JSON was truncated.
    UnexpectedEof,
    /// A token appeared where the grammar did not allow it.
    UnexpectedToken,
    /// A number literal was malformed.
    InvalidNumber,
    /// A string escape was malformed.
    InvalidEscape,
    /// A `\u` escape was not a valid code point (bad surrogate pairing).
    InvalidUnicodeEscape,
    /// A raw control byte (U+0000..U+001F) appeared unescaped in a string.
    RawControlInString,
    /// Nesting exceeded [`MAX_JSON_DEPTH`].
    DepthExceeded,
    /// The value count exceeded [`MAX_JSON_VALUES`].
    ValueLimitExceeded,
    /// A string exceeded [`MAX_STRING_BYTES`].
    StringTooLong,
    /// An object repeated a key.
    DuplicateKey,
}

/// Why a syntactically valid JSON body was refused by the closed schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestRejection {
    /// The body was not syntactically valid bounded JSON.
    Malformed(SchemaError),
    /// The top level was not a JSON object.
    NotAnObject,
    /// A required field was missing (`messages`) or `model` was absent.
    MissingField,
    /// An unknown top-level field (or a named generation-control alias).
    UnknownField,
    /// `model` did not equal the grant's exact model.
    ModelMismatch,
    /// `max_tokens` was present but not a positive integer.
    InvalidMaxTokens,
    /// `stream` was present but not a boolean.
    InvalidStream,
    /// `stream_options` was malformed.
    InvalidStreamOptions,
    /// A message or its content was outside the pinned text/tool shapes.
    InvalidMessage,
    /// A remote-fetch/file-upload content form (`image_url`, file ids, input audio, ...) was used.
    RemoteContentRefused,
    /// A tool or tool_choice was outside the pinned function-tool shape.
    InvalidTool,
    /// More than [`MAX_MESSAGES`] messages or [`MAX_TOOLS`] tools.
    TooManyItems,
}

/// A parsed JSON value. Object order is preserved as written (the re-serialized body is built from
/// this tree, so the adapter's output order is deterministic).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Json {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    fn as_object(&self) -> Option<&[(String, Json)]> {
        match self {
            Json::Object(entries) => Some(entries),
            _ => None,
        }
    }

    fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(items) => Some(items),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(value) => Some(value),
            _ => None,
        }
    }

    fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(value) => Some(*value),
            _ => None,
        }
    }

    fn get(&self, key: &str) -> Option<&Json> {
        self.as_object()?
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    }

    /// Convert a bounded value into `serde_json::Value` for outbound re-serialization.
    fn to_value(&self) -> Value {
        match self {
            Json::Null => Value::Null,
            Json::Bool(value) => Value::Bool(*value),
            Json::Number(number) => number.to_value(),
            Json::String(value) => Value::String(value.clone()),
            Json::Array(items) => Value::Array(items.iter().map(Json::to_value).collect()),
            Json::Object(entries) => {
                let mut map = Map::new();
                for (key, value) in entries {
                    map.insert(key.clone(), value.to_value());
                }
                Value::Object(map)
            }
        }
    }
}

/// A JSON number, kept as its source text so integer checks are exact (no float rounding).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Number(String);

impl Number {
    /// The integer value, or `None` for a fractional/exponent form or an out-of-range value.
    fn as_u64(&self) -> Option<u64> {
        // JSON integer: no `.`, no `e`/`E`, no leading `-` (a `max_tokens` must be positive).
        if self
            .0
            .bytes()
            .any(|byte| matches!(byte, b'.' | b'e' | b'E' | b'-'))
        {
            return None;
        }
        self.0.parse::<u64>().ok()
    }

    fn to_value(&self) -> Value {
        match self.0.parse::<serde_json::Number>() {
            Ok(number) => Value::Number(number),
            // The parser only accepts a syntactically valid JSON number, so this should not happen;
            // a safe non-panicking fallback keeps a malformed value from aborting the daemon.
            Err(_) => Value::Null,
        }
    }
}

/// Parse a bounded JSON value from a UTF-8 byte slice, rejecting anything past the structural
/// limits in this module.
pub(crate) fn parse_bounded(input: &[u8]) -> Result<Json, SchemaError> {
    let text = std::str::from_utf8(input).map_err(|_| SchemaError::UnexpectedToken)?;
    let mut parser = Parser {
        bytes: text.as_bytes(),
        pos: 0,
        values: 0,
    };
    parser.skip_whitespace();
    if parser.pos >= parser.bytes.len() {
        return Err(SchemaError::Empty);
    }
    let value = parser.parse_value(0)?;
    parser.skip_whitespace();
    if parser.pos != parser.bytes.len() {
        return Err(SchemaError::TrailingData);
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
    values: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn count_value(&mut self) -> Result<(), SchemaError> {
        self.values += 1;
        if self.values > MAX_JSON_VALUES {
            return Err(SchemaError::ValueLimitExceeded);
        }
        Ok(())
    }

    fn parse_value(&mut self, depth: usize) -> Result<Json, SchemaError> {
        if depth > MAX_JSON_DEPTH {
            return Err(SchemaError::DepthExceeded);
        }
        self.skip_whitespace();
        let byte = self.peek().ok_or(SchemaError::UnexpectedEof)?;
        match byte {
            b'{' => self.parse_object(depth + 1),
            b'[' => self.parse_array(depth + 1),
            b'"' => {
                self.count_value()?;
                Ok(Json::String(self.parse_string()?))
            }
            b't' => {
                self.expect_literal(b"true")?;
                self.count_value()?;
                Ok(Json::Bool(true))
            }
            b'f' => {
                self.expect_literal(b"false")?;
                self.count_value()?;
                Ok(Json::Bool(false))
            }
            b'n' => {
                self.expect_literal(b"null")?;
                self.count_value()?;
                Ok(Json::Null)
            }
            b'-' | b'0'..=b'9' => {
                self.count_value()?;
                Ok(Json::Number(self.parse_number()?))
            }
            _ => Err(SchemaError::UnexpectedToken),
        }
    }

    fn expect_literal(&mut self, literal: &[u8]) -> Result<(), SchemaError> {
        if self.bytes[self.pos..].starts_with(literal) {
            self.pos += literal.len();
            Ok(())
        } else {
            Err(SchemaError::UnexpectedToken)
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<Json, SchemaError> {
        if depth > MAX_JSON_DEPTH {
            return Err(SchemaError::DepthExceeded);
        }
        self.count_value()?;
        self.pos += 1; // consume `{`
        let mut entries: Vec<(String, Json)> = Vec::new();
        // A per-object hash set keeps duplicate detection O(n) instead of comparing every new key
        // against every earlier one (design §5.3.8).
        let mut seen: HashSet<String> = HashSet::new();
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Object(entries));
        }
        loop {
            self.skip_whitespace();
            if self.peek() != Some(b'"') {
                return Err(SchemaError::UnexpectedToken);
            }
            let key = self.parse_string()?;
            if !seen.insert(key.clone()) {
                return Err(SchemaError::DuplicateKey);
            }
            self.skip_whitespace();
            if self.peek() != Some(b':') {
                return Err(SchemaError::UnexpectedToken);
            }
            self.pos += 1;
            let value = self.parse_value(depth)?;
            entries.push((key, value));
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Object(entries));
                }
                _ => return Err(SchemaError::UnexpectedToken),
            }
        }
    }

    fn parse_array(&mut self, depth: usize) -> Result<Json, SchemaError> {
        if depth > MAX_JSON_DEPTH {
            return Err(SchemaError::DepthExceeded);
        }
        self.count_value()?;
        self.pos += 1; // consume `[`
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Array(items));
        }
        loop {
            let value = self.parse_value(depth)?;
            items.push(value);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Array(items));
                }
                _ => return Err(SchemaError::UnexpectedToken),
            }
        }
    }

    fn parse_number(&mut self) -> Result<Number, SchemaError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        // Integer part.
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(SchemaError::InvalidNumber),
        }
        // Fraction.
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(SchemaError::InvalidNumber);
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        // Exponent.
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(SchemaError::InvalidNumber);
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| SchemaError::InvalidNumber)?;
        // Refuse a number that `serde_json` cannot represent without changing it: a huge integer
        // would otherwise re-serialize as a float and a huge exponent as `null`, so the forwarded
        // body would silently differ from the validated one (design §5.3.9).
        if !number_round_trips(text) {
            return Err(SchemaError::InvalidNumber);
        }
        Ok(Number(text.to_owned()))
    }

    fn parse_string(&mut self) -> Result<String, SchemaError> {
        self.pos += 1; // consume opening quote
        let mut out = String::new();
        loop {
            let byte = self.peek().ok_or(SchemaError::UnexpectedEof)?;
            match byte {
                b'"' => {
                    self.pos += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.pos += 1;
                    self.parse_escape(&mut out)?;
                }
                0x00..=0x1F => return Err(SchemaError::RawControlInString),
                _ => {
                    // Copy one UTF-8 code point. `text` was validated as UTF-8 up front, so the
                    // multibyte lead byte dictates the width.
                    let width = utf8_width(byte);
                    let end = self.pos + width;
                    if end > self.bytes.len() {
                        return Err(SchemaError::UnexpectedEof);
                    }
                    let piece = std::str::from_utf8(&self.bytes[self.pos..end])
                        .map_err(|_| SchemaError::UnexpectedToken)?;
                    out.push_str(piece);
                    self.pos = end;
                }
            }
            if out.len() > MAX_STRING_BYTES {
                return Err(SchemaError::StringTooLong);
            }
        }
    }

    fn parse_escape(&mut self, out: &mut String) -> Result<(), SchemaError> {
        let escape = self.peek().ok_or(SchemaError::UnexpectedEof)?;
        self.pos += 1;
        match escape {
            b'"' => out.push('"'),
            b'\\' => out.push('\\'),
            b'/' => out.push('/'),
            b'b' => out.push('\u{0008}'),
            b'f' => out.push('\u{000C}'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'u' => {
                let first = self.parse_hex4()?;
                let code = if (0xD800..=0xDBFF).contains(&first) {
                    // A high surrogate must be followed by a low surrogate.
                    if self.peek() != Some(b'\\') {
                        return Err(SchemaError::InvalidUnicodeEscape);
                    }
                    self.pos += 1;
                    if self.peek() != Some(b'u') {
                        return Err(SchemaError::InvalidUnicodeEscape);
                    }
                    self.pos += 1;
                    let second = self.parse_hex4()?;
                    if !(0xDC00..=0xDFFF).contains(&second) {
                        return Err(SchemaError::InvalidUnicodeEscape);
                    }
                    0x1_0000 + ((u32::from(first) - 0xD800) << 10) + (u32::from(second) - 0xDC00)
                } else if (0xDC00..=0xDFFF).contains(&first) {
                    return Err(SchemaError::InvalidUnicodeEscape);
                } else {
                    u32::from(first)
                };
                out.push(char::from_u32(code).ok_or(SchemaError::InvalidUnicodeEscape)?);
            }
            _ => return Err(SchemaError::InvalidEscape),
        }
        Ok(())
    }

    fn parse_hex4(&mut self) -> Result<u16, SchemaError> {
        if self.pos + 4 > self.bytes.len() {
            return Err(SchemaError::UnexpectedEof);
        }
        let digits = &self.bytes[self.pos..self.pos + 4];
        let mut value: u16 = 0;
        for &digit in digits {
            let nibble = match digit {
                b'0'..=b'9' => digit - b'0',
                b'a'..=b'f' => digit - b'a' + 10,
                b'A'..=b'F' => digit - b'A' + 10,
                _ => return Err(SchemaError::InvalidUnicodeEscape),
            };
            value = (value << 4) | u16::from(nibble);
        }
        self.pos += 4;
        Ok(value)
    }
}

/// Whether a JSON number literal survives a `serde_json` parse/serialize round-trip unchanged, so
/// the re-serialized outbound body is exactly the validated value.
fn number_round_trips(text: &str) -> bool {
    match text.parse::<serde_json::Number>() {
        Ok(number) => number.to_string() == text,
        Err(_) => false,
    }
}

fn utf8_width(lead: u8) -> usize {
    if lead < 0x80 {
        1
    } else if lead >> 5 == 0b110 {
        2
    } else if lead >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

/// The grant-derived policy the schema validates against.
#[derive(Debug, Clone, Copy)]
pub struct ChatRequestPolicy<'a> {
    /// The exact model id the grant accepts.
    pub model: &'a str,
    /// The grant's per-request output-token maximum.
    pub max_output_tokens: u64,
}

/// The validated + normalized request the adapter re-serializes upstream.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatRequest {
    /// The exact accepted model.
    pub model: String,
    /// Whether the child asked for a streaming response.
    pub stream: bool,
    /// The effective `max_tokens` (inserted or clamped to the grant maximum).
    pub max_tokens: u64,
    /// Validated message objects, preserved as written.
    messages: Vec<Json>,
    /// Validated tool definitions, when present.
    tools: Option<Vec<Json>>,
    /// Validated tool choice, when present.
    tool_choice: Option<Json>,
}

impl ChatRequest {
    /// Serialize the normalized request. `max_tokens` is always present, and a streaming request
    /// always carries `stream_options.include_usage = true` (design §5.3.4, §5.3.5).
    pub fn to_json_bytes(&self) -> Vec<u8> {
        let mut map = Map::new();
        map.insert("model".to_owned(), Value::String(self.model.clone()));
        map.insert(
            "messages".to_owned(),
            Value::Array(self.messages.iter().map(Json::to_value).collect()),
        );
        map.insert(
            "max_tokens".to_owned(),
            Value::Number(self.max_tokens.into()),
        );
        map.insert("stream".to_owned(), Value::Bool(self.stream));
        if self.stream {
            let mut options = Map::new();
            options.insert("include_usage".to_owned(), Value::Bool(true));
            map.insert("stream_options".to_owned(), Value::Object(options));
        }
        if let Some(tools) = &self.tools {
            map.insert(
                "tools".to_owned(),
                Value::Array(tools.iter().map(Json::to_value).collect()),
            );
        }
        if let Some(choice) = &self.tool_choice {
            map.insert("tool_choice".to_owned(), choice.to_value());
        }
        // A `Value::Object` from `Map` serializes without failure for these types.
        serde_json::to_vec(&Value::Object(map)).unwrap_or_default()
    }
}

/// Parse and validate a request body against the closed schema.
pub fn validate_chat_request(
    body: &[u8],
    policy: ChatRequestPolicy<'_>,
) -> Result<ChatRequest, RequestRejection> {
    let root = parse_bounded(body).map_err(RequestRejection::Malformed)?;
    let entries = root.as_object().ok_or(RequestRejection::NotAnObject)?;

    for (key, _) in entries {
        if ALLOWED_TOP_LEVEL.contains(&key.as_str()) {
            continue;
        }
        if GENERATION_CONTROL_ALIASES.contains(&key.as_str()) {
            return Err(RequestRejection::UnknownField);
        }
        return Err(RequestRejection::UnknownField);
    }

    let model = root.get("model").and_then(Json::as_str);
    let model = model.ok_or(RequestRejection::MissingField)?;
    if model != policy.model {
        return Err(RequestRejection::ModelMismatch);
    }

    let messages = root
        .get("messages")
        .and_then(Json::as_array)
        .ok_or(RequestRejection::MissingField)?;
    if messages.len() > MAX_MESSAGES {
        return Err(RequestRejection::TooManyItems);
    }
    for message in messages {
        validate_message(message)?;
    }

    let max_tokens = match root.get("max_tokens") {
        Some(Json::Number(number)) => number
            .as_u64()
            .filter(|value| *value > 0)
            .ok_or(RequestRejection::InvalidMaxTokens)?,
        Some(_) => return Err(RequestRejection::InvalidMaxTokens),
        None => policy.max_output_tokens,
    };
    let max_tokens = max_tokens.min(policy.max_output_tokens);

    let stream = match root.get("stream") {
        Some(value) => value.as_bool().ok_or(RequestRejection::InvalidStream)?,
        None => false,
    };

    if let Some(options) = root.get("stream_options") {
        validate_stream_options(options)?;
    }

    let tools = match root.get("tools") {
        Some(Json::Array(items)) => {
            if items.len() > MAX_TOOLS {
                return Err(RequestRejection::TooManyItems);
            }
            for tool in items {
                validate_tool(tool)?;
            }
            Some(items.clone())
        }
        Some(_) => return Err(RequestRejection::InvalidTool),
        None => None,
    };

    let tool_choice = match root.get("tool_choice") {
        Some(value) => {
            validate_tool_choice(value)?;
            Some(value.clone())
        }
        None => None,
    };

    Ok(ChatRequest {
        model: model.to_owned(),
        stream,
        max_tokens,
        messages: messages.to_vec(),
        tools,
        tool_choice,
    })
}

fn validate_stream_options(value: &Json) -> Result<(), RequestRejection> {
    let entries = value
        .as_object()
        .ok_or(RequestRejection::InvalidStreamOptions)?;
    for (key, entry) in entries {
        if key != "include_usage" {
            return Err(RequestRejection::InvalidStreamOptions);
        }
        entry
            .as_bool()
            .ok_or(RequestRejection::InvalidStreamOptions)?;
    }
    Ok(())
}

/// Content part types refused in v1 because they are remote-fetch/file-upload forms needing a
/// separate data-egress review (design §5.3.7). Named so the refusal can be the precise one.
const REMOTE_CONTENT_TYPES: &[&str] = &[
    "image_url",
    "image",
    "input_image",
    "file",
    "file_id",
    "input_file",
    "input_audio",
    "audio",
    "input_audio_url",
    "video",
];

fn validate_message(message: &Json) -> Result<(), RequestRejection> {
    let entries = message
        .as_object()
        .ok_or(RequestRejection::InvalidMessage)?;
    let mut role: Option<&str> = None;
    for (key, value) in entries {
        match key.as_str() {
            "role" => role = Some(value.as_str().ok_or(RequestRejection::InvalidMessage)?),
            "content" => validate_content(value)?,
            "tool_calls" => validate_tool_calls(value)?,
            "tool_call_id" => {
                value.as_str().ok_or(RequestRejection::InvalidMessage)?;
            }
            _ => return Err(RequestRejection::InvalidMessage),
        }
    }
    let role = role.ok_or(RequestRejection::InvalidMessage)?;
    if !matches!(role, "system" | "user" | "assistant" | "tool") {
        return Err(RequestRejection::InvalidMessage);
    }
    Ok(())
}

fn validate_content(content: &Json) -> Result<(), RequestRejection> {
    match content {
        // `null` is the pinned assistant-with-tool-calls shape; a bare string is the text shape.
        Json::Null | Json::String(_) => Ok(()),
        Json::Array(parts) => {
            for part in parts {
                validate_content_part(part)?;
            }
            Ok(())
        }
        _ => Err(RequestRejection::InvalidMessage),
    }
}

fn validate_content_part(part: &Json) -> Result<(), RequestRejection> {
    let entries = part.as_object().ok_or(RequestRejection::InvalidMessage)?;
    let kind = entries
        .iter()
        .find(|(key, _)| key == "type")
        .and_then(|(_, value)| value.as_str())
        .ok_or(RequestRejection::InvalidMessage)?;
    if REMOTE_CONTENT_TYPES.contains(&kind) {
        return Err(RequestRejection::RemoteContentRefused);
    }
    match kind {
        "text" => {
            for (key, value) in entries {
                match key.as_str() {
                    "type" => {}
                    "text" => {
                        value.as_str().ok_or(RequestRejection::InvalidMessage)?;
                    }
                    _ => return Err(RequestRejection::InvalidMessage),
                }
            }
            Ok(())
        }
        _ => Err(RequestRejection::InvalidMessage),
    }
}

fn validate_tool_calls(value: &Json) -> Result<(), RequestRejection> {
    let calls = value.as_array().ok_or(RequestRejection::InvalidMessage)?;
    for call in calls {
        let entries = call.as_object().ok_or(RequestRejection::InvalidMessage)?;
        for (key, entry) in entries {
            match key.as_str() {
                "id" => {
                    entry.as_str().ok_or(RequestRejection::InvalidMessage)?;
                }
                "type" => {
                    if entry.as_str() != Some("function") {
                        return Err(RequestRejection::InvalidMessage);
                    }
                }
                "function" => validate_function_call(entry)?,
                _ => return Err(RequestRejection::InvalidMessage),
            }
        }
    }
    Ok(())
}

fn validate_function_call(value: &Json) -> Result<(), RequestRejection> {
    let entries = value.as_object().ok_or(RequestRejection::InvalidMessage)?;
    for (key, entry) in entries {
        match key.as_str() {
            "name" => {
                entry.as_str().ok_or(RequestRejection::InvalidMessage)?;
            }
            "arguments" => {
                entry.as_str().ok_or(RequestRejection::InvalidMessage)?;
            }
            _ => return Err(RequestRejection::InvalidMessage),
        }
    }
    Ok(())
}

fn validate_tool(tool: &Json) -> Result<(), RequestRejection> {
    let entries = tool.as_object().ok_or(RequestRejection::InvalidTool)?;
    for (key, value) in entries {
        match key.as_str() {
            "type" => {
                if value.as_str() != Some("function") {
                    return Err(RequestRejection::InvalidTool);
                }
            }
            "function" => validate_function_schema(value)?,
            _ => return Err(RequestRejection::InvalidTool),
        }
    }
    Ok(())
}

fn validate_function_schema(value: &Json) -> Result<(), RequestRejection> {
    let entries = value.as_object().ok_or(RequestRejection::InvalidTool)?;
    for (key, entry) in entries {
        match key.as_str() {
            "name" => {
                entry.as_str().ok_or(RequestRejection::InvalidTool)?;
            }
            "description" => {
                entry.as_str().ok_or(RequestRejection::InvalidTool)?;
            }
            // `parameters` is an opaque JSON-schema object from the pinned tool set; it is still
            // bounded by the parser, but its interior shape is not part of the broker contract.
            "parameters" => {
                entry.as_object().ok_or(RequestRejection::InvalidTool)?;
            }
            _ => return Err(RequestRejection::InvalidTool),
        }
    }
    Ok(())
}

fn validate_tool_choice(value: &Json) -> Result<(), RequestRejection> {
    match value {
        Json::String(choice) => {
            if matches!(choice.as_str(), "auto" | "none" | "required") {
                Ok(())
            } else {
                Err(RequestRejection::InvalidTool)
            }
        }
        Json::Object(entries) => {
            let mut has_type = false;
            let mut has_function = false;
            for (key, entry) in entries {
                match key.as_str() {
                    "type" => {
                        if entry.as_str() != Some("function") {
                            return Err(RequestRejection::InvalidTool);
                        }
                        has_type = true;
                    }
                    "function" => {
                        let function = entry.as_object().ok_or(RequestRejection::InvalidTool)?;
                        for (name, value) in function {
                            if name.as_str() != "name" {
                                return Err(RequestRejection::InvalidTool);
                            }
                            value.as_str().ok_or(RequestRejection::InvalidTool)?;
                        }
                        has_function = true;
                    }
                    _ => return Err(RequestRejection::InvalidTool),
                }
            }
            if has_type && has_function {
                Ok(())
            } else {
                Err(RequestRejection::InvalidTool)
            }
        }
        _ => Err(RequestRejection::InvalidTool),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> ChatRequestPolicy<'static> {
        ChatRequestPolicy {
            model: "probe-model",
            max_output_tokens: 32_000,
        }
    }

    fn parse(text: &str) -> Json {
        parse_bounded(text.as_bytes()).expect("valid bounded json")
    }

    #[test]
    fn parses_scalars_and_nesting() {
        assert_eq!(parse("null"), Json::Null);
        assert_eq!(parse("true"), Json::Bool(true));
        assert_eq!(parse("[]"), Json::Array(vec![]));
        assert_eq!(parse("{}"), Json::Object(vec![]));
        assert_eq!(
            parse(r#"{"a":[1,"x",{"b":null}]}"#),
            Json::Object(vec![(
                "a".to_owned(),
                Json::Array(vec![
                    Json::Number(Number("1".to_owned())),
                    Json::String("x".to_owned()),
                    Json::Object(vec![("b".to_owned(), Json::Null)]),
                ])
            )])
        );
    }

    #[test]
    fn rejects_duplicate_object_keys() {
        assert_eq!(
            parse_bounded(br#"{"a":1,"a":2}"#),
            Err(SchemaError::DuplicateKey)
        );
    }

    #[test]
    fn many_distinct_keys_parse_in_linear_time() {
        // Duplicate detection must be a hash set, not a comparison against every earlier key. A
        // quadratic implementation is visibly slower here (minutes in a debug build).
        let count = 50_000usize;
        let mut body = String::from("{");
        for index in 0..count {
            if index > 0 {
                body.push(',');
            }
            body.push_str(&format!("\"k{index}\":0"));
        }
        body.push('}');
        let started = std::time::Instant::now();
        let parsed = parse_bounded(body.as_bytes()).expect("distinct keys parse");
        assert!(matches!(parsed, Json::Object(entries) if entries.len() == count));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "duplicate detection must be linear, took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn non_representable_numbers_are_refused_not_silently_rewritten() {
        // A huge integer would re-serialize as a float and a huge exponent as `null`.
        assert_eq!(
            parse_bounded(b"123456789012345678901234567890"),
            Err(SchemaError::InvalidNumber)
        );
        assert_eq!(parse_bounded(b"1e400"), Err(SchemaError::InvalidNumber));
        // Representable numbers still parse exactly.
        assert_eq!(
            parse_bounded(b"32000"),
            Ok(Json::Number(Number("32000".to_owned())))
        );
        assert_eq!(
            parse_bounded(b"1.5"),
            Ok(Json::Number(Number("1.5".to_owned())))
        );
    }

    #[test]
    fn a_non_representable_parameters_number_is_refused() {
        let body = br#"{"messages":[],"model":"probe-model","tools":[{"type":"function","function":{"name":"f","parameters":{"a":1e400}}}]}"#;
        assert_eq!(
            validate_chat_request(body, policy()),
            Err(RequestRejection::Malformed(SchemaError::InvalidNumber))
        );
    }

    #[test]
    fn rejects_trailing_data_and_empty_and_malformed() {
        assert_eq!(parse_bounded(b"   "), Err(SchemaError::Empty));
        assert_eq!(parse_bounded(b"{} {}"), Err(SchemaError::TrailingData));
        assert_eq!(
            parse_bounded(b"{\"a\":}"),
            Err(SchemaError::UnexpectedToken)
        );
        assert_eq!(parse_bounded(b"01"), Err(SchemaError::TrailingData));
    }

    #[test]
    fn rejects_depth_over_limit() {
        let deep = format!("{}0{}", "[".repeat(70), "]".repeat(70));
        assert_eq!(
            parse_bounded(deep.as_bytes()),
            Err(SchemaError::DepthExceeded)
        );
    }

    #[test]
    fn rejects_raw_control_and_bad_escapes() {
        assert_eq!(
            parse_bounded(b"\"a\nb\""),
            Err(SchemaError::RawControlInString)
        );
        assert_eq!(parse_bounded(b"\"\\x\""), Err(SchemaError::InvalidEscape));
        assert_eq!(
            parse_bounded(b"\"\\uD800\""),
            Err(SchemaError::InvalidUnicodeEscape)
        );
    }

    #[test]
    fn decodes_escapes_and_surrogate_pairs() {
        assert_eq!(
            parse(r#""\u0041\u00e9\ud83d\ude00""#),
            Json::String("Aé😀".to_owned())
        );
        assert_eq!(
            parse(r#""a\nb\tc\\d\"e""#),
            Json::String("a\nb\tc\\d\"e".to_owned())
        );
    }

    #[test]
    fn rejects_oversized_string() {
        let big = format!("\"{}\"", "a".repeat(MAX_STRING_BYTES + 1));
        assert_eq!(
            parse_bounded(big.as_bytes()),
            Err(SchemaError::StringTooLong)
        );
    }

    #[test]
    fn accepts_the_pinned_happy_shape_and_normalizes() {
        let body = br#"{"max_tokens":32000,"messages":[{"role":"system","content":"s"},{"role":"user","content":"u"}],"model":"probe-model","stream":true,"stream_options":{"include_usage":true},"tool_choice":"auto","tools":[{"type":"function","function":{"name":"bash","description":"run","parameters":{"type":"object"}}}]}"#;
        let request = validate_chat_request(body, policy()).expect("valid");
        assert!(request.stream);
        assert_eq!(request.max_tokens, 32_000);
        let out = request.to_json_bytes();
        let reparsed: Value = serde_json::from_slice(&out).expect("json");
        assert_eq!(
            reparsed["stream_options"]["include_usage"],
            Value::Bool(true)
        );
        assert_eq!(reparsed["model"], Value::String("probe-model".to_owned()));
    }

    #[test]
    fn absent_max_tokens_inserts_the_grant_maximum_and_larger_clamps() {
        let absent = br#"{"messages":[],"model":"probe-model"}"#;
        let request = validate_chat_request(absent, policy()).expect("valid");
        assert_eq!(request.max_tokens, 32_000);

        let larger = br#"{"messages":[],"model":"probe-model","max_tokens":999999}"#;
        let request = validate_chat_request(larger, policy()).expect("valid");
        assert_eq!(request.max_tokens, 32_000);
    }

    #[test]
    fn non_positive_or_fractional_max_tokens_is_refused() {
        for body in [
            br#"{"messages":[],"model":"probe-model","max_tokens":0}"#.as_slice(),
            br#"{"messages":[],"model":"probe-model","max_tokens":-1}"#.as_slice(),
            br#"{"messages":[],"model":"probe-model","max_tokens":1.5}"#.as_slice(),
            br#"{"messages":[],"model":"probe-model","max_tokens":"32000"}"#.as_slice(),
        ] {
            assert_eq!(
                validate_chat_request(body, policy()),
                Err(RequestRejection::InvalidMaxTokens)
            );
        }
    }

    #[test]
    fn unknown_generation_controls_are_refused() {
        for field in [
            "max_completion_tokens",
            "max_output_tokens",
            "n",
            "best_of",
            "temperature",
        ] {
            let body = format!(r#"{{"messages":[],"model":"probe-model","{field}":1}}"#);
            assert_eq!(
                validate_chat_request(body.as_bytes(), policy()),
                Err(RequestRejection::UnknownField),
                "{field} must be refused"
            );
        }
    }

    #[test]
    fn model_mismatch_is_refused_not_routed() {
        let body = br#"{"messages":[],"model":"other-model"}"#;
        assert_eq!(
            validate_chat_request(body, policy()),
            Err(RequestRejection::ModelMismatch)
        );
    }

    #[test]
    fn remote_fetch_content_forms_are_refused() {
        let body = br#"{"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"http://x"}}]}],"model":"probe-model"}"#;
        assert_eq!(
            validate_chat_request(body, policy()),
            Err(RequestRejection::RemoteContentRefused)
        );
    }

    #[test]
    fn alternate_tool_types_are_refused() {
        let body = br#"{"messages":[],"model":"probe-model","tools":[{"type":"web_search"}]}"#;
        assert_eq!(
            validate_chat_request(body, policy()),
            Err(RequestRejection::InvalidTool)
        );
    }

    #[test]
    fn missing_messages_or_model_is_refused() {
        assert_eq!(
            validate_chat_request(br#"{"model":"probe-model"}"#, policy()),
            Err(RequestRejection::MissingField)
        );
        assert_eq!(
            validate_chat_request(br#"{"messages":[]}"#, policy()),
            Err(RequestRejection::MissingField)
        );
        assert_eq!(
            validate_chat_request(br#"[]"#, policy()),
            Err(RequestRejection::NotAnObject)
        );
    }
}
