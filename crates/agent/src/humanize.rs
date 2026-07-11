//! Event humanizer for the UI — a parity port of Go `internal/agent/humanize.go`.
//!
//! [`humanize_stream_line`] parses one raw claude stream-json line into zero or more [`LogEntry`]
//! values (the shape the `/log` API endpoint consumes). It is a pure, defensive transform: it never
//! panics on missing/odd fields and tolerates a partially-written final line (invalid JSON → empty).

use serde::Deserialize;
use serde_json::value::RawValue;
use std::collections::BTreeMap;

/// `LogEntry` is one humanized line of a claude stream-json transcript: the shared shape produced
/// by [`humanize_stream_line`] and consumed by the `/log` API endpoint (the HTTP layer assigns the
/// 1-based `seq` + the wire json tags). It carries no `seq` and no json tags on purpose, mirroring
/// the reference split so the humanizer stays a pure transform.
///
/// `kind` is one of: `"thinking"`, `"text"`, `"tool_use"`, `"tool_result"`, `"event"`. `tool` is
/// set only on `tool_use` entries (the tool's name). `text` is a short, single-line summary
/// appropriate to the kind (assistant text, a compact tool-input summary, a short tool-result
/// excerpt, or an event label).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogEntry {
    pub kind: String,
    pub tool: String,
    pub text: String,
}

/// Bounds summarized tool I/O (inputs/results) — the usual verbosity offenders.
const MAX_LOG_TEXT_RUNES: usize = 240;

/// Bounds agent PROSE (text/thinking). Far more generous than [`MAX_LOG_TEXT_RUNES`] so reasoning
/// reads in full, while still capping pathological dumps (an echoed skill doc / a whole
/// WORKFLOW.md). The dashboard collapses anything long behind a "Show more" toggle.
const MAX_PROSE_RUNES: usize = 4000;

/// The defensively-decoded shape used only by the humanizer. Kept separate from the parser's line
/// type so the richer fields the transcript needs don't perturb event mapping or billing. Every
/// field is optional (container-level `#[serde(default)]`): any absent field contributes its zero
/// value rather than failing the decode.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct HumanizeLine {
    #[serde(rename = "type")]
    r#type: String,
    subtype: String,
    /// `result` carries `is_error`.
    is_error: bool,
    /// Content can live at the top level (user/tool_result lines) or nested under `message`
    /// (assistant lines). Both are decoded; `message.content` is preferred when present.
    content: Vec<HumanizeBlock>,
    message: Option<HumanizeMessage>,
    rate_limit_info: Option<RateLimitInfo>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct HumanizeMessage {
    content: Vec<HumanizeBlock>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RateLimitInfo {
    #[serde(rename = "rateLimitType")]
    rate_limit_type: String,
    #[serde(rename = "resetsAt")]
    resets_at: String,
}

/// One content block. Fields are read defensively: any block whose shape we don't recognize
/// contributes nothing rather than panicking. `input` / `content` stay as raw JSON (Go's
/// `json.RawMessage`) and are re-parsed contextually by the summarizers.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct HumanizeBlock {
    #[serde(rename = "type")]
    r#type: String,
    /// text block
    text: String,
    /// thinking block
    thinking: String,
    /// tool_use block: the tool's name
    name: String,
    /// tool_use block: the tool input object
    input: Option<Box<RawValue>>,
    /// tool_result content can be a string OR an array of blocks (e.g. `[{type:text,text:…}]`).
    content: Option<Box<RawValue>>,
}

/// Parses one raw stream-json line into zero or more [`LogEntry`] values, oldest content-block
/// first. The returned vec is empty for lines that carry nothing meaningful (unknown types, empty
/// assistant messages). It never panics on missing/odd fields and tolerates a partially-written
/// final line (invalid JSON → empty).
pub fn humanize_stream_line(raw: &[u8]) -> Vec<LogEntry> {
    if String::from_utf8_lossy(raw).trim().is_empty() {
        return Vec::new();
    }
    let line: HumanizeLine = match serde_json::from_slice(raw) {
        Ok(l) => l,
        Err(_) => return Vec::new(),
    };

    match line.r#type.as_str() {
        "system" => {
            if line.subtype == "init" {
                vec![event_entry("session started")]
            } else {
                Vec::new()
            }
        }
        "result" => {
            if line.is_error {
                let label = if line.subtype.is_empty() {
                    "turn failed".to_string()
                } else {
                    format!("turn failed: {}", line.subtype)
                };
                vec![event_entry(&label)]
            } else {
                vec![event_entry("turn completed")]
            }
        }
        "rate_limit_event" => match &line.rate_limit_info {
            Some(info) => vec![event_entry(&format!(
                "rate limit: {} resets {}",
                info.rate_limit_type, info.resets_at
            ))],
            None => vec![event_entry("rate limit")],
        },
        "assistant" | "user" => {
            let blocks = match &line.message {
                Some(m) if !m.content.is_empty() => &m.content,
                _ => &line.content,
            };
            humanize_blocks(blocks)
        }
        _ => Vec::new(),
    }
}

/// Builds an `"event"`-kind entry with the given label.
fn event_entry(text: &str) -> LogEntry {
    LogEntry {
        kind: "event".to_string(),
        tool: String::new(),
        text: text.to_string(),
    }
}

/// Folds a content-block slice into entries (one per meaningful block).
fn humanize_blocks(blocks: &[HumanizeBlock]) -> Vec<LogEntry> {
    let mut out = Vec::new();
    for b in blocks {
        match b.r#type.as_str() {
            "text" => {
                // Agent PROSE keeps its newlines and most of its length (generous MAX_PROSE_RUNES
                // cap; the dashboard collapses long entries behind "Show more") — no first-line clip.
                let t = b.text.trim();
                if !t.is_empty() {
                    out.push(LogEntry {
                        kind: "text".to_string(),
                        tool: String::new(),
                        text: truncate(t, MAX_PROSE_RUNES),
                    });
                }
            }
            "thinking" => {
                let t = b.thinking.trim();
                if !t.is_empty() {
                    out.push(LogEntry {
                        kind: "thinking".to_string(),
                        tool: String::new(),
                        text: truncate(t, MAX_PROSE_RUNES),
                    });
                }
            }
            "tool_use" | "server_tool_use" => {
                out.push(LogEntry {
                    kind: "tool_use".to_string(),
                    tool: b.name.clone(),
                    text: truncate(&summarize_input(b.input.as_deref()), MAX_LOG_TEXT_RUNES),
                });
            }
            "tool_result" => {
                let s = summarize_result(b.content.as_deref());
                let text = if s.is_empty() {
                    "(ok)".to_string()
                } else {
                    truncate(&s, MAX_LOG_TEXT_RUNES)
                };
                out.push(LogEntry {
                    kind: "tool_result".to_string(),
                    tool: String::new(),
                    text,
                });
            }
            _ => {}
        }
    }
    out
}

/// Renders a tool_use input object as a compact one-line summary. Object inputs become `key=value`
/// pairs (sorted for determinism via [`BTreeMap`], values clipped); other JSON shapes are collapsed
/// to a single line. Empty/absent input yields `""`.
fn summarize_input(raw: Option<&RawValue>) -> String {
    let Some(raw) = raw else {
        return String::new();
    };
    let text = raw.get();
    if let Ok(obj) = serde_json::from_str::<BTreeMap<String, Box<RawValue>>>(text) {
        // BTreeMap iterates in sorted key order — the mirror of Go's `sort.Strings(keys)`.
        let parts: Vec<String> = obj
            .iter()
            .map(|(k, v)| format!("{k}={}", clip_value(v)))
            .collect();
        return parts.join(" ");
    }
    collapse_ws(text)
}

/// Renders one JSON value for an input summary: strings unquoted+clipped, other scalars/compound
/// shapes collapsed to a single short line.
fn clip_value(raw: &RawValue) -> String {
    if let Ok(s) = serde_json::from_str::<String>(raw.get()) {
        return truncate(&collapse_ws(&s), 60);
    }
    truncate(&collapse_ws(raw.get()), 60)
}

/// Renders tool_result content (which may be a bare string or an array of content blocks) as a
/// short single line.
fn summarize_result(raw: Option<&RawValue>) -> String {
    let Some(raw) = raw else {
        return String::new();
    };
    let text = raw.get();
    if let Ok(s) = serde_json::from_str::<String>(text) {
        return first_line(&s);
    }
    if let Ok(blocks) = serde_json::from_str::<Vec<HumanizeBlock>>(text) {
        for b in &blocks {
            let t = first_line(&b.text);
            if !t.is_empty() {
                return t;
            }
        }
        return String::new();
    }
    collapse_ws(text)
}

/// Flattens all runs of whitespace (incl. newlines) to single spaces and trims, so a multi-line
/// JSON value renders as one tidy line.
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Collapses `s` to its first non-empty line and trims surrounding whitespace, so a multi-line
/// assistant message renders as a single timeline row.
fn first_line(s: &str) -> String {
    for ln in s.split('\n') {
        let t = ln.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    s.trim().to_string()
}

/// Clamps `s` to at most `max` runes (chars), appending an ellipsis when it cuts.
fn truncate(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    if max <= 1 {
        return chars[..max].iter().collect();
    }
    let mut out: String = chars[..max - 1].iter().collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors Go `agent.TestHumanizeStreamLine_SystemInit` (humanize_test.go).
    #[test]
    fn system_init() {
        let got = humanize_stream_line(br#"{"type":"system","subtype":"init"}"#);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, "event");
        assert_eq!(got[0].text, "session started");
        // Non-init system carries nothing.
        assert!(humanize_stream_line(br#"{"type":"system","subtype":"other"}"#).is_empty());
    }

    // Mirrors Go `agent.TestHumanizeStreamLine_Result`.
    #[test]
    fn result() {
        let ok = humanize_stream_line(br#"{"type":"result"}"#);
        assert_eq!(ok.len(), 1);
        assert_eq!(ok[0].kind, "event");
        assert_eq!(ok[0].text, "turn completed");

        let err =
            humanize_stream_line(br#"{"type":"result","is_error":true,"subtype":"max_turns"}"#);
        assert_eq!(err.len(), 1);
        assert_eq!(err[0].kind, "event");
        assert_eq!(err[0].text, "turn failed: max_turns");

        let err_no_subtype = humanize_stream_line(br#"{"type":"result","is_error":true}"#);
        assert_eq!(err_no_subtype.len(), 1);
        assert_eq!(err_no_subtype[0].text, "turn failed");
    }

    // Mirrors Go `agent.TestHumanizeStreamLine_RateLimit`.
    #[test]
    fn rate_limit() {
        let got = humanize_stream_line(
            br#"{"type":"rate_limit_event","rate_limit_info":{"rateLimitType":"primary","resetsAt":"2026-06-01T00:00:00Z"}}"#,
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, "event");
        assert_eq!(
            got[0].text,
            "rate limit: primary resets 2026-06-01T00:00:00Z"
        );

        let no_info = humanize_stream_line(br#"{"type":"rate_limit_event"}"#);
        assert_eq!(no_info.len(), 1);
        assert_eq!(no_info[0].text, "rate limit");
    }

    // Mirrors Go `agent.TestHumanizeStreamLine_AssistantText`: prose kept in full (multi-line), no
    // first-line clip.
    #[test]
    fn assistant_text() {
        let got = humanize_stream_line(
            br#"{"type":"assistant","message":{"content":[{"type":"text","text":"hello\nworld"}]}}"#,
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, "text");
        assert_eq!(got[0].text, "hello\nworld");
    }

    // Mirrors Go `agent.TestHumanizeStreamLine_Thinking`: full thinking kept (multi-line).
    #[test]
    fn thinking() {
        let got = humanize_stream_line(
            br#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"let me think\nmore"}]}}"#,
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, "thinking");
        assert_eq!(got[0].text, "let me think\nmore");
    }

    // Mirrors Go `agent.TestHumanizeStreamLine_ToolUse`: sorted k=v summary, command before timeout.
    #[test]
    fn tool_use() {
        let got = humanize_stream_line(
            br#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls -la","timeout":5}}]}}"#,
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, "tool_use");
        assert_eq!(got[0].tool, "Bash");
        assert_eq!(got[0].text, "command=ls -la timeout=5");
    }

    // Mirrors Go `agent.TestHumanizeStreamLine_ToolResult`.
    #[test]
    fn tool_result() {
        // String content.
        let s = humanize_stream_line(
            br#"{"type":"user","message":{"content":[{"type":"tool_result","content":"output line 1\nline 2"}]}}"#,
        );
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].kind, "tool_result");
        assert_eq!(s[0].text, "output line 1");

        // Array-of-blocks content.
        let blocks = humanize_stream_line(
            br#"{"type":"user","message":{"content":[{"type":"tool_result","content":[{"type":"text","text":"blocked output"}]}]}}"#,
        );
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "blocked output");

        // Empty content -> "(ok)".
        let empty = humanize_stream_line(
            br#"{"type":"user","message":{"content":[{"type":"tool_result"}]}}"#,
        );
        assert_eq!(empty.len(), 1);
        assert_eq!(empty[0].text, "(ok)");
    }

    // Mirrors Go `agent.TestHumanizeStreamLine_TopLevelContent`: content at top level (not nested
    // under message).
    #[test]
    fn top_level_content() {
        let got = humanize_stream_line(
            br#"{"type":"user","content":[{"type":"text","text":"top-level"}]}"#,
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "top-level");
    }

    // Mirrors Go `agent.TestHumanizeStreamLine_Tolerant`.
    #[test]
    fn tolerant() {
        assert!(humanize_stream_line(b"").is_empty());
        assert!(humanize_stream_line(b"   \n").is_empty());
        assert!(humanize_stream_line(br#"{"type":"assistant","message":{"#).is_empty());
        assert!(humanize_stream_line(br#"{"type":"unknown"}"#).is_empty());
    }

    // Mirrors Go `agent.TestHumanizeStreamLine_Truncation`.
    #[test]
    fn truncation() {
        let big = "x".repeat(400);
        // Agent PROSE (text) is shown in full — no rune cap.
        let full = humanize_stream_line(
            format!(
                r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{big}"}}]}}}}"#
            )
            .as_bytes(),
        );
        assert_eq!(full.len(), 1);
        assert_eq!(full[0].kind, "text");
        assert_eq!(full[0].text.chars().count(), 400, "prose not truncated");

        // Tool OUTPUTS stay summarized — capped to MAX_LOG_TEXT_RUNES.
        let got = humanize_stream_line(
            format!(r#"{{"type":"user","content":[{{"type":"tool_result","content":"{big}"}}]}}"#)
                .as_bytes(),
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text.chars().count(), MAX_LOG_TEXT_RUNES);
    }
}
