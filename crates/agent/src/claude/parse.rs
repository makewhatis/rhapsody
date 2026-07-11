//! stream-json line parsing — parity port of Go `parse.go`.
//!
//! [`classify`] maps one `claude` stream-json line to a normalized [`crate::Event`] (plus the
//! session id, terminal flag, [`crate::TurnResult`], and the `apiKeySource` the billing guard
//! needs). It is lenient: blank lines, non-JSON, and event types we don't surface produce
//! `ok == false`. Usage extraction keeps `input`/`output` as the UNCACHED counts while
//! `total_tokens` is the BILLED total (uncached in + out + cache-creation + cache-read).

use chrono::Utc;
use serde::Deserialize;

use crate::{
    EVENT_NOTIFICATION, EVENT_SESSION_STARTED, EVENT_TURN_COMPLETED, EVENT_TURN_FAILED, Event,
    TURN_FAILED, TURN_SUCCEEDED, TurnResult, Usage,
};

/// Bounds the assistant-notification text surfaced on [`crate::Event::message`].
const MAX_MESSAGE_LEN: usize = 2048;

/// Bounds the final result text surfaced on [`crate::TurnResult::result_text`]. The TAIL is kept
/// (the `HANDOFF:` marker is the last line); see [`classify`]'s `result` case.
const MAX_RESULT_TEXT: usize = 4096;

/// One classified stream-json line — the parity mirror of Go's `classifyWithAPIKeySource` return
/// tuple `(ev, sessionID, terminal, tr, ok, apiKeySource)`.
///
/// `session_id` and `api_key_source` are surfaced even when `ok == false` (Go returns the session
/// id on non-surfaced lines so the runner can still seed its thread id from a system-noninit or
/// unknown line).
#[derive(Debug, Clone, Default)]
pub struct Classified {
    pub event: Event,
    pub session_id: String,
    pub terminal: bool,
    pub result: TurnResult,
    /// `false` for blank lines, non-JSON, or event types we don't surface (Go's `ok`).
    pub ok: bool,
    /// The `apiKeySource` carried on the system/init line (empty for every other line).
    pub api_key_source: String,
}

/// A lenient view of one claude stream-json event (Go `rawLine`). Every field is optional via the
/// container-level `#[serde(default)]`, so an absent field contributes its zero value.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawLine {
    #[serde(rename = "type")]
    r#type: String,
    subtype: String,
    session_id: String,
    is_error: bool,
    result: String,
    /// top-level usage (present on `result` lines)
    usage: Option<RawUsage>,
    /// nested message (present on `assistant` lines; carries its own per-call usage)
    message: Option<RawMessage>,
    /// present on the system/init line; the billing guard asserts it equals `"none"`.
    #[serde(rename = "apiKeySource")]
    api_key_source: String,
}

#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(default)]
struct RawUsage {
    input_tokens: i64,
    output_tokens: i64,
    cache_creation_input_tokens: i64,
    cache_read_input_tokens: i64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawMessage {
    usage: Option<RawUsage>,
    content: Vec<RawContentBlock>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawContentBlock {
    #[serde(rename = "type")]
    r#type: String,
    text: String,
}

/// Maps one stream-json line to a normalized event (Go `classify`/`classifyWithAPIKeySource`).
pub fn classify(line: &[u8]) -> Classified {
    if String::from_utf8_lossy(line).trim().is_empty() {
        return Classified::default();
    }
    let r: RawLine = match serde_json::from_slice(line) {
        Ok(r) => r,
        Err(_) => return Classified::default(),
    };
    let now = Some(Utc::now());

    match r.r#type.as_str() {
        "system" => {
            if r.subtype != "init" {
                // Not surfaced, but the session id is still carried (mirrors Go).
                return Classified {
                    session_id: r.session_id,
                    ..Default::default()
                };
            }
            Classified {
                event: Event {
                    event_type: EVENT_SESSION_STARTED.to_string(),
                    timestamp: now,
                    message: r.session_id.clone(),
                    ..Default::default()
                },
                session_id: r.session_id,
                ok: true,
                api_key_source: r.api_key_source,
                ..Default::default()
            }
        }
        "assistant" => {
            // Each assistant message carries its own per-call message.usage. Surface it as a LIVE
            // in-turn usage estimate (the authoritative per-turn total still arrives on the result
            // event, which the orchestrator commits — so there is no double-count).
            let (text, usage) = assistant_text_and_usage(r.message.as_ref());
            let mut event = Event {
                event_type: EVENT_NOTIFICATION.to_string(),
                timestamp: now,
                message: text,
                ..Default::default()
            };
            if let Some(u) = usage {
                event.usage = Some(usage_from_raw(&u));
            }
            Classified {
                event,
                session_id: r.session_id,
                ok: true,
                ..Default::default()
            }
        }
        "result" => {
            let usage = r.usage.map(|u| usage_from_raw(&u)).unwrap_or_default();
            // Surface the final result text so the orchestrator can detect the agent's HANDOFF:
            // declaration. Keep the TAIL — the marker is the last line.
            let text = truncate_tail(&r.result, MAX_RESULT_TEXT);
            let (event_type, status) = if r.is_error {
                (EVENT_TURN_FAILED, TURN_FAILED)
            } else {
                (EVENT_TURN_COMPLETED, TURN_SUCCEEDED)
            };
            Classified {
                event: Event {
                    event_type: event_type.to_string(),
                    timestamp: now,
                    message: r.subtype.clone(),
                    usage: Some(usage),
                    ..Default::default()
                },
                session_id: r.session_id,
                terminal: true,
                result: TurnResult {
                    status: status.to_string(),
                    usage,
                    result_text: text,
                },
                ok: true,
                ..Default::default()
            }
        }
        _ => Classified {
            session_id: r.session_id,
            ..Default::default()
        },
    }
}

/// Converts a stream-json usage object into the normalized [`Usage`]. `input`/`output` stay the
/// UNCACHED input/output (so the "(in/out)" breakdown keeps its meaning), while `total_tokens` is
/// the BILLED total = uncached input + output + cache_creation + cache_read.
fn usage_from_raw(u: &RawUsage) -> Usage {
    Usage {
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        cache_creation_tokens: u.cache_creation_input_tokens,
        cache_read_tokens: u.cache_read_input_tokens,
        total_tokens: u.input_tokens
            + u.output_tokens
            + u.cache_creation_input_tokens
            + u.cache_read_input_tokens,
    }
}

/// Joins the text content blocks of an assistant message (truncated to [`MAX_MESSAGE_LEN`]) and
/// returns its per-call `message.usage` (`None` when absent).
fn assistant_text_and_usage(msg: Option<&RawMessage>) -> (String, Option<RawUsage>) {
    let Some(m) = msg else {
        return (String::new(), None);
    };
    let mut s = String::new();
    for c in &m.content {
        if c.r#type == "text" {
            s.push_str(&c.text);
        }
    }
    (truncate_head(&s, MAX_MESSAGE_LEN), m.usage)
}

/// Head-truncates `s` to at most `max` BYTES + a marker, mirroring Go's `s[:maxMessageLen] +
/// "...(truncated)"`. Go slices raw bytes; a Rust `String` can't hold a split rune, so the cut
/// backs off to the largest char boundary ≤ `max` (drops a partial trailing rune). The truncation
/// branch is untested in Go and only reached by >2KB assistant prose.
fn truncate_head(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...(truncated)", &s[..end])
}

/// Tail-truncates `s` to at most `max` BYTES, mirroring Go's `text[len(text)-maxResultText:]` plus
/// the leading-continuation-byte drop that keeps the surfaced tail valid UTF-8 (the `HANDOFF:`
/// marker lives at the end, so head-truncating would lose it).
fn truncate_tail(s: &str, max: usize) -> String {
    let bytes = s.as_bytes();
    if bytes.len() <= max {
        return s.to_string();
    }
    let mut start = bytes.len() - max;
    // Drop leading UTF-8 continuation bytes (0b10xx_xxxx) so the tail begins on a char boundary.
    while start < bytes.len() && (bytes[start] & 0xC0) == 0x80 {
        start += 1;
    }
    // `bytes[start..]` is now valid UTF-8 (we only trimmed a partial leading rune from a valid
    // string), so the lossy conversion reproduces it byte-for-byte without replacement chars.
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors Go `claude.TestClassifyInitEvent`.
    #[test]
    fn classify_init_event() {
        let c = classify(br#"{"type":"system","subtype":"init","session_id":"sess-1"}"#);
        assert!(c.ok);
        assert_eq!(c.event.event_type, EVENT_SESSION_STARTED);
        assert_eq!(c.session_id, "sess-1");
        assert!(!c.terminal, "init is not terminal");
    }

    // Mirrors Go `claude.TestClassifyInitSurfacesAPIKeySource`.
    #[test]
    fn classify_init_surfaces_api_key_source() {
        let c = classify(
            br#"{"type":"system","subtype":"init","session_id":"sess-1","apiKeySource":"none"}"#,
        );
        assert!(c.ok, "init should classify");
        assert_eq!(c.api_key_source, "none");
        // A non-init/non-system line carries no apiKeySource.
        let c2 = classify(br#"{"type":"result","subtype":"success","session_id":"s"}"#);
        assert_eq!(
            c2.api_key_source, "",
            "non-init apiKeySource should be empty"
        );
    }

    // Mirrors Go `claude.TestClassifyAssistantNotification`.
    #[test]
    fn classify_assistant_notification() {
        let line = br#"{"type":"assistant","session_id":"s","message":{"content":[{"type":"text","text":"hello "},{"type":"text","text":"world"}]}}"#;
        let c = classify(line);
        assert!(c.ok);
        assert_eq!(c.event.event_type, EVENT_NOTIFICATION);
        assert!(!c.terminal, "assistant is not terminal");
        assert_eq!(c.event.message, "hello world");
    }

    // Mirrors Go `claude.TestClassifyAssistantUsage`.
    #[test]
    fn classify_assistant_usage() {
        let line = br#"{"type":"assistant","session_id":"s","message":{"usage":{"input_tokens":50,"output_tokens":10},"content":[{"type":"text","text":"hi"}]}}"#;
        let c = classify(line);
        assert!(c.ok);
        assert_eq!(c.event.event_type, EVENT_NOTIFICATION);
        assert_eq!(c.event.message, "hi");
        let u = c
            .event
            .usage
            .expect("assistant with usage should set Usage");
        assert_eq!(u.input_tokens, 50);
        assert_eq!(u.output_tokens, 10);
        assert_eq!(u.total_tokens, 60);
    }

    // Mirrors Go `claude.TestClassifyAssistantNoUsage`.
    #[test]
    fn classify_assistant_no_usage() {
        let line = br#"{"type":"assistant","session_id":"s","message":{"content":[{"type":"text","text":"hi"}]}}"#;
        let c = classify(line);
        assert!(c.ok);
        assert_eq!(c.event.event_type, EVENT_NOTIFICATION);
        assert_eq!(c.event.message, "hi");
        assert!(
            c.event.usage.is_none(),
            "assistant without usage should leave Usage None"
        );
    }

    // Mirrors Go `claude.TestClassifyAssistantUsageCacheTokens`: in/out stay uncached, cache tokens
    // fold into the BILLED total.
    #[test]
    fn classify_assistant_usage_cache_tokens() {
        let line = br#"{"type":"assistant","session_id":"s","message":{"usage":{"input_tokens":50,"output_tokens":10,"cache_creation_input_tokens":200,"cache_read_input_tokens":1000},"content":[{"type":"text","text":"hi"}]}}"#;
        let c = classify(line);
        assert!(c.ok);
        let u = c.event.usage.expect("usage present");
        assert_eq!(u.input_tokens, 50, "in stays uncached");
        assert_eq!(u.output_tokens, 10, "out stays uncached");
        assert_eq!(u.cache_creation_tokens, 200);
        assert_eq!(u.cache_read_tokens, 1000);
        // Billed total = 50+10+200+1000 = 1260.
        assert_eq!(u.total_tokens, 1260);
    }

    // Mirrors Go `claude.TestClassifyResultSuccess`.
    #[test]
    fn classify_result_success() {
        let line = br#"{"type":"result","subtype":"success","is_error":false,"session_id":"s","usage":{"input_tokens":120,"output_tokens":80}}"#;
        let c = classify(line);
        assert!(c.ok && c.terminal, "result should be a terminal event");
        assert_eq!(c.event.event_type, EVENT_TURN_COMPLETED);
        assert_eq!(c.result.status, TURN_SUCCEEDED);
        assert_eq!(c.result.usage.input_tokens, 120);
        assert_eq!(c.result.usage.output_tokens, 80);
        assert_eq!(c.result.usage.total_tokens, 200);
        assert_eq!(c.result.usage.cache_creation_tokens, 0);
        assert_eq!(c.result.usage.cache_read_tokens, 0);
    }

    // Mirrors Go `claude.TestClassifyResultCacheTokens`.
    #[test]
    fn classify_result_cache_tokens() {
        let line = br#"{"type":"result","subtype":"success","is_error":false,"session_id":"s","usage":{"input_tokens":120,"output_tokens":80,"cache_creation_input_tokens":5000,"cache_read_input_tokens":40000}}"#;
        let c = classify(line);
        assert!(c.ok, "result should classify");
        assert_eq!(c.result.usage.input_tokens, 120);
        assert_eq!(c.result.usage.output_tokens, 80);
        assert_eq!(c.result.usage.cache_creation_tokens, 5000);
        assert_eq!(c.result.usage.cache_read_tokens, 40000);
        assert_eq!(c.result.usage.total_tokens, 120 + 80 + 5000 + 40000);
    }

    // Mirrors Go `claude.TestClassifyResultError`.
    #[test]
    fn classify_result_error() {
        let line = br#"{"type":"result","subtype":"error_max_turns","is_error":true,"session_id":"s","usage":{"input_tokens":1,"output_tokens":2}}"#;
        let c = classify(line);
        assert!(c.ok && c.terminal, "error result should be terminal");
        assert_eq!(c.event.event_type, EVENT_TURN_FAILED);
        assert_eq!(c.result.status, TURN_FAILED);
    }

    // resultLine helper (Go `resultLine`): a `result` stream-json line carrying `text`, with JSON
    // escaping handled by the encoder.
    fn result_line(is_error: bool, text: &str) -> Vec<u8> {
        let payload = serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": is_error,
            "session_id": "s",
            "result": text,
            "usage": {"input_tokens": 1, "output_tokens": 2},
        });
        serde_json::to_vec(&payload).expect("encode result line")
    }

    // Mirrors Go `claude.TestClassifyResultSurfacesText`.
    #[test]
    fn classify_result_surfaces_text() {
        let c = classify(&result_line(false, "All done.\nHANDOFF: in-review"));
        assert!(c.ok, "result should classify");
        assert_eq!(c.result.result_text, "All done.\nHANDOFF: in-review");
        // Error results carry their text too.
        let c_err = classify(&result_line(true, "ran out of turns"));
        assert_eq!(c_err.result.status, TURN_FAILED);
        assert_eq!(c_err.result.result_text, "ran out of turns");
    }

    // Mirrors Go `claude.TestClassifyResultKeepsTail`: a >4KB final message keeps its LAST 4096
    // bytes so a trailing HANDOFF: line survives.
    #[test]
    fn classify_result_keeps_tail() {
        let marker = "\nHANDOFF: in-review";
        let text = format!("{}{marker}", "x".repeat(8000));
        let c = classify(&result_line(false, &text));
        assert!(c.ok, "result should classify");
        assert_eq!(
            c.result.result_text.len(),
            4096,
            "tail-truncated to 4096 bytes"
        );
        assert!(
            c.result.result_text.ends_with(marker),
            "must keep the trailing marker"
        );
    }

    // Mirrors Go `claude.TestClassifyResultTailIsValidUTF8`: when the 4096-byte cut lands mid-rune,
    // the leading partial rune is dropped so the surfaced tail stays valid UTF-8.
    #[test]
    fn classify_result_tail_is_valid_utf8() {
        let marker = "\nHANDOFF: in-review";
        // "世" is 3 bytes. Pad so the total is MAX_RESULT_TEXT+1 bytes: the tail cut lands one byte
        // into the leading rune, leaving two continuation bytes at the front.
        let body = format!("{}{marker}", "a".repeat(MAX_RESULT_TEXT - 2 - marker.len()));
        let text = format!("世{body}"); // 3 + (MAX_RESULT_TEXT-2) = MAX_RESULT_TEXT+1 bytes
        let c = classify(&result_line(false, &text));
        assert!(c.ok, "result should classify");
        assert!(
            c.result.result_text.len() <= MAX_RESULT_TEXT,
            "len {} must be <= {MAX_RESULT_TEXT}",
            c.result.result_text.len()
        );
        assert_eq!(
            c.result.result_text, body,
            "should drop the split leading rune and keep the rest intact"
        );
        assert!(
            c.result.result_text.ends_with(marker),
            "must keep the marker"
        );
    }

    // Mirrors Go `claude.TestClassifyUnknownAndBlank`.
    #[test]
    fn classify_unknown_and_blank() {
        assert!(
            !classify(br#"{"type":"user"}"#).ok,
            "unknown type → no event"
        );
        assert!(!classify(b"   ").ok, "blank line → no event");
        assert!(!classify(b"not json").ok, "non-json → no event");
    }
}
